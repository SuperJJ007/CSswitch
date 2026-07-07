//! CSSwitch 桌面 app 后端（进程管家）。
//!
//! 职责：管理「翻译代理」与「沙箱 Science」两个子进程的生命周期；读写
//! `~/.csswitch/config.json`（多 profile 形态）；把第三方 key 以【环境变量】注入代理子进程
//! （绝不进 argv）；探活；把沙箱 URL 交系统浏览器打开。已验证的越权/翻译逻辑仍留在
//! Python/Node/shell 里被当作子进程调用，以保住铁律护栏与已验证行为。
//!
//! 运行行为由生效 profile 的 `template_id` 经 [`templates`] 注册表派生出 adapter
//! （deepseek | qwen | relay | openai-custom | openai-responses），再传给 python 代理 `--provider`。
//!
//! 铁律相关：key 只在内存与 0600 的 config.json；回显前端只给掩码；沙箱端口/目录护栏
//! 由被调脚本负责（对 8765 与真实目录失败关闭）；退 app 默认停代理、保留沙箱。

mod config;
mod config_legacy;
mod lifecycle;
mod oauth_forge;
mod proc;
mod runtime;
mod scratch;
mod templates;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tauri::{Manager, State};

use runtime::diagnostics::{status_lights, StatusProbeInput};
use runtime::profile::{
    build_get_config, build_list_templates, clear_profile_key_inner, create_profile_inner,
    delete_profile_inner, merge_and_sort_models, nonactive_probe_verdict, probe_kind_for,
    update_profile_connection_inner, update_profile_metadata_inner, ConnectionEdit,
};
use runtime::provider::{
    assert_format_supported, is_native_adapter, is_openai_adapter, key_env_for_adapter,
    proxy_args_for, proxy_fingerprint, reject_openai_custom_anthropic_base, relay_missing_base_url,
    relay_missing_model, should_scratch_candidate, upstream_host,
};
use runtime::proxy::{health_timeout_reason, should_write_back, ProxyAction};
use runtime::science::{first_http_url, sandbox_home, settings_change_needs_teardown};
use runtime::transaction::{
    decide_switch, rollback_status_clause, skip_scratch_verify, SwitchOutcome,
};

const SCIENCE_BIN: &str = "/Applications/Claude Science.app/Contents/Resources/bin/claude-science";

#[derive(Default)]
struct AppState {
    proxy: Option<Child>,
    proxy_port: u16,
    secret: String,
    /// 当前代理进程所用 adapter 名（deepseek | qwen | relay | openai-custom | openai-responses）；用于健康复用判定。
    provider: String,
    /// 当前代理进程所用 key 的非加密指纹（仅内存、绝不落盘/打印）。
    /// 换 key/换上游后指纹变化 → 触发重启，避免复用带旧配置的代理。
    key_fp: u64,
    sandbox: Option<Child>,
    sandbox_port: u16,
    sandbox_url: Option<String>,
}

// ---------- 路径与日志 ----------
/// 定位 CSSwitch 仓库根（含 proxy/csswitch_proxy.py）。优先 CSSWITCH_REPO，
/// 否则从可执行文件逐级上溯。找不到返回 None。
fn repo_root() -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    if let Some(r) = std::env::var_os("CSSWITCH_REPO") {
        if let Ok(p) = std::fs::canonicalize(PathBuf::from(r)) {
            if p.join(marker).is_file() {
                return Some(p);
            }
        }
    }
    // 只从【可执行文件位置】上溯。刻意不看 current_dir：启动目录可被影响，
    // 若据此找到别处的 csswitch_proxy.py，会把带 key 的环境交给来路不明的脚本。
    if let Ok(exe) = std::env::current_exe() {
        let mut dir: Option<&Path> = exe.parent();
        while let Some(d) = dir {
            if d.join(marker).is_file() {
                return Some(d.to_path_buf());
            }
            dir = d.parent();
        }
    }
    None
}

/// 定位「资源根」（含 proxy/、scripts/）。打包成 .app 后 bundle 进 `Contents/Resources`；
/// 开发态则回退到仓库根。找不到返回 None。
fn asset_root(app: &tauri::AppHandle) -> Option<PathBuf> {
    let marker = Path::new("proxy/csswitch_proxy.py");
    if let Ok(res) = app.path().resource_dir() {
        if res.join(marker).is_file() {
            return Some(res);
        }
    }
    repo_root()
}

fn log_path(name: &str) -> PathBuf {
    config::default_dir().join("logs").join(name)
}

/// `O_NOFOLLOW` 的平台常量（本项目不引 libc）。macOS/BSD=0x0100，Linux=0x20000。
const fn libc_o_nofollow() -> i32 {
    if cfg!(target_os = "linux") {
        0x2_0000
    } else {
        0x0100
    }
}

