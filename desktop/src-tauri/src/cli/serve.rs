//! Helper 的持久 JSON-line 会话模式。
//!
//! 从 stdin 逐行读取 JSON 请求、执行命令、向 stdout 逐行写回 JSON 响应。
//! 协议：每行一个 JSON `{"id":"...","command":[...]}` → `{"id":"...","ok":true,"data":...}`。
//!
//! 此模式避免每次操作都重新建立 SSH 连接，适用于频繁操作的场景。

use std::io::{self, BufRead, Write};

use super::types::{CliServeRequest, CliServeResponse};

/// 以 JSON-line 协议在 stdin/stdout 上循环服务，直到 stdin EOF。
pub fn run_stdio() {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // I/O 错误，退出
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // 解析请求
        let request: CliServeRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                // 无法解析请求时返回错误但不退出
                let resp = CliServeResponse {
                    id: "unknown".to_string(),
                    ok: false,
                    data: None,
                    error: Some(super::types::CliError {
                        code: "parse_error".to_string(),
                        message: format!("无法解析请求 JSON：{e}"),
                        details: None,
                        suggestion: None,
                    }),
                };
                let _ = serde_json::to_writer(&mut stdout, &resp);
                let _ = writeln!(stdout);
                let _ = stdout.flush();
                continue;
            }
        };

        // 执行命令
        let result = super::dispatch(&request.command);

        // 构建响应
        let response = CliServeResponse {
            id: request.id,
            ok: result.ok,
            data: result.data,
            error: result.error,
        };

        // 写回响应
        if serde_json::to_writer(&mut stdout, &response).is_err() {
            break;
        }
        if writeln!(stdout).is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
    }
}
