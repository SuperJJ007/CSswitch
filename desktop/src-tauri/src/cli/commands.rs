//! Helper CLI 的命令实现。
//!
//! 每个命令返回 `CliEnvelope`，由 `mod.rs` 中的 `dispatch()` 函数调用。
//! 管理远程服务器上的 `csswitch_proxy.py` 代理进程、`~/.csswitch/config.json` 配置、
//! Claude Science 沙箱和日志文件。

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use serde_json::{json, Value};

use super::types::CliEnvelope;

// ============================================================================
// 全局状态（进程句柄，仅在 serve 模式下跨请求复用）
// ============================================================================

/// 代理子进程句柄（用于 serve 模式跨请求管理代理生命周期）。
static PROXY_CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// 代理运行时信息（PID、端口）。
static PROXY_INFO: Mutex<Option<ProxyInfo>> = Mutex::new(None);

struct ProxyInfo {
    pid: u32,
    port: u16,
    secret: String,
}

// ============================================================================
// 路径工具
// ============================================================================

/// 获取 `~/.csswitch` 目录路径。
fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".csswitch")
}

/// 获取 `~/.csswitch/config.json` 路径。
fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// 获取 `~/.csswitch/logs/` 目录路径。
fn logs_dir() -> PathBuf {
    config_dir().join("logs")
}

/// 定位 `proxy/csswitch_proxy.py`：
/// 1. `CSSWITCH_PROXY_DIR` 环境变量
/// 2. Helper 二进制同级目录（部署态）
/// 3. 相对路径（开发态）
fn proxy_script_path() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CSSWITCH_PROXY_DIR") {
        let p = PathBuf::from(&dir).join("csswitch_proxy.py");
        if p.is_file() {
            return Ok(p);
        }
    }
    // Helper 二进制同级目录
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("proxy").join("csswitch_proxy.py");
            if p.is_file() {
                return Ok(p);
            }
            let p = dir.join("..").join("proxy").join("csswitch_proxy.py");
            if p.is_file() {
                return Ok(p.canonicalize().unwrap_or(p));
            }
        }
    }
    Err("找不到 proxy/csswitch_proxy.py。请设置 CSSWITCH_PROXY_DIR 环境变量。".to_string())
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 从 `~/.csswitch/config.json` 读取指定 provider 的 key。
fn load_key_from_config(provider: &str) -> Result<Option<String>, String> {
    let cfg = config_path();
    if !cfg.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&cfg).map_err(|e| format!("读配置失败：{e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析配置失败：{e}"))?;
    Ok(v.get("providers")
        .and_then(|p| p.get(provider))
        .and_then(|p| p.get("key"))
        .and_then(|k| k.as_str())
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string()))
}

/// 通过 HTTP GET /health 探活本地代理。
fn proxy_health(port: u16, secret: &str) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = format!("127.0.0.1:{port}");
    let Ok(mut stream) = TcpStream::connect_timeout(
        &addr.parse().unwrap(),
        std::time::Duration::from_millis(500),
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let req = format!("GET /{secret}/health HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.lines().next().map_or(false, |line| line.contains("200"))
}

// ============================================================================
// 命令实现
// ============================================================================

/// `status` — 返回 Helper 版本、能力列表、代理/沙箱运行状态。
pub fn cmd_status() -> CliEnvelope {
    let capabilities: Vec<&str> = vec!["proxy", "sandbox", "config", "logs", "doctor", "verify"];
    let proxy_running = PROXY_INFO.lock().unwrap().is_some();
    CliEnvelope::ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "capabilities": capabilities,
        "proxy_running": proxy_running,
        "sandbox_running": false,
    }))
}

/// `config get` — 读取 `~/.csswitch/config.json` 并返回（key 已掩码）。
pub fn cmd_config_get() -> CliEnvelope {
    let path = config_path();
    if !path.exists() {
        return CliEnvelope::ok(json!({
            "provider": "deepseek",
            "proxy_port": 18991,
            "sandbox_port": 8990,
            "mode": "proxy",
            "keys": {}
        }));
    }
    match fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(mut cfg) => {
                // 掩码所有 provider key（只保留末 4 位）
                if let Some(providers) = cfg.get_mut("providers").and_then(|v| v.as_object_mut()) {
                    for (_name, prov) in providers.iter_mut() {
                        if let Some(key) = prov.get("key").and_then(|k| k.as_str()) {
                            let masked = if key.len() > 4 {
                                format!("{}{}", "•".repeat(key.len() - 4), &key[key.len() - 4..])
                            } else {
                                "••••".to_string()
                            };
                            prov["key"] = json!(masked);
                        }
                    }
                }
                CliEnvelope::ok(cfg)
            }
            Err(e) => CliEnvelope::err("config_parse_error", &format!("配置文件格式错误：{e}")),
        },
        Err(e) => CliEnvelope::err("config_read_error", &format!("无法读取配置文件：{e}")),
    }
}

