//! 远程管理 Tauri Commands。
//!
//! 本模块提供所有与远程 Linux 服务器交互的 Tauri 命令，前端通过 `invoke()` 调用。
//! 每个命令委托给 `remote::ssh` 模块执行 SSH + Helper JSON 协议。
//! SSH 操作本身是阻塞的，Tauri 会自动在后台线程池执行 `#[tauri::command]` fn。
//! 对于需要在 async 上下文中调用的场景（如 health 内部递归调用），使用
//! [`run_blocking`] 在独立线程中执行以避免阻塞当前 async runtime。
//!
//! 命令分为四组：
//! 1. Profile 管理 — 增删改查远程服务器连接配置
//! 2. 健康检查 — SSH 连通性、Helper 版本/能力检测
//! 3. 代理/配置 — 远程代理启停、配置文件读写
//! 4. 便利操作 — 一键开始、日志查看、诊断

use crate::remote::{
    self, RemoteAuthMethod, RemoteHealth, RemoteHostProfile, RemoteTargetKind,
    REQUIRED_CAPABILITIES,
};
use crate::{config, templates};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::Manager;

// P1-8 修复：health check 结果缓存，减少频繁 SSH 连接
lazy_static::lazy_static! {
    static ref HEALTH_CACHE: Arc<Mutex<HashMap<String, (RemoteHealth, std::time::Instant)>>> =
        Arc::new(Mutex::new(HashMap::new()));
}

const HEALTH_CACHE_TTL_SECS: u64 = 5;
/// 缓存最大条目数，防止管理大量远程服务器时内存泄漏。
const HEALTH_CACHE_MAX_ENTRIES: usize = 32;

// ============================================================================
// 线程辅助 — 在独立 OS 线程中执行阻塞 I/O，避免卡住 Tauri 事件循环
// ============================================================================

/// 在当前线程之外的独立 OS 线程中运行一段阻塞代码，通过 channel 取回结果。
/// 用于 async 上下文中需要执行 SSH（需要 `Send + 'static`）的场景。
/// 当前所有远程命令已改为 sync fn（由 Tauri 运行时自动分派到线程池），
/// 此函数预留给将来可能的持久会话模式（serve）或高频轮询场景。
#[allow(dead_code)]
fn run_blocking<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv().unwrap_or(Err("后台任务线程异常退出".to_string()))
}

// ============================================================================
// 1. Profile 管理（纯本地 I/O，无需 spawn）
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

/// 删除指定 ID 的远程服务器 Profile。同时清理关联的健康检查缓存。
#[tauri::command]
pub fn remote_delete_profile(id: String) -> Result<bool, String> {
    let existing = remote::load_profiles()?
        .into_iter()
        .find(|profile| profile.id == id);
    // P1-8 修复：删除 profile 时同步清理健康检查缓存
    {
        let mut cache = HEALTH_CACHE.lock().unwrap();
        cache.remove(&id);
    }
    let deleted = remote::delete_profile(&id)?;
    if deleted {
        let _ =
            remote::credentials::delete_secret(&id, remote::credentials::CredentialKind::Password);
        if let Some(RemoteHostProfile {
            auth_method: RemoteAuthMethod::KeyFile { path, .. },
            ..
        }) = existing
        {
            let _ = remote::credentials::delete_secret(
                &id,
                remote::credentials::CredentialKind::KeyPassword(&path),
            );
        }
    }
    Ok(deleted)
}

/// 校验 Profile 字段但不保存。
#[tauri::command]
pub fn remote_validate_profile(profile: RemoteHostProfile) -> Result<bool, String> {
    remote::validate_profile(&profile).map(|_| true)
}

#[tauri::command]
pub fn remote_list_wsl_distributions() -> Result<Vec<remote::wsl::WslDistribution>, String> {
    remote::wsl::list_wsl_distributions().map_err(|e| e.message)
}

/// 保存远程登录信息到系统安全存储。不会写入 remote-hosts.json。
#[tauri::command]
pub fn remote_save_login_secret(
    profile_id: String,
    kind: String,
    key_path: Option<String>,
    secret: String,
) -> Result<(), String> {
    if profile_id.trim().is_empty() {
        return Err("远程服务器 ID 不能为空".to_string());
    }
    let credential_kind =
        remote::credentials::credential_kind_from_parts(&kind, key_path.as_deref())?;
    remote::credentials::save_secret(&profile_id, credential_kind, &secret)
}

