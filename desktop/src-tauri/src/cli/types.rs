//! Helper CLI 的类型定义。
//!
//! 这是 csswitch-helper 的命令响应信封，与桌面端 `remote/types.rs` 中的
//! `RemoteRequest`/`RemoteResponse` 结构保持一致。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 单次命令的 JSON 响应信封。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliEnvelope {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CliError>,
}

/// serve 模式下的请求行格式。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliServeRequest {
    pub id: String,
    pub command: Vec<String>,
}

/// serve 模式下的响应行格式。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliServeResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CliError>,
}

/// 错误信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

impl CliEnvelope {
    /// 成功响应。
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// 无数据的成功响应（如 stop、delete 等）。
    pub fn ok_empty() -> Self {
        Self {
            ok: true,
            data: None,
            error: None,
        }
    }

    /// 错误响应。
    pub fn err(code: &str, message: &str) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(CliError {
                code: code.to_string(),
                message: message.to_string(),
                details: None,
                suggestion: None,
            }),
        }
    }

    /// 带建议的错误响应。
    pub fn err_with_hint(code: &str, message: &str, suggestion: &str) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(CliError {
                code: code.to_string(),
                message: message.to_string(),
                details: None,
                suggestion: Some(suggestion.to_string()),
            }),
        }
    }
}