/// `config set <json>` — 写入 `~/.csswitch/config.json`。
pub fn cmd_config_set(json_str: &str) -> CliEnvelope {
    let v: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return CliEnvelope::err("config_parse_error", &format!("JSON 解析失败：{e}")),
    };
    let dir = config_dir();
    let path = config_path();
    if let Err(e) = fs::create_dir_all(&dir) {
        return CliEnvelope::err("config_write_error", &format!("创建配置目录失败：{e}"));
    }
    let json = match serde_json::to_vec_pretty(&v) {
        Ok(j) => j,
        Err(e) => return CliEnvelope::err("config_serialize_error", &format!("序列化失败：{e}")),
    };
    if let Err(e) = fs::write(&path, &json) {
        return CliEnvelope::err("config_write_error", &format!("无法写入配置文件：{e}"));
    }
    CliEnvelope::ok_empty()
}

/// `config save-key <provider> <key>` — 保存 provider key。
pub fn cmd_config_save_key(provider: &str, key: &str) -> CliEnvelope {
    let path = config_path();
    let dir = config_dir();
    let _ = fs::create_dir_all(&dir);

    let mut cfg: Value = if path.exists() {
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or(json!({})),
            Err(_) => json!({}),
        }
    } else {
        json!({
            "provider": "deepseek",
            "proxy_port": 18991,
            "sandbox_port": 8990,
            "mode": "proxy",
        })
    };

    // 确保 providers 对象存在
    if cfg.get("providers").is_none() {
        cfg["providers"] = json!({});
    }
    cfg["providers"][provider] = json!({"key": key});

    let json_bytes = match serde_json::to_vec_pretty(&cfg) {
        Ok(j) => j,
        Err(e) => return CliEnvelope::err("config_serialize_error", &format!("序列化失败：{e}")),
    };
    if let Err(e) = fs::write(&path, &json_bytes) {
        return CliEnvelope::err("config_write_error", &format!("无法写入配置文件：{e}"));
    }

    // 返回掩码后的 key
    let masked = if key.len() > 4 {
        format!("{}{}", "•".repeat(key.len() - 4), &key[key.len() - 4..])
    } else {
        "••••".to_string()
    };
    CliEnvelope::ok(json!({"masked": masked}))
}

/// `proxy start <provider> <port> <secret>` — 启动代理进程。
pub fn cmd_proxy_start(provider: &str, port: u16, secret: &str) -> CliEnvelope {
    // 检查是否已在运行
    {
        let info = PROXY_INFO.lock().unwrap();
        if let Some(ref pi) = *info {
            if proxy_health(pi.port, &pi.secret) {
                return CliEnvelope::err("proxy_already_running", &format!("代理已在端口 {} 上运行", pi.port));
            }
        }
    }

    // 获取需要注入的 key
    let key = match load_key_from_config(provider) {
        Ok(Some(k)) => k,
        Ok(None) => return CliEnvelope::err_with_hint(
            "key_not_found",
            &format!("配置中未找到 {provider} 的 API key"),
            "请先在客户端面板填写并保存 API Key。",
        ),
        Err(e) => return CliEnvelope::err("config_read_error", &e),
    };

    // 定位 python3
    let python = match find_cmd("python3") {
        Some(p) => p,
        None => {
            // 尝试 python
            match find_cmd("python") {
                Some(p) => p,
                None => return CliEnvelope::err_with_hint(
                    "python_not_found",
                    "远程服务器上未找到 Python 3。",
                    "请在服务器上安装 Python 3.8+（apt install python3 或 yum install python3）。",
                ),
            }
        }
    };

    let script = match proxy_script_path() {
        Ok(p) => p,
        Err(e) => return CliEnvelope::err("proxy_script_not_found", &e),
    };

    let key_env = match provider {
        "qwen" => "DASHSCOPE_API_KEY",
        _ => "DEEPSEEK_API_KEY",
    };

    // 启代理子进程
    match Command::new(&python)
        .arg(&script)
        .arg("--provider")
        .arg(provider)
        .arg("--port")
        .arg(port.to_string())
        .arg("--auth-token")
        .arg(secret)
        .env(key_env, &key)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => {
            let pid = child.id();
            let mut pi = PROXY_CHILD.lock().unwrap();
            *pi = Some(child);
            let mut info = PROXY_INFO.lock().unwrap();
            *info = Some(ProxyInfo {
                pid,
                port,
                secret: secret.to_string(),
            });
            CliEnvelope::ok(json!({
                "port": port,
                "pid": pid,
                "message": "代理已启动",
            }))
        }
        Err(e) => {
            let hint = if e.to_string().contains("AddrInUse") || e.to_string().contains("address in use") {
                format!("端口 {port} 已被占用。请更改端口或停止占用程序。")
            } else {
                format!("启动代理失败：{e}")
            };
            CliEnvelope::err_with_hint("proxy_start_failed", &format!("启动代理失败：{e}"), &hint)
        }
    }
}

/// `proxy stop` — 停止代理进程。
pub fn cmd_proxy_stop() -> CliEnvelope {
    let mut child = PROXY_CHILD.lock().unwrap();
    if let Some(mut c) = child.take() {
        // SIGTERM → 等待 3s → SIGKILL
        let _ = c.kill();
        let _ = c.wait();
    }
    let mut info = PROXY_INFO.lock().unwrap();
    *info = None;
    CliEnvelope::ok_empty()
}