/// 删除系统安全存储中的远程登录信息。不存在时视为已删除。
#[tauri::command]
pub fn remote_delete_login_secret(
    profile_id: String,
    kind: String,
    key_path: Option<String>,
) -> Result<(), String> {
    if profile_id.trim().is_empty() {
        return Err("远程服务器 ID 不能为空".to_string());
    }
    let credential_kind =
        remote::credentials::credential_kind_from_parts(&kind, key_path.as_deref())?;
    remote::credentials::delete_secret(&profile_id, credential_kind)
}

#[tauri::command]
pub fn remote_auth_prompt_respond(
    session_id: String,
    request_id: String,
    secret: Option<String>,
    cancelled: bool,
    remember: bool,
) -> Result<(), String> {
    remote::askpass::respond(&session_id, &request_id, secret, cancelled, remember)
}

// ============================================================================
// 2. 健康检查（SSH，阻塞 I/O）
// ============================================================================

/// 检查远程服务器健康状态：SSH 连通性 + Helper 版本/能力。
/// SSH 是阻塞 I/O，Tauri 自动在后台线程执行此命令。
/// P1-8 修复：使用缓存减少频繁 SSH 连接（TTL 5秒）。
#[tauri::command]
pub fn remote_check_health(profile: RemoteHostProfile) -> Result<RemoteHealth, String> {
    // P1-8 修复：先检查缓存
    {
        let cache = HEALTH_CACHE.lock().unwrap();
        if let Some((cached_health, cached_time)) = cache.get(&profile.id) {
            if cached_time.elapsed().as_secs() < HEALTH_CACHE_TTL_SECS {
                // 缓存未过期，直接返回
                return Ok(cached_health.clone());
            }
        }
    }

    // 缓存过期或不存在，执行实际检查
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let status_result =
        remote::transport::run_helper_json_with_retry::<Value>(&profile, &["status".to_string()]);
    let health = health_from_status_result(status_result, now);

    // P1-8 修复：更新缓存（带上限保护）
    cache_health(&profile.id, &health);

    Ok(health)
}

fn remote_check_health_uncached(profile: &RemoteHostProfile) -> RemoteHealth {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let status_result =
        remote::transport::run_helper_json_with_retry::<Value>(profile, &["status".to_string()]);
    health_from_status_result(status_result, now)
}

fn cache_health(profile_id: &str, health: &RemoteHealth) {
    let mut cache = HEALTH_CACHE.lock().unwrap();
    // 缓存已满时，移除最旧的条目（简单 FIFO 策略）
    if cache.len() >= HEALTH_CACHE_MAX_ENTRIES && !cache.contains_key(profile_id) {
        if let Some(oldest_key) = cache
            .iter()
            .min_by_key(|(_, (_, t))| *t)
            .map(|(k, _)| k.clone())
        {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(
        profile_id.to_string(),
        (health.clone(), std::time::Instant::now()),
    );
}

fn helper_ready_for_profile(health: &RemoteHealth) -> bool {
    let has_required = REQUIRED_CAPABILITIES
        .iter()
        .chain(["sandbox"].iter())
        .all(|req| health.capabilities.iter().any(|cap| cap.as_str() == *req));
    let version_matches = health.helper_version.as_deref() == Some(health.desktop_version.as_str());

    health.reachable
        && health.helper_installed
        && health.compatible
        && version_matches
        && health.platform.as_deref() == Some("linux")
        && has_required
}

fn install_helper_from_github(profile: &RemoteHostProfile) -> Result<(), String> {
    remote::transport::run_helper_install(profile)
        .map(|_| ())
        .map_err(|e| format!("GitHub Release 安装失败：{}", e.message))
}

fn bundled_helper_candidates(app: &tauri::AppHandle, arch: &str) -> Vec<PathBuf> {
    let filename = format!("csswitch-helper-linux-{arch}");
    let mut candidates = Vec::new();
    if let Ok(res) = app.path().resource_dir() {
        candidates.push(res.join("helper-assets").join(&filename));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("helper-assets")
            .join(&filename),
    );
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("helper-assets").join(&filename));
        }
    }
    candidates
}

