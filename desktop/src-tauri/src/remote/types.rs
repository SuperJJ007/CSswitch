//! 远程服务器管理的数据类型。
//!
//! 定义与远程 Linux 服务器通信所需的全部结构体：
//! - SSH 连接 Profile（RemoteHostProfile）
//! - 健康报告（RemoteHealth）
//! - JSON-line 协议信封（RemoteRequest / RemoteResponse）
//!
//! 设计参考 cc-switch-remote 的 `remote/types.rs`，按 CSSwitch 实际需求大幅简化。

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_legacy_ssh_agent_auth_method() {
        let auth: RemoteAuthMethod = serde_json::from_str(r#"{"type":"sshAgent"}"#).unwrap();
        assert_eq!(auth, RemoteAuthMethod::SshAgent);
    }

    #[test]
    fn legacy_ssh_agent_profile_still_deserializes() {
        let raw = r#"{
            "id":"r1",
            "name":"old",
            "host":"example.com",
            "port":22,
            "username":"ubuntu",
            "authMethod":{"type":"sshAgent"},
            "helperPath":"~/.csswitch/bin/csswitch-helper"
        }"#;

        let profile: RemoteHostProfile = serde_json::from_str(raw).unwrap();

        assert!(matches!(profile.auth_method, RemoteAuthMethod::SshAgent));
        assert!(!profile.ssh_options.legacy_compat);
        assert!(profile.ssh_options.extra_args.is_empty());
    }

    #[test]
    fn deserializes_recommended_auth_method() {
        let auth: RemoteAuthMethod = serde_json::from_str(
            r#"{
                "type":"recommended",
                "useSavedKeys":true,
                "useDefaultKeyFiles":true,
                "allowPassword":true,
                "allowVerificationCode":true,
                "rememberConnection":true
            }"#,
        )
        .unwrap();

        assert_eq!(
            auth,
            RemoteAuthMethod::Recommended {
                use_saved_keys: true,
                use_default_key_files: true,
                allow_password: true,
                allow_verification_code: true,
                remember_connection: true,
                strict: false,
            }
        );
    }

    #[test]
    fn recommended_auth_defaults_are_user_friendly() {
        let auth: RemoteAuthMethod = serde_json::from_str(r#"{"type":"recommended"}"#).unwrap();

        assert_eq!(
            auth,
            RemoteAuthMethod::Recommended {
                use_saved_keys: true,
                use_default_key_files: true,
                allow_password: true,
                allow_verification_code: true,
                remember_connection: true,
                strict: false,
            }
        );
    }
}

// ============================================================================
// Profile 与认证
// ============================================================================

/// 远程服务器连接 Profile，持久存储在本地 `~/.csswitch/remote-hosts.json`。
/// 每个 Profile 描述如何通过 SSH 连接到一台远程 Linux 服务器。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSshAdvancedOptions {
    #[serde(default)]
    pub legacy_compat: bool,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostProfile {
    /// 唯一标识符（UUID v4）。
    pub id: String,
    /// 用户友好名称，如 "实验室服务器"。
    pub name: String,
    /// 服务器 IP 地址或域名。
    pub host: String,
    /// SSH 端口，默认 22。
    pub port: u16,
    /// SSH 登录用户名。
    pub username: String,
    /// SSH 认证方式。
    pub auth_method: RemoteAuthMethod,
    /// 远程 Helper 二进制路径，通常为 `~/.csswitch/bin/csswitch-helper`。
    pub helper_path: String,
    /// 最近一次成功连接的时间戳（Unix 秒），用于 UI 排序与提示。
    #[serde(default)]
    pub last_connected: Option<i64>,
    #[serde(default)]
    pub ssh_options: RemoteSshAdvancedOptions,
}

/// SSH 认证方式。
/// MVP 阶段不支持 Password（Windows 上 SSH_ASKPASS 兼容性不佳），
/// 推荐使用 SSH Agent 或私钥文件。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum RemoteAuthMethod {
    /// 使用本地 SSH Agent（`ssh-agent`），无需指定密钥路径。
    SshAgent,
    Recommended {
        #[serde(default = "default_true")]
        use_saved_keys: bool,
        #[serde(default = "default_true")]
        use_default_key_files: bool,
        #[serde(default = "default_true")]
        allow_password: bool,
        #[serde(default = "default_true")]
        allow_verification_code: bool,
        #[serde(default = "default_true")]
        remember_connection: bool,
        #[serde(default)]
        strict: bool,
    },
    Password {
        #[serde(default = "default_true")]
        save_password: bool,
        #[serde(default = "default_true")]
        allow_verification_code: bool,
        #[serde(default = "default_true")]
        remember_connection: bool,
    },
    /// 使用指定私钥文件（如 `~/.ssh/id_ed25519`）。
    KeyFile {
        /// 私钥文件的绝对路径。
        path: String,
        #[serde(default = "default_true")]
        save_key_password: bool,
        #[serde(default = "default_true")]
        allow_password_fallback: bool,
        #[serde(default = "default_true")]
        allow_verification_code: bool,
        #[serde(default = "default_true")]
        remember_connection: bool,
    },
}

