use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tauri::State;

use crate::runtime::diagnostics::{status_lights, StatusProbeInput};
use crate::runtime::operation::{
    self, OperationKind, OperationStage, OperationTrace, POLL_INTERVAL_MS,
};
use crate::runtime::profile::merge_and_sort_models;
use crate::runtime::provider::{
    assert_format_supported, is_native_adapter, is_openai_adapter, key_env_for_adapter,
    proxy_args_for, proxy_fingerprint, reject_openai_custom_anthropic_base, upstream_host,
};
use crate::runtime::proxy::{ere_escape, health_timeout_reason, should_write_back, ProxyAction};
use crate::runtime::science::{
    sandbox_home, sandbox_running_ours, sandbox_url, settings_change_needs_teardown, stop_sandbox,
};
use crate::runtime::system::{
    asset_root, kill_child, log_path, open_in_browser, open_log, redact, tail_file,
};
use crate::{
    config, lifecycle, lock, oauth_forge, proc, run_blocking, scratch, templates, AppState,
    SharedAppState, SharedLifecycle,
};

fn stop_sandbox_state(app: &tauri::AppHandle, st: &mut AppState) -> Result<(), String> {
    stop_sandbox(app, &mut st.sandbox, &mut st.sandbox_url)
}

// ---------- 代理生命周期核心 ----------
/// 确保代理在跑且健康；返回 (端口, secret, 本次动作)。幂等：已健康则复用。
/// 读【生效 profile】派生 adapter/base_url/model/key，委托 [`start_proxy_for`]。
fn ensure_proxy(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    trace: Option<&OperationTrace>,
) -> Result<(u16, String, ProxyAction), String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let profile = cfg
        .active_profile()
        .cloned()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    start_proxy_for(app, state, lifecycle, &profile, trace)
}