fn bundled_helper_path(app: &tauri::AppHandle, arch: &str) -> Option<PathBuf> {
    bundled_helper_candidates(app, arch)
        .into_iter()
        .find(|path| path.is_file())
}

fn install_helper_from_bundle(
    app: &tauri::AppHandle,
    profile: &RemoteHostProfile,
    arch: &str,
) -> Result<(), String> {
    let path = bundled_helper_path(app, arch)
        .ok_or_else(|| format!("安装包内没有适用于 linux/{arch} 的 Helper 资源"))?;
    let bytes =
        fs::read(&path).map_err(|e| format!("读取内置 Helper 失败（{}）：{e}", path.display()))?;
    remote::transport::install_helper_from_stdin(profile, &bytes)
        .map(|_| ())
        .map_err(|e| e.message)
}

fn install_or_update_helper(
    app: &tauri::AppHandle,
    profile: &RemoteHostProfile,
    arch: &str,
) -> Result<(), String> {
    match profile.kind {
        RemoteTargetKind::Wsl => {
            let bundle_result = install_helper_from_bundle(app, profile, arch);
            if let Err(bundle_err) = bundle_result {
                install_helper_from_github(profile).map_err(|github_err| {
                    format!(
                        "自动安装 Helper 失败。内置上传失败：{bundle_err}；GitHub 下载失败：{github_err}"
                    )
                })?;
            }
            Ok(())
        }
        RemoteTargetKind::Ssh => {
            let github_result = install_helper_from_github(profile);
            if let Err(github_err) = github_result {
                install_helper_from_bundle(app, profile, arch).map_err(|bundle_err| {
                    format!(
                        "自动安装 Helper 失败。GitHub 下载失败：{github_err}；内置上传失败：{bundle_err}"
                    )
                })?;
            }
            Ok(())
        }
    }
}

/// 安装/升级远程 Helper。
/// 通过 SSH 执行安装脚本：从 GitHub Releases 下载 helper 二进制到远程服务器。
#[tauri::command]
pub fn remote_install_helper(
    app: tauri::AppHandle,
    profile: RemoteHostProfile,
) -> Result<RemoteHealth, String> {
    remote::validate_profile(&profile)?;
    let (os, arch) = remote::transport::detect_remote_platform(&profile)
        .map_err(|e| format!("连接目标失败：{}", e.message))?;
    if os != "linux" {
        return Err(format!(
            "远程 Helper 目前仅支持 Linux，当前服务器是 {os}/{arch}。"
        ));
    }
    if arch != "x86_64" && arch != "aarch64" {
        return Err(format!("远程 Helper 暂不支持 {arch} 架构。"));
    }
    install_or_update_helper(&app, &profile, &arch)?;
    let health = remote_check_health_uncached(&profile);
    cache_health(&profile.id, &health);
    Ok(health)
}

#[tauri::command]
pub fn remote_prepare_helper(
    app: tauri::AppHandle,
    profile: RemoteHostProfile,
) -> Result<RemoteHealth, String> {
    remote::validate_profile(&profile)?;

    let initial = remote_check_health_uncached(&profile);
    if helper_ready_for_profile(&initial) {
        cache_health(&profile.id, &initial);
        return Ok(initial);
    }

    let (os, arch) = remote::transport::detect_remote_platform(&profile)
        .map_err(|e| format!("连接目标失败：{}", e.message))?;
    if os != "linux" {
        return Err(format!(
            "远程 Helper 目前仅支持 Linux，当前服务器是 {os}/{arch}。"
        ));
    }
    if arch != "x86_64" && arch != "aarch64" {
        return Err(format!("远程 Helper 暂不支持 {arch} 架构。"));
    }

    install_or_update_helper(&app, &profile, &arch)?;

    let health = remote_check_health_uncached(&profile);
    cache_health(&profile.id, &health);
    if helper_ready_for_profile(&health) {
        Ok(health)
    } else {
        Err(health.last_error.unwrap_or_else(|| {
            "Helper 已安装但能力不完整，请重新安装最新版 CSSwitch 后重试。".to_string()
        }))
    }
}

