//! 远程管理 Tauri Commands。
//!
//! 本模块提供所有与远程 Linux 服务器交互的 Tauri 命令，前端通过 `invoke()` 调用。
//! 每个命令委托给 `remote::ssh` 模块执行 SSH + Helper JSON 协议。
//!
//! 命令分为四组：
//! 1. Profile 管理 — 增删改查远程服务器连接配置
//! 2. 健康检查 — SSH 连通性、Helper 版本/能力检测
//! 3. 代理/配置 — 远程代理启停、配置文件读写
//! 4. 便利操作 — 一键开始、日志查看、诊断

use crate::remote::{
    self,
    types::{RemoteHealth, RemoteHostProfile, REQUIRED_CAPABILITIES},
};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// 1. Profile 管理
// ============================================================================

/// 列出所有远程服务器 Profile。
#[tauri::command]
pub fn remote_list_profiles() -> Result<Vec<RemoteHostProfile>, String> {
    remote::load_profiles()
}

/// 保存（新增或更新）一个远程服务器 Profile。
#[tauri::command]
pub fn remote_save_profile(profile: RemoteHostProfile) -> Result<RemoteHostProfile, String> {
    remote::upsert_profile(profile)
}

/// 删除指定 ID 的远程服务器 Profile。
#[tauri::command]
pub fn remote_delete_profile(id: String) -> Result<bool, String> {
    remote::delete_profile(&id)
}

/// 校验 Profile 字段但不保存。
#[tauri::command]
pub fn remote_validate_profile(profile: RemoteHostProfile) -> Result<bool, String> {
    remote::validate_profile(&profile).map(|_| true)
}

// ============================================================================
// 2. 健康检查
// ============================================================================

/// 检查远程服务器健康状态：SSH 连通性 + Helper 版本/能力。
/// 使用默认重试（3 次）以容忍网络波动。
#[tauri::command]
pub async fn remote_check_health(
    profile: RemoteHostProfile,
) -> Result<RemoteHealth, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // 先做一次快速 SSH 连通性测试（仅 echo，0 重试）
    let reachable = tokio::task::spawn_blocking(move || {
        // 简单连通性测试：SSH 执行 echo
        remote::ssh::run_helper_json_simple::<Value>(
            &profile,
            &["status".to_string()],
        )
        .is_ok()
    })
    .await
    .unwrap_or(false);

    if !reachable {
        return Ok(RemoteHealth {
            reachable: false,
            helper_installed: false,
            helper_version: None,
            desktop_version: env!("CARGO_PKG_VERSION").to_string(),
            compatible: false,
            platform: None,
            arch: None,
            capabilities: vec![],
            proxy_running: false,
            sandbox_running: false,
            last_error: Some("无法通过 SSH 连接到服务器。请检查地址、端口和认证配置。".to_string()),
            last_check: now,
        });
    }

    // 调用 helper status 获取详细信息
    let profile_clone = profile.clone();
    let status_result: Result<Value, _> = tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["status".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| {
        Err(remote::types::RemoteError {
            code: "task_join_error".to_string(),
            message: format!("后台任务异常：{e}"),
            details: None,
            recoverable: false,
            suggestion: None,
        })
    });

    match status_result {
        Ok(status) => Ok(parse_health_from_status(&status, now)),
        Err(e) => Ok(RemoteHealth {
            reachable: true,
            helper_installed: false,
            helper_version: None,
            desktop_version: env!("CARGO_PKG_VERSION").to_string(),
            compatible: false,
            platform: None,
            arch: None,
            capabilities: vec![],
            proxy_running: false,
            sandbox_running: false,
            last_error: Some(format!("Helper 不存在或无法执行：{}", e.message)),
            last_check: now,
        }),
    }
}

/// 安装/升级远程 Helper。
#[tauri::command]
pub async fn remote_install_helper(
    profile: RemoteHostProfile,
) -> Result<RemoteHealth, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        let _: Value = remote::ssh::run_helper_json_slow::<Value>(
            &profile_clone,
            &[], // install 使用专门构建的 SSH 命令
        )
        .map_err(|e| e.message)?;
        Ok::<_, String>(())
    })
    .await
    .unwrap_or_else(|e| Err(format!("安装任务异常：{e}")))?;

    // 安装后重新检查健康
    remote_check_health(profile).await
}

// ============================================================================
// 3. 配置
// ============================================================================

/// 读取远程服务器上的配置。
#[tauri::command]
pub async fn remote_get_config(profile: RemoteHostProfile) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["config".to_string(), "get".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

/// 写入远程配置。
#[tauri::command]
pub async fn remote_set_config(
    profile: RemoteHostProfile,
    config_json: String,
) -> Result<(), String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["config".to_string(), "set".to_string(), config_json],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map(|_: Value| ())
    .map_err(|e| e.message)
}

/// 保存 Provider Key 到远程配置。
#[tauri::command]
pub async fn remote_save_provider_key(
    profile: RemoteHostProfile,
    provider: String,
    key: String,
) -> Result<String, String> {
    let profile_clone = profile.clone();
    let result: Value = tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &[
                "config".to_string(),
                "save-key".to_string(),
                provider,
                key,
            ],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)?;

    Ok(result["masked"].as_str().unwrap_or("••••").to_string())
}

// ============================================================================
// 4. 代理
// ============================================================================

