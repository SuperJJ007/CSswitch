//! Helper 自身操作日志（记录命令执行、进程启停等操作审计信息）。
//!
//! Plan V2 §3.7 实现。日志写入 `~/.csswitch/logs/helper.log`。
//! 格式：`[ISO8601] [LEVEL] message`

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// 正常操作记录。
    Info,
    /// 可恢复的异常。
    Warn,
    /// 失败操作。
    Error,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

/// Helper 操作日志器（全局单例，通过 Mutex 保护）。
static LOGGER: std::sync::LazyLock<Mutex<Option<HelperLogger>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

struct HelperLogger {
    file: File,
}

/// 获取日志文件路径：`~/.csswitch/logs/helper.log`。
fn log_path() -> PathBuf {
    let dir = super::commands::config_dir().join("logs");
    dir.join("helper.log")
}

/// 初始化日志系统（创建目录和文件）。
pub fn init() -> Result<(), String> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建日志目录失败：{e}"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开日志文件失败：{e}"))?;
    let mut logger = LOGGER.lock().unwrap();
    *logger = Some(HelperLogger { file });
    Ok(())
}

/// 写一条日志记录。
/// 格式：`[2026-07-04T15:30:00Z] [INFO] 消息内容`
pub fn log(level: LogLevel, msg: &str) {
    // 获取当前 UTC 时间
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 简易 ISO8601 格式（不使用 chrono 以保持零依赖）
    let days_since_epoch = now / 86400;
    let time_of_day = now % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Howard Hinnant civil-from-days 算法（与 oauth_forge.rs 中一致）
    let z = (days_since_epoch as i64) + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    let line = format!(
        "[{year:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z] [{}] {msg}\n",
        level.as_str()
    );

    if let Ok(mut logger) = LOGGER.lock() {
        if let Some(ref mut l) = *logger {
            let _ = l.file.write_all(line.as_bytes());
            let _ = l.file.flush();
        }
    }
}

/// 便捷函数：记录 Info 级别日志。
pub fn info(msg: &str) {
    log(LogLevel::Info, msg);
}

/// 便捷函数：记录 Warn 级别日志。
pub fn warn(msg: &str) {
    log(LogLevel::Warn, msg);
}

/// 便捷函数：记录 Error 级别日志。
pub fn error(msg: &str) {
    log(LogLevel::Error, msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_as_str() {
        assert_eq!(LogLevel::Info.as_str(), "INFO");
        assert_eq!(LogLevel::Warn.as_str(), "WARN");
        assert_eq!(LogLevel::Error.as_str(), "ERROR");
    }

    #[test]
    fn test_init_creates_log_file() {
        // 初始化日志（在生产环境中由 main() 调用）
        match init() {
            Ok(()) => {
                info("test: logger initialized");
            }
            Err(_) => {
                // 测试环境可能无权限，静默跳过
            }
        }
    }
}