// ============================================================================
// 3. 配置（SSH，阻塞 I/O）// ============================================================================

/// 读取远程服务器上的配置。
#[tauri::command]
pub fn remote_get_config(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::transport::run_helper_json_with_retry::<Value>(
        &profile,
        &["config".to_string(), "get".to_string()],
    )
    .map_err(|e| e.message)
}

/// 写入远程配置。
#[tauri::command]
pub fn remote_set_config(profile: RemoteHostProfile, config_json: String) -> Result<(), String> {
    remote::transport::run_helper_json_stdin_with_retry::<Value>(
        &profile,
        &["config".to_string(), "set".to_string(), config_json],
    )
    .map(|_| ())
    .map_err(|e| e.message)
}

/// 保存 Provider Key 到远程配置。
/// 返回掩码后的 key（仅末 4 位可见）。
#[tauri::command]
pub fn remote_save_provider_key(
    profile: RemoteHostProfile,
    provider: String,
    key: String,
) -> Result<String, String> {
    let result: Value = remote::transport::run_helper_json_stdin_with_retry::<Value>(
        &profile,
        &["config".to_string(), "save-key".to_string(), provider, key],
    )
    .map_err(|e| e.message)?;

    Ok(result["masked"].as_str().unwrap_or("••••").to_string())
}

// ============================================================================
// 4. 代理（SSH，阻塞 I/O）
// ============================================================================

fn remote_active_config_for_start(
    provider: &str,
    proxy_port: u16,
    sandbox_port: Option<u16>,
    secret: &str,
) -> Result<(config::Config, String), String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let active = cfg
        .active_profile()
        .cloned()
        .ok_or("没有生效的配置 Profile。请先在本地配置里选择当前模型来源。")?;
    let adapter = templates::adapter_for(&active.template_id).to_string();
    if !provider.is_empty() && provider != adapter && provider != active.template_id {
        return Err(format!(
            "远程启动来源不匹配：当前 Profile 是 {}，但启动请求是 {provider}。",
            active.template_id
        ));
    }
    if active.api_key.trim().is_empty() {
        return Err("当前 Profile 未填写 API Key。请填写后重试。".into());
    }
    if adapter == "relay" {
        if active.base_url.trim().is_empty()
            || !(active.base_url.starts_with("http://") || active.base_url.starts_with("https://"))
        {
            return Err("relay 配置需要 http(s):// 开头的 base_url。".into());
        }
        if active.model.trim().is_empty() {
            return Err("relay 配置需要选择或填写模型。".into());
        }
    }

    let remote_cfg = config::Config {
        schema_version: config::CURRENT_SCHEMA_VERSION,
        profiles: vec![active.clone()],
        active_id: active.id.clone(),
        proxy_port,
        sandbox_port: sandbox_port.unwrap_or(cfg.sandbox_port),
        secret: secret.to_string(),
        mode: cfg.mode,
        pending_notice: None,
    };
    Ok((remote_cfg, adapter))
}

/// 启动远程代理。
#[tauri::command]
pub fn remote_start_proxy(
    profile: RemoteHostProfile,
    provider: String,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    if port == 8765 {
        return Err("端口 8765 是真实 Science 实例保留端口，不能用。".into());
    }
    if port == 0 {
        return Err("端口不能为 0。".into());
    }

    let (remote_cfg, adapter) = remote_active_config_for_start(&provider, port, None, &secret)?;
    let config_json = serde_json::to_string(&remote_cfg).map_err(|e| e.to_string())?;

    stop_remote_proxy(&profile).map_err(|e| format!("停止旧远程代理失败：{}", e.message))?;

    remote::transport::run_helper_json_stdin_with_retry::<Value>(
        &profile,
        &["config".to_string(), "set".to_string(), config_json],
    )
    .map_err(|e| format!("同步当前 Profile 到服务器失败：{}", e.message))?;

    remote::transport::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "proxy".to_string(),
            "start".to_string(),
            adapter,
            port.to_string(),
            secret,
        ],
    )
    .map_err(|e| e.message)
}

/// 停止远程代理。
fn stop_remote_proxy(profile: &RemoteHostProfile) -> Result<Value, remote::RemoteError> {
    remote::transport::run_helper_json_with_retry::<Value>(
        profile,
        &["proxy".to_string(), "stop".to_string()],
    )
}