/// 打开（truncate）一个子进程日志文件，父目录 0700、文件 0600（防同机其它用户读到 secret 尾巴）。
fn open_log(name: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = log_path(name);
    if let Some(parent) = p.parent() {
        config::assert_not_symlink(parent)?;
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    config::assert_not_symlink(&p)?;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc_o_nofollow())
        .open(&p)?;
    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    Ok(f)
}

/// 把字符串里的 secret 明文替换成 ****，用于任何要回显给前端的错误尾巴。
fn redact(s: &str, secret: &str) -> String {
    if secret.is_empty() {
        s.to_string()
    } else {
        s.replace(secret, "****")
    }
}

fn tail_file(path: &Path, max: usize) -> String {
    match std::fs::read(path) {
        Ok(b) => {
            let start = b.len().saturating_sub(max);
            String::from_utf8_lossy(&b[start..]).trim().to_string()
        }
        Err(_) => String::new(),
    }
}

fn kill_child(slot: &mut Option<Child>) {
    if let Some(mut c) = slot.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 取锁并从 poison 中恢复：某线程持锁时 panic 不应把整个 app 卡死。
fn lock(m: &Mutex<AppState>) -> std::sync::MutexGuard<'_, AppState> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 用系统浏览器打开 URL（macOS `open`）。校验退出码：非零视为失败（P2c）。
fn open_in_browser(url: &str) -> Result<(), String> {
    let st = Command::new("open")
        .arg(url)
        .status()
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    if !st.success() {
        return Err(format!("open 非零退出（{:?}）", st.code()));
    }
    Ok(())
}

// ---------- 代理生命周期核心 ----------
/// 转义 ERE 元字符，让路径按字面参与 `pkill -f` 匹配。
fn ere_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if "\\.^$*+?()[]{}|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 确保代理在跑且健康；返回 (端口, secret, 本次动作)。幂等：已健康则复用。
/// 读【生效 profile】派生 adapter/base_url/model/key，委托 [`start_proxy_for`]。
fn ensure_proxy(
    app: &tauri::AppHandle,
    state: &State<'_, Mutex<AppState>>,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<(u16, String, ProxyAction), String> {
    let cfg = config::load_from(&config::default_dir()).map_err(|e| e.to_string())?;
    let profile = cfg
        .active_profile()
        .cloned()
        .ok_or("未配置生效 profile，请先在面板选择或新建一条配置。")?;
    start_proxy_for(app, state, lifecycle, &profile)
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
fn start_proxy_for(
    app: &tauri::AppHandle,
    state: &State<'_, Mutex<AppState>>,
    lifecycle: &lifecycle::Lifecycle,
    profile: &config::Profile,
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
            && proc::http_health(port, Some(&st.secret), 500)
        {
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
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        if proc::http_health(port, Some(&secret), 400) {
            ok = true;
            break;
        }
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

/// 停沙箱。返回 Err 表示 stop 脚本非零退出（Science 可能没停干净），调用方据此如实报告。
fn stop_sandbox_inner(app: &tauri::AppHandle, st: &mut AppState) -> Result<(), String> {
    let mut err = None;
    match asset_root(app) {
        Some(root) => {
            let stop = root.join("scripts/stop-science-sandbox.sh");
            if stop.is_file() {
                match Command::new("zsh")
                    .arg(&stop)
                    .env("SANDBOX_HOME", sandbox_home())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => {}
                    Ok(s) => err = Some(format!("停止沙箱脚本非零退出（{:?}）。", s.code())),
                    Err(e) => err = Some(format!("调用停止沙箱脚本失败：{e}")),
                }
            } else {
                err = Some(format!(
                    "找不到停止脚本 {}，无法确认沙箱已停止（沙箱可能仍在运行）。",
                    stop.display()
                ));
            }
        }
        None => {
            err = Some(
                "定位不到资源根，取不到停止脚本，无法确认沙箱已停止（沙箱可能仍在运行）。"
                    .to_string(),
            );
        }
    }
    kill_child(&mut st.sandbox);
    st.sandbox_url = None;
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------- Tauri commands ----------
#[tauri::command]
fn get_config() -> Result<serde_json::Value, String> {
    build_get_config(&config::default_dir())
}

/// 模板注册表交前端铺 UI（新建向导用）。
#[tauri::command]
fn list_templates() -> Vec<serde_json::Value> {
    build_list_templates()
}

/// 切换运行模式（"proxy" 第三方 / "official" 官方）。切官方要先拆第三方链路成功再落盘。
#[tauri::command]
fn set_mode(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
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
            stop_sandbox_inner(&app, &mut st).map_err(|e| {
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
fn open_official() -> Result<(), String> {
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
struct UiSettings {
    proxy_port: u16,
    sandbox_port: u16,
}

/// 端口设置（provider/连接改走 profile CRUD + set_active_profile）。
/// 经串行器（修 P1-c）：端口一旦变化，正在跑的代理绑在旧端口、正在跑的沙箱又烘死了旧代理 URL，
/// 与新端口不一致；此处把这条陈旧链路拆掉（只停我们的沙箱、绝不碰 8765），逼下次「一键开始」按新端口重建，
/// 杜绝「复用旧沙箱指向死端口、UI 却报沿用不变」。
#[tauri::command]
fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
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
            stop_sandbox_inner(&app, &mut st).map_err(|e| {
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

// ---------- profile CRUD 命令（薄包装 *_inner，统一经串行器） ----------
#[tauri::command]
fn create_profile(
    lifecycle: State<'_, lifecycle::Lifecycle>,
    template_id: String,
    name: String,
    key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
) -> Result<String, String> {
    lifecycle.with_serialized(|| {
        create_profile_inner(
            &config::default_dir(),
            &template_id,
            &name,
            key.as_deref(),
            base_url.as_deref(),
            model.as_deref(),
        )
    })
}

#[tauri::command]
fn update_profile_metadata(
    lifecycle: State<'_, lifecycle::Lifecycle>,
    id: String,
    name: String,
    notes: Option<String>,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        update_profile_metadata_inner(&config::default_dir(), &id, &name, notes.as_deref())
    })
}

/// 清 key：经串行器；若清的是【生效】profile → bump_generation 作废在途启动 + 停运行中代理
/// （不再拿旧 key 服务，比照 spec §8.2 运行态撤销）。
#[tauri::command]
fn clear_profile_key(
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
    id: String,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let was_active = config::load_from(&dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        clear_profile_key_inner(&dir, &id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(&state);
            kill_child(&mut st.proxy);
            st.provider.clear();
            st.key_fp = 0;
        }
        Ok(())
    })
}

/// 删 profile：经串行器；删的是【生效】profile → active 置空（inner 内）+ bump + 停代理。
#[tauri::command]
fn delete_profile(
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
    id: String,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let was_active = config::load_from(&dir)
            .map(|c| c.active_id == id)
            .unwrap_or(false);
        delete_profile_inner(&dir, &id)?;
        if was_active {
            lifecycle.bump_generation();
            let mut st = lock(&state);
            kill_child(&mut st.proxy);
            st.provider.clear();
            st.key_fp = 0;
        }
        Ok(())
    })
}

/// 对候选连接做一次上游 scratch 校验（非 active 编辑用，P2-d）。起临时代理探完即杀，
/// **绝不碰 config / AppState / 正在服务的正式代理**。返回是否【已通过上游校验】（供调用方据实措辞）：
/// 空 key / relay 家族空 base_url → `Ok(false)`（无从预检，标记未校验）；
/// native(deepseek/qwen) 即便 base_url 空也【会】走各自官方端点探测（修真机 P1）；
/// 明确接受(200) → `Ok(true)`；明确拒绝 → `Err(hint)`；无法确认 → `Ok(false)`（见 [`nonactive_probe_verdict`]）。
fn scratch_validate_candidate(
    app: &tauri::AppHandle,
    candidate: &config::Profile,
) -> Result<bool, String> {
    let launch = proxy_args_for(candidate);
    if !should_scratch_candidate(&launch.adapter, &launch.key, &launch.base_url) {
        return Ok(false); // 跳过 = 未校验（如实标记）
    }
    let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
    let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
    let script = root.join("proxy/csswitch_proxy.py");
    let res = scratch::scratch_probe(
        &py,
        &script,
        &scratch::ScratchTarget {
            provider: &launch.adapter,
            key_env: launch.key_env,
            base_url: &launch.base_url,
            key: &launch.key,
            model: Some(&launch.model),
            relay_thinking: launch.thinking_policy,
        },
        probe_kind_for(&launch.adapter, &launch.model),
    );
    nonactive_probe_verdict(&scratch::classify(res.status))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn update_profile_connection(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
    id: String,
    base_url: Option<String>,
    api_format: Option<String>,
    model: Option<String>,
    key: Option<String>,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        let dir = config::default_dir();
        let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
        // 未命中 id → Err（不静默 Ok）。
        let mut candidate = cfg
            .profile_by_id(&id)
            .cloned()
            .ok_or_else(|| format!("找不到 profile：{id}"))?;
        // 生效【后】的候选连接（None=不改则沿用旧值），active/非 active 共用一份。
        let edit = ConnectionEdit::new(
            base_url.clone(),
            api_format.clone(),
            model.clone(),
            key.clone(),
        );
        edit.apply(&mut candidate);
        reject_openai_custom_anthropic_base(&candidate.template_id, &candidate.base_url)?;
        // 保存前守卫（修 P2）：relay/自定义端点清空 base_url → 不可用连接（激活必失败）。
        // 校验生效后的 base_url，空则拒绝落盘、绝不谎报「已保存」；native 走硬编码端点，空无妨。
        if relay_missing_base_url(
            templates::adapter_for(&candidate.template_id),
            &candidate.base_url,
        ) {
            return Err("中转 / 自定义端点必须填写连接地址（base_url），连接未保存。".to_string());
        }
        // 保存前守卫（修 #9 P1-a）：relay/自定义端点空 model → 无 force → 退回 passthrough（显示 claude）。
        if relay_missing_model(
            templates::adapter_for(&candidate.template_id),
            &candidate.model,
        ) {
            return Err("中转 / 自定义端点必须选择或填写一个模型，连接未保存。".to_string());
        }
        if cfg.active_id == id {
            // active（有正在服务的代理）：validate-before-persist —— 新连接作【内存候选】喂进
            // 切换事务（校验→起正式→健康），探活健康【才】连同落盘；失败则磁盘连接零改动、
            // 仍跑旧连接（杜绝「盘新运行旧」，修 P1-4）。
            let v =
                set_active_profile_txn(&app, &state, lifecycle.inner(), &id, false, Some(&edit))?;
            // 连接编辑：committed:false（scratch 分类失败）也如实作为错误上抛（磁盘未改、代理仍跑旧的）。
            if v.get("committed").and_then(|b| b.as_bool()) == Some(false) {
                let hint = v
                    .get("hint")
                    .and_then(|h| h.as_str())
                    .unwrap_or("连接校验未通过，连接未保存。")
                    .to_string();
                return Err(hint);
            }
            // active：已连同起正式代理探活并落盘，视为已校验。
            Ok(json!({ "validated": true }))
        } else {
            // 非 active：无正在服务的代理。先对候选做上游 scratch 校验（仅明确拒绝才拦，其余
            // best-effort 落盘并如实标记「未校验」，修 P2-d：贴合设计「校验候选后提交」+ 如实报告），
            // 再落盘（inner 内含格式门 + 覆盖前留底）。
            let validated = scratch_validate_candidate(&app, &candidate)?;
            update_profile_connection_inner(
                &dir,
                &id,
                base_url.as_deref(),
                api_format.as_deref(),
                model.as_deref(),
                key.as_deref(),
            )?;
            Ok(json!({ "validated": validated }))
        }
    })
}

/// 一键切生效 profile：经串行器走 [`set_active_profile_txn`] 切换事务。
#[tauri::command]
fn set_active_profile(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
    id: String,
    skip_verify: bool,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| {
        set_active_profile_txn(&app, &state, lifecycle.inner(), &id, skip_verify, None)
    })
}

/// 切换事务本体（spec §7）：scratch 校验候选 → 起正式代理探活 → 探活健康【才】提交 active_id；
/// 任一步失败杀候选 + 恢复旧代理，`active_id` 不动，**不停沙箱**（path-secret 持久，端口+secret
/// 不变，沙箱链路不断，停沙箱只会扩大失败面）。**本函数不取串行器锁**（调用方命令已持有）。
fn set_active_profile_txn(
    app: &tauri::AppHandle,
    state: &State<'_, Mutex<AppState>>,
    lifecycle: &lifecycle::Lifecycle,
    id: &str,
    skip_verify: bool,
    conn_edit: Option<&ConnectionEdit>,
) -> Result<serde_json::Value, String> {
    let dir = config::default_dir();
    let cfg = config::load_from(&dir).map_err(|e| e.to_string())?;
    let mut candidate = cfg
        .profile_by_id(id)
        .cloned()
        .ok_or_else(|| format!("找不到 profile：{id}"))?;
    // active 连接编辑：把新连接字段套到【内存候选】做校验（validate-before-persist）——
    // 磁盘此刻仍是旧连接；只有探活健康提交时才落盘（见下方 Commit 分支）。
    if let Some(edit) = conn_edit {
        edit.apply(&mut candidate);
    }
    reject_openai_custom_anthropic_base(&candidate.template_id, &candidate.base_url)?;
    let is_edit = conn_edit.is_some();
    // 失败措辞：连接编辑说「未保存/仍在用原配置运行」，普通切换说「未切换/当前配置不变」。
    let (verb, tail): (&str, &str) = if is_edit {
        ("未保存", "仍在用原配置运行")
    } else {
        ("未切换", "当前配置不变")
    };
    assert_format_supported(&candidate)?;
    let launch = proxy_args_for(&candidate);
    if launch.key.is_empty() {
        return Err(format!("「{}」还没填 API key，请先填写。", candidate.name));
    }
    let native = is_native_adapter(&launch.adapter);
    if !native && launch.base_url.is_empty() {
        return Err("该配置需要填 base_url（http:// 或 https:// 开头）。".into());
    }
    // 守卫（修 #9 P1-a）：relay/自定义端点空 model 无法激活（无 force → 退回 passthrough 显示 claude）。
    if relay_missing_model(&launch.adapter, &candidate.model) {
        return Err(
            "该配置需要选择或填写一个模型（中转/自定义端点必填），请在连接编辑里补上。".into(),
        );
    }
    // 快照旧 active（回滚锚点）：旧 profile 仍在盘上未动、active_id 未改，恢复据它重起旧代理。
    let old_active = cfg.active_id.clone();

    // 1) scratch 校验候选（临时端口+secret+候选 key，避开 8765；绝不碰正式链路）。
    //    所有 adapter 都预检：native(deepseek/qwen) 用各自官方端点 + Message 探测（其 /v1/models 静态，
    //    探不出坏 key）；只有用户显式 skip_verify 才跳过（修真机 P1：原生免校验会让无效 key 提交为
    //    active 并谎报「已切到」，首个真实推理才 401）。分类失败保留结构化提示（committed:false/can_skip）。
    let scratch_ok = if skip_scratch_verify(native, skip_verify) {
        true
    } else {
        let root = asset_root(app).ok_or("找不到代理脚本 proxy/csswitch_proxy.py。")?;
        let py = proc::find_exe("python3").ok_or("缺少依赖 python3（起临时代理需要）。")?;
        let script = root.join("proxy/csswitch_proxy.py");
        let res = scratch::scratch_probe(
            &py,
            &script,
            &scratch::ScratchTarget {
                provider: &launch.adapter,
                key_env: launch.key_env,
                base_url: &launch.base_url,
                key: &launch.key,
                model: Some(&launch.model),
                relay_thinking: launch.thinking_policy,
            },
            probe_kind_for(&launch.adapter, &launch.model),
        );
        match scratch::classify(res.status) {
            scratch::ProbeOutcome::Ok => true,
            scratch::ProbeOutcome::Auth(code) => {
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝（{code}），key/权限有误，{verb}（{tail}）。") }));
            }
            scratch::ProbeOutcome::ModelError(code) => {
                return Ok(json!({ "committed": false,
                    "hint": format!("上游拒绝该模型（{code}），{verb}。请换一个模型或核对 base_url。") }));
            }
            scratch::ProbeOutcome::Ambiguous(_)
            | scratch::ProbeOutcome::NoResponse
            | scratch::ProbeOutcome::Unsupported(_) => {
                return Ok(json!({ "committed": false, "can_skip": true,
                    "hint": format!("无法确认（网络/上游繁忙），{verb}。可重试，或用「跳过验证」。") }));
            }
        }
    };

    // 2/3) 用候选起【正式代理】并探活。bump_generation 使并发中的旧启动（如同时的 verify_key）作废。
    lifecycle.bump_generation();
    let real_healthy = scratch_ok && start_proxy_for(app, state, lifecycle, &candidate).is_ok();

    match decide_switch(scratch_ok, real_healthy) {
        SwitchOutcome::Commit => {
            // 探活健康【才】落盘：连接编辑连同 active_id 一起提交（validate-before-persist），
            // 盘上与运行态一致，杜绝「盘新运行旧」。
            if is_edit {
                config::write_rolling_backup(&dir).ok(); // 覆盖连接前留底（仅编辑路径需要）
            }
            if let Err(e) = config::update(&dir, |c| {
                c.active_id = id.to_string();
                if let Some(edit) = conn_edit {
                    if let Some(p) = c.profile_by_id_mut(id) {
                        edit.apply(p);
                    }
                }
            }) {
                // spec §7 步 5：config 提交失败也要回滚进程——正式代理已起，若不回滚就成「运行新/盘旧」。
                // 恢复旧 active 代理，active_id 仍为旧值，用户可重试。
                let restored = restore_proxy_for_active(app, state, lifecycle, &cfg, &old_active);
                return Err(format!(
                    "校验通过、代理已起，但写盘失败（{e}），{}。请检查磁盘空间/权限后重试。",
                    rollback_status_clause(restored)
                ));
            }
            let hint = if is_edit {
                format!("已保存并应用「{}」的新连接。", candidate.name)
            } else {
                format!("已切到「{}」。", candidate.name)
            };
            Ok(json!({ "committed": true, "active_id": id, "hint": hint }))
        }
        SwitchOutcome::RollbackToOld => {
            // 候选正式代理起/探活失败：恢复旧代理，active_id 不动，连接不落盘，不停沙箱。
            let restored = restore_proxy_for_active(app, state, lifecycle, &cfg, &old_active);
            let clause = rollback_status_clause(restored);
            if is_edit {
                Err(format!(
                    "连接已校验通过，但正式代理启动/探活失败，连接未保存，{clause}。"
                ))
            } else {
                Err(format!(
                    "候选配置校验通过，但正式代理启动/探活失败，{clause}。"
                ))
            }
        }
        SwitchOutcome::AbortBeforeStart => {
            // scratch 校验未过；旧态零改动、连接不落盘。（明确拒绝/含糊态在上面已 committed:false 早返，
            // 此分支是 scratch_ok=false 的兜底措辞。）
            if is_edit {
                Err("连接上游校验失败（key/base_url/网络？），连接未保存。".into())
            } else {
                Err("候选上游校验失败（key/base_url/网络？），未切换。".into())
            }
        }
    }
}

/// 切换失败回滚：按【旧 active】重起旧代理（旧 profile 仍在盘上）；best-effort，失败则代理暂停、
/// active_id 仍为旧值，用户可重试。旧 active 为空（此前未配置生效）→ 不复活，保持代理停着。
/// 返回是否已把旧代理恢复到位（供调用方据实措辞，修 P2-e）。
fn restore_proxy_for_active(
    app: &tauri::AppHandle,
    state: &State<'_, Mutex<AppState>>,
    lifecycle: &lifecycle::Lifecycle,
    cfg: &config::Config,
    old_active: &str,
) -> bool {
    if old_active.is_empty() {
        return true; // 此前无生效配置 → 本就无代理可恢复，状态与切换前一致
    }
    match cfg.profile_by_id(old_active) {
        Some(old) => {
            lifecycle.bump_generation();
            start_proxy_for(app, state, lifecycle, old).is_ok()
        }
        None => false, // 旧 active 指向已不存在的 profile（罕见）→ 无法恢复，代理已停
    }
}

#[tauri::command]
fn start_proxy(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
) -> Result<serde_json::Value, String> {
    // 经串行器：与切换/连接编辑/清 key/删/停等 ensure_proxy 竞争串行化，防陈旧读起旧配置代理
    // 又写回运行态（修 P1-a，比照 spec §8.1「ensure_proxy 都经一把 app 级 mutex」）。
    lifecycle.with_serialized(|| {
        let (port, _secret, _action) = ensure_proxy(&app, &state, lifecycle.inner())?;
        Ok(json!({ "port": port }))
    })
}

/// 「存 key 即验证」：确保代理在跑，再经代理向上游发一个最小请求，据状态码判断 key 是否可用。
#[tauri::command]
fn verify_key(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
) -> Result<serde_json::Value, String> {
    // 经串行器（修 P1-a）：ensure_proxy 与其它生命周期操作不并发交叠。
    lifecycle.with_serialized(|| {
        let (port, secret, _action) = ensure_proxy(&app, &state, lifecycle.inner())?;
        let body = br#"{"model":"claude-opus-4-8","max_tokens":1,"messages":[{"role":"user","content":"ping"}]}"#;
        match proc::http_post_status(port, Some(&secret), "/v1/messages", body, 15000) {
            Some(200) => Ok(json!({ "ok": true, "hint": "key 有效，上游已接受。" })),
            Some(code @ (401 | 403)) => Ok(
                json!({ "ok": false, "hint": format!("上游拒绝（{code}），key 可能无效或无权限。") }),
            ),
            Some(code) => Ok(json!({
                "ok": false,
                "hint": format!("上游返回 {code}，可能是 key 无效、额度不足或上游异常。")
            })),
            None => Err("验证请求无响应（多为网络或上游不通）。".to_string()),
        }
    })
}

#[derive(Deserialize)]
struct FetchModelsReq {
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
fn fetch_models(app: tauri::AppHandle, req: FetchModelsReq) -> Result<serde_json::Value, String> {
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
    );
    let builtin = tpl.builtin_models;
    match scratch::classify(res.status) {
        scratch::ProbeOutcome::Ok => {
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
                return Ok(json!({
                    "models": merge_and_sort_models(vec![], builtin),
                    "source": "builtin", "error_kind": null, "upstream_status": 200
                }));
            }
            Ok(json!({
                "models": merge_and_sort_models(live, builtin),
                "source": "live", "error_kind": null, "upstream_status": 200
            }))
        }
        scratch::ProbeOutcome::Auth(code) => {
            Err(format!("上游拒绝（{code}），key 或权限可能有误。"))
        }
        // 非 200 且非 Auth：一律 builtin 兜底，但按语义分「发现不支持」(4xx) 与「网络/上游临时」(5xx/429/无响应)，
        // 供前端区分提示（spec v3 §3.4.3）。绝不把 Auth 混进来掩盖坏 key。
        other => {
            let source = scratch::discovery_fallback_source(&other);
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
fn stop_all(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
) -> Result<(), String> {
    lifecycle.with_serialized(|| {
        lifecycle.bump_generation(); // 作废任何在途启动（防被停后又拿旧 key 复活）
        let mut st = lock(&state);
        let sandbox_res = stop_sandbox_inner(&app, &mut st);
        kill_child(&mut st.proxy);
        st.secret.clear();
        st.provider.clear();
        st.key_fp = 0;
        sandbox_res.map_err(|e| format!("代理已停；但{e}真实实例 8765 未受影响。"))
    })
}

#[tauri::command]
fn one_click_login(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: State<'_, lifecycle::Lifecycle>,
) -> Result<serde_json::Value, String> {
    lifecycle.with_serialized(|| one_click_login_inner(app, state, lifecycle.inner()))
}

/// 一键开始本体（经串行器）：确保代理在跑且健康 → 幂等虚拟登录 → 起沙箱 → 打开 UI。
fn one_click_login_inner(
    app: tauri::AppHandle,
    state: State<'_, Mutex<AppState>>,
    lifecycle: &lifecycle::Lifecycle,
) -> Result<serde_json::Value, String> {
    // 1~3. 确保代理在跑且健康（内部已查生效 profile、key、探活）。带回本次是复用还是重启。
    let (pport, secret, proxy_action) = ensure_proxy(&app, &state, lifecycle)?;

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
            return Ok(json!({ "url": url, "msg": msg, "action": "reopened" }));
        }
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st);
        }
    }

    // 沙箱没起 / 挂了 / 登录失效已停 → 需要 launch 资源，此时才定位。确保虚拟登录（幂等）+ launch。
    let root = asset_root(&app)
        .ok_or("找不到 scripts/launch-virtual-sandbox.sh（打包资源或仓库根均未命中）。")?;

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
        return Err(format!("起沙箱脚本失败。\n{tail}"));
    }

    // 5. 轮询沙箱 /health 直到就绪或超时（~8s）。
    let mut ok = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if proc::http_health(sport, None, 400) {
            ok = true;
            break;
        }
    }
    if !ok {
        let tail = redact(&tail_file(&log_path("sandbox.log"), 600), &secret);
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st);
        }
        return Err(format!(
            "沙箱起后探活超时（端口 {sport}）。已尝试停掉刚起的沙箱。\n{tail}"
        ));
    }

    // 5b. 身份确认：/health 200 只证明端口在服务，用 data-dir 强身份再确认一次。
    if !sandbox_running_ours(sport) {
        {
            let mut st = lock(&state);
            let _ = stop_sandbox_inner(&app, &mut st);
        }
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
    Ok(json!({ "url": url, "msg": msg, "action": "started" }))
}