// ============================================================================
// 健康报告
// ============================================================================

/// 远程服务器健康状态报告。
/// 由 `remote_check_health` Tauri 命令通过 SSH 调用 helper `status` 获得。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHealth {
    /// SSH 连接是否成功（`ssh echo test` 通过）。
    pub reachable: bool,
    /// Helper 二进制是否存在且可执行。
    pub helper_installed: bool,
    /// Helper 版本号（如 "0.3.0"），未安装时为 None。
    pub helper_version: Option<String>,
    /// 桌面端版本号（`CARGO_PKG_VERSION`），用于版本兼容性检查。
    pub desktop_version: String,
    /// Helper 版本与桌面端是否兼容。
    pub compatible: bool,
    /// 远程平台，如 "linux"、"darwin"。
    pub platform: Option<String>,
    /// 远程 CPU 架构，如 "x86_64"、"aarch64"。
    pub arch: Option<String>,
    /// Helper 支持的能力列表（`proxy`、`sandbox`、`config` 等）。
    pub capabilities: Vec<String>,
    /// 代理进程是否正在运行。
    pub proxy_running: bool,
    /// 沙箱 Science 是否正在运行。
    pub sandbox_running: bool,
    /// 最近一次错误信息。
    pub last_error: Option<String>,
    /// 健康检查的时间戳（Unix 秒）。
    pub last_check: i64,
}

// ============================================================================
// JSON-line 协议信封
// ============================================================================

/// 发送给远程 Helper 的请求。
/// 在 serve 模式下，桌面端通过 SSH stdin 逐行发送 JSON 格式的请求。
/// 当前仅在一次命令模式使用，serve 持久会话模式预留。
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRequest {
    /// 请求唯一 ID（UUID v4），用于 serve 模式匹配响应。
    pub id: String,
    /// Helper 命令参数，如 `["proxy", "start", "deepseek", "18991", "<secret>"]`。
    pub command: Vec<String>,
}

/// 远程 Helper 返回的响应。
/// 在 serve 模式下，Helper 通过 SSH stdout 逐行返回 JSON 格式的响应。
/// serve 持久会话模式预留。
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteResponse {
    /// 对应请求的 ID。
    pub id: String,
    /// 操作是否成功。
    pub ok: bool,
    /// 成功时的返回数据。
    pub data: Option<Value>,
    /// 失败时的错误详情。
    pub error: Option<RemoteError>,
}

// ============================================================================
// 错误类型
// ============================================================================

/// 远程操作错误结构，提供用于用户提示和故障诊断的完整信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteError {
    /// 错误码，如 `ssh_timeout`、`helper_not_found`、`port_in_use`。
    pub code: String,
    /// 用户友好的错误消息。
    pub message: String,
    /// 技术细节（可选），用于日志和高级诊断。
    #[serde(default)]
    pub details: Option<String>,
    /// 错误是否可重试（true=用户可点击重试，false=需先修复根本原因）。
    #[serde(default)]
    pub recoverable: bool,
    /// 修复建议（可选），如 "点击'安装 Helper'按钮"、"检查网络连接"。
    #[serde(default)]
    pub suggestion: Option<String>,
}

// ============================================================================
// CSSwitch Helper 能力列表
// ============================================================================

/// Helper 应支持的最少能力集。桌面端通过 capability 检查（而非 semver 比较）
/// 确认 Helper 版本是否兼容。
/// 预留给 future 版本兼容性检查逻辑使用。
#[allow(dead_code)]
pub const MIN_HELPER_VERSION: &str = "0.3.0";

/// Helper 必须支持的 capability 列表。
/// 桌面端调用 `status` 命令后检查返回值中的 `capabilities` 是否包含所有这些项。
pub const REQUIRED_CAPABILITIES: &[&str] = &[
    "proxy",  // 翻译代理进程管理
    "config", // ~/.csswitch/config.json 读写
    "logs",   // 日志文件查看
    "doctor", // 诊断命令
    "verify", // Key 有效性验证
];

/// Helper 可选 capability（sandbox 在无 Science 的服务器上可能不可用）。
/// 预留给 future 能力检测和 UI 适配使用。
#[allow(dead_code)]
pub const OPTIONAL_CAPABILITIES: &[&str] = &[
    "sandbox", // Claude Science 沙箱管理（需 Science 二进制）
];