#[tauri::command]
pub fn remote_stop_proxy(profile: RemoteHostProfile) -> Result<(), String> {
    stop_remote_proxy(&profile)
        .map(|_| ())
        .map_err(|e| e.message)
}

fn stop_remote_sandbox(profile: &RemoteHostProfile) -> Result<Value, remote::RemoteError> {
    remote::transport::run_helper_json_with_retry::<Value>(
        profile,
        &["sandbox".to_string(), "stop".to_string()],
    )
}

/// 停止远程沙箱与代理。
#[tauri::command]
pub fn remote_stop_all(profile: RemoteHostProfile) -> Result<Value, String> {
    let sandbox_profile = profile.clone();
    let sandbox_thread = std::thread::spawn(move || stop_remote_sandbox(&sandbox_profile));
    let proxy_res = stop_remote_proxy(&profile);
    let sandbox_res = sandbox_thread.join().unwrap_or_else(|_| {
        Err(remote::RemoteError {
            code: "sandbox_stop_thread_panic".to_string(),
            message: "停止远程沙箱线程异常退出".to_string(),
            details: None,
            recoverable: false,
            suggestion: None,
        })
    });

    match (sandbox_res, proxy_res) {
        (Ok(sandbox), Ok(proxy)) => Ok(json!({
            "ok": true,
            "sandbox": sandbox,
            "proxy": proxy,
        })),
        (Err(sandbox_err), Ok(_proxy)) => Err(format!(
            "远程代理已停；但停止远程沙箱失败：{}",
            sandbox_err.message
        )),
        (Ok(_), Err(proxy_err)) => Err(format!(
            "远程沙箱已停；但停止远程代理失败：{}",
            proxy_err.message
        )),
        (Err(sandbox_err), Err(proxy_err)) => Err(format!(
            "停止远程沙箱失败：{}；停止远程代理失败：{}",
            sandbox_err.message, proxy_err.message
        )),
    }
}

/// 查询远程代理状态。
#[tauri::command]
pub fn remote_proxy_status(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::transport::run_helper_json_with_retry::<Value>(
        &profile,
        &["proxy".to_string(), "status".to_string()],
    )
    .map_err(|e| e.message)
}

/// 验证远程代理上的 Key 有效性（慢速：需经代理→上游往返）。
#[tauri::command]
pub fn remote_verify_key(
    profile: RemoteHostProfile,
    port: u16,
    secret: String,
) -> Result<Value, String> {
    remote::transport::run_helper_json_slow::<Value>(
        &profile,
        &["verify".to_string(), port.to_string(), secret],
    )
    .map_err(|e| e.message)
}

// ============================================================================
// 5. 便利操作
// ============================================================================

/// 远程综合状态（三盏灯：proxy / sandbox / upstream）。
/// 返回格式与本地 `status` 命令一致，前端 `refreshStatus()` 无需修改。
#[tauri::command]
pub fn remote_status(profile: RemoteHostProfile) -> Result<Value, String> {
    let status: Value =
        remote::transport::run_helper_json_with_retry::<Value>(&profile, &["status".to_string()])
            .map_err(|e| e.message)?;

    let proxy_running = status["proxy_running"].as_bool().unwrap_or(false);
    let upstream_reachable = if proxy_running {
        status["upstream_reachable"].as_bool().unwrap_or(false)
            || status["proxy_healthy"].as_bool().unwrap_or(false)
    } else {
        false
    };

    Ok(json!({
        "proxy": if proxy_running { "green" } else { "amber" },
        "sandbox": if status["sandbox_running"].as_bool().unwrap_or(false) { "green" } else { "amber" },
        "upstream": if upstream_reachable { "green" } else { "amber" },
        "remote": true,
    }))
}

/// 查看远程日志。
#[tauri::command]
pub fn remote_logs(
    profile: RemoteHostProfile,
    name: String,
    lines: Option<u32>,
) -> Result<Value, String> {
    let mut args = vec!["logs".to_string(), name];
    if let Some(n) = lines {
        args.push(n.to_string());
    }
    remote::transport::run_helper_json_with_retry::<Value>(&profile, &args).map_err(|e| e.message)
}

