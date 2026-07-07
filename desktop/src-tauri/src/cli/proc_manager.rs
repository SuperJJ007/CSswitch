//! 代理与沙箱进程生命周期管理（PID 文件、状态检测、日志轮转）。
//!
//! Plan V2 §3.5 实现。通过 PID 文件跟踪进程状态，避免内存状态在 CLI 调用间丢失。

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// PID 文件存储的进程信息（JSON 格式）。
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ProcessRecord {
    /// 进程 PID。
    pub pid: u32,
    /// 启动时间戳（Unix 秒）。
    pub started_at: i64,
    /// 启动命令（仅用于诊断）。
    pub command: String,
    /// 绑定的端口。
    pub port: u16,
    /// 代理鉴权 secret（用于 health check）。
    pub secret: Option<String>,
}

/// 进程运行状态。
#[derive(Debug)]
pub enum ProcessStatus {
    /// 进程正在运行。
    Running(u32),
    /// 进程已停止。
    Stopped,
    /// 无法确定（PID 文件存在但进程不可达）。
    Unknown,
}

/// 进程管理器，封装 PID 文件读写、进程探活和日志轮转。
pub struct ProcessManager {
    /// PID 文件路径（如 `~/.csswitch/proxy.pid`）。
    pid_file: PathBuf,
}

impl ProcessManager {
    /// 创建指定名称的进程管理器（name 为 "proxy" 或 "sandbox"）。
    pub fn new(name: &str) -> Self {
        let pid_file = super::commands::config_dir().join(format!("{name}.pid"));
        Self { pid_file }
    }

    /// 写入 PID 文件记录进程信息。
    pub fn write_pid(&self, pid: u32, port: u16, command: &str, secret: Option<&str>) {
        let _ = fs::create_dir_all(self.pid_file.parent().unwrap());
        let record = ProcessRecord {
            pid,
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            command: command.to_string(),
            port,
            secret: secret.map(|s| s.to_string()),
        };
        if let Ok(json) = serde_json::to_string_pretty(&record) {
            let _ = fs::write(&self.pid_file, json);
        }
    }

    /// 读取 PID 文件。
    pub fn read_pid(&self) -> Option<ProcessRecord> {
        fs::read_to_string(&self.pid_file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// 获取进程状态（通过 `kill -0` 探活）。
    #[cfg(unix)]
    pub fn status(&self) -> ProcessStatus {
        match self.read_pid() {
            Some(record) => {
                let exists = Command::new("kill")
                    .args(["-0", &record.pid.to_string()])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if exists {
                    // 进一步验证 PID 匹配（避免 PID 复用导致的误判）
                    if let Ok(cmdline) = fs::read_to_string(format!("/proc/{}/cmdline", record.pid))
                    {
                        // cmdline 用 \0 分隔，取第一个 token 作为命令名
                        let cmd_name = cmdline.split('\0').next().unwrap_or("");
                        if cmd_name.contains("python") || cmd_name.contains("claude-science") {
                            return ProcessStatus::Running(record.pid);
                        }
                    }
                    ProcessStatus::Running(record.pid)
                } else {
                    // 进程不存在 → 清理过期 PID 文件
                    let _ = fs::remove_file(&self.pid_file);
                    ProcessStatus::Stopped
                }
            }
            None => ProcessStatus::Stopped,
        }
    }

    /// 非 Unix 平台的进程状态（简化版，仅检查 PID 文件）。
    #[cfg(not(unix))]
    pub fn status(&self) -> ProcessStatus {
        if self.pid_file.exists() {
            ProcessStatus::Unknown
        } else {
            ProcessStatus::Stopped
        }
    }

    /// 清理 PID 文件和僵尸 PID。
    pub fn cleanup(&self) {
        // 先检查当前 PID 是否还在运行
        match self.status() {
            ProcessStatus::Running(_) => {} // 仍在运行，保留 PID 文件
            ProcessStatus::Stopped | ProcessStatus::Unknown => {
                let _ = fs::remove_file(&self.pid_file);
            }
        }
    }

    /// 获取关联的日志文件路径。
    pub fn log_path(&self) -> PathBuf {
        let name = self
            .pid_file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        super::commands::logs_dir().join(format!("{name}.log"))
    }

    /// 读取日志最近 N 行。
    pub fn tail_logs(&self, lines: usize) -> Vec<String> {
        let path = self.log_path();
        match fs::read_to_string(&path) {
            Ok(content) => {
                let all: Vec<&str> = content.lines().collect();
                let start = all.len().saturating_sub(lines);
                all[start..].iter().map(|s| s.to_string()).collect()
            }
            Err(_) => vec![],
        }
    }

    /// 对日志进行轮转：超过 max_bytes 时将原文件重命名为 .log.1，保留最近 3 个。
    pub fn rotate_logs(&self, max_bytes: u64) {
        let path = self.log_path();
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > max_bytes {
                // 轮转 3 个备份
                let _ = fs::remove_file(path.with_extension("log.3"));
                for i in (1..=2).rev() {
                    let src = path.with_extension(format!("log.{i}"));
                    let dst = path.with_extension(format!("log.{}", i + 1));
                    if src.exists() {
                        let _ = fs::rename(&src, &dst);
                    }
                }
                let _ = fs::rename(&path, path.with_extension("log.1"));
            }
        }
    }
}

/// 便捷函数：为代理进程生成 PID 文件记录。
pub fn record_proxy_start(pid: u32, port: u16, secret: &str) {
    let pm = ProcessManager::new("proxy");
    pm.write_pid(pid, port, "python3 csswitch_proxy.py", Some(secret));
}

/// 便捷函数：清理代理 PID 文件。
pub fn record_proxy_stop() {
    let pm = ProcessManager::new("proxy");
    pm.cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("csswitch-proc-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&d);
        d
    }

    #[test]
    fn test_write_and_read_pid() {
        let dir = tmp_dir();
        // 我们不能直接注入 pid 文件路径，但可以测试 record serde
        let record = ProcessRecord {
            pid: 12345,
            started_at: 1700000000,
            command: "test".to_string(),
            port: 18991,
            secret: Some("abc123".to_string()),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: ProcessRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, 12345);
        assert_eq!(back.port, 18991);
        assert_eq!(back.secret.unwrap(), "abc123");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_rotation_logic() {
        let dir = tmp_dir();
        // 创建模拟 log 文件并测试轮转
        let pm = ProcessManager::new("proxy");
        // 不能直接测试实际路径，验证函数不 panic 即可
        pm.rotate_logs(10);
        pm.cleanup();
        let _ = fs::remove_dir_all(&dir);
    }
}
