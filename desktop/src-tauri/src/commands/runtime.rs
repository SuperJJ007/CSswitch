use std::path::Path;
use std::process::Command;

use serde::Deserialize;
use serde_json::json;
use tauri::State;

use crate::runtime::capability_catalog::diagnostics_for_profile;
use crate::runtime::diagnostics::{
    build_status_response, proxy_status_last_error, science_diagnostics, status_lights,
    ScienceDiagnosticsInput, StatusProbeInput,
};
use crate::runtime::operation::{self, OperationKind, OperationTrace};
use crate::runtime::profile::profile_capabilities;
use crate::runtime::provider::{
    adapter_for_profile, current_shim_mode_for_adapter, gateway_kind_for_adapter,
    status_upstream_endpoint,
};
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{settings_change_needs_teardown, stop_sandbox};
use crate::runtime::settings::validate_runtime_ports;
use crate::runtime::system::open_in_browser;
use crate::{config, lock, proc, run_blocking, AppState, SharedAppState, SharedLifecycle};

fn config_last_error_json(error: &dyn std::fmt::Display) -> serde_json::Value {
    json!({
        "type": "config_error",
        "message": error.to_string(),
    })
}

fn status_response_for_config_error(error: &dyn std::fmt::Display) -> serde_json::Value {
    build_status_response(
        status_lights(StatusProbeInput {
            proxy_ok: false,
            sandbox_ok: false,
            upstream_ok: false,
        }),
        serde_json::Value::Null,
        "",
        "off",
        diagnostics_for_profile(None, "off"),
        science_diagnostics(ScienceDiagnosticsInput {
            sandbox_port: 0,
            sandbox_ok: false,
        }),
        Some(config_last_error_json(error)),
    )
}

fn status_runtime_identity(
    adapter: &str,
    secret: &str,
    launched_gateway_kind: String,
    launched_shim_mode: String,
) -> (String, String, &'static str) {
    let current_shim_mode = current_shim_mode_for_adapter(adapter);
    let gateway_kind = if !launched_gateway_kind.is_empty() {
        launched_gateway_kind
    } else if !secret.is_empty() {
        String::new()
    } else {
        gateway_kind_for_adapter(adapter).to_string()
    };
    let runtime_shim_mode = if !launched_shim_mode.is_empty() {
        launched_shim_mode
    } else if !secret.is_empty() {
        String::new()
    } else {
        current_shim_mode.to_string()
    };
    (gateway_kind, runtime_shim_mode, current_shim_mode)
}

fn stop_sandbox_state(app: &tauri::AppHandle, st: &mut AppState) -> Result<(), String> {
    stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url)
}

/// 切换运行模式（"proxy" 第三方 / "official" 官方）。切官方要先拆第三方链路成功再落盘。
#[tauri::command]
pub(crate) async fn set_mode(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    mode: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_mode_inner(app, state, lifecycle, mode)).await
}

fn set_mode_inner(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    mode: String,
) -> Result<(), String> {
    if mode != "proxy" && mode != "official" {
        return Err(format!("未知模式：{mode}（只支持 proxy / official）。"));
    }
    // 经串行器（修 P1-b）：切官方的「拆链路 + 落盘」必须与「一键开始」等互斥，否则一键起到一半时
    // 切官方会先停链路、一键随后又把沙箱/OAuth 起起来 → 显示官方却有第三方沙箱在跑。bump_generation
    // 作废任何在途启动，防被停后又拿旧配置写回运行态。
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        if mode == "official" {
            lifecycle.bump_generation();
            let mut st = lock(&state);
            stop_sandbox_state(&app, &mut st).map_err(|e| {
                format!("停止沙箱失败，未切换到官方模式：{e}（真实实例 8765 未受影响）")
            })?;
            st.stop_proxy();
        }
        config::update(&dir, {
            let mode = mode.clone();
            move |c| c.mode = mode
        })
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// 官方模式：干净地打开用户【真实】的 Claude Science（不碰/复制真实凭证，抹掉 ANTHROPIC_*）。
#[tauri::command]
pub(crate) fn open_official() -> Result<(), String> {
    let app_path = "/Applications/Claude Science.app";
    let mut cmd = Command::new("open");
    if Path::new(app_path).is_dir() {
        cmd.arg(app_path);
    } else {
        cmd.arg("-a").arg("Claude Science");
    }
    cmd.env_remove("ANTHROPIC_BASE_URL")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err("未能打开 Claude Science。请确认已安装官方 Claude Science。".into()),
        Err(e) => Err(format!("打开官方 Claude Science 失败：{e}")),
    }
}

#[derive(Deserialize)]
pub(crate) struct UiSettings {
    proxy_port: u16,
    sandbox_port: u16,
}

/// 端口设置（provider/连接改走 profile CRUD + set_active_profile）。
/// 经串行器（修 P1-c）：端口一旦变化，正在跑的代理绑在旧端口、正在跑的沙箱又烘死了旧代理 URL，
/// 与新端口不一致；此处把这条陈旧链路拆掉（只停我们的沙箱、绝不碰 8765），逼下次「一键开始」按新端口重建，
/// 杜绝「复用旧沙箱指向死端口、UI 却报沿用不变」。
#[tauri::command]
pub(crate) async fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
    cfg: UiSettings,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || set_settings_inner(app, state, lifecycle, cfg)).await
}

fn set_settings_inner(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
    cfg: UiSettings,
) -> Result<(), String> {
    validate_runtime_ports(cfg.proxy_port, cfg.sandbox_port)?;
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let old = config::load_from(&dir).map_err(|e| e.to_string())?;
        let teardown = settings_change_needs_teardown(
            old.proxy_port,
            cfg.proxy_port,
            old.sandbox_port,
            cfg.sandbox_port,
        );
        // 拆链路【先】于落盘，且停沙箱结果必须据实处理（修增量 P1）：停不掉就【不改端口】——
        // 否则会留下「config 已是新端口、旧沙箱仍在旧端口指向旧代理」的不一致态，下次一键还会复用这条死链路。
        // 保持端口不变则一切仍自洽（旧沙箱指旧代理端口、下次一键在旧端口重建代理，链路照通）。
        if teardown {
            let mut st = lock(&state);
            stop_sandbox_state(&app, &mut st).map_err(|e| {
                format!(
                    "端口未更改：无法停止指向旧端口的沙箱（{e}），为避免留下失效链路，端口保持不变。请手动停止沙箱或重启 app 后重试。（真实实例 8765 未受影响）"
                )
            })?;
            lifecycle.bump_generation(); // 停成功后作废在途启动
            st.stop_proxy();
        }
        // 拆链路成功（或无需拆）→ 才落盘新端口，保证 config 与运行态一致。
        config::update(&dir, move |c| {
            c.proxy_port = cfg.proxy_port;
            c.sandbox_port = cfg.sandbox_port;
        })
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

#[tauri::command]
pub(crate) async fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || start_proxy_inner_cmd(app, state, lifecycle)).await
}

fn start_proxy_inner_cmd<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    // 经串行器：与切换/连接编辑/清 key/删/停等 ensure_proxy 竞争串行化，防陈旧读起旧配置代理
    // 又写回运行态（修 P1-a，比照 spec §8.1「ensure_proxy 都经一把 app 级 mutex」）。
    lifecycle.with_serialized(|| {
        let trace = OperationTrace::start(OperationKind::StartProxy, "command=start_proxy");
        let (port, _secret, _action) =
            ensure_proxy(&app, &state, lifecycle.as_ref(), Some(&trace))?;
        trace.finish(format!("ok port={port}"));
        Ok(json!({ "port": port }))
    })
}

#[derive(Deserialize)]
pub(crate) struct FetchModelsReq {
    /// 模板 id（决定 builtin / base_url 可编辑性 / 默认 base_url）。
    template_id: String,
    /// 编辑已存 profile 时的实际 api_format；为空则按模板默认值。
    #[serde(default)]
    api_format: Option<String>,
    /// 自定义模板时用户填的 base_url（不可编辑模板忽略）。
    #[serde(default)]
    base_url: String,
    /// 用户新填的 key；为空表示沿用 profile_id 已存的 key（后端不回传完整 key）。
    #[serde(default)]
    key: String,
    /// 编辑已存 profile 时传其 id（用于沿用已存 key）。
    #[serde(default)]
    profile_id: Option<String>,
}

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
#[tauri::command]
pub(crate) async fn fetch_models(
    app: tauri::AppHandle,
    req: FetchModelsReq,
) -> Result<serde_json::Value, String> {
    run_blocking(move || {
        crate::runtime::model_discovery::fetch_models(
            app,
            crate::runtime::model_discovery::ModelDiscoveryRequest {
                template_id: req.template_id,
                api_format: req.api_format,
                base_url: req.base_url,
                key: req.key,
                profile_id: req.profile_id,
            },
        )
    })
    .await
}

#[tauri::command]
pub(crate) async fn stop_all(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<(), String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || stop_all_inner_cmd(app, state, lifecycle)).await
}

fn stop_all_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        lifecycle.bump_generation(); // 作废任何在途启动（防被停后又拿旧 key 复活）
        let mut st = lock(&state);
        let sandbox_res = stop_sandbox_state(&app, &mut st);
        st.stop_proxy();
        sandbox_res.map_err(|e| format!("代理已停；但{e}真实实例 8765 未受影响。"))
    })
}

#[tauri::command]
pub(crate) async fn one_click_login(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || one_click_login_cmd(app, state, lifecycle)).await
}

pub(crate) fn one_click_login_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        crate::runtime::sandbox_session::one_click_login(app, state, lifecycle.as_ref())
    })
}

#[tauri::command]
pub(crate) fn status(state: State<'_, SharedAppState>) -> serde_json::Value {
    // 只在锁内取值，锁外做短超时探活。这里是高频 UI 状态灯，
    // 不能反复调用外部 `claude-science status`，否则前端轮询会卡住主线程。
    // 沙箱强身份确认保留在 one_click_login 的启动/复用边界。
    let (
        pport,
        secret,
        sport,
        adapter,
        base_url,
        active_profile,
        catalog_profile,
        tracked_proxy_child_alive,
        launched_provider,
        launched_gateway_kind,
        launched_shim_mode,
        launched_launch_id,
    ) = {
        let mut st = lock(state.inner());
        let cfg = match config::load_from(&config::default_dir()) {
            Ok(cfg) => cfg,
            Err(e) => return status_response_for_config_error(&e),
        };
        let pport = if st.proxy_port != 0 {
            st.proxy_port
        } else {
            cfg.proxy_port
        };
        let sport = if st.sandbox_port != 0 {
            st.sandbox_port
        } else {
            cfg.sandbox_port
        };
        let tracked_proxy_child_alive = proc::tracked_child_is_running(&mut st.proxy);
        // 上游灯读生效 profile 的 adapter/base_url；无生效配置 → 空（灯显黄，不误探）。
        let (adapter, base_url, active_profile, catalog_profile) = match cfg.active_profile() {
            Some(p) => {
                let adapter = adapter_for_profile(p).to_string();
                (
                    adapter,
                    p.base_url.clone(),
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "template_id": p.template_id,
                        "api_format": p.api_format,
                        "model": p.model,
                        "capabilities": profile_capabilities(p),
                    }),
                    Some(p.clone()),
                )
            }
            None => (String::new(), String::new(), serde_json::Value::Null, None),
        };
        (
            pport,
            st.secret.clone(),
            sport,
            adapter,
            base_url,
            active_profile,
            catalog_profile,
            tracked_proxy_child_alive,
            st.provider.clone(),
            st.gateway_kind.clone(),
            st.shim_mode.clone(),
            st.launch_id.clone(),
        )
    };
    let diagnostic_override = std::env::var_os("CSSWITCH_UPSTREAM_URL");
    let upstream = status_upstream_endpoint(&adapter, &base_url, diagnostic_override.as_deref());
    let proxy_ok = tracked_proxy_child_alive
        && !secret.is_empty()
        && !launched_gateway_kind.is_empty()
        && !launched_provider.is_empty()
        && proc::http_health_gateway(
            pport,
            Some(&secret),
            operation::STATUS_HEALTH_TIMEOUT_MS,
            &launched_gateway_kind,
            Some(&launched_provider),
            Some(launched_shim_mode.as_str()),
            Some(launched_launch_id.as_str()),
        );
    let last_error = proxy_status_last_error(!secret.is_empty(), proxy_ok, pport);
    let sandbox_ok = proc::http_health(sport, None, operation::STATUS_HEALTH_TIMEOUT_MS);
    let upstream_ok = upstream
        .as_ref()
        .map(|e| proc::tcp_reachable(&e.host, e.port, operation::STATUS_UPSTREAM_TIMEOUT_MS))
        .unwrap_or(false);
    let lights = status_lights(StatusProbeInput {
        proxy_ok,
        sandbox_ok,
        upstream_ok,
    });
    let (gateway_kind, shim_mode, catalog_shim_mode) =
        status_runtime_identity(&adapter, &secret, launched_gateway_kind, launched_shim_mode);
    build_status_response(
        lights,
        active_profile,
        &gateway_kind,
        &shim_mode,
        diagnostics_for_profile(catalog_profile.as_ref(), catalog_shim_mode),
        science_diagnostics(ScienceDiagnosticsInput {
            sandbox_port: sport,
            sandbox_ok,
        }),
        last_error,
    )
}

#[tauri::command]
pub(crate) fn boot_error(state: State<'_, SharedAppState>) -> Option<String> {
    lock(state.inner()).boot_error.clone()
}