/// 远程诊断。
#[tauri::command]
pub fn remote_doctor(profile: RemoteHostProfile) -> Result<Value, String> {
    remote::transport::run_helper_json_with_retry::<Value>(&profile, &["doctor".to_string()])
        .map_err(|e| e.message)
}

fn remote_tunnel_hint(profile: &RemoteHostProfile, sandbox_port: u16) -> String {
    if matches!(profile.kind, RemoteTargetKind::Wsl) {
        return "本机 WSL 目标无需 SSH 隧道；直接打开本机地址即可。".to_string();
    }
    let mut parts = vec!["ssh".to_string()];
    if let RemoteAuthMethod::KeyFile { path, .. } = &profile.auth_method {
        parts.push("-i".to_string());
        parts.push(path.clone());
    }
    parts.push("-p".to_string());
    parts.push(profile.port.to_string());
    parts.push("-N".to_string());
    parts.push("-L".to_string());
    parts.push(format!("{sandbox_port}:127.0.0.1:{sandbox_port}"));
    parts.push(format!("{}@{}", profile.username, profile.host));
    parts.join(" ")
}

/// 远程一键开始：同步当前 Profile → 起代理 → 起沙箱。
#[tauri::command]
pub fn remote_one_click(
    profile: RemoteHostProfile,
    provider: String,
    proxy_port: u16,
    sandbox_port: u16,
) -> Result<Value, String> {
    // 端口校验（与本地的 set_config 保持一致）
    if proxy_port == 8765 || sandbox_port == 8765 {
        return Err("端口 8765 是真实 Science 实例保留端口，不能用。".into());
    }
    if proxy_port == 0 || sandbox_port == 0 {
        return Err("端口不能为 0。".into());
    }
    if proxy_port == sandbox_port {
        return Err("代理端口与沙箱端口不能相同。".into());
    }

    let secret = config::new_id();
    let (remote_cfg, adapter) =
        remote_active_config_for_start(&provider, proxy_port, Some(sandbox_port), &secret)?;
    let config_json = serde_json::to_string(&remote_cfg).map_err(|e| e.to_string())?;

    stop_remote_sandbox(&profile).map_err(|e| format!("停止旧远程沙箱失败：{}", e.message))?;

    stop_remote_proxy(&profile).map_err(|e| format!("停止旧远程代理失败：{}", e.message))?;

    remote::transport::run_helper_json_stdin_with_retry::<Value>(
        &profile,
        &["config".to_string(), "set".to_string(), config_json],
    )
    .map_err(|e| format!("同步当前 Profile 到服务器失败：{}", e.message))?;

    let proxy_result = remote::transport::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "proxy".to_string(),
            "start".to_string(),
            adapter,
            proxy_port.to_string(),
            secret.clone(),
        ],
    )
    .map_err(|e| format!("启动远程代理失败：{}", e.message))?;

    let proxy_url = format!("http://127.0.0.1:{proxy_port}/{secret}");
    let sandbox_result = match remote::transport::run_helper_json_with_retry::<Value>(
        &profile,
        &[
            "sandbox".to_string(),
            "start".to_string(),
            sandbox_port.to_string(),
            proxy_url.clone(),
        ],
    ) {
        Ok(result) => result,
        Err(err) => {
            let _ = stop_remote_proxy(&profile);
            return Err(format!("启动远程沙箱失败：{}", err.message));
        }
    };

    let local_url = sandbox_result["url"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("http://127.0.0.1:{sandbox_port}"));

    let remote_url = if matches!(profile.kind, RemoteTargetKind::Wsl) {
        local_url.clone()
    } else {
        format!("http://{}:{sandbox_port}", profile.host)
    };

    Ok(json!({
        "ok": true,
        "proxy_port": proxy_port,
        "sandbox_port": sandbox_port,
        "proxy_url": proxy_url,
        "local_url": local_url,
        "remote_url": remote_url,
        "tunnel_hint": remote_tunnel_hint(&profile, sandbox_port),
        "proxy": proxy_result,
        "sandbox": sandbox_result,
    }))
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 将 Helper 的 `status` 命令返回值解析为 `RemoteHealth` 结构。
fn health_from_status_result(
    status_result: Result<Value, remote::RemoteError>,
    now: i64,
) -> RemoteHealth {
    match status_result {
        Ok(status) => parse_health_from_status(&status, now),
        Err(e) if e.code == "helper_not_found" => RemoteHealth {
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
        },
        Err(e) => RemoteHealth {
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
            last_error: Some(format!("无法连接到目标。请检查连接配置：{}", e.message)),
            last_check: now,
        },
    }
}