/// 用【给定 profile】（不读 active）起代理并探活；返回 (端口, secret, 动作)。
///
/// 并发正确性（spec §8.1）：
/// - **读-spawn 原子**：复用判定 / 清残留 / spawn 都在同一把 AppState 锁内；新 child 先握本地。
/// - **探活锁外**：探活刻意在 AppState 锁外做，不阻塞 status 等命令。
/// - **generation token**：spawn 前抓 `gen`；探活健康后回锁校验 `current_generation()==gen`，
///   若期间被清 key/停/切 bump 过 → 杀掉自己刚起的 child、**不写回 st.proxy**（不拿旧配置复活）。
///
/// 本函数**绝不取串行器锁**（调用方命令才取），故与命令层的 `with_serialized` 不会自死锁。
pub(crate) fn start_proxy_for(
    app: &tauri::AppHandle,
    state: &SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
    profile: &config::Profile,
    trace: Option<&OperationTrace>,
) -> Result<(u16, String, ProxyAction), String> {
    assert_format_supported(profile)?;
    let launch = proxy_args_for(profile);
    if launch.key.is_empty() {
        return Err(format!(
            "「{}」还没填 API key，请先在面板填写并保存。",
            profile.name
        ));
    }
    let native = is_native_adapter(&launch.adapter);
    if !native && launch.base_url.is_empty() {
        return Err(
            "该配置需要填 base_url（如 https://your-relay/claude），请先在面板填写并保存。".into(),
        );
    }
    // 换任一协议语义或上游字段都触发代理重启，避免不同配置切换时复用旧进程。
    let key_fp = proxy_fingerprint(profile, &launch);
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let port = cfg.proxy_port;
    let root = asset_root(app)
        .ok_or("找不到代理脚本 proxy/csswitch_proxy.py（打包资源或仓库根均未命中）。开发态可设 CSSWITCH_REPO。")?;
    let py = proc::find_exe("python3")
        .ok_or("缺少依赖 python3（起翻译代理需要）。已查 PATH、常见目录与登录 shell 仍未找到；macOS 一般自带 /usr/bin/python3（装 Xcode 命令行工具：xcode-select --install）。")?;

    // path-secret：**持久化复用**（已在跑的沙箱把该 secret 嵌进了 ANTHROPIC_BASE_URL，
    // 若每次起代理都换 secret，代理一重启沙箱就会拿旧 secret 打到新代理 → 全部 403）。
    let secret = if !cfg.secret.is_empty() {
        cfg.secret.clone()
    } else {
        let s = proc::gen_secret().map_err(|e| format!("无法生成安全 secret：{e}"))?;
        let s2 = s.clone();
        config::update(&dir, move |c| c.secret = s2).map_err(|e| e.to_string())?;
        s
    };

    // generation token：**spawn 前**抓当前号；探活后回锁比对，防被更晚操作取代还写回。
    let gen = lifecycle.current_generation();

    // 「检查复用 → 清残留 → 起进程」在同一把 AppState 锁内完成（读-spawn 原子）。
    // 但新 child 只握在本地，**探活健康 + generation 未变**才写回 st.proxy。
    let child = {
        let mut st = lock(state);
        // 幂等：已在跑且健康、且【端口 + adapter + key 指纹】都一致才复用。
        if st.proxy.is_some()
            && st.proxy_port == port
            && st.provider == launch.adapter
            && st.key_fp == key_fp
            && proc::http_health(
                port,
                Some(&st.secret),
                operation::PROXY_REUSE_HEALTH_TIMEOUT_MS,
            )
        {
            if let Some(t) = trace {
                t.stage(
                    OperationStage::ProxyHealth,
                    format!("reused port={port} adapter={}", launch.adapter),
                );
            }
            return Ok((port, st.secret.clone(), ProxyAction::Reused));
        }
        // 端口要让给新进程 → 先杀掉旧占用者（st.proxy）与同端口孤儿；期间 st.proxy=None。
        kill_child(&mut st.proxy);
        st.provider.clear();
        st.key_fp = 0;
        // 预置 st.secret = 本次 secret（persistent path-secret）：使探活后写回门的 secret 合取
        // 在合法启动上恒真；只有并发另起用「不同 secret」重置了它，才会挡下写回（冷启动双起窄窗防御）。
        st.secret = secret.clone();
        let script = root.join("proxy/csswitch_proxy.py");
        // 再清掉上次会话遗留、绑在同端口上的孤儿代理（匹配本安装的绝对脚本路径 + 端口）。
        let pat = format!("{}.*--port {port}", ere_escape(&script.to_string_lossy()));
        let _ = Command::new("pkill").arg("-f").arg(&pat).status();

        let logf = open_log("proxy.log").map_err(|e| format!("建日志失败：{e}"))?;
        let logf2 = logf.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&py);
        if let Some(t) = trace {
            t.stage(
                OperationStage::ProxySpawn,
                format!("port={port} adapter={}", launch.adapter),
            );
        }
        cmd.arg(&script)
            .arg("--provider")
            .arg(&launch.adapter)
            .arg("--port")
            .arg(port.to_string())
            .arg("--auth-token")
            .arg(&secret)
            // key 经环境变量注入，绝不作为命令行参数（避免 ps 泄露）。
            .env(launch.key_env, &launch.key);
        // 非 native 家族：base_url + 选中模型经环境变量交给代理（均非密钥，但与 key 一致走 env）。
        if !native {
            if is_openai_adapter(&launch.adapter) {
                cmd.env("CSSWITCH_OPENAI_BASE_URL", &launch.base_url);
                if !launch.model.is_empty() {
                    cmd.env("CSSWITCH_OPENAI_MODEL", &launch.model);
                }
            } else {
                cmd.env("CSSWITCH_RELAY_BASE_URL", &launch.base_url);
                if !launch.model.is_empty() {
                    cmd.env("CSSWITCH_RELAY_MODEL", &launch.model);
                }
                if !launch.thinking_policy.is_empty() {
                    cmd.env("CSSWITCH_RELAY_THINKING", launch.thinking_policy);
                }
            }
        }
        cmd.stdout(Stdio::from(logf))
            .stderr(Stdio::from(logf2))
            .spawn()
            .map_err(|e| format!("启动代理失败：{e}"))?
        // 注意：child 未写入 st.proxy——探活通过且 generation 未变时才回锁写回。
    };

    // 探活最多 ~4s（AppState 锁外，不阻塞 status 等命令）。
    let mut ok = false;
    for _ in 0..(operation::PROXY_HEALTH_BUDGET_MS / POLL_INTERVAL_MS) {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if proc::http_health(port, Some(&secret), operation::LOCAL_HEALTH_TIMEOUT_MS) {
            ok = true;
            break;
        }
    }
    if let Some(t) = trace {
        t.stage(
            OperationStage::ProxyHealth,
            if ok { "ready" } else { "not_ready" },
        );
    }
    if !ok {
        // 探活失败：杀掉自己刚起的 child（它从未写入 st.proxy，绝不留孤儿）。
        let mut c = child;
        let _ = c.kill();
        let _ = c.wait();
        let tail = redact(&tail_file(&log_path("proxy.log"), 500), &secret);
        // 本地 /health 不验上游 key，故探活超时与 key 有效性无关：按日志区分端口占用 vs 依赖/脚本异常
        // （修真机 P2：旧措辞含糊说「或 key 无效」会误导用户去查 key）。
        return Err(format!("{}\n{tail}", health_timeout_reason(port, &tail)));
    }

    // 健康 → 回 AppState 锁，校验 generation 未被 bump 且 secret 仍是本次的（未被清 key/停/切/并发另起取代）才写回。
    {
        let mut st = lock(state);
        if !should_write_back(gen, lifecycle.current_generation(), &st.secret, &secret) {
            // 被更晚的操作取代（generation 变）或被并发另起用不同 secret 占了槽：
            // 杀掉自己刚起的 child、不写回 st.proxy（不拿旧配置复活、不覆盖他人的槽）。
            let mut c = child;
            let _ = c.kill();
            let _ = c.wait();
            return Err("代理启动期间配置已变更（被更晚的操作取代），本次启动未生效。".into());
        }
        st.proxy = Some(child);
        st.proxy_port = port;
        st.secret = secret.clone();
        st.provider = launch.adapter.clone();
        st.key_fp = key_fp;
    }
    Ok((port, secret, ProxyAction::Restarted))
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
            kill_child(&mut st.proxy);
            st.secret.clear();
            st.provider.clear();
            st.key_fp = 0;
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
    if cfg.proxy_port == 8765 || cfg.sandbox_port == 8765 {
        return Err("端口 8765 是真实 Science 实例保留端口，不能用。".into());
    }
    if cfg.proxy_port == 0 || cfg.sandbox_port == 0 {
        return Err("端口不能为 0。".into());
    }
    if cfg.proxy_port == cfg.sandbox_port {
        return Err("代理端口与沙箱端口不能相同。".into());
    }
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
            kill_child(&mut st.proxy);
            st.secret.clear();
            st.provider.clear();
            st.key_fp = 0;
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