#[tauri::command]
pub(crate) fn open_url(state: State<'_, SharedAppState>) -> Result<(), String> {
    let url = { lock(state.inner()).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
pub(crate) fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    // 显式退出统一交给 RunEvent::ExitRequested 清理，避免多个退出入口各自停代理。
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        config_last_error_json, status_response_for_config_error, status_runtime_identity,
    };
    use crate::commands::skills::scan_named_and_reconcile_skills_for_test;
    use crate::runtime::capability_catalog::load_static_catalog;
    use crate::{
        config::{self, Config, Profile},
        lifecycle, lock, oauth_forge,
        runtime::{sandbox_session, science},
        skill_manager::{
            compatibility::{
                evaluate_compatibility_gate, BooleanCapability, CapabilityAvailability,
                LocalCommandPolicy, NetworkMode, RuntimeContext, SandboxState,
                SshCapabilitySummary,
            },
            discovery::ScienceProbeState,
            error::SkillErrorCode,
            model::{DeploymentStatus, DiscoveryStatus, InstalledSkill, SkillId, SkillSource},
            store::SkillManager,
        },
        AppState, SharedAppState,
    };
    use sha2::{Digest, Sha256};
    use std::{
        env,
        ffi::OsStr,
        fs,
        fs::OpenOptions,
        io::{Read, Seek, SeekFrom, Write},
        net::{TcpListener, TcpStream},
        os::unix::ffi::OsStrExt,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        path::{Path, PathBuf},
        process::{Child, Command, Output, Stdio},
        sync::{Arc, Mutex},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use tauri::Manager;

    #[test]
    fn config_last_error_json_preserves_typed_config_error() {
        let err = config_last_error_json(&"bad config");
        assert_eq!(
            err.get("type").and_then(|v| v.as_str()),
            Some("config_error")
        );
        assert_eq!(
            err.get("message").and_then(|v| v.as_str()),
            Some("bad config")
        );
    }

    #[test]
    fn status_response_for_config_error_is_fail_closed() {
        let v = status_response_for_config_error(&"bad config");
        assert_eq!(v["proxy"], "amber");
        assert_eq!(v["sandbox"], "amber");
        assert_eq!(v["upstream"], "amber");
        assert_eq!(v["active_profile"], serde_json::Value::Null);
        assert_eq!(v["science"]["sandbox"]["port"], 0);
        assert_eq!(v["last_error"]["type"], "config_error");
        assert_eq!(v["last_error"]["message"], "bad config");
    }

    #[test]
    fn status_runtime_identity_prefers_launched_identity_and_fail_closes_partial_launch() {
        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "", String::new(), String::new());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "off");
        assert_eq!(catalog_shim, "off");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", "rust".into(), "off".into());
        assert_eq!(gateway, "rust");
        assert_eq!(shim, "off");
        assert_eq!(catalog_shim, "off");

        let (gateway, shim, catalog_shim) =
            status_runtime_identity("deepseek", "secret-present", String::new(), String::new());
        assert_eq!(gateway, "");
        assert_eq!(shim, "");
        assert_eq!(catalog_shim, "off");
    }

    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self { saved: Vec::new() }
        }

        fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
            self.saved.push((key.to_string(), env::var_os(key)));
            env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.iter().rev() {
                match value {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
            }
        }
    }

    fn tmpdir(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("csswitch-{label}-{}-{now}", std::process::id()))
    }

    fn free_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        port
    }

    fn write_executable(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn prepare_safe_e2e_bin(root: &Path) -> PathBuf {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = root.join("security-stub-invoked.log");
        fs::write(&marker, b"").unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        let metadata = fs::symlink_metadata(&marker).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        write_executable(
            &bin.join("security"),
            "#!/bin/sh\nset -eu\numask 077\ncase \"${1:-}\" in\n  find-generic-password) command=find-generic-password ;;\n  *) command=unexpected ;;\nesac\nprintf 'science_pid=%s argv=%s exit=1\\n' \"$PPID\" \"$command\" >> \"$CSSWITCH_E2E_SECURITY_MARKER\"\nexit 1\n",
        );
        bin
    }

    fn sanitize_e2e_command(command: &mut Command, sandbox_home: &Path, safe_bin: &Path) {
        command
            .env_clear()
            .env("HOME", sandbox_home)
            .env(
                "PATH",
                format!(
                    "{}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                    safe_bin.display()
                ),
            )
            .env("TMPDIR", "/private/tmp")
            .env("LANG", "en_US.UTF-8")
            .env("LC_ALL", "en_US.UTF-8")
            .env(
                "CSSWITCH_E2E_SECURITY_MARKER",
                safe_bin.parent().unwrap().join("security-stub-invoked.log"),
            );
    }

    fn safe_e2e_command(
        program: impl AsRef<OsStr>,
        sandbox_home: &Path,
        safe_bin: &Path,
    ) -> Command {
        let mut command = Command::new(program);
        sanitize_e2e_command(&mut command, sandbox_home, safe_bin);
        command
    }

    fn write_test_bins(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        write_executable(
            &dir.join("open"),
            r#"#!/bin/sh
if [ -n "${CSSWITCH_FAKE_OPEN_LOG:-}" ]; then
  printf '%s\n' "$*" >> "$CSSWITCH_FAKE_OPEN_LOG"
fi
exit 0
"#,
        );
        write_executable(
            &dir.join("security"),
            r#"#!/bin/sh
exit 0
"#,
        );
        let science_bin = dir.join("claude-science");
        write_executable(
            &science_bin,
            r#"#!/bin/sh
set -eu
cmd="${1:-}"
if [ "$#" -gt 0 ]; then shift; fi
data_dir=""
port=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --data-dir) data_dir="$2"; shift 2 ;;
    --port) port="$2"; shift 2 ;;
    *) shift ;;
  esac
done
if [ "$cmd" = "--version" ]; then
  echo 'claude-science 0.1.18 (release, public)'
  exit 0
fi
state="$data_dir/fake-science"
mkdir -p "$state"
case "$cmd" in
  serve)
    count="$(cat "$state/serve-count" 2>/dev/null || echo 0)"
    count=$((count + 1))
    printf '%s' "$count" > "$state/serve-count"
    printf '%s' "$port" > "$state/port"
    python3 - "$port" "$state/pid" >/dev/null 2>&1 <<'PY' &
import http.server
import os
import socketserver
import sys
port = int(sys.argv[1])
pidfile = sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        if self.path.startswith("/health"):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"status":"ok"}')
        else:
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"fake science")
class ReusableTCPServer(socketserver.TCPServer):
    allow_reuse_address = True
with open(pidfile, "w", encoding="utf-8") as f:
    f.write(str(os.getpid()))
with ReusableTCPServer(("127.0.0.1", port), Handler) as httpd:
    httpd.serve_forever()
PY
    exit 0
    ;;
  status)
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      echo '{"running":true}'
    else
      echo '{"running":false}'
    fi
    ;;
  url)
    p="$(cat "$state/port")"
    echo "http://127.0.0.1:$p"
    ;;
  stop)
    if [ "${CSSWITCH_FAKE_STOP_FAIL:-0}" = "1" ]; then
      echo "forced stop failure" >&2
      exit 1
    fi
    pid="$(cat "$state/pid" 2>/dev/null || true)"
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
    rm -f "$state/pid"
    echo "stopped"
    ;;
  *)
    echo "unsupported fake science command: $cmd" >&2
    exit 2
    ;;