/// 取沙箱 UI 链接：`<bin> url --data-dir <home>/.claude-science`，HOME 指向沙箱 HOME。
fn sandbox_url(port: u16) -> String {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        if let Ok(out) = Command::new(SCIENCE_BIN)
            .arg("url")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(url) = first_http_url(&s) {
                return url;
            }
        }
    }
    format!("http://127.0.0.1:{port}")
}

/// 判断「我们自己的」沙箱 Science 是否在跑（供一键健康分派）。优先用 Science 二进制按
/// 【我们的 data-dir】查 `{"running":true}`（强身份）；再叠加端口 /health 确认。
fn sandbox_running_ours(port: u16) -> bool {
    let home = sandbox_home();
    let data_dir = home.join(".claude-science");
    if Path::new(SCIENCE_BIN).is_file() {
        match Command::new(SCIENCE_BIN)
            .arg("status")
            .arg("--data-dir")
            .arg(&data_dir)
            .env("HOME", &home)
            .output()
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout);
                let running = s.contains("\"running\":true") || s.contains("\"running\": true");
                return running && proc::http_health(port, None, 400);
            }
            Err(_) => return proc::http_health(port, None, 400),
        }
    }
    proc::http_health(port, None, 400)
}

#[tauri::command]
fn status(state: State<'_, Mutex<AppState>>) -> serde_json::Value {
    // 只在锁内取值，锁外做阻塞探活。
    let (pport, secret, sport, adapter, base_url) = {
        let st = lock(&state);
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
        proxy_ok: !secret.is_empty() && proc::http_health(pport, Some(&secret), 300),
        sandbox_ok: sandbox_running_ours(sport),
        upstream_ok: !uhost.is_empty() && proc::tcp_reachable(&uhost, 443, 500),
    });
    json!({ "proxy": lights.proxy, "sandbox": lights.sandbox, "upstream": lights.upstream })
}