fn start_proxy_inner_cmd(
    app: tauri::AppHandle,
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

/// 「存 key 即验证」：确保代理在跑，再经代理向上游发一个最小请求，据状态码判断 key 是否可用。
#[tauri::command]
pub(crate) async fn verify_key(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
    lifecycle: State<'_, SharedLifecycle>,
) -> Result<serde_json::Value, String> {
    let state = state.inner().clone();
    let lifecycle = lifecycle.inner().clone();
    run_blocking(move || verify_key_inner_cmd(app, state, lifecycle)).await
}

fn verify_key_inner_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    // 经串行器（修 P1-a）：ensure_proxy 与其它生命周期操作不并发交叠。
    lifecycle.with_serialized(|| {
        let trace = OperationTrace::start(OperationKind::VerifyKey, "command=verify_key");
        let (port, secret, _action) =
            ensure_proxy(&app, &state, lifecycle.as_ref(), Some(&trace))?;
        let body = br#"{"model":"claude-opus-4-8","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
        trace.stage(OperationStage::UpstreamProbe, "POST /v1/messages via active proxy");
        match proc::http_post_status(
            port,
            Some(&secret),
            "/v1/messages",
            body,
            operation::VERIFY_KEY_TIMEOUT_MS,
        ) {
            Some(200) => {
                trace.finish("ok status=200");
                Ok(json!({ "ok": true, "hint": "key 有效，上游已接受。" }))
            }
            Some(code @ (401 | 403)) => {
                trace.finish(format!("rejected status={code}"));
                Ok(
                    json!({ "ok": false, "hint": format!("上游拒绝（{code}），key 可能无效或无权限。") }),
                )
            }
            Some(code) => {
                trace.finish(format!("upstream_status={code}"));
                Ok(json!({
                    "ok": false,
                    "hint": format!("上游返回 {code}，可能是 key 无效、额度不足或上游异常。")
                }))
            }
            None => {
                trace.finish("error=no_response");
                Err("验证请求无响应（多为网络或上游不通）。".to_string())
            }
        }
    })
}

#[derive(Deserialize)]
pub(crate) struct FetchModelsReq {
    /// 模板 id（决定 builtin / base_url 可编辑性 / 默认 base_url）。
    template_id: String,
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

/// 解析探测用 key：新填的优先，否则沿用 profile_id 已存的（后端内部用，绝不回传前端）。
fn resolve_probe_key(profile_id: Option<&str>, candidate: &str) -> Result<String, String> {
    let c = candidate.trim();
    if !c.is_empty() {
        return Ok(c.to_string());
    }
    let pid = profile_id.ok_or("请先填写 API Key / Token。")?;
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    cfg.profile_by_id(pid)
        .map(|p| p.api_key.clone())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| "请先填写 API Key / Token。".to_string())
}

