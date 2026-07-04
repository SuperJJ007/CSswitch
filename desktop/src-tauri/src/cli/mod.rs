//! Helper CLI 的命令路由与分发。
//!
//! `dispatch()` 函数解析命令行参数并路由到 `commands` 模块的对应实现。
//! 模式匹配风格参考 cc-switch-remote 的 `cli/mod.rs`。

pub mod commands;
pub mod logger;
pub mod proc_manager;
pub mod serve;
pub mod types;

use types::CliEnvelope;

/// 根据参数列表分发命令。格式：`[group] [action] [args...]`
pub fn dispatch(args: &[String]) -> CliEnvelope {
    let group = args.first().map(|s| s.as_str()).unwrap_or("");
    let action = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let rest = args.get(2..).unwrap_or(&[]);

    match (group, action) {
        // ---- 状态 ----
        ("status", _) => commands::cmd_status(),

        // ---- 配置 ----
        ("config", "get") => commands::cmd_config_get(),
        ("config", "set") => {
            if let Some(json_str) = rest.first() {
                commands::cmd_config_set(json_str)
            } else {
                CliEnvelope::err("missing_argument", "config set 需要 JSON 参数")
            }
        }
        ("config", "save-key") => {
            if rest.len() >= 2 {
                commands::cmd_config_save_key(&rest[0], &rest[1])
            } else {
                CliEnvelope::err(
                    "missing_argument",
                    "config save-key 需要 <provider> <key> 参数",
                )
            }
        }

        // ---- 代理 ----
        ("proxy", "start") => {
            if rest.len() >= 3 {
                let port: u16 = match rest[1].parse() {
                    Ok(p) => p,
                    Err(_) => return CliEnvelope::err("invalid_port", "端口号无效"),
                };
                commands::cmd_proxy_start(&rest[0], port, &rest[2])
            } else {
                CliEnvelope::err(
                    "missing_argument",
                    "proxy start 需要 <provider> <port> <secret> 参数",
                )
            }
        }
        ("proxy", "stop") => commands::cmd_proxy_stop(),
        ("proxy", "status") => commands::cmd_proxy_status(),

        // ---- 沙箱 ----
        // ---- 沙箱 ----
        ("sandbox", "status") => commands::cmd_sandbox_status(),
        ("sandbox", "start") => {
            if rest.len() >= 2 {
                let port: u16 = match rest[0].parse() {
                    Ok(p) => p,
                    Err(_) => return CliEnvelope::err("invalid_port", "端口号无效"),
                };
                commands::cmd_sandbox_start(port, &rest[1])
            } else {
                CliEnvelope::err(
                    "missing_argument",
                    "sandbox start 需要 <port> <proxy_url> 参数",
                )
            }
        }
        ("sandbox", "stop") => commands::cmd_sandbox_stop(),

        // ---- 日志 ----
        ("logs", name) => {
            let lines: Option<usize> = rest.first().and_then(|s| s.parse().ok());
            commands::cmd_logs(name, lines)
        }

        // ---- 诊断 ----
        ("doctor", _) => commands::cmd_doctor(),

        // ---- Key 验证 ----
        ("verify", _) => {
            if rest.len() >= 2 {
                let port: u16 = match rest[0].parse() {
                    Ok(p) => p,
                    Err(_) => return CliEnvelope::err("invalid_port", "端口号无效"),
                };
                commands::cmd_verify(port, &rest[1])
            } else {
                CliEnvelope::err("missing_argument", "verify 需要 <port> <secret> 参数")
            }
        }

        // ---- 未知命令 ----
        _ => CliEnvelope::err("unknown_command", &format!("未知命令：{group} {action}")),
    }
}