esac
"#,
        );
        science_bin
    }

    fn start_mock_upstream() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_ne!(port, 8765);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
            }
        });
        port
    }

    fn wait_http_health(port: u16) {
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("mock service on port {port} did not become reachable");
    }

    fn wait_http_unreachable(port: u16) {
        for _ in 0..50 {
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("mock service on port {port} remained reachable");
    }

    fn kill_tracked_proxy(state: &SharedAppState, proxy_port: u16) {
        let mut proxy_child = {
            let mut st = lock(state);
            assert_eq!(st.proxy_port, proxy_port);
            assert!(!st.secret.is_empty());
            st.proxy.take().expect("proxy child should be tracked")
        };
        let _ = proxy_child.kill();
        let _ = proxy_child.wait();
        wait_http_unreachable(proxy_port);
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SkillMockRequestKind {
        Auxiliary,
        MainInitial,
        MainToolResult,
    }

    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    struct SkillMockObservation {
        main_count: usize,
        skill_schema_fields: Vec<String>,
        tool_order: Vec<String>,
        tool_input_matches_runtime: Option<bool>,
        tool_result_count: usize,
        tool_result_is_error: Option<bool>,
        marker_present: bool,
    }

    fn skill_mock_request_kind(value: &serde_json::Value) -> SkillMockRequestKind {
        let is_main = value.get("stream").and_then(serde_json::Value::as_bool) == Some(true)
            && value
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|tools| {
                    tools.iter().any(|tool| {
                        tool.get("name").and_then(serde_json::Value::as_str) == Some("skill")
                    })
                });
        if !is_main {
            return SkillMockRequestKind::Auxiliary;
        }
        let has_tool_result = value
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|message| message.get("content"))
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .any(|block| {
                block.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
            });
        if has_tool_result {
            SkillMockRequestKind::MainToolResult
        } else {
            SkillMockRequestKind::MainInitial
        }
    }

    fn json_contains_marker(value: &serde_json::Value, marker: &str) -> bool {
        match value {
            serde_json::Value::String(text) => text.contains(marker),
            serde_json::Value::Array(values) => values
                .iter()
                .any(|value| json_contains_marker(value, marker)),
            serde_json::Value::Object(values) => values
                .values()
                .any(|value| json_contains_marker(value, marker)),
            _ => false,
        }
    }

    fn observe_skill_mock_request(
        value: &serde_json::Value,
        expected_runtime_name: &str,
        marker: &str,
        observation: &mut SkillMockObservation,
    ) -> SkillMockRequestKind {
        let kind = skill_mock_request_kind(value);
        if kind == SkillMockRequestKind::Auxiliary {
            return kind;
        }
        observation.main_count += 1;
        if observation.skill_schema_fields.is_empty() {
            let mut fields = value
                .get("tools")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .find(|tool| tool.get("name").and_then(serde_json::Value::as_str) == Some("skill"))
                .and_then(|tool| tool.get("input_schema"))
                .and_then(|schema| schema.get("properties"))
                .and_then(serde_json::Value::as_object)
                .map(|properties| properties.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            fields.sort();
            observation.skill_schema_fields = fields;
        }
        if kind == SkillMockRequestKind::MainToolResult {
            for block in value
                .get("messages")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|message| message.get("content"))
                .filter_map(serde_json::Value::as_array)
                .flatten()
            {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("tool_use") => {
                        if let Some(name) = block.get("name").and_then(serde_json::Value::as_str) {
                            observation.tool_order.push(format!("request:{name}"));
                            if name == "skill" {
                                observation.tool_input_matches_runtime = Some(
                                    block
                                        .get("input")
                                        .and_then(|input| input.get("skill"))
                                        .and_then(serde_json::Value::as_str)
                                        == Some(expected_runtime_name),
                                );
                            }
                        }
                    }
                    Some("tool_result") => {
                        observation.tool_result_count += 1;
                        observation
                            .tool_order
                            .push("request:tool_result".to_string());
                        observation.tool_result_is_error = Some(
                            block
                                .get("is_error")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false),
                        );
                        observation.marker_present |= json_contains_marker(block, marker);
                    }
                    _ => {}
                }
            }
        }
        kind
    }

    fn anthropic_json_response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": "msg_csswitch_aux",
            "type": "message",
            "role": "assistant",
            "model": "csswitch-local-mock",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
        .unwrap()
    }

    fn anthropic_terminal_sse() -> Vec<u8> {
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_csswitch_done\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"csswitch-local-mock\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        )
        .as_bytes()
        .to_vec()
    }

    fn anthropic_skill_tool_sse(runtime_name: &str) -> Vec<u8> {
        let input = serde_json::json!({
            "skill": runtime_name,
            "human_description": "Load the isolated CSSwitch trigger probe"
        });
        let partial_json = serde_json::to_string(&input).unwrap();
        let delta = serde_json::to_string(&serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": partial_json}
        }))
        .unwrap();
        format!(
            concat!(
                "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_csswitch_skill\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"csswitch-local-mock\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\n",
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"toolu_csswitch_skill\",\"name\":\"skill\",\"input\":{{}}}}}}\n\n",
                "event: content_block_delta\ndata: {delta}\n\n",
                "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
                "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            ),
            delta = delta
        )
        .into_bytes()
    }

    struct SkillMockServer {
        port: u16,
        observation: Arc<Mutex<SkillMockObservation>>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl SkillMockServer {
        fn start(runtime_name: String, marker: String) -> Self {
            use std::sync::atomic::Ordering;

            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            assert_ne!(port, 8765);
            let observation = Arc::new(Mutex::new(SkillMockObservation::default()));
            let thread_observation = observation.clone();
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread_shutdown = shutdown.clone();
            let server_thread = thread::spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                            if let Ok((headers, body)) = read_mock_http_request(&mut stream) {
                                let value = serde_json::from_slice::<serde_json::Value>(&body)
                                    .unwrap_or(serde_json::Value::Null);
                                let kind = {
                                    let mut locked = thread_observation
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner());
                                    observe_skill_mock_request(
                                        &value,
                                        &runtime_name,
                                        &marker,
                                        &mut locked,
                                    )
                                };
                                let stream_requested =
                                    value.get("stream").and_then(serde_json::Value::as_bool)
                                        == Some(true);
                                let (content_type, response) = match kind {
                                    SkillMockRequestKind::MainInitial => {
                                        let mut locked = thread_observation
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner());
                                        locked.tool_order.push("response:skill".to_string());
                                        (
                                            "text/event-stream",
                                            anthropic_skill_tool_sse(&runtime_name),
                                        )
                                    }
                                    SkillMockRequestKind::MainToolResult => {
                                        ("text/event-stream", anthropic_terminal_sse())
                                    }
                                    SkillMockRequestKind::Auxiliary if stream_requested => {
                                        ("text/event-stream", anthropic_terminal_sse())
                                    }
                                    SkillMockRequestKind::Auxiliary => {
                                        ("application/json", anthropic_json_response())
                                    }
                                };
                                let _ = headers;
                                let response_head = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    response.len()
                                );
                                let _ = stream.write_all(response_head.as_bytes());
                                let _ = stream.write_all(&response);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                observation,
                shutdown,
                thread: Some(server_thread),
            }
        }

        fn endpoint(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        fn snapshot(&self) -> SkillMockObservation {
            self.observation
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn reset(&self) {
            *self
                .observation
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = SkillMockObservation::default();
        }
    }

    impl Drop for SkillMockServer {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;

            self.shutdown.store(true, Ordering::Release);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(server_thread) = self.thread.take() {
                let _ = server_thread.join();
            }
        }
    }

    fn read_mock_http_request(stream: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
        const MAX_HEADERS: usize = 64 * 1024;
        const MAX_BODY: usize = 8 * 1024 * 1024;
        let mut bytes = Vec::new();
        let header_end = loop {
            if bytes.len() > MAX_HEADERS {
                return Err("mock request headers too large".to_string());
            }
            let mut buffer = [0_u8; 4096];
            let count = stream.read(&mut buffer).map_err(|_| "mock read failed")?;
            if count == 0 {
                return Err("mock request ended before headers".to_string());
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(offset) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = bytes[..header_end].to_vec();
        let header_text = std::str::from_utf8(&headers).map_err(|_| "mock headers not utf8")?;
        let content_length = header_text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        if content_length > MAX_BODY {
            return Err("mock request body too large".to_string());
        }
        while bytes.len() - header_end < content_length {
            let mut buffer = [0_u8; 4096];
            let count = stream.read(&mut buffer).map_err(|_| "mock read failed")?;
            if count == 0 {
                return Err("mock request body truncated".to_string());
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        Ok((
            headers,
            bytes[header_end..header_end + content_length].to_vec(),
        ))
    }

    struct RemoveTempTree(PathBuf);

    impl Drop for RemoveTempTree {
        fn drop(&mut self) {
            if env::var("CSSWITCH_KEEP_E2E_TMP").as_deref() == Ok("1") {
                eprintln!("CSSWITCH_E2E_EVIDENCE_DIR={}", self.0.display());
                return;
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct RealScienceE2eGuard {
        science_bin: PathBuf,
        sandbox_home: PathBuf,
        safe_bin: PathBuf,
        data_dir: PathBuf,
        child: Option<Child>,
        playwright_cli: PathBuf,
        playwright_session: String,
        playwright_cache: PathBuf,
        playwright_workdir: PathBuf,
        browser_open: bool,
        port: u16,
    }

    impl RealScienceE2eGuard {
        fn close_browser(&mut self) {
            if !self.browser_open {
                return;
            }
            let mut close =
                safe_e2e_command(&self.playwright_cli, &self.sandbox_home, &self.safe_bin);
            close
                .args(["close"])
                .current_dir(&self.playwright_workdir)
                .env("PLAYWRIGHT_CLI_SESSION", &self.playwright_session)
                .env("NPM_CONFIG_CACHE", &self.playwright_cache);
            let _ = command_output_with_timeout(&mut close, Duration::from_secs(15));
            self.browser_open = false;
        }

        fn stop(&mut self) {
            self.close_browser();
            let mut stop = safe_e2e_command(&self.science_bin, &self.sandbox_home, &self.safe_bin);
            stop.arg("stop").arg("--data-dir").arg(&self.data_dir);
            let _ = command_output_with_timeout(&mut stop, Duration::from_secs(15));
            if let Some(mut child) = self.child.take() {
                for _ in 0..50 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for RealScienceE2eGuard {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn wait_port_reachable(port: u16) {
        for _ in 0..300 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("isolated Science port did not become reachable");
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SecurityStubBoundaryState {
        Pending,
        Ready(usize),
    }

    fn security_stub_boundary_state(
        root: &Path,
        expected_count: usize,
        stderr_path: &Path,
    ) -> Result<SecurityStubBoundaryState, String> {
        let stderr_metadata = match fs::symlink_metadata(stderr_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SecurityStubBoundaryState::Pending)
            }
            Err(_) => return Err("isolated Science stderr metadata is unreadable".to_string()),
        };
        if !stderr_metadata.file_type().is_file() || stderr_metadata.len() > 8 * 1024 * 1024 {
            return Err("isolated Science stderr is not a bounded regular file".to_string());
        }
        let stderr = fs::read(stderr_path)
            .map_err(|_| "isolated Science stderr is unreadable".to_string())?;
        let stderr = String::from_utf8_lossy(&stderr);
        if stderr.contains("ETIMEDOUT") {
            return Err("isolated Science attempted a non-stub security command".to_string());
        }
        let security_results = stderr
            .lines()
            .filter(|line| line.contains("security find-generic-password"))
            .collect::<Vec<_>>();
        if security_results.is_empty() {
            return Ok(SecurityStubBoundaryState::Pending);
        }
        const EXPECTED_SECURITY_PREFIX: &str = "ensureEncryptionKeys: could not mirror keys to the macOS Keychain (security find-generic-password failed (exit 1)); continuing with ";
        if security_results.len() != 1
            || !security_results[0].starts_with(EXPECTED_SECURITY_PREFIX)
            || !security_results[0].ends_with("/encryption.key")
        {
            return Err(
                "isolated Science security result is not exactly one exit 1 failure".to_string(),
            );
        }

        let marker = root.join("security-stub-invoked.log");
        let marker_metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SecurityStubBoundaryState::Pending)
            }
            Err(_) => return Err("fake security marker metadata is unreadable".to_string()),
        };
        if !marker_metadata.file_type().is_file()
            || marker_metadata.permissions().mode() & 0o777 != 0o600
            || marker_metadata.len() > 64 * 1024
        {
            return Err("fake security marker is not a bounded 0600 regular file".to_string());
        }
        let marker_text = fs::read_to_string(&marker)
            .map_err(|_| "fake security marker is unreadable".to_string())?;
        let records = marker_text.lines().collect::<Vec<_>>();
        if records.iter().any(|line| {
            !line.starts_with("science_pid=")
                || !line.contains(" argv=find-generic-password exit=1")
                || line.contains("unexpected")
        }) {
            return Err("fake security marker contains an unexpected argv or exit".to_string());
        }
        let mut pids = std::collections::BTreeSet::new();
        for record in &records {
            let pid = record
                .split_once(' ')
                .map(|(pid, _)| pid)
                .unwrap_or_default();
            if !pids.insert(pid) {
                return Err("fake security marker contains repeated Science PID".to_string());
            }
        }
        let count = records.len();
        if count < expected_count {
            return Ok(SecurityStubBoundaryState::Pending);
        }
        if count > expected_count {
            return Err(format!(
                "fake security marker count exceeded expected stage count {expected_count}: {count}"
            ));
        }
        Ok(SecurityStubBoundaryState::Ready(count))
    }

    fn assert_security_stub_boundary(
        root: &Path,
        expected_count: usize,
        stderr_path: &Path,
    ) -> usize {
        for _ in 0..100 {
            match security_stub_boundary_state(root, expected_count, stderr_path) {
                Ok(SecurityStubBoundaryState::Ready(count)) => return count,
                Ok(SecurityStubBoundaryState::Pending) => {}
                Err(error) => panic!("fake security boundary failed: {error}"),
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("fake security boundary did not become ready for expected count {expected_count}");
    }

    fn command_output_with_timeout(
        command: &mut Command,
        timeout: Duration,
    ) -> Result<Output, String> {
        struct CommandOutputFiles {
            stdout_path: PathBuf,
            stderr_path: PathBuf,
            stdout: fs::File,
            stderr: fs::File,
        }

        impl Drop for CommandOutputFiles {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.stdout_path);
                let _ = fs::remove_file(&self.stderr_path);
            }
        }

        fn output_file(label: &str) -> Result<(PathBuf, fs::File), String> {
            for attempt in 0..16_u8 {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let path = env::temp_dir().join(format!(
                    "csswitch-command-output-{}-{nonce}-{attempt}-{label}",
                    std::process::id()
                ));
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                {
                    Ok(file) => return Ok((path, file)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => return Err("隔离测试子命令输出文件无法创建".to_string()),
                }
            }
            Err("隔离测试子命令输出文件发生冲突".to_string())
        }

        let (stdout_path, stdout_file) = output_file("stdout")?;
        let (stderr_path, stderr_file) = match output_file("stderr") {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_file(&stdout_path);
                return Err(error);
            }
        };
        let output_files = CommandOutputFiles {
            stdout_path,
            stderr_path,
            stdout: stdout_file,
            stderr: stderr_file,
        };
        let stdout_child = output_files
            .stdout
            .try_clone()
            .map_err(|_| "隔离测试 stdout 无法复制".to_string())?;
        let stderr_child = output_files
            .stderr
            .try_clone()
            .map_err(|_| "隔离测试 stderr 无法复制".to_string())?;
        command
            .stdout(Stdio::from(stdout_child))
            .stderr(Stdio::from(stderr_child));
        let mut child = command
            .spawn()
            .map_err(|_| "隔离测试子命令无法启动".to_string())?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|_| "隔离测试子命令状态不可读".to_string())?
            {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("隔离测试子命令超时并已终止".to_string());
            }
            thread::sleep(Duration::from_millis(50));
        };
        fn read_bounded_output(mut file: &fs::File) -> Result<Vec<u8>, String> {
            const MAX_COMMAND_OUTPUT: u64 = 8 * 1024 * 1024;
            file.seek(SeekFrom::Start(0))
                .map_err(|_| "隔离测试子命令输出无法定位".to_string())?;
            let mut bytes = Vec::new();
            file.take(MAX_COMMAND_OUTPUT + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "隔离测试子命令输出无法读取".to_string())?;
            if bytes.len() as u64 > MAX_COMMAND_OUTPUT {
                return Err("隔离测试子命令输出超过安全限制".to_string());
            }
            Ok(bytes)
        }
        let stdout = read_bounded_output(&output_files.stdout)?;
        let stderr = read_bounded_output(&output_files.stderr)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    struct InstalledScienceSpawn<'a> {
        science_bin: &'a Path,
        sandbox_home: &'a Path,
        safe_bin: &'a Path,
        data_dir: &'a Path,
        port: u16,
        sandbox_port: u16,
        anthropic_base_url: &'a str,
        stdout_path: &'a Path,
        stderr_path: &'a Path,
    }

    fn spawn_installed_science(spec: InstalledScienceSpawn<'_>) -> Child {
        let stdout = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(spec.stdout_path)
            .unwrap();
        let stderr = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(spec.stderr_path)
            .unwrap();
        let mut command = safe_e2e_command(spec.science_bin, spec.sandbox_home, spec.safe_bin);
        command
            .arg("serve")
            .arg("--data-dir")
            .arg(spec.data_dir)
            .arg("--port")
            .arg(spec.port.to_string())
            .arg("--sandbox-port")
            .arg(spec.sandbox_port.to_string())
            .arg("--no-browser")
            .arg("--no-auto-update")
            .env("ANTHROPIC_BASE_URL", spec.anthropic_base_url)
            .env("http_proxy", "http://127.0.0.1:9")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("https_proxy", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("no_proxy", "127.0.0.1,localhost,::1")
            .env("NO_PROXY", "127.0.0.1,localhost,::1")
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap()
    }

    fn local_science_login_url(output: &str, expected_port: u16) -> Option<String> {
        let url = output
            .split_whitespace()
            .find(|part| part.starts_with("http://") || part.starts_with("https://"))
            .map(|part| part.trim_matches(|value: char| value == '\'' || value == '"'))
            .map(str::to_string)?;
        let localhost = format!("http://localhost:{expected_port}/");
        let loopback = format!("http://127.0.0.1:{expected_port}/");
        if url.starts_with(&localhost) || url.starts_with(&loopback) {
            Some(url)
        } else {
            None
        }
    }

    #[test]
    fn installed_e2e_login_url_is_exact_dynamic_loopback_only() {
        assert_eq!(
            local_science_login_url("http://localhost:54321/?nonce=test", 54321).as_deref(),
            Some("http://localhost:54321/?nonce=test")
        );
        assert!(local_science_login_url("http://127.0.0.1:54321/?nonce=test", 54321).is_some());
        for unsafe_url in [
            "https://localhost:54321/?nonce=test",
            "http://localhost:54322/?nonce=test",
            "http://localhost:54321.evil.test/?nonce=test",
            "http://localhost:54321@evil.test/?nonce=test",
            "https://example.test/",
        ] {
            assert!(local_science_login_url(unsafe_url, 54321).is_none());
        }
    }

    #[test]
    fn safe_e2e_command_clears_injected_parent_credentials_before_child_probe() {
        let requested_tmp = tmpdir("safe-command-env-probe");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let _temp_guard = RemoveTempTree(tmp.clone());
        let sandbox_home = tmp.join("home");
        fs::create_dir_all(&sandbox_home).unwrap();
        let safe_bin = prepare_safe_e2e_bin(&tmp);
        let injected_names = [
            "ANTHROPIC_AUTH_TOKEN",
            "OPENROUTER_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CSSWITCH_RANDOM_PROBE_KEY",
            "CSSWITCH_RANDOM_PROBE_TOKEN",
        ];
        let mut command = Command::new("/usr/bin/env");
        for name in injected_names {
            command.env(name, "must-not-reach-child");
        }
        sanitize_e2e_command(&mut command, &sandbox_home, &safe_bin);
        let output = command_output_with_timeout(&mut command, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        let child_environment = String::from_utf8(output.stdout).unwrap();
        for name in injected_names {
            assert!(!child_environment.lines().any(|line| {
                line.strip_prefix(name)
                    .is_some_and(|remainder| remainder.starts_with('='))
            }));
        }
        let allowed = [
            "HOME",
            "PATH",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "CSSWITCH_E2E_SECURITY_MARKER",
        ];
        for line in child_environment.lines() {
            let name = line.split_once('=').map(|(name, _)| name).unwrap_or(line);
            assert!(
                allowed.contains(&name),
                "unexpected child environment name: {name}"
            );
            assert!(!name.ends_with("_KEY"));
            assert!(!name.ends_with("_TOKEN"));
        }
    }

    #[test]
    fn security_stub_boundary_blocks_missing_stderr_wrong_exit_and_timeout() {
        let requested_tmp = tmpdir("security-stub-boundary");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let _temp_guard = RemoveTempTree(tmp.clone());
        let sandbox_home = tmp.join("home");
        fs::create_dir_all(&sandbox_home).unwrap();
        let marker = sandbox_home.join("security-stub-invoked.log");
        fs::write(
            &marker,
            b"science_pid=12345 argv=find-generic-password exit=1\n",
        )
        .unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        let stderr = tmp.join("science.stderr.log");

        assert_eq!(
            security_stub_boundary_state(&sandbox_home, 1, &stderr).unwrap(),
            SecurityStubBoundaryState::Pending
        );

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: security find-generic-password failed (exit 2)\n",
        )
        .unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: security find-generic-password could not run (ETIMEDOUT)\n",
        )
        .unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: could not mirror keys to the macOS Keychain (security find-generic-password failed (exit 1)); continuing with /tmp/encryption.key\n",
        )
        .unwrap();
        assert_eq!(
            security_stub_boundary_state(&sandbox_home, 1, &stderr).unwrap(),
            SecurityStubBoundaryState::Ready(1)
        );

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: security find-generic-password failed (exit 1)\nensureEncryptionKeys: security find-generic-password failed (exit 1)\n",
        )
        .unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: could not mirror keys to the macOS Keychain (security find-generic-password failed (exit 1)); continuing with /tmp/encryption.key unexpected-suffix\n",
        )
        .unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: could not mirror keys to the macOS Keychain (security find-generic-password failed (exit 1)); continuing with /tmp/encryption.key\nensureEncryptionKeys: security find-generic-password failed (exit 2)\n",
        )
        .unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());

        fs::write(
            &stderr,
            b"ensureEncryptionKeys: could not mirror keys to the macOS Keychain (security find-generic-password failed (exit 1)); continuing with isolated encryption.key\n",
        )
        .unwrap();
        fs::write(&marker, b"invoked\ninvoked\n").unwrap();
        assert!(security_stub_boundary_state(&sandbox_home, 1, &stderr).is_err());
    }

    #[test]
    fn external_browser_evidence_requires_exact_challenge_hash_and_marker() {
        assert_eq!(browser_driver_label(true), "runtime_selected_browser");
        assert_eq!(browser_driver_label(false), "playwright_cli");

        let requested_tmp = tmpdir("browser-evidence-protocol");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let _temp_guard = RemoveTempTree(tmp.clone());
        let snapshot_path = tmp.join("phase.snapshot.txt");
        let screenshot_path = tmp.join("phase.screenshot.png");
        let done_path = tmp.join("phase.done.json");
        let marker = "button \"View Nature Figure Probe\"";
        fs::write(&snapshot_path, format!("- {marker}\n")).unwrap();
        fs::write(&screenshot_path, b"\x89PNG\r\n\x1a\nprobe").unwrap();
        for path in [&snapshot_path, &screenshot_path] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let artifacts = serde_json::json!({
            "snapshot": {
                "path": snapshot_path,
                "marker": marker,
                "sha256": sha256_bytes(&fs::read(&snapshot_path).unwrap())
            },
            "screenshot": {
                "path": screenshot_path,
                "marker": marker,
                "sha256": sha256_bytes(&fs::read(&screenshot_path).unwrap())
            }
        });
        let write_done = |challenge: &str| {
            fs::write(
                &done_path,
                serde_json::to_vec(&serde_json::json!({
                    "run_token": "run-token",
                    "phase": "phase-one",
                    "challenge": challenge,
                    "expected_label": marker,
                    "runtime_name": "nature-figure--probe",
                    "artifacts": artifacts
                }))
                .unwrap(),
            )
            .unwrap();
            fs::set_permissions(&done_path, fs::Permissions::from_mode(0o600)).unwrap();
        };
        let expectations = [
            BrowserArtifactExpectation {
                key: "snapshot",
                path: &snapshot_path,
                marker,
                kind: BrowserArtifactKind::Snapshot,
            },
            BrowserArtifactExpectation {
                key: "screenshot",
                path: &screenshot_path,
                marker,
                kind: BrowserArtifactKind::Screenshot,
            },
        ];
        write_done("exact-challenge");
        wait_for_external_browser_evidence(BrowserEvidenceContext {
            done_path: &done_path,
            evidence_root: &tmp,
            run_token: "run-token",
            phase: "phase-one",
            challenge: "exact-challenge",
            expected_label: marker,
            runtime_name: "nature-figure--probe",
            artifacts: &expectations,
        });
        write_done("wrong-challenge");
        let wrong: serde_json::Value =
            serde_json::from_slice(&fs::read(&done_path).unwrap()).unwrap();
        assert!(!browser_evidence_identity_matches(
            &wrong,
            "run-token",
            "phase-one",
            "exact-challenge",
            marker,
            "nature-figure--probe",
        ));
    }

    #[test]
    fn installed_e2e_subcommands_are_killed_at_timeout() {
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let started = Instant::now();
        assert!(command_output_with_timeout(&mut command, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn installed_e2e_timeout_does_not_wait_for_descendant_output_handles() {
        let pid_file = env::temp_dir().join(format!(
            "csswitch-command-descendant-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 5 & echo \"$!\" > \"$PID_FILE\"; wait"])
            .env("PID_FILE", &pid_file);
        let started = Instant::now();
        assert!(command_output_with_timeout(&mut command, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        if let Ok(pid) = fs::read_to_string(&pid_file) {
            let _ = Command::new("/bin/kill").arg(pid.trim()).status();
        }
        let _ = fs::remove_file(pid_file);
    }

    #[test]
    fn installed_e2e_subcommand_output_is_bounded() {
        let mut command = Command::new("/bin/dd");
        command.args(["if=/dev/zero", "bs=1048576", "count=9"]);
        assert_eq!(
            command_output_with_timeout(&mut command, Duration::from_secs(10)).unwrap_err(),
            "隔离测试子命令输出超过安全限制"
        );
    }

    fn playwright_output(guard: &RealScienceE2eGuard, args: &[&str]) -> Result<String, String> {
        let mut command =
            safe_e2e_command(&guard.playwright_cli, &guard.sandbox_home, &guard.safe_bin);
        command
            .args(args)
            .current_dir(&guard.playwright_workdir)
            .env("PLAYWRIGHT_CLI_SESSION", &guard.playwright_session)
            .env("NPM_CONFIG_CACHE", &guard.playwright_cache);
        // A fresh isolated npm cache may need to resolve the pinned Playwright CLI on the first
        // `open`. Keep every subsequent UI operation tightly bounded, while giving only that
        // bootstrap step enough time to finish instead of misclassifying dependency setup as a
        // Science UI timeout.
        let timeout = if args.first().copied() == Some("open") {
            Duration::from_secs(300)
        } else {
            Duration::from_secs(30)
        };
        let output = command_output_with_timeout(&mut command, timeout)?;
        if !output.status.success() {
            return Err(format!(
                "Playwright CLI 步骤失败（exit={:?}）",
                output.status.code()
            ));
        }
        String::from_utf8(output.stdout).map_err(|_| "Playwright 输出不是 UTF-8".to_string())
    }

    fn playwright_snapshot(guard: &RealScienceE2eGuard) -> Result<String, String> {
        let output = playwright_output(guard, &["--json", "snapshot"])?;
        let value: serde_json::Value = serde_json::from_str(&output)
            .map_err(|_| "Playwright snapshot JSON 无效".to_string())?;
        value["snapshot"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "Playwright snapshot 缺少结构化内容".to_string())
    }

    fn wait_snapshot_for_control(
        guard: &RealScienceE2eGuard,
        needle: &str,
        attempts: usize,
        timeout: Duration,
    ) -> Result<String, String> {
        if attempts == 0 || timeout.is_zero() {
            return Err(format!("隔离 UI 等待参数无效：{needle}"));
        }
        let started = Instant::now();
        for attempt in 0..attempts {
            let snapshot = playwright_snapshot(guard)?;
            if snapshot_ref(&snapshot, needle).is_some() {
                return Ok(snapshot);
            }
            if attempt + 1 >= attempts || started.elapsed() >= timeout {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err(format!("隔离 UI 控件在有界等待内未出现：{needle}"))
    }

    fn snapshot_ref(snapshot: &str, needle: &str) -> Option<String> {
        let line = snapshot.lines().find(|line| line.contains(needle))?;
        let start = line.find("[ref=")? + "[ref=".len();
        let end = line[start..].find(']')? + start;
        let value = &line[start..end];
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte == b'f' || byte == b'e' || byte.is_ascii_digit())
        {
            return None;
        }
        Some(value.to_string())
    }

    fn playwright_click(
        guard: &RealScienceE2eGuard,
        snapshot: &str,
        needle: &str,
    ) -> Result<(), String> {
        let element = snapshot_ref(snapshot, needle)
            .ok_or_else(|| format!("隔离 UI 缺少预期控件：{needle}"))?;
        playwright_output(guard, &["click", &element]).map(|_| ())
    }

    fn science_catalog_title(runtime_name: &str) -> String {
        runtime_name
            .split('-')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn open_science_home_ui(guard: &mut RealScienceE2eGuard) -> Result<String, String> {
        let login_url = wait_science_login_url(
            &guard.science_bin,
            &guard.sandbox_home,
            &guard.safe_bin,
            &guard.data_dir,
            guard.port,
        )?;
        guard.browser_open = true;
        playwright_output(guard, &["open", &login_url])?;
        let login_snapshot = playwright_snapshot(guard)?;
        // Current Science may consume the one-time URL and land directly in the isolated
        // workspace; older builds still render an explicit Sign in button. Both are valid only
        // after the same nonce URL was obtained from this owned daemon.
        if login_snapshot.contains("button \"Sign in\"") {
            playwright_click(guard, &login_snapshot, "button \"Sign in\"")?;
            thread::sleep(Duration::from_secs(2));
        }

        for _ in 0..5 {
            let snapshot = playwright_snapshot(guard)?;
            if snapshot.contains("button \"Keep defaults\"") {
                playwright_click(guard, &snapshot, "button \"Keep defaults\"")?;
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            if snapshot.contains("button \"Close\"") {
                playwright_click(guard, &snapshot, "button \"Close\"")?;
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            if snapshot.contains("button \"Account menu") {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        playwright_snapshot(guard)
    }

    fn wait_science_login_url(
        science_bin: &Path,
        sandbox_home: &Path,
        safe_bin: &Path,
        data_dir: &Path,
        port: u16,
    ) -> Result<String, String> {
        for _ in 0..150 {
            let mut command = safe_e2e_command(science_bin, sandbox_home, safe_bin);
            command.arg("url").arg("--data-dir").arg(data_dir);
            let login_output = command_output_with_timeout(&mut command, Duration::from_secs(5))?;
            if login_output.status.success() {
                let output = String::from_utf8(login_output.stdout)
                    .map_err(|_| "Science URL 输出不是 UTF-8".to_string())?;
                if let Some(url) = local_science_login_url(&output, port) {
                    return Ok(url);
                }
            }
            thread::sleep(Duration::from_millis(200));
        }
        Err("Science URL 命令未就绪".to_string())
    }

    #[derive(Clone, Copy)]
    enum BrowserArtifactKind {
        Snapshot,
        Screenshot,
    }

    fn browser_driver_label(external_browser: bool) -> &'static str {
        if external_browser {
            "runtime_selected_browser"
        } else {
            "playwright_cli"
        }
    }

    struct BrowserArtifactExpectation<'a> {
        key: &'a str,
        path: &'a Path,
        marker: &'a str,
        kind: BrowserArtifactKind,
    }

    struct BrowserEvidenceContext<'a> {
        done_path: &'a Path,
        evidence_root: &'a Path,
        run_token: &'a str,
        phase: &'a str,
        challenge: &'a str,
        expected_label: &'a str,
        runtime_name: &'a str,
        artifacts: &'a [BrowserArtifactExpectation<'a>],
    }

    fn sha256_bytes(bytes: &[u8]) -> String {
        let mut digest = Sha256::new();
        digest.update(bytes);
        format!("{:x}", digest.finalize())
    }

    fn browser_evidence_identity_matches(
        done: &serde_json::Value,
        run_token: &str,
        phase: &str,
        challenge: &str,
        expected_label: &str,
        runtime_name: &str,
    ) -> bool {
        done["run_token"].as_str() == Some(run_token)
            && done["phase"].as_str() == Some(phase)
            && done["challenge"].as_str() == Some(challenge)
            && done["expected_label"].as_str() == Some(expected_label)
            && done["runtime_name"].as_str() == Some(runtime_name)
    }

    fn wait_for_external_browser_evidence(context: BrowserEvidenceContext<'_>) {
        for _ in 0..2_400 {
            if context.done_path.is_file() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
        assert!(
            context.done_path.is_file(),
            "应用内 Browser 在有界等待内未完成：{}",
            context.done_path.display()
        );
        let done_metadata = fs::symlink_metadata(context.done_path).unwrap();
        assert!(done_metadata.file_type().is_file());
        assert_eq!(done_metadata.permissions().mode() & 0o077, 0);
        assert!(done_metadata.len() <= 1024 * 1024);
        let done: serde_json::Value =
            serde_json::from_slice(&fs::read(context.done_path).unwrap()).unwrap();
        assert!(browser_evidence_identity_matches(
            &done,
            context.run_token,
            context.phase,
            context.challenge,
            context.expected_label,
            context.runtime_name,
        ));
        let artifacts = done["artifacts"].as_object().unwrap();
        assert_eq!(artifacts.len(), context.artifacts.len());
        let canonical_root = fs::canonicalize(context.evidence_root).unwrap();
        for expected in context.artifacts {
            let artifact = artifacts.get(expected.key).unwrap().as_object().unwrap();
            assert_eq!(
                artifact.get("path").and_then(serde_json::Value::as_str),
                expected.path.to_str()
            );
            assert_eq!(
                artifact.get("marker").and_then(serde_json::Value::as_str),
                Some(expected.marker)
            );
            let metadata = fs::symlink_metadata(expected.path).unwrap();
            assert!(metadata.file_type().is_file());
            assert_eq!(metadata.permissions().mode() & 0o077, 0);
            assert!(metadata.len() > 0 && metadata.len() <= 16 * 1024 * 1024);
            let canonical_path = fs::canonicalize(expected.path).unwrap();
            assert_eq!(canonical_path.parent(), Some(canonical_root.as_path()));
            let bytes = fs::read(expected.path).unwrap();
            assert_eq!(
                artifact.get("sha256").and_then(serde_json::Value::as_str),
                Some(sha256_bytes(&bytes).as_str())
            );
            match expected.kind {
                BrowserArtifactKind::Snapshot => {
                    assert!(String::from_utf8(bytes).unwrap().contains(expected.marker));
                }
                BrowserArtifactKind::Screenshot => {
                    assert!(
                        bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                            || bytes.starts_with(b"\xff\xd8\xff")
                    );
                }
            }
        }
    }

    fn open_science_skills_ui(guard: &mut RealScienceE2eGuard) -> Result<String, String> {
        let home_snapshot = open_science_home_ui(guard)?;
        playwright_click(guard, &home_snapshot, "button \"Account menu")?;
        let menu_snapshot =
            wait_snapshot_for_control(guard, "menuitem \"Settings\"", 25, Duration::from_secs(8))?;
        playwright_click(guard, &menu_snapshot, "menuitem \"Settings\"")?;
        let settings_snapshot =
            wait_snapshot_for_control(guard, "button \"Skills\"", 25, Duration::from_secs(8))?;
        playwright_click(guard, &settings_snapshot, "button \"Skills\"")?;
        playwright_snapshot(guard)
    }

    fn open_science_example_chat(guard: &mut RealScienceE2eGuard) -> Result<String, String> {
        open_science_home_ui(guard)?;
        let home_snapshot = wait_snapshot_for_control(
            guard,
            "button \"Open project Example project\"",
            25,
            Duration::from_secs(8),
        )?;
        playwright_click(
            guard,
            &home_snapshot,
            "button \"Open project Example project\"",
        )?;
        let project_snapshot =
            wait_snapshot_for_control(guard, "button \"New\"", 25, Duration::from_secs(8))?;
        if project_snapshot.contains("button \"New\"") {
            playwright_click(guard, &project_snapshot, "button \"New\"")?;
        }
        wait_snapshot_for_control(guard, "textbox \"Ask anything", 30, Duration::from_secs(10))
    }

    fn send_science_chat_prompt(
        guard: &RealScienceE2eGuard,
        snapshot: &str,
        prompt: &str,
    ) -> Result<(), String> {
        let current = if snapshot_ref(snapshot, "textbox \"Ask anything").is_some() {
            snapshot.to_string()
        } else {
            wait_snapshot_for_control(guard, "textbox \"Ask anything", 25, Duration::from_secs(8))?
        };
        let textbox = snapshot_ref(&current, "textbox \"Ask anything")
            .ok_or_else(|| "隔离 UI 缺少聊天输入框".to_string())?;
        playwright_output(guard, &["fill", &textbox, prompt])?;
        let filled =
            wait_snapshot_for_control(guard, "button \"Send\"", 25, Duration::from_secs(8))?;
        playwright_click(guard, &filled, "button \"Send\"")
    }

    fn wait_for_skill_mock_round(server: &SkillMockServer) -> SkillMockObservation {
        for _ in 0..600 {
            let observation = server.snapshot();
            if observation.main_count >= 2 && observation.tool_result_count >= 1 {
                return observation;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("isolated Skill trigger mock did not observe a completed tool round");
    }

    #[test]
    fn skill_trigger_mock_classifies_only_streaming_requests_with_skill_tool_as_main() {
        let initial = serde_json::json!({
            "stream": true,
            "tools": [{"name": "skill", "input_schema": {"properties": {}}}],
            "messages": [{"role": "user", "content": "do not retain this prompt"}]
        });
        assert_eq!(
            skill_mock_request_kind(&initial),
            SkillMockRequestKind::MainInitial
        );
        let tool_result = serde_json::json!({
            "stream": true,
            "tools": [{"name": "skill", "input_schema": {"properties": {}}}],
            "messages": [{"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu", "is_error": false,
                "content": "marker"
            }]}]
        });
        assert_eq!(
            skill_mock_request_kind(&tool_result),
            SkillMockRequestKind::MainToolResult
        );
        for auxiliary in [
            serde_json::json!({"stream": false, "tools": [{"name": "skill"}]}),
            serde_json::json!({"stream": true, "tools": [{"name": "web_search"}]}),
            serde_json::json!({"stream": true}),
        ] {
            assert_eq!(
                skill_mock_request_kind(&auxiliary),
                SkillMockRequestKind::Auxiliary
            );
        }
    }

    #[test]
    fn skill_trigger_mock_observation_is_structural_and_redacted() {
        let marker = "CSSWITCH_TEST_MARKER_MUST_NOT_BE_RETAINED";
        let prompt = "sensitive prompt must not be retained";
        let value = serde_json::json!({
            "stream": true,
            "tools": [{
                "name": "skill",
                "input_schema": {"properties": {
                    "skill": {"type": "string"},
                    "filter": {"type": "string"},
                    "human_description": {"type": "string"}
                }}
            }],
            "messages": [
                {"role": "user", "content": prompt},
                {"role": "assistant", "content": [{
                    "type": "tool_use", "id": "toolu", "name": "skill", "input": {}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result", "tool_use_id": "toolu",
                    "content": [{"type": "text", "text": marker}]
                }]}
            ]
        });
        let mut observation = SkillMockObservation::default();
        assert_eq!(
            observe_skill_mock_request(&value, "expected-runtime", marker, &mut observation),
            SkillMockRequestKind::MainToolResult
        );
        assert_eq!(observation.main_count, 1);
        assert_eq!(
            observation.skill_schema_fields,
            ["filter", "human_description", "skill"]
        );
        assert_eq!(
            observation.tool_order,
            ["request:skill", "request:tool_result"]
        );
        assert_eq!(observation.tool_input_matches_runtime, Some(false));
        assert_eq!(observation.tool_result_count, 1);
        assert_eq!(observation.tool_result_is_error, Some(false));
        assert!(observation.marker_present);
        let debug = format!("{observation:?}");
        assert!(!debug.contains(prompt));
        assert!(!debug.contains(marker));
    }

    #[test]
    fn skill_trigger_mock_responses_use_exact_runtime_name_and_valid_envelopes() {
        let runtime_name = "science-trigger-probe-1a2b3c4d";
        let skill_sse = String::from_utf8(anthropic_skill_tool_sse(runtime_name)).unwrap();
        assert!(skill_sse.contains("event: message_start"));
        assert!(skill_sse.contains("event: content_block_delta"));
        assert!(skill_sse.contains("event: message_stop"));
        assert!(skill_sse.contains(runtime_name));
        assert!(skill_sse.contains("human_description"));
        assert!(skill_sse.contains("\"stop_reason\":\"tool_use\""));

        let terminal = String::from_utf8(anthropic_terminal_sse()).unwrap();
        assert!(terminal.contains("text_delta"));
        assert!(terminal.contains("event: message_stop"));
        let auxiliary: serde_json::Value =
            serde_json::from_slice(&anthropic_json_response()).unwrap();
        assert_eq!(auxiliary["type"], "message");
        assert_eq!(auxiliary["stop_reason"], "end_turn");
    }

    fn e2e_compatibility_context(science_version: &str) -> RuntimeContext {
        e2e_compatibility_context_for(science_version, DeploymentStatus::Pending)
    }

    fn e2e_compatibility_context_for(
        science_version: &str,
        deployment_status: DeploymentStatus,
    ) -> RuntimeContext {
        RuntimeContext {
            science_version: Some(science_version.to_string()),
            platform: "macos".to_string(),
            sandbox_state: SandboxState::Ready,
            deployment_status,
            discovery_status: DiscoveryStatus::Unknown,
            network_mode: NetworkMode::Gateway,
            network: CapabilityAvailability::Unknown,
            mcp: CapabilityAvailability::Unknown,
            local_command_policy: LocalCommandPolicy::Unknown,
            ssh: SshCapabilitySummary {
                transport: CapabilityAvailability::Unknown,
                agent_visible: BooleanCapability::Unknown,
                config_available: BooleanCapability::Unknown,
            },
            available_binaries: std::collections::BTreeSet::new(),
            binary_inventory: CapabilityAvailability::Unknown,
            available_environment: std::collections::BTreeSet::new(),
            environment_inventory: CapabilityAvailability::Unknown,
            available_runtime_assets: std::collections::BTreeSet::new(),
            runtime_asset_inventory: CapabilityAvailability::Unknown,
        }
    }

    fn e2e_enable_skill_with_exact_compatibility_ack(
        manager: &SkillManager,
        installed: &InstalledSkill,
        science_version: &str,
        data_dir: &Path,
    ) {
        let catalog = load_static_catalog().unwrap();
        let context = e2e_compatibility_context_for(science_version, DeploymentStatus::NotDeployed);
        let before = fs::read(&manager.paths.inventory).unwrap();
        let data_dir_before = metadata_tree_summary(data_dir);
        let registry = manager.paths.root.join("deployments.v1.json");
        assert!(!registry.exists());
        let no_ack = manager
            .set_enabled_with_compatibility(&installed.skill_id, true, &[], &context, &catalog)
            .unwrap_err();
        assert_eq!(
            no_ack.code,
            SkillErrorCode::CompatibilityAcknowledgmentRequired
        );
        assert_eq!(before, fs::read(&manager.paths.inventory).unwrap());
        assert_eq!(data_dir_before, metadata_tree_summary(data_dir));
        assert!(!registry.exists());
        assert!(!data_dir_before.iter().any(|entry| {
            entry
                .relative_path
                .ends_with(b".csswitch-skill-deployment.v1.json")
        }));
        let runtime_suffix = format!("skills/{}", installed.runtime_name).into_bytes();
        assert!(!data_dir_before
            .iter()
            .any(|entry| entry.relative_path.ends_with(&runtime_suffix)));
        let gate = evaluate_compatibility_gate(installed, &context, &catalog).unwrap();
        manager
            .set_enabled_with_compatibility(
                &installed.skill_id,
                true,
                &gate.required_rule_ids,
                &context,
                &catalog,
            )
            .unwrap();
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct MetadataSummaryEntry {
        relative_path: Vec<u8>,
        file_type: u8,
        mode: u32,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    }

    fn metadata_tree_summary(root: &Path) -> Vec<MetadataSummaryEntry> {
        fn walk(root: &Path, current: &Path, output: &mut Vec<MetadataSummaryEntry>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let file_type = if metadata.file_type().is_dir() {
                    1
                } else if metadata.file_type().is_file() {
                    2
                } else if metadata.file_type().is_symlink() {
                    3
                } else {
                    4
                };
                output.push(MetadataSummaryEntry {
                    relative_path: path
                        .strip_prefix(root)
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                    file_type,
                    mode: metadata.mode(),
                    size: metadata.size(),
                    modified_seconds: metadata.mtime(),
                    modified_nanoseconds: metadata.mtime_nsec(),
                });
                if metadata.file_type().is_dir() {
                    walk(root, &path, output);
                }
            }
        }

        let mut output = Vec::new();
        if root.is_dir() {
            walk(root, root, &mut output);
        }
        output
    }

    fn e2e_compatibility_reconcile(
        manager: &SkillManager,
        data_dir: &Path,
        science_version: &str,
        reason: &str,
    ) -> crate::skill_manager::deployment::ReconcileReport {
        let catalog = load_static_catalog().unwrap();
        manager
            .reconcile_with_compatibility(data_dir, false, reason, &catalog, |installed| {
                Ok(e2e_compatibility_context_for(
                    science_version,
                    installed.deployment_status,
                ))
            })
            .unwrap()
    }

    fn installed_science_version(
        science_bin: &Path,
        sandbox_home: &Path,
        safe_bin: &Path,
    ) -> String {
        let mut command = safe_e2e_command(science_bin, sandbox_home, safe_bin);
        command.arg("--version");
        let output = command_output_with_timeout(&mut command, Duration::from_secs(10)).unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    #[ignore = "explicit installed Science discovery E2E; temp HOME, dynamic ports, local browser only"]
    fn isolated_installed_science_discovers_manager_skill_in_public_catalog() {
        assert_eq!(
            env::var("CSSWITCH_REAL_SCIENCE_E2E").as_deref(),
            Ok("1"),
            "必须显式设置 CSSWITCH_REAL_SCIENCE_E2E=1"
        );
        let _serial = crate::skill_manager::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let requested_tmp = tmpdir("real-skill-discovery");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let _temp_guard = RemoveTempTree(tmp.clone());
        let outer_home = tmp.join("home");
        let sandbox_home = outer_home.join(".csswitch/sandbox/home");
        let safe_bin = prepare_safe_e2e_bin(&sandbox_home);
        let data_dir = sandbox_home.join(".claude-science");
        let source = tmp.join("skill-source");
        let browser_workdir = tmp.join("browser-workdir");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&browser_workdir).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: Science Discovery Probe\ndescription: Isolated installed Science discovery marker\n---\nReturn the unique marker only when explicitly requested.\n",
        )
        .unwrap();
        let home_control = sandbox_home.join(".claude/skills/home-control-4d22");
        fs::create_dir_all(&home_control).unwrap();
        fs::write(
            home_control.join("SKILL.md"),
            "---\nname: Home Control 4d22\ndescription: Must remain undiscovered\n---\ncontrol\n",
        )
        .unwrap();

        let config_dir = outer_home.join(".csswitch");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let science_bin = env::var_os("CSSWITCH_REAL_SCIENCE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science",
                )
            });
        let version = installed_science_version(&science_bin, &sandbox_home, &safe_bin);
        let manager = SkillManager::new(config_dir);
        let installed = manager.import_source(&source).unwrap().skill;
        e2e_enable_skill_with_exact_compatibility_ack(&manager, &installed, &version, &data_dir);
        let (forged, _) = oauth_forge::ensure_virtual_login(
            &data_dir,
            "virtual@localhost.invalid",
            &sandbox_home,
        )
        .unwrap();
        assert!(!forged.org_uuid.is_empty());
        let reconcile =
            e2e_compatibility_reconcile(&manager, &data_dir, &version, "real_discovery");
        assert!(reconcile.errors.is_empty());
        let playwright_cli = env::var_os("CSSWITCH_PLAYWRIGHT_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/Users/superjj/.codex/skills/playwright/scripts/playwright_cli.sh")
            });
        assert!(science_bin.is_file());
        assert!(playwright_cli.is_file());
        let port = free_port();
        let sandbox_port = free_port();
        assert_ne!(port, sandbox_port);
        let child = spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: "http://127.0.0.1:9",
            stdout_path: &tmp.join("science.stdout.log"),
            stderr_path: &tmp.join("science.stderr.log"),
        });
        let mut guard = RealScienceE2eGuard {
            science_bin: science_bin.clone(),
            sandbox_home: sandbox_home.clone(),
            safe_bin: safe_bin.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
            playwright_cli,
            playwright_session: format!("csswitch-discovery-{}", installed.skill_id.short()),
            playwright_cache: env::var_os("CSSWITCH_E2E_NPM_CACHE")
                .map(PathBuf::from)
                .unwrap_or_else(|| tmp.join("npm-cache")),
            playwright_workdir: browser_workdir,
            browser_open: false,
            port,
        };
        wait_port_reachable(port);
        manager.mark_science_started(&data_dir).unwrap();

        let mut version_command = safe_e2e_command(&science_bin, &sandbox_home, &safe_bin);
        version_command.arg("--version");
        let version_output =
            command_output_with_timeout(&mut version_command, Duration::from_secs(10)).unwrap();
        assert!(version_output.status.success());
        let version = String::from_utf8(version_output.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_string();
        let skills_snapshot = open_science_skills_ui(&mut guard).unwrap();
        let expected_catalog_button = format!(
            "button \"View {}\"",
            science_catalog_title(&installed.runtime_name)
        );
        assert!(skills_snapshot.contains(&expected_catalog_button));
        assert!(!skills_snapshot.contains("home-control-4d22"));
        assert!(!skills_snapshot.contains("Home Control 4d22"));

        let observed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        manager
            .record_discovery_observation(
                &data_dir,
                &installed.skill_id,
                &version,
                true,
                observed_at,
            )
            .unwrap();
        let status = manager
            .status(&data_dir, ScienceProbeState::Running, Some(&version))
            .unwrap();
        assert_eq!(status.skills.len(), 1);
        assert_eq!(
            status.skills[0].discovery_status,
            DiscoveryStatus::Discovered
        );

        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);

        fs::remove_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let rebuilt_active_org = data_dir.join("active-org.json");
        fs::write(
            &rebuilt_active_org,
            serde_json::to_vec(&serde_json::json!({ "org_uuid": forged.org_uuid })).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&rebuilt_active_org, fs::Permissions::from_mode(0o600)).unwrap();
        let (rebuilt_login, _) = oauth_forge::ensure_virtual_login(
            &data_dir,
            "virtual@localhost.invalid",
            &sandbox_home,
        )
        .unwrap();
        assert_eq!(rebuilt_login.org_uuid, forged.org_uuid);
        let rebuilt = e2e_compatibility_reconcile(&manager, &data_dir, &version, "sandbox_rebuild");
        assert!(rebuilt.errors.is_empty());
        guard.child = Some(spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: "http://127.0.0.1:9",
            stdout_path: &tmp.join("science-rebuilt.stdout.log"),
            stderr_path: &tmp.join("science-rebuilt.stderr.log"),
        }));
        wait_port_reachable(port);
        manager.mark_science_started(&data_dir).unwrap();
        let before_reprobe = manager
            .status(&data_dir, ScienceProbeState::Running, Some(&version))
            .unwrap();
        assert_eq!(
            before_reprobe.skills[0].discovery_status,
            DiscoveryStatus::Unknown
        );
        let rebuilt_snapshot = open_science_skills_ui(&mut guard).unwrap();
        assert!(rebuilt_snapshot.contains(&expected_catalog_button));
        assert!(!rebuilt_snapshot.contains("home-control-4d22"));
        manager
            .record_discovery_observation(
                &data_dir,
                &installed.skill_id,
                &version,
                true,
                observed_at.saturating_add(1),
            )
            .unwrap();
        let rebuilt_status = manager
            .status(&data_dir, ScienceProbeState::Running, Some(&version))
            .unwrap();
        assert_eq!(
            rebuilt_status.skills[0].discovery_status,
            DiscoveryStatus::Discovered
        );

        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);
        drop(guard);
        drop(_temp_guard);
        assert!(env::var("CSSWITCH_KEEP_E2E_TMP").as_deref() == Ok("1") || !tmp.exists());
    }

    #[test]
    #[ignore = "explicit installed Science trigger E2E; temp HOME, dynamic ports, local mock/browser only"]
    fn isolated_installed_science_triggers_enabled_skill_and_rejects_disabled_skill() {
        assert_eq!(
            env::var("CSSWITCH_REAL_SCIENCE_TRIGGER_E2E").as_deref(),
            Ok("1"),
            "必须显式设置 CSSWITCH_REAL_SCIENCE_TRIGGER_E2E=1"
        );
        let _serial = crate::skill_manager::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let requested_tmp = tmpdir("real-skill-trigger");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let _temp_guard = RemoveTempTree(tmp.clone());
        let outer_home = tmp.join("home");
        let sandbox_home = outer_home.join(".csswitch/sandbox/home");
        let safe_bin = prepare_safe_e2e_bin(&sandbox_home);
        let data_dir = sandbox_home.join(".claude-science");
        let source = tmp.join("skill-source");
        let browser_workdir = tmp.join("browser-workdir");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&browser_workdir).unwrap();
        const MARKER: &str = "CSSWITCH_SKILL_TRIGGER_6F8D2A91";
        fs::write(
            source.join("SKILL.md"),
            format!(
                "---\nname: Science Trigger Probe\ndescription: Isolated installed Science trigger proof\n---\nWhen this Skill is loaded, return exactly this marker: {MARKER}\n"
            ),
        )
        .unwrap();

        let config_dir = outer_home.join(".csswitch");
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let science_bin = env::var_os("CSSWITCH_REAL_SCIENCE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science",
                )
            });
        let version = installed_science_version(&science_bin, &sandbox_home, &safe_bin);
        let manager = SkillManager::new(config_dir);
        let installed = manager.import_source(&source).unwrap().skill;
        e2e_enable_skill_with_exact_compatibility_ack(&manager, &installed, &version, &data_dir);
        oauth_forge::ensure_virtual_login(&data_dir, "virtual@localhost.invalid", &sandbox_home)
            .unwrap();
        let deployed =
            e2e_compatibility_reconcile(&manager, &data_dir, &version, "real_trigger_enabled");
        assert!(deployed.errors.is_empty());
        assert!(deployed
            .applied
            .iter()
            .any(|item| item.skill_id == installed.skill_id));

        let playwright_cli = env::var_os("CSSWITCH_PLAYWRIGHT_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/Users/superjj/.codex/skills/playwright/scripts/playwright_cli.sh")
            });
        assert!(science_bin.is_file());
        assert!(playwright_cli.is_file());
        let mock = SkillMockServer::start(installed.runtime_name.clone(), MARKER.to_string());
        let port = free_port();
        let sandbox_port = free_port();
        assert_ne!(port, sandbox_port);
        assert_ne!(port, mock.port);
        assert_ne!(sandbox_port, mock.port);
        let mock_endpoint = mock.endpoint();
        let child = spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: &mock_endpoint,
            stdout_path: &tmp.join("science-enabled.stdout.log"),
            stderr_path: &tmp.join("science-enabled.stderr.log"),
        });
        let mut guard = RealScienceE2eGuard {
            science_bin: science_bin.clone(),
            sandbox_home: sandbox_home.clone(),
            safe_bin: safe_bin.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
            playwright_cli,
            playwright_session: format!("csswitch-trigger-{}", installed.skill_id.short()),
            playwright_cache: env::var_os("CSSWITCH_E2E_NPM_CACHE")
                .map(PathBuf::from)
                .unwrap_or_else(|| tmp.join("npm-cache")),
            playwright_workdir: browser_workdir,
            browser_open: false,
            port,
        };
        wait_port_reachable(port);
        let _ = assert_security_stub_boundary(
            &sandbox_home,
            1,
            &tmp.join("science-enabled.stderr.log"),
        );
        manager.mark_science_started(&data_dir).unwrap();

        let skills_snapshot = open_science_skills_ui(&mut guard).unwrap();
        let expected_catalog_button = format!(
            "button \"View {}\"",
            science_catalog_title(&installed.runtime_name)
        );
        assert!(skills_snapshot.contains(&expected_catalog_button));
        guard.close_browser();

        let chat_snapshot = open_science_example_chat(&mut guard).unwrap();
        send_science_chat_prompt(
            &guard,
            &chat_snapshot,
            "Use the installed Science Trigger Probe Skill now.",
        )
        .unwrap();
        let enabled = wait_for_skill_mock_round(&mock);
        assert_eq!(enabled.main_count, 2);
        assert_eq!(
            enabled.skill_schema_fields,
            ["filter", "human_description", "skill"]
        );
        assert_eq!(
            enabled.tool_order,
            ["response:skill", "request:skill", "request:tool_result"]
        );
        assert_eq!(enabled.tool_input_matches_runtime, Some(true));
        assert_eq!(enabled.tool_result_count, 1);
        assert_eq!(enabled.tool_result_is_error, Some(false));
        assert!(enabled.marker_present);

        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);
        manager.set_enabled(&installed.skill_id, false).unwrap();
        let removed = manager
            .reconcile(&data_dir, false, "real_trigger_disabled")
            .unwrap();
        assert!(removed.errors.is_empty());
        assert!(removed
            .applied
            .iter()
            .any(|item| item.skill_id == installed.skill_id));
        mock.reset();

        guard.child = Some(spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: &mock_endpoint,
            stdout_path: &tmp.join("science-disabled.stdout.log"),
            stderr_path: &tmp.join("science-disabled.stderr.log"),
        }));
        wait_port_reachable(port);
        manager.mark_science_started(&data_dir).unwrap();
        let disabled_chat = open_science_example_chat(&mut guard).unwrap();
        send_science_chat_prompt(
            &guard,
            &disabled_chat,
            "Try to use the disabled Science Trigger Probe Skill now.",
        )
        .unwrap();
        let disabled = wait_for_skill_mock_round(&mock);
        assert_eq!(disabled.main_count, 2);
        assert_eq!(
            disabled.tool_order,
            ["response:skill", "request:skill", "request:tool_result"]
        );
        assert_eq!(disabled.tool_input_matches_runtime, Some(true));
        assert_eq!(disabled.tool_result_count, 1);
        assert_eq!(disabled.tool_result_is_error, Some(true));
        assert!(!disabled.marker_present);

        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);
        for log_name in [
            "science-enabled.stdout.log",
            "science-enabled.stderr.log",
            "science-disabled.stdout.log",
            "science-disabled.stderr.log",
        ] {
            let log = fs::read(tmp.join(log_name)).unwrap_or_default();
            assert!(!String::from_utf8_lossy(&log).contains(MARKER));
        }
        drop(guard);
        drop(mock);
        drop(_temp_guard);
        assert!(env::var("CSSWITCH_KEEP_E2E_TMP").as_deref() == Ok("1") || !tmp.exists());
    }

    #[test]
    #[ignore = "explicit verified scientific-agent-skills source-tree regression; no Science/UI process"]
    fn verified_scientific_skill_shapes_scan_import_deploy_and_rebuild() {
        #[derive(Clone, Copy)]
        struct Fixture {
            directory: &'static str,
            expected_paths: &'static [&'static str],
            expected_executable: Option<&'static str>,
            actual_function: &'static str,
        }

        fn copy_regular_tree(source: &Path, destination: &Path) {
            fs::create_dir_all(destination).unwrap();
            let mut entries = fs::read_dir(source)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let source_path = entry.path();
                let destination_path = destination.join(entry.file_name());
                let metadata = fs::symlink_metadata(&source_path).unwrap();
                assert!(
                    !metadata.file_type().is_symlink(),
                    "fixture symlinks are forbidden"
                );
                if metadata.file_type().is_dir() {
                    copy_regular_tree(&source_path, &destination_path);
                } else {
                    assert!(metadata.file_type().is_file());
                    fs::copy(&source_path, &destination_path).unwrap();
                    fs::set_permissions(
                        &destination_path,
                        fs::Permissions::from_mode(metadata.permissions().mode() & 0o777),
                    )
                    .unwrap();
                }
            }
        }

        assert_eq!(
            env::var("CSSWITCH_REAL_SCIENTIFIC_SKILLS_E2E").as_deref(),
            Ok("1"),
            "必须显式设置 CSSWITCH_REAL_SCIENTIFIC_SKILLS_E2E=1"
        );
        let verified_root = env::var_os("CSSWITCH_REAL_SCIENTIFIC_SKILLS_ROOT")
            .map(PathBuf::from)
            .expect("CSSWITCH_REAL_SCIENTIFIC_SKILLS_ROOT must point to the repository skills/");
        assert!(verified_root.is_dir());
        let fixtures = [
            Fixture {
                directory: "bgpt-paper-search",
                expected_paths: &["SKILL.md"],
                expected_executable: None,
                actual_function: "not_run_requires_network_and_bgpt_mcp",
            },
            Fixture {
                directory: "arbor",
                expected_paths: &[
                    "SKILL.md",
                    "scripts/tree.py",
                    "references/htr-methodology.md",
                ],
                expected_executable: None,
                actual_function: "not_run_requires_git_worktrees_evaluator_and_agent_subprocesses",
            },
            Fixture {
                directory: "citation-management",
                expected_paths: &[
                    "SKILL.md",
                    "scripts/format_bibtex.py",
                    "assets/bibtex_template.bib",
                    "references/citation_validation.md",
                ],
                expected_executable: Some("scripts/format_bibtex.py"),
                actual_function: "not_run_requires_python_dependencies_or_network_for_search",
            },
            Fixture {
                directory: "database-lookup",
                expected_paths: &[
                    "SKILL.md",
                    "references/retrieval-contract.md",
                    "references/pubchem.md",
                    "references/uniprot.md",
                ],
                expected_executable: None,
                actual_function: "not_run_requires_external_database_network_access",
            },
        ];
        for fixture in fixtures {
            let source = verified_root.join(fixture.directory);
            assert!(
                source.is_dir(),
                "missing verified fixture {}",
                source.display()
            );
            for relative in fixture.expected_paths {
                assert!(
                    source.join(relative).is_file(),
                    "missing {} in {}",
                    relative,
                    source.display()
                );
            }
            if let Some(relative) = fixture.expected_executable {
                assert_ne!(
                    fs::symlink_metadata(source.join(relative))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o100,
                    0,
                    "expected executable fixture file {relative}"
                );
            }
        }

        let _serial = crate::skill_manager::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let requested_tmp = tmpdir("verified-scientific-skills");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let canonical_temp_root = fs::canonicalize(env::temp_dir()).unwrap();
        assert!(tmp.starts_with(canonical_temp_root));
        let _temp_guard = RemoveTempTree(tmp.clone());
        let external_root = tmp.join("home/.claude/skills");
        let sandbox_home = tmp.join("home/.csswitch/sandbox/home");
        let data_dir = sandbox_home.join(".claude-science");
        let config_dir = tmp.join("config");
        fs::create_dir_all(&external_root).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let (forged, _) = oauth_forge::ensure_virtual_login(
            &data_dir,
            "virtual@localhost.invalid",
            &sandbox_home,
        )
        .unwrap();
        let manager = SkillManager::new(config_dir);
        let mut evidence = Vec::new();

        for fixture in fixtures {
            copy_regular_tree(
                &verified_root.join(fixture.directory),
                &external_root.join(fixture.directory),
            );
            let (scan, reconcile) = scan_named_and_reconcile_skills_for_test(
                &manager,
                &external_root,
                fixture.directory,
                &data_dir,
                "verified_scientific_shape",
                Some("claude-science 0.1.18 (release, public)"),
            )
            .unwrap();
            assert_eq!(scan.discovered, 1);
            assert_eq!(scan.imported, 1);
            assert!(scan.diagnostics.is_empty(), "{scan:?}");
            assert!(reconcile.errors.is_empty(), "{reconcile:?}");
            let skill_id = scan.skill_ids[0].clone();
            let installed = manager
                .load_inventory()
                .unwrap()
                .skills
                .into_iter()
                .find(|skill| skill.skill_id == skill_id)
                .unwrap();
            let store_payload = manager.paths.payload(&skill_id, &installed.content_hash);
            let runtime_dir = data_dir
                .join("orgs")
                .join(&forged.org_uuid)
                .join("skills")
                .join(&installed.runtime_name);
            for relative in fixture.expected_paths {
                assert!(store_payload.join(relative).is_file());
                assert!(runtime_dir.join(relative).is_file());
            }
            if let Some(relative) = fixture.expected_executable {
                assert_ne!(
                    fs::symlink_metadata(runtime_dir.join(relative))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o100,
                    0
                );
            }

            let original_content_hash = installed.content_hash.clone();
            let original_runtime_name = installed.runtime_name.clone();
            let original_store_skill = fs::read(store_payload.join("SKILL.md")).unwrap();
            fs::remove_dir_all(external_root.join(fixture.directory)).unwrap();
            let (missing_scan, missing_reconcile) = scan_named_and_reconcile_skills_for_test(
                &manager,
                &external_root,
                fixture.directory,
                &data_dir,
                "verified_scientific_source_missing",
                Some("claude-science 0.1.18 (release, public)"),
            )
            .unwrap();
            assert_eq!(missing_scan.discovered, 0);
            assert_eq!(missing_scan.imported, 0);
            assert_eq!(missing_scan.updated, 0);
            assert_eq!(missing_scan.unchanged, 0);
            assert_eq!(missing_scan.retained_missing, 1);
            assert!(missing_scan.diagnostics.is_empty(), "{missing_scan:?}");
            assert!(missing_reconcile.errors.is_empty(), "{missing_reconcile:?}");
            let inventory_after_source_loss = manager.load_inventory().unwrap();
            let retained = inventory_after_source_loss
                .skills
                .iter()
                .find(|skill| skill.skill_id == skill_id)
                .expect("missing source must retain imported inventory identity");
            assert_eq!(retained.content_hash, original_content_hash);
            assert_eq!(retained.runtime_name, original_runtime_name);
            assert_eq!(
                manager.paths.payload(&skill_id, &retained.content_hash),
                store_payload
            );
            assert_eq!(
                fs::read(store_payload.join("SKILL.md")).unwrap(),
                original_store_skill
            );
            fs::remove_dir_all(&data_dir).unwrap();
            fs::create_dir_all(&data_dir).unwrap();
            fs::write(
                data_dir.join("active-org.json"),
                serde_json::to_vec(&serde_json::json!({ "org_uuid": forged.org_uuid })).unwrap(),
            )
            .unwrap();
            oauth_forge::ensure_virtual_login(
                &data_dir,
                "virtual@localhost.invalid",
                &sandbox_home,
            )
            .unwrap();
            let rebuilt = e2e_compatibility_reconcile(
                &manager,
                &data_dir,
                "claude-science 0.1.18 (release, public)",
                "verified_scientific_rebuild",
            );
            assert!(rebuilt.errors.is_empty(), "{rebuilt:?}");
            assert!(runtime_dir.join("SKILL.md").is_file());
            evidence.push(serde_json::json!({
                "skill": fixture.directory,
                "scan": "success",
                "automatic_import": "success",
                "store_inventory": "success",
                "deploy": "success",
                "science_discovery": "not_run_no_science_process_in_shape_regression",
                "skill_trigger": "not_run_no_science_process_in_shape_regression",
                "actual_scientific_function": fixture.actual_function,
                "missing_source_rescan_retained": "success",
                "source_loss_preserves_import": "success",
                "skill_id_and_content_hash_stable": "success",
                "sandbox_rebuild_restore": "success"
            }));
        }
        let evidence_bytes = serde_json::to_vec(&evidence).unwrap();
        let evidence_hash = sha256_bytes(&evidence_bytes);
        println!(
            "CSSWITCH_VERIFIED_SCIENTIFIC_SKILLS_EVIDENCE_JSON={}",
            String::from_utf8(evidence_bytes).unwrap()
        );
        println!("CSSWITCH_VERIFIED_SCIENTIFIC_SKILLS_EVIDENCE_SHA256={evidence_hash}");
    }

    #[test]
    #[ignore = "explicit real HOME nature-figure auto-scan E2E; isolated Science, dynamic ports, local mock"]
    fn real_home_nature_figure_auto_scan_reconcile_discovery_trigger_and_rebuild() {
        assert_eq!(
            env::var("CSSWITCH_REAL_NATURE_SKILL_E2E").as_deref(),
            Ok("1"),
            "必须显式设置 CSSWITCH_REAL_NATURE_SKILL_E2E=1"
        );
        let _serial = crate::skill_manager::store::TEST_OPERATION_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        let external_root = PathBuf::from("/Users/superjj/.claude/skills");
        let source = external_root.join("nature-figure");
        assert!(source.is_dir());
        assert!(source.join("SKILL.md").is_file());

        let requested_tmp = tmpdir("real-nature-scan-e2e");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        assert!(tmp.starts_with("/private/tmp"));
        let outer_home = tmp.join("home");
        let sandbox_home = outer_home.join(".csswitch/sandbox/home");
        let data_dir = sandbox_home.join(".claude-science");
        let config_dir = tmp.join("csswitch-config");
        let browser_workdir = tmp.join("browser-workdir");
        let safe_bin = prepare_safe_e2e_bin(&sandbox_home);
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&browser_workdir).unwrap();
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let science_bin = env::var_os("CSSWITCH_REAL_SCIENCE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    "/Applications/Claude Science.app/Contents/Resources/bin/claude-science",
                )
            });
        assert!(science_bin.is_file());
        let version = installed_science_version(&science_bin, &sandbox_home, &safe_bin);
        let (forged, _) = oauth_forge::ensure_virtual_login(
            &data_dir,
            "virtual@localhost.invalid",
            &sandbox_home,
        )
        .unwrap();
        assert!(!forged.org_uuid.is_empty());

        let manager = SkillManager::new(config_dir.clone());
        let (first_scan, first_reconcile) = scan_named_and_reconcile_skills_for_test(
            &manager,
            &external_root,
            "nature-figure",
            &data_dir,
            "real_home_nature_first_scan",
            Some(&version),
        )
        .unwrap();
        assert!(first_scan.root_present);
        assert_eq!(first_scan.discovered, 1);
        assert_eq!(first_scan.imported, 1);
        assert_eq!(first_scan.updated, 0);
        assert_eq!(first_scan.unchanged, 0);
        assert!(first_scan.diagnostics.is_empty(), "{first_scan:?}");
        assert!(first_reconcile.errors.is_empty(), "{first_reconcile:?}");
        assert_eq!(first_scan.skill_ids.len(), 1);

        let inventory = manager.load_inventory().unwrap();
        assert_eq!(inventory.skills.len(), 1);
        let installed = inventory.skills[0].clone();
        assert_eq!(installed.skill_id, first_scan.skill_ids[0]);
        assert_eq!(installed.manifest.name, "nature-figure");
        assert_eq!(
            installed.source,
            SkillSource::ExternalHomeDirectory {
                directory_name: "nature-figure".to_string()
            }
        );
        assert_eq!(installed.content_hash.len(), 64);
        let store_payload = manager
            .paths
            .payload(&installed.skill_id, &installed.content_hash);
        assert!(store_payload.join("SKILL.md").is_file());
        assert!(store_payload
            .join("scripts/nature_figure_backend.py")
            .is_file());
        assert_ne!(
            fs::symlink_metadata(store_payload.join("scripts/nature_figure_backend.py"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );

        let runtime_dir = data_dir
            .join("orgs")
            .join(&forged.org_uuid)
            .join("skills")
            .join(&installed.runtime_name);
        assert!(runtime_dir.join("SKILL.md").is_file());
        assert_eq!(
            fs::read(runtime_dir.join("SKILL.md")).unwrap(),
            fs::read(source.join("SKILL.md")).unwrap()
        );
        assert_eq!(
            fs::symlink_metadata(runtime_dir.join("SKILL.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::symlink_metadata(runtime_dir.join("scripts/nature_figure_backend.py"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let (repeat_scan, repeat_reconcile) = scan_named_and_reconcile_skills_for_test(
            &manager,
            &external_root,
            "nature-figure",
            &data_dir,
            "real_home_nature_repeat_scan",
            Some(&version),
        )
        .unwrap();
        assert_eq!(repeat_scan.imported, 0);
        assert_eq!(repeat_scan.updated, 0);
        assert_eq!(repeat_scan.unchanged, 1);
        assert_eq!(repeat_scan.skill_ids, first_scan.skill_ids);
        assert!(repeat_reconcile.errors.is_empty(), "{repeat_reconcile:?}");
        let repeated = manager.load_inventory().unwrap().skills.remove(0);
        assert_eq!(repeated.skill_id, installed.skill_id);
        assert_eq!(repeated.content_hash, installed.content_hash);

        let playwright_cli = env::var_os("CSSWITCH_PLAYWRIGHT_CLI")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/Users/superjj/.codex/skills/playwright/scripts/playwright_cli.sh")
            });
        assert!(playwright_cli.is_file());
        let playwright_cache = env::var_os("CSSWITCH_NATURE_PLAYWRIGHT_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| tmp.join("npm-cache"));
        fs::create_dir_all(&playwright_cache).unwrap();
        const NATURE_CONTENT_MARKER: &str = "Nature Figure Making";
        let mock = SkillMockServer::start(
            installed.runtime_name.clone(),
            NATURE_CONTENT_MARKER.to_string(),
        );
        let port = free_port();
        let sandbox_port = free_port();
        assert_ne!(port, sandbox_port);
        assert_ne!(port, mock.port);
        assert_ne!(sandbox_port, mock.port);
        let mock_endpoint = mock.endpoint();
        let science_stderr = tmp.join("science.stderr.log");
        let child = spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: &mock_endpoint,
            stdout_path: &tmp.join("science.stdout.log"),
            stderr_path: &science_stderr,
        });
        let first_pid = child.id();
        let mut guard = RealScienceE2eGuard {
            science_bin: science_bin.clone(),
            sandbox_home: sandbox_home.clone(),
            safe_bin: safe_bin.clone(),
            data_dir: data_dir.clone(),
            child: Some(child),
            playwright_cli,
            playwright_session: format!("csswitch-nature-{}", installed.skill_id.short()),
            playwright_cache: playwright_cache.clone(),
            playwright_workdir: browser_workdir,
            browser_open: false,
            port,
        };
        wait_port_reachable(port);
        let initial_security_invocations =
            assert_security_stub_boundary(&sandbox_home, 1, &science_stderr);
        manager.mark_science_started(&data_dir).unwrap();

        let expected_catalog_button = format!(
            "button \"View {}\"",
            science_catalog_title(&installed.runtime_name)
        );
        let run_token = SkillId::new_random().unwrap().as_str().to_string();
        let phase_1_challenge = SkillId::new_random().unwrap().as_str().to_string();
        let phase_1_done = tmp.join("browser-phase-1.done.json");
        let initial_discovery_snapshot = tmp.join("initial-discovery.snapshot.txt");
        let initial_discovery_screenshot = tmp.join("initial-discovery.screenshot.jpg");
        let trigger_snapshot = tmp.join("trigger-complete.snapshot.txt");
        let trigger_screenshot = tmp.join("trigger-complete.screenshot.jpg");
        const TRIGGER_COMPLETE_MARKER: &str = "Response complete";
        let external_browser = env::var("CSSWITCH_NATURE_EXTERNAL_BROWSER").as_deref() == Ok("1");
        if external_browser {
            let phase = tmp.join("browser-phase-1.json");
            let login_url =
                wait_science_login_url(&science_bin, &sandbox_home, &safe_bin, &data_dir, port)
                    .unwrap();
            fs::write(
                &phase,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "run_token": run_token,
                    "phase": "initial_discovery_and_trigger",
                    "challenge": phase_1_challenge,
                    "login_url": login_url,
                    "expected_catalog_button": expected_catalog_button,
                    "runtime_name": installed.runtime_name,
                    "prompt": "Use the installed nature-figure Skill to prepare a manuscript figure workflow.",
                    "done_path": phase_1_done,
                    "artifacts": {
                        "initial_discovery_snapshot": {
                            "path": initial_discovery_snapshot,
                            "marker": expected_catalog_button
                        },
                        "initial_discovery_screenshot": {
                            "path": initial_discovery_screenshot,
                            "marker": expected_catalog_button
                        },
                        "trigger_snapshot": {
                            "path": trigger_snapshot,
                            "marker": TRIGGER_COMPLETE_MARKER
                        },
                        "trigger_screenshot": {
                            "path": trigger_screenshot,
                            "marker": TRIGGER_COMPLETE_MARKER
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            eprintln!("CSSWITCH_NATURE_E2E_BROWSER_PHASE_1={}", phase.display());
            wait_for_external_browser_evidence(BrowserEvidenceContext {
                done_path: &phase_1_done,
                evidence_root: &tmp,
                run_token: &run_token,
                phase: "initial_discovery_and_trigger",
                challenge: &phase_1_challenge,
                expected_label: &expected_catalog_button,
                runtime_name: &installed.runtime_name,
                artifacts: &[
                    BrowserArtifactExpectation {
                        key: "initial_discovery_snapshot",
                        path: &initial_discovery_snapshot,
                        marker: &expected_catalog_button,
                        kind: BrowserArtifactKind::Snapshot,
                    },
                    BrowserArtifactExpectation {
                        key: "initial_discovery_screenshot",
                        path: &initial_discovery_screenshot,
                        marker: &expected_catalog_button,
                        kind: BrowserArtifactKind::Screenshot,
                    },
                    BrowserArtifactExpectation {
                        key: "trigger_snapshot",
                        path: &trigger_snapshot,
                        marker: TRIGGER_COMPLETE_MARKER,
                        kind: BrowserArtifactKind::Snapshot,
                    },
                    BrowserArtifactExpectation {
                        key: "trigger_screenshot",
                        path: &trigger_screenshot,
                        marker: TRIGGER_COMPLETE_MARKER,
                        kind: BrowserArtifactKind::Screenshot,
                    },
                ],
            });
        } else {
            let skills_snapshot = open_science_skills_ui(&mut guard).unwrap();
            assert!(skills_snapshot.contains(&expected_catalog_button));
            guard.close_browser();
            let chat_snapshot = open_science_example_chat(&mut guard).unwrap();
            send_science_chat_prompt(
                &guard,
                &chat_snapshot,
                "Use the installed nature-figure Skill to prepare a manuscript figure workflow.",
            )
            .unwrap();
        }
        let triggered = wait_for_skill_mock_round(&mock);
        assert_eq!(triggered.main_count, 2);
        assert_eq!(triggered.tool_input_matches_runtime, Some(true));
        assert_eq!(triggered.tool_result_count, 1);
        assert_eq!(triggered.tool_result_is_error, Some(false));
        assert!(triggered.marker_present);

        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);

        fs::remove_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let rebuilt_active_org = data_dir.join("active-org.json");
        fs::write(
            &rebuilt_active_org,
            serde_json::to_vec(&serde_json::json!({ "org_uuid": forged.org_uuid })).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&rebuilt_active_org, fs::Permissions::from_mode(0o600)).unwrap();
        oauth_forge::ensure_virtual_login(&data_dir, "virtual@localhost.invalid", &sandbox_home)
            .unwrap();
        let rebuilt = e2e_compatibility_reconcile(
            &manager,
            &data_dir,
            &version,
            "real_home_nature_sandbox_rebuild",
        );
        assert!(rebuilt.errors.is_empty(), "{rebuilt:?}");
        assert!(runtime_dir.join("SKILL.md").is_file());
        assert_eq!(
            fs::read(runtime_dir.join("SKILL.md")).unwrap(),
            fs::read(store_payload.join("SKILL.md")).unwrap()
        );

        let rebuilt_science_stderr = tmp.join("science-rebuilt.stderr.log");
        guard.child = Some(spawn_installed_science(InstalledScienceSpawn {
            science_bin: &science_bin,
            sandbox_home: &sandbox_home,
            safe_bin: &safe_bin,
            data_dir: &data_dir,
            port,
            sandbox_port,
            anthropic_base_url: &mock_endpoint,
            stdout_path: &tmp.join("science-rebuilt.stdout.log"),
            stderr_path: &rebuilt_science_stderr,
        }));
        let rebuilt_pid = guard.child.as_ref().unwrap().id();
        wait_port_reachable(port);
        let rebuilt_security_invocations = assert_security_stub_boundary(
            &sandbox_home,
            initial_security_invocations + 1,
            &rebuilt_science_stderr,
        );
        manager.mark_science_started(&data_dir).unwrap();
        let phase_2_challenge = SkillId::new_random().unwrap().as_str().to_string();
        let phase_2_done = tmp.join("browser-phase-2.done.json");
        let rebuilt_discovery_snapshot = tmp.join("rebuilt-discovery.snapshot.txt");
        let rebuilt_discovery_screenshot = tmp.join("rebuilt-discovery.screenshot.jpg");
        if external_browser {
            let phase = tmp.join("browser-phase-2.json");
            let login_url =
                wait_science_login_url(&science_bin, &sandbox_home, &safe_bin, &data_dir, port)
                    .unwrap();
            fs::write(
                &phase,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "run_token": run_token,
                    "phase": "rebuilt_discovery",
                    "challenge": phase_2_challenge,
                    "login_url": login_url,
                    "expected_catalog_button": expected_catalog_button,
                    "runtime_name": installed.runtime_name,
                    "done_path": phase_2_done,
                    "artifacts": {
                        "rebuilt_discovery_snapshot": {
                            "path": rebuilt_discovery_snapshot,
                            "marker": expected_catalog_button
                        },
                        "rebuilt_discovery_screenshot": {
                            "path": rebuilt_discovery_screenshot,
                            "marker": expected_catalog_button
                        }
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            eprintln!("CSSWITCH_NATURE_E2E_BROWSER_PHASE_2={}", phase.display());
            wait_for_external_browser_evidence(BrowserEvidenceContext {
                done_path: &phase_2_done,
                evidence_root: &tmp,
                run_token: &run_token,
                phase: "rebuilt_discovery",
                challenge: &phase_2_challenge,
                expected_label: &expected_catalog_button,
                runtime_name: &installed.runtime_name,
                artifacts: &[
                    BrowserArtifactExpectation {
                        key: "rebuilt_discovery_snapshot",
                        path: &rebuilt_discovery_snapshot,
                        marker: &expected_catalog_button,
                        kind: BrowserArtifactKind::Snapshot,
                    },
                    BrowserArtifactExpectation {
                        key: "rebuilt_discovery_screenshot",
                        path: &rebuilt_discovery_screenshot,
                        marker: &expected_catalog_button,
                        kind: BrowserArtifactKind::Screenshot,
                    },
                ],
            });
        } else {
            let rebuilt_snapshot = open_science_skills_ui(&mut guard).unwrap();
            assert!(rebuilt_snapshot.contains(&expected_catalog_button));
        }
        guard.stop();
        wait_http_unreachable(port);
        wait_http_unreachable(sandbox_port);

        let evidence = serde_json::json!({
            "external_root": external_root,
            "source_directory": "nature-figure",
            "science_binary": science_bin,
            "science_version": version,
            "science_pid": first_pid,
            "rebuilt_science_pid": rebuilt_pid,
            "science_data_dir": data_dir,
            "science_port": port,
            "sandbox_port": sandbox_port,
            "mock_port": mock.port,
            "browser_driver": browser_driver_label(external_browser),
            "security_stub_invocations_initial": initial_security_invocations,
            "security_stub_invocations_after_rebuild": rebuilt_security_invocations,
            "run_token": run_token,
            "phase_1_challenge": phase_1_challenge,
            "phase_2_challenge": phase_2_challenge,
            "browser_evidence": {
                "initial_discovery_snapshot": initial_discovery_snapshot,
                "initial_discovery_screenshot": initial_discovery_screenshot,
                "trigger_snapshot": trigger_snapshot,
                "trigger_screenshot": trigger_screenshot,
                "rebuilt_discovery_snapshot": rebuilt_discovery_snapshot,
                "rebuilt_discovery_screenshot": rebuilt_discovery_screenshot,
                "phase_1_done": phase_1_done,
                "phase_2_done": phase_2_done
            },
            "skill_id": installed.skill_id.as_str(),
            "source_kind": "external_home_directory",
            "source_key": "nature-figure",
            "content_hash": installed.content_hash,
            "runtime_name": installed.runtime_name,
            "scan": "success",
            "import": "success",
            "repeat_scan": "unchanged",
            "deploy": "success",
            "science_discovery": "success",
            "skill_trigger_local_mock": "success",
            "actual_plot_generation": "not_run_no_live_model_or_credentials",
            "sandbox_rebuild_restore": "success"
        });
        fs::write(
            tmp.join("evidence.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
        eprintln!("CSSWITCH_NATURE_E2E_EVIDENCE={}", tmp.display());
        drop(guard);
        drop(mock);
    }

    #[test]
    #[ignore = "explicit isolated runtime smoke; uses fake Science and local loopback ports"]
    fn isolated_one_click_reuse_status_smoke_with_fake_science() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let requested_tmp = tmpdir("isolated-runtime-smoke");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let home = tmp.join("home");
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(&home).unwrap();
        let fake_science = write_test_bins(&bin_dir);
        let open_log = tmp.join("open.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                bin_dir.to_string_lossy()
            ),
        );

        let fake_key = "csswitch-isolated-fake-key-never-log";
        let profile = Profile {
            id: "mock-relay".into(),
            name: "Mock Relay".into(),
            template_id: "custom".into(),
            category: "custom".into(),
            api_format: "anthropic".into(),
            base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
            api_key: fake_key.into(),
            model: "mock-model".into(),
            ..Default::default()
        };
        let cfg = Config {
            profiles: vec![profile],
            active_id: "mock-relay".into(),
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        let config_dir = config::default_dir();
        config::save_to(&config_dir, &cfg).unwrap();
        let skill_source = tmp.join("skill-source");
        fs::create_dir(&skill_source).unwrap();
        fs::write(
            skill_source.join("SKILL.md"),
            "---\nname: Lifecycle Probe\ndescription: Isolated lifecycle probe\n---\none\n",
        )
        .unwrap();
        fs::write(
            skill_source.join("csswitch.skill.json"),
            br#"{"schema_version":1,"requirements":{"needs_network":false,"needs_ssh":false,"needs_mcp":false,"needs_local_command":false,"required_binaries":[],"required_environment":[],"required_runtime_assets":[],"supported_platforms":["macos"],"minimum_runtime_version":"0.1.0"}}"#,
        )
        .unwrap();
        let skill_manager = crate::skill_manager::store::SkillManager::new(config_dir.clone());
        let installed = skill_manager.import_source(&skill_source).unwrap().skill;
        e2e_enable_skill_with_exact_compatibility_ack(
            &skill_manager,
            &installed,
            "claude-science 0.1.18 (release, public)",
            &home.join(".csswitch/sandbox/home/.claude-science"),
        );

        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let first =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert_eq!(first["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(".csswitch")
            .join("sandbox")
            .join("home")
            .join(".claude-science")
            .join("fake-science");
        let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );
        let active_org = home.join(".csswitch/sandbox/home/.claude-science/active-org.json");
        let active_org_value: serde_json::Value =
            serde_json::from_slice(&fs::read(&active_org).unwrap()).unwrap();
        let org_uuid = active_org_value["org_uuid"].as_str().unwrap();
        let skill_runtime = home
            .join(".csswitch/sandbox/home/.claude-science/orgs")
            .join(org_uuid)
            .join("skills")
            .join(&installed.runtime_name);
        assert!(skill_runtime.join("SKILL.md").is_file());
        let deployed_before_update = fs::read(skill_runtime.join("SKILL.md")).unwrap();
        assert!(!skill_manager.has_pending_restart().unwrap());

        let second =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("second one-click should reuse running sandbox");
        assert_eq!(second["action"], "reopened");
        assert_eq!(second["url"], format!("http://127.0.0.1:{sandbox_port}"));
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            deployed_before_update
        );

        let active_org_bytes = fs::read(&active_org).unwrap();
        fs::write(&active_org, b"{}\n").unwrap();
        let invalid_org =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .unwrap_err();
        assert!(invalid_org.contains("Skill"));
        wait_http_health(sandbox_port);
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            deployed_before_update
        );
        fs::write(&active_org, &active_org_bytes).unwrap();

        let token_dir = home.join(".csswitch/sandbox/home/.claude-science/.oauth-tokens");
        let token_path = fs::read_dir(&token_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("enc"))
            .unwrap();
        let token_bytes = fs::read(&token_path).unwrap();
        fs::write(&token_path, b"invalid-test-token").unwrap();
        env_guard.set("CSSWITCH_FAKE_STOP_FAIL", "1");
        let stop_failed =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .unwrap_err();
        assert!(stop_failed.contains("停止失败"));
        wait_http_health(sandbox_port);
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            deployed_before_update
        );
        fs::write(&token_path, token_bytes).unwrap();
        env_guard.set("CSSWITCH_FAKE_STOP_FAIL", "0");

        fs::write(
            skill_source.join("SKILL.md"),
            "---\nname: Lifecycle Probe\ndescription: Isolated lifecycle probe\n---\ntwo\n",
        )
        .unwrap();
        let updated = skill_manager
            .update_source(&installed.skill_id, &skill_source, false)
            .unwrap()
            .skill;
        let fake_version = "claude-science 0.1.18 (release, public)";
        let catalog = load_static_catalog().unwrap();
        let context = e2e_compatibility_context(fake_version);
        let gate = evaluate_compatibility_gate(&updated, &context, &catalog).unwrap();
        skill_manager
            .set_enabled_with_compatibility(
                &updated.skill_id,
                true,
                &gate.required_rule_ids,
                &context,
                &catalog,
            )
            .unwrap();
        let pending =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("running Science should report a required Skill restart");
        assert_eq!(pending["action"], "restart_required");
        assert_eq!(pending["restart_required"], true);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            deployed_before_update
        );
        {
            let mut st = lock(&state);
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *st;
            science::stop_sandbox(&handle, sandbox, sandbox_url).unwrap();
        }
        wait_http_unreachable(sandbox_port);
        fs::write(fake_state_dir.join("pid"), std::process::id().to_string()).unwrap();
        let unknown =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .unwrap_err();
        assert!(unknown.contains("无法确认隔离 Science 是否已停止"));
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            deployed_before_update
        );
        fs::remove_file(fake_state_dir.join("pid")).unwrap();
        let restarted =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("stopped Science should reconcile and restart");
        assert_eq!(restarted["action"], "started");
        wait_http_health(sandbox_port);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "2"
        );
        assert_eq!(
            fs::read(skill_runtime.join("SKILL.md")).unwrap(),
            fs::read(skill_source.join("SKILL.md")).unwrap()
        );
        assert_eq!(
            skill_manager.load_inventory().unwrap().skills[0].content_hash,
            updated.content_hash
        );
        assert!(!skill_manager.has_pending_restart().unwrap());

        let status = super::status(app.state::<SharedAppState>());
        assert_eq!(status["proxy"], "green");
        assert_eq!(status["sandbox"], "green");
        assert_eq!(status["upstream"], "green");
        assert_eq!(status["active_profile"]["id"], "mock-relay");
        assert_eq!(status["science"]["sandbox"]["port"], sandbox_port);
        assert_eq!(status["science"]["schema_version"], 1);
        assert!(status["last_error"].is_null());

        let doctor = std::process::Command::new(root.join("scripts/doctor.sh"))
            .env("HOME", &home)
            .env("SCIENCE_BIN", &fake_science)
            .env("CSSWITCH_CONFIG", config_dir.join("config.json"))
            .env("CSSWITCH_PROXY_PORT", proxy_port.to_string())
            .env("CSSWITCH_SANDBOX_PORT", sandbox_port.to_string())
            .output()
            .expect("doctor should run");
        assert!(doctor.status.success());
        let doctor_out = String::from_utf8_lossy(&doctor.stdout);
        assert!(doctor_out.contains("真实 HOME 检查默认跳过"));
        assert!(!doctor_out.contains(&format!("{}/.claude-science", home.display())));

        let cfg_after = config::load_from(&config_dir).unwrap();
        let secret = cfg_after.secret;
        assert!(!secret.is_empty());
        let doctor_err = String::from_utf8_lossy(&doctor.stderr);
        assert!(!doctor_out.contains(fake_key));
        assert!(!doctor_out.contains(&secret));
        assert!(!doctor_err.contains(fake_key));
        assert!(!doctor_err.contains(&secret));
        assert!(!first.to_string().contains(fake_key));
        assert!(!first.to_string().contains(&secret));
        assert!(!second.to_string().contains(fake_key));
        assert!(!second.to_string().contains(&secret));
        let opened = fs::read_to_string(&open_log).unwrap_or_default();
        assert!(!opened.contains(fake_key));
        assert!(!opened.contains(&secret));
        for name in ["proxy.log", "sandbox.log", "operation.log"] {
            let body = fs::read_to_string(config_dir.join("logs").join(name))
                .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
            assert!(!body.contains(fake_key), "{name} leaked fake key");
            assert!(!body.contains(&secret), "{name} leaked path secret");
        }

        {
            let mut st = lock(&state);
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *st;
            science::stop_sandbox(&handle, sandbox, sandbox_url).unwrap();
            st.stop_proxy();
        }
        wait_http_unreachable(sandbox_port);
        wait_http_unreachable(proxy_port);
        drop(handle);
        drop(app);
        drop(state);
        drop(lifecycle);
        drop(env_guard);
        fs::remove_dir_all(&tmp).expect("isolated runtime smoke temp tree must be removable");
        assert!(env::var("CSSWITCH_KEEP_E2E_TMP").as_deref() == Ok("1") || !tmp.exists());
    }

    #[test]
    #[ignore = "explicit isolated recovery proof; uses fake Science and local loopback ports"]
    fn isolated_manual_actions_recover_dead_proxy_with_fake_science() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let requested_tmp = tmpdir("isolated-recovery-proof");
        fs::create_dir_all(&requested_tmp).unwrap();
        let tmp = fs::canonicalize(requested_tmp).unwrap();
        let home = tmp.join("home");
        let bin_dir = tmp.join("bin");
        fs::create_dir_all(&home).unwrap();
        let fake_science = write_test_bins(&bin_dir);
        let open_log = tmp.join("open.log");
        let mock_upstream_port = start_mock_upstream();
        let proxy_port = free_port();
        let sandbox_port = free_port();
        assert_ne!(proxy_port, sandbox_port);

        let mut env_guard = EnvGuard::new();
        env_guard.set("HOME", &home);
        env_guard.set("CSSWITCH_REPO", &root);
        env_guard.set("SCIENCE_BIN", &fake_science);
        env_guard.set("CSSWITCH_FAKE_OPEN_LOG", &open_log);
        env_guard.set("CSSWITCH_DOCTOR_CHECK_REAL_HOME", "0");
        env_guard.set(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                bin_dir.to_string_lossy()
            ),
        );

        let fake_key = "csswitch-isolated-fake-key-never-log";
        let profile = Profile {
            id: "mock-relay".into(),
            name: "Mock Relay".into(),
            template_id: "custom".into(),
            category: "custom".into(),
            api_format: "anthropic".into(),
            base_url: format!("http://127.0.0.1:{mock_upstream_port}/anthropic"),
            api_key: fake_key.into(),
            model: "mock-model".into(),
            ..Default::default()
        };
        let cfg = Config {
            profiles: vec![profile],
            active_id: "mock-relay".into(),
            proxy_port,
            sandbox_port,
            ..Default::default()
        };
        let config_dir = config::default_dir();
        config::save_to(&config_dir, &cfg).unwrap();

        let state: SharedAppState = Arc::new(Mutex::new(AppState::default()));
        let lifecycle = Arc::new(lifecycle::Lifecycle::new());
        let app = tauri::test::mock_builder()
            .manage(state.clone())
            .manage(lifecycle.clone())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        let handle = app.handle().clone();

        let first =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("first one-click should start proxy and sandbox");
        assert_eq!(first["action"], "started");
        assert_eq!(first["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(proxy_port);
        wait_http_health(sandbox_port);
        let fake_state_dir = home
            .join(".csswitch")
            .join("sandbox")
            .join("home")
            .join(".claude-science")
            .join("fake-science");
        let first_pid = fs::read_to_string(fake_state_dir.join("pid")).unwrap();

        kill_tracked_proxy(&state, proxy_port);

        let down_status = super::status(app.state::<SharedAppState>());
        assert_eq!(down_status["proxy"], "amber");
        assert_eq!(down_status["sandbox"], "green");
        assert_eq!(down_status["last_error"]["type"], "proxy_unhealthy");
        assert_eq!(
            down_status["last_error"]["message"],
            "代理进程不可达或已退出，请点击「一键开始」或「启动代理」恢复。"
        );
        assert_eq!(down_status["last_error"]["port"], proxy_port);

        let start_proxy_recovered =
            super::start_proxy_inner_cmd(handle.clone(), state.clone(), lifecycle.clone())
                .expect("start_proxy should manually recover a dead proxy");
        assert_eq!(start_proxy_recovered["port"], proxy_port);
        wait_http_health(proxy_port);

        let start_proxy_status = super::status(app.state::<SharedAppState>());
        assert_eq!(start_proxy_status["proxy"], "green");
        assert_eq!(start_proxy_status["sandbox"], "green");
        assert_eq!(start_proxy_status["upstream"], "green");
        assert!(start_proxy_status["last_error"].is_null());

        kill_tracked_proxy(&state, proxy_port);
        let down_again_status = super::status(app.state::<SharedAppState>());
        assert_eq!(down_again_status["proxy"], "amber");
        assert_eq!(down_again_status["sandbox"], "green");
        assert_eq!(down_again_status["last_error"]["type"], "proxy_unhealthy");

        let recovered =
            sandbox_session::one_click_login(handle.clone(), state.clone(), lifecycle.as_ref())
                .expect("one-click should manually recover a dead proxy");
        assert_eq!(recovered["action"], "reopened");
        assert_eq!(
            recovered["msg"],
            "已用新配置重启代理，Science 沿用不变，已重新打开 Science。"
        );
        assert_eq!(recovered["url"], format!("http://127.0.0.1:{sandbox_port}"));
        wait_http_health(proxy_port);
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("pid")).unwrap(),
            first_pid
        );
        assert_eq!(
            fs::read_to_string(fake_state_dir.join("serve-count")).unwrap(),
            "1"
        );

        let recovered_status = super::status(app.state::<SharedAppState>());
        assert_eq!(recovered_status["proxy"], "green");
        assert_eq!(recovered_status["sandbox"], "green");
        assert_eq!(recovered_status["upstream"], "green");
        assert!(recovered_status["last_error"].is_null());

        let cfg_after = config::load_from(&config_dir).unwrap();
        let secret = cfg_after.secret;
        assert!(!secret.is_empty());
        assert!(!down_status.to_string().contains(fake_key));
        assert!(!down_status.to_string().contains(&secret));
        assert!(!recovered.to_string().contains(fake_key));
        assert!(!recovered.to_string().contains(&secret));
        assert!(!recovered_status.to_string().contains(fake_key));
        assert!(!recovered_status.to_string().contains(&secret));
        for name in ["proxy.log", "sandbox.log", "operation.log"] {
            let body = fs::read_to_string(config_dir.join("logs").join(name))
                .unwrap_or_else(|e| panic!("expected {name} to exist: {e}"));
            assert!(!body.contains(fake_key), "{name} leaked fake key");
            assert!(!body.contains(&secret), "{name} leaked path secret");
        }

        {
            let mut st = lock(&state);
            let AppState {
                sandbox,
                sandbox_url,
                ..
            } = &mut *st;
            science::stop_sandbox(&handle, sandbox, sandbox_url).unwrap();
            st.stop_proxy();
        }
        wait_http_unreachable(sandbox_port);
        wait_http_unreachable(proxy_port);
        drop(handle);
        drop(app);
        drop(state);
        drop(lifecycle);
        drop(env_guard);
        fs::remove_dir_all(&tmp).expect("isolated recovery temp tree must be removable");
        assert!(env::var("CSSWITCH_KEEP_E2E_TMP").as_deref() == Ok("1") || !tmp.exists());
    }
}