/// 「获取可用模型」——纯 scratch 探测：只用临时代理探候选 base_url/key 的 /v1/models，
/// 绝不写 config、不改 AppState、不碰正在服务 Science 的正式代理。
#[tauri::command]
pub(crate) async fn fetch_models(
    app: tauri::AppHandle,
    req: FetchModelsReq,
) -> Result<serde_json::Value, String> {
    run_blocking(move || fetch_models_inner_cmd(app, req)).await
}

fn fetch_models_inner_cmd(
    app: tauri::AppHandle,
    req: FetchModelsReq,
) -> Result<serde_json::Value, String> {
    let tid = req.template_id.trim();
    let tpl = templates::by_id(tid).ok_or_else(|| format!("未知模板：{tid}"))?;
    let base_url = if tpl.base_url_editable {
        req.base_url.trim().to_string()
    } else {
        tpl.base_url.to_string()
    };
    if base_url.is_empty() || !(base_url.starts_with("http://") || base_url.starts_with("https://"))
    {
        return Err("请先填写 base_url（http:// 或 https:// 开头）。".into());
    }
    reject_openai_custom_anthropic_base(tid, &base_url)?;
    let key = resolve_probe_key(req.profile_id.as_deref(), &req.key)?;
    let root = asset_root(&app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
    let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
    let script = root.join("proxy/csswitch_proxy.py");
    let adapter = templates::adapter_for(tid);
    let trace = OperationTrace::start(
        OperationKind::FetchModels,
        format!("template_id={tid} adapter={adapter}"),
    );

    let res = scratch::scratch_probe(
        &py,
        &script,
        &scratch::ScratchTarget {
            provider: adapter,
            key_env: key_env_for_adapter(adapter),
            base_url: &base_url,
            key: &key,
            model: None,
            relay_thinking: tpl.thinking_policy,
        },
        scratch::ProbeKind::Models,
        Some(&trace),
    );
    let builtin = tpl.builtin_models;
    match scratch::classify(res.status) {
        scratch::ProbeOutcome::Ok => {
            trace.stage(OperationStage::ScratchUpstreamProbe, "outcome=ok");
            let v: serde_json::Value =
                serde_json::from_str(&res.body).map_err(|e| format!("解析模型列表失败：{e}"))?;
            let live: Vec<(String, Option<bool>)> = v
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            let id = m.get("id")?.as_str()?.to_string();
                            let st = m.get("supports_tools").and_then(|b| b.as_bool());
                            Some((id, st))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if live.is_empty() {
                trace.finish("ok source=builtin empty_live");
                return Ok(json!({
                    "models": merge_and_sort_models(vec![], builtin),
                    "source": "builtin", "error_kind": null, "upstream_status": 200
                }));
            }
            trace.finish(format!("ok source=live count={}", live.len()));
            Ok(json!({
                "models": merge_and_sort_models(live, builtin),
                "source": "live", "error_kind": null, "upstream_status": 200
            }))
        }
        scratch::ProbeOutcome::Auth(code) => {
            trace.finish(format!("rejected status={code}"));
            Err(format!("上游拒绝（{code}），key 或权限可能有误。"))
        }
        // 非 200 且非 Auth：一律 builtin 兜底，但按语义分「发现不支持」(4xx) 与「网络/上游临时」(5xx/429/无响应)，
        // 供前端区分提示（spec v3 §3.4.3）。绝不把 Auth 混进来掩盖坏 key。
        other => {
            let source = scratch::discovery_fallback_source(&other);
            trace.finish(format!("fallback source={source} outcome={other:?}"));
            let error_kind = if source == "network" {
                json!("network")
            } else {
                json!(null)
            };
            Ok(json!({
                "models": merge_and_sort_models(vec![], builtin),
                "source": source,
                "error_kind": error_kind,
                "upstream_status": res.status
            }))
        }
    }
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
        kill_child(&mut st.proxy);
        st.secret.clear();
        st.provider.clear();
        st.key_fp = 0;
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

fn one_click_login_cmd(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: SharedLifecycle,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| one_click_login_inner(app, state, lifecycle.as_ref()))
}

/// 一键开始本体（经串行器）：确保代理在跑且健康 → 幂等虚拟登录 → 起沙箱 → 打开 UI。
fn one_click_login_inner(
    app: tauri::AppHandle,
    state: SharedAppState,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<serde_json::Value, String> {
    let trace = OperationTrace::start(OperationKind::OneClickLogin, "command=one_click_login");
    // 1~3. 确保代理在跑且健康（内部已查生效 profile、key、探活）。带回本次是复用还是重启。
    let (pport, secret, proxy_action) = ensure_proxy(&app, &state, lifecycle, Some(&trace))?;

    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let sport = cfg.sandbox_port;

    let sbx_home = sandbox_home();
    let auth_dir = sbx_home.join(".claude-science");

    // 沙箱已健康 → 但「daemon 活着」≠「登录态可用」：先只读校验虚拟登录是否自洽。
    if sandbox_running_ours(sport) {
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
            let msg = match open_in_browser(&url) {
                Ok(()) => format!("{base}，已重新打开 Science。"),
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
            let _ = stop_sandbox_state(&app, &mut st);
        }
    }

    // 沙箱没起 / 挂了 / 登录失效已停 → 需要 launch 资源，此时才定位。确保虚拟登录（幂等）+ launch。
    let root = asset_root(&app)
        .ok_or("找不到 scripts/launch-virtual-sandbox.sh（打包资源或仓库根均未命中）。")?;

    trace.stage(OperationStage::SandboxLogin, "ensure_virtual_login");
    let (forged, login_action) =
        oauth_forge::ensure_virtual_login(&auth_dir, "virtual@localhost.invalid", &sbx_home)
            .map_err(|e| format!("写虚拟登录失败：{e}"))?;

    let launch = root.join("scripts/launch-virtual-sandbox.sh");
    if !launch.is_file() {
        return Err("找不到 scripts/launch-virtual-sandbox.sh。".into());
    }

    // 4. 起沙箱：脚本以 --detached 起 Science，然后返回。
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
        .stdout(Stdio::from(logf))
        .stderr(Stdio::from(logf2))
        .status()
        .map_err(|e| format!("起沙箱失败：{e}"))?;
    if !status.success() {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        trace.finish("error=sandbox_launch_failed");
        return Err(format!("起沙箱脚本失败。\n{tail}"));
    }

    // 5. 轮询沙箱 /health 直到就绪或超时（~8s）。
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

    // 5b. 身份确认：/health 200 只证明端口在服务，用 data-dir 强身份再确认一次。
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

    // 6. 取 UI URL（登录态），交系统浏览器打开。
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
    let msg = match open_in_browser(&url) {
        Ok(()) => format!("{started}。"),
        Err(_) => format!("{started}，服务已就绪，请手动打开：{url}"),
    };
    trace.stage(OperationStage::OpenBrowser, "done");
    trace.finish(format!(
        "ok action=started proxy_action={}",
        proxy_action.as_str()
    ));
    Ok(json!({ "url": url, "msg": msg, "action": "started" }))
}

#[tauri::command]
pub(crate) fn status(state: State<'_, SharedAppState>) -> serde_json::Value {
    // 只在锁内取值，锁外做短超时探活。这里是高频 UI 状态灯，
    // 不能反复调用外部 `claude-science status`，否则前端轮询会卡住主线程。
    // 沙箱强身份确认保留在 one_click_login 的启动/复用边界。
    let (pport, secret, sport, adapter, base_url) = {
        let st = lock(state.inner());
        let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
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
        // 上游灯读生效 profile 的 adapter/base_url；无生效配置 → 空（灯显黄，不误探）。
        let (adapter, base_url) = match cfg.active_profile() {
            Some(p) => (
                templates::adapter_for(&p.template_id).to_string(),
                p.base_url.clone(),
            ),
            None => (String::new(), String::new()),
        };
        (pport, st.secret.clone(), sport, adapter, base_url)
    };
    let uhost = upstream_host(&adapter, &base_url);
    let lights = status_lights(StatusProbeInput {
        proxy_ok: !secret.is_empty()
            && proc::http_health(pport, Some(&secret), operation::STATUS_HEALTH_TIMEOUT_MS),
        sandbox_ok: proc::http_health(sport, None, operation::STATUS_HEALTH_TIMEOUT_MS),
        upstream_ok: !uhost.is_empty()
            && proc::tcp_reachable(&uhost, 443, operation::STATUS_UPSTREAM_TIMEOUT_MS),
    });
    json!({ "proxy": lights.proxy, "sandbox": lights.sandbox, "upstream": lights.upstream })
}

#[tauri::command]
pub(crate) fn open_url(state: State<'_, SharedAppState>) -> Result<(), String> {
    let url = { lock(state.inner()).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
pub(crate) fn quit_app(
    app: tauri::AppHandle,
    state: State<'_, SharedAppState>,
) -> Result<(), String> {
    // 默认：退 app 停代理、保留沙箱运行（spec §5.1）。
    {
        let mut st = lock(state.inner());
        kill_child(&mut st.proxy);
        st.secret.clear();
    }
    app.exit(0);
    Ok(())
}
