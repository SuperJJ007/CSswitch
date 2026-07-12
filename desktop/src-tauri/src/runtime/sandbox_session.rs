use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{Manager, Runtime};

use crate::commands::skills::{
    scan_and_reconcile_skills_for_runtime, RuntimeSkillReconcileContext,
};
use crate::runtime::operation::{
    self, OperationKind, OperationStage, OperationTrace, POLL_INTERVAL_MS,
};
use crate::runtime::proxy::ProxyAction;
use crate::runtime::proxy_lifecycle::ensure_proxy;
use crate::runtime::science::{
    sandbox_data_dir, sandbox_home, sandbox_running_ours, sandbox_science_state,
    sandbox_science_version, sandbox_url, stop_sandbox, SandboxScienceState,
};
use crate::runtime::system::{asset_root, log_path, open_in_browser, open_log, redact, tail_file};
use crate::skill_manager::deployment::ReconcileReport;
use crate::skill_manager::discovery::ScienceProbeState;
use crate::skill_manager::error::SkillManagerError;
use crate::skill_manager::external::external_skills_root_from_process_home;
use crate::skill_manager::store::SkillManager;
use crate::{config, lifecycle, lock, oauth_forge, proc, AppState, SharedAppState};

fn skill_error_text(error: SkillManagerError) -> String {
    let skill = error
        .skill_id
        .as_ref()
        .map(|id| format!(" skill_id={}", id.as_str()))
        .unwrap_or_default();
    format!(
        "Skill Manager [{}]{skill}: {}；{}",
        error.code.as_str(),
        error.message,
        error.remediation
    )
}

fn reconcile_error_text(report: &ReconcileReport) -> Option<String> {
    report.errors.first().map(|error| {
        let skill = error
            .skill_id
            .as_ref()
            .map(|id| format!(" skill_id={}", id.as_str()))
            .unwrap_or_default();
        format!(
            "Skill reconcile [{}]{skill}: {}；{}",
            error.code, error.message, error.remediation
        )
    })
}

fn stop_sandbox_state<R: Runtime>(
    app: &tauri::AppHandle<R>,
    st: &mut AppState,
) -> Result<(), String> {
    stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url)
}

fn open_science_surface<R: Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
) -> Result<&'static str, String> {
    if std::env::var("CSSWITCH_SCIENCE_WEBVIEW_SPIKE")
        .ok()
        .as_deref()
        == Some("1")
    {
        if let Some(win) = app.get_webview_window("science") {
            let _ = win.close();
        }
        let parsed = url
            .parse()
            .map_err(|e| format!("Science URL 解析失败：{e}"))?;
        match tauri::WebviewWindowBuilder::new(app, "science", tauri::WebviewUrl::External(parsed))
            .title("Claude Science")
            .inner_size(1100.0, 800.0)
            .build()
        {
            Ok(win) => {
                let _ = win.set_focus();
                return Ok("webview");
            }
            Err(_) => {
                // Spike-only path: construction failure falls through to the existing browser surface.
            }
        }
    }
    open_in_browser(url)?;
    Ok("browser")
}