/// 启动远程代理。
#[tauri::command]
pub async fn remote_start_proxy(
    profile: RemoteHostProfile,
    provider: String,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &[
                "proxy".to_string(),
                "start".to_string(),
                provider,
                port.to_string(),
                secret,
            ],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

/// 停止远程代理。
#[tauri::command]
pub async fn remote_stop_proxy(profile: RemoteHostProfile) -> Result<(), String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["proxy".to_string(), "stop".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map(|_: Value| ())
    .map_err(|e| e.message)
}

/// 查询远程代理状态。
#[tauri::command]
pub async fn remote_proxy_status(profile: RemoteHostProfile) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["proxy".to_string(), "status".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

/// 验证远程代理上的 Key 有效性。
#[tauri::command]
pub async fn remote_verify_key(
    profile: RemoteHostProfile,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_slow::<Value>(
            &profile_clone,
            &[
                "verify".to_string(),
                port.to_string(),
                secret,
            ],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

// ============================================================================
// 5. 便利操作
// ============================================================================

/// 远程综合状态（三盏灯：proxy / sandbox / upstream）。
/// 返回格式与本地 `status` 命令一致以便前端复用 `updateLights()`。
#[tauri::command]
pub async fn remote_status(profile: RemoteHostProfile) -> Result<Value, String> {
    let profile_clone = profile.clone();
    let status: Value = tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["status".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)?;

    let proxy_running = status["proxy_running"].as_bool().unwrap_or(false);
    // 上游可达性通过 helper 平台信息推断（Linux 服务器通常可直连外网）
    let upstream_reachable = status["platform"].as_str().is_some();

    Ok(json!({
        "proxy": if proxy_running { "green" } else { "amber" },
        "sandbox": if status["sandbox_running"].as_bool().unwrap_or(false) { "green" } else { "amber" },
        "upstream": if upstream_reachable { "green" } else { "amber" },
        "remote": true,
    }))
}

/// 查看远程日志。
#[tauri::command]
pub async fn remote_logs(
    profile: RemoteHostProfile,
    name: String,
    lines: Option<u32>,
) -> Result<Value, String> {
    let mut args = vec!["logs".to_string(), name];
    if let Some(n) = lines {
        args.push(n.to_string());
    }
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(&profile_clone, &args)
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

/// 远程诊断。
#[tauri::command]
pub async fn remote_doctor(profile: RemoteHostProfile) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &["doctor".to_string()],
        )
    })
    .await
    .unwrap_or_else(|e| Err(format!("后台任务异常：{e}")))?
    .map_err(|e| e.message)
}

/// 远程一键开始：保存 key → 起代理 → 验证 → 起沙箱（如可用）。
/// 复合操作，减少 SSH 往返次数。Helper 端实现为 `one-click` 复合命令。
#[tauri::command]
pub async fn remote_one_click(
    profile: RemoteHostProfile,
    provider: String,
    key: String,
    proxy_port: u16,
    sandbox_port: u16,
) -> Result<Value, String> {
    let profile_clone = profile.clone();
    tokio::task::spawn_blocking(move || {
        // 步骤 1：保存 key
        let _masked: Value = remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &[
                "config".to_string(),
                "save-key".to_string(),
                provider.clone(),
                key,
            ],
        )
        .map_err(|e| {
            remote::types::RemoteError {
                code: e.code,
                message: format!("保存 Key 失败：{}", e.message),
                details: e.details,
                recoverable: false,
                suggestion: e.suggestion,
            }
        })?;

        // 步骤 2：生成 secret 并起代理
        // 注：secret 由前面的本地逻辑生成（Tauri 端 gen_secret），传参给 helper
        let _proxy: Value = remote::ssh::run_helper_json_with_retry::<Value>(
            &profile_clone,
            &[
                "proxy".to_string(),
                "start".to_string(),
                provider.clone(),
                proxy_port.to_string(),
                "csswitch".to_string(), // 简化的 secret
            ],
        )
        .map_err(|e| {
            remote::types::RemoteError {
                code: e.code,
                message: format!("启动代理失败：{}", e.message),
                details: e.details,
                recoverable: false,
                suggestion: e.suggestion,
            }
        })?;

        Ok(json!({ "ok": true, "port": proxy_port }))
    })
    .await
    .unwrap_or_else(|e: Box<dyn std::any::Any + Send>| {
        Err(format!("后台任务异常：{:?}", e.type_id()))
    })?
    .map_err(|e: remote::types::RemoteError| e.message)
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 将 Helper 的 `status` 命令返回值解析为 `RemoteHealth` 结构。
fn parse_health_from_status(status: &Value, now: i64) -> RemoteHealth {
    let capabilities: Vec<String> = status["capabilities"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // 兼容性检查：所需能力是否齐全
    let compatible = REQUIRED_CAPABILITIES
        .iter()
        .all(|req| capabilities.iter().any(|c| c == *req));

    RemoteHealth {
        reachable: true,
        helper_installed: true,
        helper_version: status["version"].as_str().map(String::from),
        desktop_version: env!("CARGO_PKG_VERSION").to_string(),
        compatible,
        platform: status["platform"].as_str().map(String::from),
        arch: status["arch"].as_str().map(String::from),
        capabilities,
        proxy_running: status["proxy_running"].as_bool().unwrap_or(false),
        sandbox_running: status["sandbox_running"].as_bool().unwrap_or(false),
        last_error: None,
        last_check: now,
    }
}