#[tauri::command]
fn open_url(state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    let url = { lock(&state).sandbox_url.clone() };
    let url = url.ok_or("还没有沙箱 URL，请先「一键开始」。")?;
    open_in_browser(&url)
}

#[tauri::command]
fn run_doctor(app: tauri::AppHandle) -> Result<String, String> {
    let root = asset_root(&app).ok_or("找不到 scripts/doctor.sh（打包资源或仓库根均未命中）。")?;
    let cfg = config::load_from(&config::default_dir()).unwrap_or_default();
    let doctor = root.join("scripts/doctor.sh");
    // 生效 profile 的展示名（template_id）+ adapter + 有无 key；无生效配置则留空。
    let (provider_label, adapter, has_key) = match cfg.active_profile() {
        Some(p) => (
            p.template_id.clone(),
            templates::adapter_for(&p.template_id),
            !p.api_key.is_empty(),
        ),
        None => (String::new(), "", false),
    };
    let mut cmd = Command::new("bash");
    // 多 profile：传 template_id + adapter + key 有无（布尔）。doctor 不再按 provider 名写死、
    // 不再去 shell 环境找 key（key 存 config.json）。绝不把真实 key 值传进其环境。
    cmd.arg(&doctor)
        .env("CSSWITCH_PROVIDER", &provider_label)
        .env("CSSWITCH_ADAPTER", adapter)
        .env("CSSWITCH_KEY_PRESENT", if has_key { "1" } else { "0" })
        .env("CSSWITCH_PROXY_PORT", cfg.proxy_port.to_string())
        .env("CSSWITCH_SANDBOX_PORT", cfg.sandbox_port.to_string());
    let out = cmd.output().map_err(|e| e.to_string())?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    if !err.trim().is_empty() {
        text.push_str("\n[stderr] ");
        text.push_str(err.trim());
    }
    Ok(text)
}