/// One-click session startup: active proxy, virtual login, sandbox, browser.
///
/// Callers must hold the command serializer lock.
pub(crate) fn one_click_login<R: Runtime>(
    app: tauri::AppHandle<R>,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<Value, String> {
    let trace = OperationTrace::start(OperationKind::OneClickLogin, "command=one_click_login");
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let sport = cfg.sandbox_port;

    let sbx_home = sandbox_home();
    let auth_dir = sbx_home.join(".claude-science");
    let data_dir = sandbox_data_dir();
    let skills = SkillManager::new(dir.clone());
    let external_root = external_skills_root_from_process_home().map_err(skill_error_text)?;

    let science_state = sandbox_science_state(sport);
    if science_state == SandboxScienceState::Unknown {
        trace.finish("error=sandbox_state_unknown_before_skill_reconcile");
        return Err(format!(
            "无法确认隔离 Science 是否已停止（端口 {sport} 或 data-dir 状态不一致）。为避免热改 Skill，未执行 reconcile；请先停止隔离 Science 并确认端口空闲。"
        ));
    }

    if science_state == SandboxScienceState::RunningHealthy {
        let science_version = sandbox_science_version();
        let (external_scan, dry) = scan_and_reconcile_skills_for_runtime(
            &skills,
            RuntimeSkillReconcileContext {
                external_root: &external_root,
                data_dir: &data_dir,
                dry_run: true,
                reason: "running_check",
                science_version: science_version.as_deref(),
                runtime_mode: &cfg.mode,
                science_state: ScienceProbeState::Running,
            },
        )
        .map_err(skill_error_text)?;
        trace_external_scan(&trace, &external_scan);
        if let Some(error) = reconcile_error_text(&dry) {
            trace.finish("error=skill_reconcile_dry_run");
            return Err(error);
        }
        let pending_restart = skills.has_pending_restart().map_err(skill_error_text)?;
        if dry.restart_required || pending_restart {
            trace.finish("ok action=restart_required reason=skills_changed");
            return Ok(json!({
                "url": sandbox_url(sport),
                "msg": "Skill 已变更，需要先停止并重新启动隔离 Science 后才会生效。",
                "action": "restart_required",
                "restart_required": true
            }));
        }
        let (_, _, proxy_action) = ensure_proxy(&app, &state, lifecycle, Some(&trace))?;
        if oauth_forge::login_intact(&auth_dir, "virtual@localhost.invalid", &sbx_home) {
            let url = sandbox_url(sport);
            {
                let mut st = lock(&state);
                st.sandbox_port = sport;
                st.sandbox_url = Some(url.clone());
            }
            let base = match proxy_action {
                ProxyAction::Reused => "已在运行",
                ProxyAction::Restarted => "已用新配置重启代理，Science 沿用不变",
            };
            let msg = match open_science_surface(&app, &url) {
                Ok("webview") => format!("{base}，已重新打开 Science 窗口。"),
                Ok(_) => format!("{base}，已重新打开 Science。"),
                Err(_) => format!("{base}，服务已就绪，请手动打开：{url}"),
            };
            trace.finish(format!(
                "ok action=reopened proxy_action={}",
                proxy_action.as_str()
            ));
            return Ok(json!({ "url": url, "msg": msg, "action": "reopened" }));
        }
        {
            let mut st = lock(&state);
            if let Err(error) = stop_sandbox_state(&app, &mut st) {
                trace.finish("error=sandbox_stop_before_skill_reconcile");
                return Err(format!(
                    "隔离 Science 停止失败，为避免热改 Skill，未执行 reconcile：{error}"
                ));
            }
        }
    }

    if sandbox_science_state(sport) != SandboxScienceState::Stopped {
        trace.finish("error=sandbox_not_stopped_before_skill_reconcile");
        return Err(format!(
            "未能确认隔离 Science 已停止且端口 {sport} 空闲。为避免热改 Skill，未执行 reconcile。"
        ));
    }
    trace.stage(OperationStage::SandboxLogin, "ensure_virtual_login");
    let (forged, login_action) =
        oauth_forge::ensure_virtual_login(&auth_dir, "virtual@localhost.invalid", &sbx_home)
            .map_err(|e| format!("写虚拟登录失败：{e}"))?;
    let science_version = sandbox_science_version();
    let (external_scan, reconcile) = scan_and_reconcile_skills_for_runtime(
        &skills,
        RuntimeSkillReconcileContext {
            external_root: &external_root,
            data_dir: &data_dir,
            dry_run: false,
            reason: "before_start",
            science_version: science_version.as_deref(),
            runtime_mode: &cfg.mode,
            science_state: ScienceProbeState::NotRunning,
        },
    )
    .map_err(skill_error_text)?;
    trace_external_scan(&trace, &external_scan);
    if let Some(error) = reconcile_error_text(&reconcile) {
        trace.finish("error=skill_reconcile_before_start");
        return Err(error);
    }
    let (pport, secret, proxy_action) = ensure_proxy(&app, &state, lifecycle, Some(&trace))?;

    let root = asset_root(&app)
        .ok_or("找不到 scripts/launch-virtual-sandbox.sh（打包资源或仓库根均未命中）。")?;

    let launch = root.join("scripts/launch-virtual-sandbox.sh");
    if !launch.is_file() {
        return Err("找不到 scripts/launch-virtual-sandbox.sh。".into());
    }

    let proxy_url = format!("http://127.0.0.1:{pport}/{secret}");
    let logf = open_log("sandbox.log").map_err(|e| format!("建日志失败：{e}"))?;
    {
        use std::io::Write;
        let mut lw = &logf;
        let _ = writeln!(
            lw,
            "[oauth] 虚拟登录已就绪（Rust，零 node；action={:?}）：auth_dir={} account={} org={} enc={}",
            login_action,
            forged.auth_dir.display(),
            forged.account_uuid,
            forged.org_uuid,
            forged.enc_file.display()
        );
    }
    let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
    trace.stage(OperationStage::SandboxLaunch, format!("port={sport}"));
    let status = Command::new("zsh")
        .arg(&launch)
        .arg("--port")
        .arg(sport.to_string())
        .arg("--proxy-url")
        .arg(&proxy_url)
        .arg("--skip-oauth-forge")
        .env("SANDBOX_HOME", sandbox_home())
        .env("CSSWITCH_RECONCILED_DATA_DIR", &data_dir)
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2))
        .status()
        .map_err(|e| format!("起沙箱失败：{e}"))?;
    if !status.success() {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        trace.finish("error=sandbox_launch_failed");
        return Err(format!("起沙箱脚本失败。\n{tail}"));
    }

    let mut ok = false;
    for _ in 0..(operation::SANDBOX_HEALTH_BUDGET_MS / POLL_INTERVAL_MS) {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if proc::http_health(sport, None, operation::LOCAL_HEALTH_TIMEOUT_MS) {
            ok = true;
            break;
        }
    }
    trace.stage(
        OperationStage::SandboxHealth,
        if ok { "ready" } else { "not_ready" },
    );
    if !ok {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=sandbox_health_timeout");
        return Err(format!(
            "沙箱起后探活超时（端口 {sport}）。已尝试停掉刚起的沙箱。\n{tail}"
        ));
    }

    if !sandbox_running_ours(sport) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=sandbox_identity_mismatch");
        return Err(format!(
            "端口 {sport} 有服务响应，但按 data-dir 确认不是本沙箱 Science（疑似被其它服务占用）。已尝试停掉刚起的沙箱。"
        ));
    }

    if let Err(error) = skills.mark_science_started(&data_dir) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_state(&app, &mut st);
        }
        trace.finish("error=skill_restart_state_commit");
        return Err(skill_error_text(error));
    }

    let url = sandbox_url(sport);
    {
        let mut st = lock(&state);
        st.sandbox_port = sport;
        st.sandbox_url = Some(url.clone());
    }
    let started = match login_action {
        oauth_forge::LoginAction::Created => "已启动",
        _ => "沙箱已重新启动，沿用原有对话",
    };
    let msg = match open_science_surface(&app, &url) {
        Ok("webview") => format!("{started}，已打开 Science 窗口。"),
        Ok(_) => format!("{started}。"),
        Err(_) => format!("{started}，服务已就绪，请手动打开：{url}"),
    };
    trace.stage(OperationStage::OpenBrowser, "done");
    trace.finish(format!(
        "ok action=started proxy_action={}",
        proxy_action.as_str()
    ));
    Ok(json!({ "url": url, "msg": msg, "action": "started" }))
}

fn trace_external_scan(
    trace: &OperationTrace,
    scan: &crate::skill_manager::external::ExternalSkillScanReport,
) {
    trace.stage(
        OperationStage::Precheck,
        format!(
            "external_skill_scan discovered={} imported={} updated={} unchanged={} retained_missing={} diagnostics={}",
            scan.discovered,
            scan.imported,
            scan.updated,
            scan.unchanged,
            scan.retained_missing,
            scan.diagnostics.len()
        ),
    );
}