/// `proxy status` — 返回代理运行状态。
pub fn cmd_proxy_status() -> CliEnvelope {
    let info = PROXY_INFO.lock().unwrap();
    match info.as_ref() {
        Some(pi) => {
            let healthy = proxy_health(pi.port, &pi.secret);
            CliEnvelope::ok(json!({
                "running": true,
                "pid": pi.pid,
                "port": pi.port,
                "healthy": healthy,
            }))
        }
        None => {
            CliEnvelope::ok(json!({
                "running": false,
                "healthy": false,
            }))
        }
    }
}

/// `sandbox status` — 这里做简单占位，沙箱管理细节待进一步实现。
pub fn cmd_sandbox_status() -> CliEnvelope {
    // 检查 Claude Science 是否在运行（简化实现）
    CliEnvelope::ok(json!({
        "running": false,
        "message": "沙箱管理暂未实现。请在服务器上手动管理 Claude Science。",
    }))
}

/// `logs <name> [lines]` — 返回日志。
pub fn cmd_logs(name: &str, lines: Option<usize>) -> CliEnvelope {
    let log_path = logs_dir().join(format!("{name}.log"));
    if !log_path.exists() {
        return CliEnvelope::ok(json!({"content": "", "exists": false}));
    }
    match fs::read_to_string(&log_path) {
        Ok(content) => {
            let lines_count = lines.unwrap_or(100);
            let tail: String = content
                .lines()
                .rev()
                .take(lines_count)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            CliEnvelope::ok(json!({"content": tail, "exists": true}))
        }
        Err(e) => CliEnvelope::err("log_read_error", &format!("无法读取日志：{e}")),
    }
}

/// `doctor` — 诊断命令。
pub fn cmd_doctor() -> CliEnvelope {
    let mut checks: Vec<Value> = Vec::new();

    // 检查 python3
    let python = find_cmd("python3").or_else(|| find_cmd("python"));
    checks.push(json!({
        "name": "Python 3",
        "ok": python.is_some(),
        "detail": python.as_deref().unwrap_or("未找到"),
    }));

    // 检查代理脚本
    let script = proxy_script_path();
    checks.push(json!({
        "name": "代理脚本 csswitch_proxy.py",
        "ok": script.is_ok(),
        "detail": script.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|e| e.clone()),
    }));

    // 检查配置目录
    let cfg = config_path();
    checks.push(json!({
        "name": "配置文件 config.json",
        "ok": cfg.exists(),
        "detail": cfg.display().to_string(),
    }));

    // 检查代理运行状态
    let info = PROXY_INFO.lock().unwrap();
    let proxy_running = info.is_some();
    checks.push(json!({
        "name": "代理运行状态",
        "ok": proxy_running,
        "detail": if proxy_running { format!("端口 {}", info.as_ref().unwrap().port) } else { "未运行".to_string() },
    }));

    CliEnvelope::ok(json!({"checks": checks}))
}

/// `verify <port> <secret>` — 通过代理发送最小请求验证 key 有效性。
pub fn cmd_verify(port: u16, secret: &str) -> CliEnvelope {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let addr = format!("127.0.0.1:{port}");
    let Ok(mut stream) = TcpStream::connect_timeout(
        &addr.parse().unwrap(),
        std::time::Duration::from_secs(5),
    ) else {
        return CliEnvelope::err("proxy_not_reachable", &format!("无法连接到代理端口 {port}"));
    };

    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let body = json!({
        "model": "claude-opus-4-8",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "ping"}]
    });
    let body_str = serde_json::to_string(&body).unwrap();
    let req = format!(
        "POST /{secret}/v1/messages HTTP/1.0\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body_str}",
        body_str.len()
    );

    if stream.write_all(req.as_bytes()).is_err() {
        return CliEnvelope::err("proxy_io_error", "发送验证请求失败");
    }

    let mut buf = vec![0u8; 4096];
    let Ok(n) = stream.read(&mut buf) else {
        return CliEnvelope::err("proxy_no_response", "代理未响应验证请求");
    };

    let head = String::from_utf8_lossy(&buf[..n]);
    let status_line = head.lines().next().unwrap_or("");
    let code = status_line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok());

    match code {
        Some(200) => CliEnvelope::ok(json!({"ok": true, "hint": "key 有效，上游已接受。"})),
        Some(c @ (401 | 403)) => CliEnvelope::ok(json!({"ok": false, "hint": format!("上游拒绝（{c}），key 可能无效或无权限。")})),
        Some(c) => CliEnvelope::ok(json!({"ok": false, "hint": format!("上游返回 {c}，可能是 key 无效或上游异常。")})),
        None => CliEnvelope::err("proxy_invalid_response", "代理返回了无效的 HTTP 响应"),
    }
}

// ============================================================================
// 工具函数
// ============================================================================

/// 简易 which：在 PATH 中查找可执行文件。
fn find_cmd(name: &str) -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let full = PathBuf::from(dir).join(name);
            if full.is_file() {
                return Some(full.display().to_string());
            }
        }
    }
    None
}