/// 当前 app 版本（供前端「检查更新」与页脚版本号用）。
#[tauri::command]
fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 打开 GitHub Releases 页（检查更新时用系统浏览器打开，浏览器走用户自己的代理）。
#[tauri::command]
fn open_release_page() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSSwitch/releases/latest")
}

/// 打开「报 bug」页（预填 bug 模板）；用系统浏览器，走用户自己的代理。
#[tauri::command]
fn report_bug() -> Result<(), String> {
    open_in_browser("https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml")
}

/// 在访达里打开日志目录 `~/.csswitch/logs`，方便用户附到 bug 反馈里（先自查有无密钥）。
#[tauri::command]
fn open_logs() -> Result<(), String> {
    let dir = config::default_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    Command::new("open")
        .arg(&dir)
        .status()
        .map_err(|e| format!("打开日志目录失败：{e}"))?;
    Ok(())
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle, state: State<'_, Mutex<AppState>>) -> Result<(), String> {
    // 默认：退 app 停代理、保留沙箱运行（spec §5.1）。
    {
        let mut st = lock(&state);
        kill_child(&mut st.proxy);
        st.secret.clear();
    }
    app.exit(0);
    Ok(())
}

// ---------- 入口 ----------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(AppState::default()))
        .manage(lifecycle::Lifecycle::new())
        .invoke_handler(tauri::generate_handler![
            get_config,
            list_templates,
            set_settings,
            set_mode,
            open_official,
            create_profile,
            update_profile_metadata,
            update_profile_connection,
            clear_profile_key,
            delete_profile,
            set_active_profile,
            start_proxy,
            verify_key,
            fetch_models,
            stop_all,
            one_click_login,
            status,
            open_url,
            run_doctor,
            app_version,
            open_release_page,
            report_bug,
            open_logs,
            quit_app
        ])
        .setup(|app| {
            // 正常桌面应用：进 Dock、走常规应用生命周期。窗口在 tauri.conf.json 里配了
            // decorations + visible + center，启动即居中弹出、可拖动。托盘图标已移除。

            // 启动即触发一次 load：若是旧 v1 固定槽文件，这里完成 v1→v2 迁移 + 落盘 + 留 .v1.bak；
            // 悬空 active 归一化为空。迁移逻辑并入 config::load_from（不再单独跑 relay_presets）。
            let _ = config::load_from(&config::default_dir());

            // 关窗即退出：与「退出」按钮一致 —— 停代理、清 secret，保留沙箱运行（spec §5.1）。
            if let Some(win) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { .. } = ev {
                        let state = handle.state::<Mutex<AppState>>();
                        let mut st = lock(&state);
                        kill_child(&mut st.proxy);
                        st.secret.clear();
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redact_scrubs_secret_and_is_noop_when_empty() {
        assert_eq!(
            redact("推理指向 http://127.0.0.1:18991/abcd1234 尾巴", "abcd1234"),
            "推理指向 http://127.0.0.1:18991/**** 尾巴"
        );
        assert_eq!(redact("原样返回", ""), "原样返回");
        assert!(!redact("leak abcd1234 leak abcd1234", "abcd1234").contains("abcd1234"));
    }
}