fn parse_health_from_status(status: &Value, now: i64) -> RemoteHealth {
    let capabilities: Vec<String> = status["capabilities"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let helper_version = status["version"].as_str().map(String::from);
    let desktop_version = env!("CARGO_PKG_VERSION").to_string();
    let version_matches = helper_version.as_deref() == Some(desktop_version.as_str());

    // 兼容性检查：所需能力是否齐全，且 helper 与桌面端同版本。
    let has_required_capabilities = REQUIRED_CAPABILITIES
        .iter()
        .all(|req| capabilities.iter().any(|c| c == *req));
    let compatible = has_required_capabilities && version_matches;
    let last_error = if !version_matches {
        Some("Helper 版本与桌面端不一致，需要更新 Helper。".to_string())
    } else if !has_required_capabilities {
        Some("Helper 能力不完整，需要更新 Helper。".to_string())
    } else {
        None
    };

    RemoteHealth {
        reachable: true,
        helper_installed: true,
        helper_version,
        desktop_version,
        compatible,
        platform: status["platform"].as_str().map(String::from),
        arch: status["arch"].as_str().map(String::from),
        capabilities,
        proxy_running: status["proxy_running"].as_bool().unwrap_or(false),
        sandbox_running: status["sandbox_running"].as_bool().unwrap_or(false),
        last_error,
        last_check: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_error(code: &str, message: &str) -> remote::RemoteError {
        remote::RemoteError {
            code: code.to_string(),
            message: message.to_string(),
            details: None,
            recoverable: false,
            suggestion: None,
        }
    }

    #[test]
    fn health_from_helper_missing_keeps_server_reachable() {
        let health =
            health_from_status_result(Err(remote_error("helper_not_found", "missing helper")), 123);

        assert!(health.reachable);
        assert!(!health.helper_installed);
        assert_eq!(health.last_check, 123);
    }

    #[test]
    fn helper_status_is_compatible_when_version_matches_and_capabilities_complete() {
        let status = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": "linux",
            "arch": "x86_64",
            "capabilities": ["proxy", "config", "logs", "doctor", "verify", "proxy-bundle-v2", "sandbox"],
            "proxy_running": false,
            "sandbox_running": false
        });

        let health = parse_health_from_status(&status, 123);

        assert!(health.compatible);
        assert!(helper_ready_for_profile(&health));
        assert_eq!(
            health.helper_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(health.desktop_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn helper_status_requires_current_proxy_bundle_capability() {
        let status = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "platform": "linux",
            "arch": "x86_64",
            "capabilities": ["proxy", "config", "logs", "doctor", "verify", "sandbox"],
            "proxy_running": false,
            "sandbox_running": false
        });

        let health = parse_health_from_status(&status, 123);

        assert!(!health.compatible);
        assert!(!helper_ready_for_profile(&health));
    }

    #[test]
    fn helper_status_is_incompatible_when_version_differs() {
        let status = serde_json::json!({
            "version": "0.0.0",
            "platform": "linux",
            "arch": "x86_64",
            "capabilities": ["proxy", "config", "logs", "doctor", "verify", "sandbox"],
            "proxy_running": false,
            "sandbox_running": false
        });

        let health = parse_health_from_status(&status, 123);

        assert!(!health.compatible);
        assert!(!helper_ready_for_profile(&health));
        assert_eq!(
            health.last_error.as_deref(),
            Some("Helper 版本与桌面端不一致，需要更新 Helper。")
        );
    }

    #[test]
    fn health_from_auth_error_marks_server_unreachable() {
        let health =
            health_from_status_result(Err(remote_error("ssh_auth_failed", "bad password")), 123);

        assert!(!health.reachable);
        assert!(!health.helper_installed);
        assert_eq!(health.last_check, 123);
    }
}
