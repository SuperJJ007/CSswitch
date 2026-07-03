// CSSwitch 菜单栏面板前端。只调用后端 Tauri command，绝不碰任何密钥落盘逻辑。
// 后端只把 key 的【掩码】回显给这里；完整 key 永不进前端。
//
// 预览兜底：在普通浏览器里打开（没有 Tauri 后端）时，用 mockInvoke 返回假数据，
// 让界面能完整渲染、不报错。真实 app 里 window.__TAURI__ 存在，走真后端，此兜底不生效。
const PREVIEW = !window.__TAURI__;
const invoke = PREVIEW
  ? (cmd, args) => mockInvoke(cmd, args)
  : window.__TAURI__.core.invoke;

function mockInvoke(cmd, args) {
  switch (cmd) {
    case "get_config":
      return Promise.resolve({ provider: "deepseek", proxy_port: 18991, sandbox_port: 8990, mode: "proxy", keys: { deepseek: "", qwen: "", relay: "" }, relay_base_url: "" });
    case "set_mode":
    case "open_official":
      return Promise.resolve(null);
    case "status":
      return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber" });
    case "save_provider_key":
      return Promise.resolve("••••••••••" + String((args && args.key) || "").slice(-4));
    case "start_proxy":
      return Promise.resolve({ port: 18991 });
    case "verify_key":
      return Promise.resolve({ ok: true, hint: "（预览模式：假装 key 有效）" });
    case "fetch_relay_models":
      return Promise.resolve({ models: ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-4-5-20251001", "claude-3-5-sonnet-20241022"] });
    case "one_click_login":
      return Promise.resolve({ url: "http://127.0.0.1:8990" });
    case "run_doctor":
      return Promise.resolve("（预览模式：后端未运行，这里是占位文本）");
    case "app_version":
      return Promise.resolve("0.0.0-preview");
    case "open_release_page":
    case "report_bug":
    case "open_logs":
      return Promise.resolve(null);
    default:
      return Promise.resolve(null);
  }
}

const $ = (id) => document.getElementById(id);
const els = {};
let statusTimer = null;
let busy = false;
let mode = "proxy"; // "proxy" 第三方 | "official" 官方

const KEY_LABELS = { deepseek: "DeepSeek API Key", qwen: "DashScope (通义千问) API Key", relay: "中转站 API Key / Token" };

function setMsg(text, kind) {
  els.msg.textContent = text;
  els.msg.className = "msg" + (kind ? " " + kind : "");
}

function setLight(el, state) {
  // state: "green" | "amber" | "red"
  const cls = { green: "g", amber: "a", red: "r" }[state] || "a";
  el.className = "lt " + cls;
}

function setBusy(on) {
  busy = on;
  [els.oneClickBtn, els.stopBtn, els.saveKeyBtn, els.fetchModelsBtn].forEach(
    (b) => b && (b.disabled = on)
  );
}

async function call(cmd, args) {
  return await invoke(cmd, args);
}

async function loadConfig() {
  try {
    const cfg = await call("get_config");
    els.provider.value = cfg.provider || "deepseek";
    els.proxyPort.value = cfg.proxy_port ?? 18991;
    els.sandboxPort.value = cfg.sandbox_port ?? 8990;
    els.relayBase.value = cfg.relay_base_url || "";
    window._keys = cfg.keys || {};
    applyMode(cfg.mode === "official" ? "official" : "proxy");
    reflectProvider();
  } catch (e) {
    setMsg("读取配置失败：" + e, "err");
  }
}

// 应用模式到 UI（不落盘）：切 panel class、分段高亮、hero 按钮文案。
function applyMode(m) {
  mode = m === "official" ? "official" : "proxy";
  els.panel.classList.toggle("mode-official", mode === "official");
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.classList.toggle("active", b.dataset.mode === mode)
  );
  els.oneClickBtn.textContent =
    mode === "official" ? "打开官方 Claude Science ↗" : "⚡ 一键开始";
  updateRelayUI();
}

// 中转站区块可见性：仅「第三方模式 且 provider=relay」时显示。用 JS 计算而非纯 CSS，
// 保证它绝不与官方模式并存（也就不必和 mode-official 抢 CSS 优先级）。
function updateRelayUI() {
  const show = mode !== "official" && els.provider.value === "relay";
  els.panel.classList.toggle("show-relay", show);
}

// 点分段切换：先落盘（切官方时后端会顺带停第三方链路），成功再翻 UI；失败保持旧模式、如实报错。
async function switchMode(m) {
  if (m === mode) return;
  setBusy(true);
  try {
    await call("set_mode", { mode: m });
  } catch (e) {
    // 失败不动 UI（旧模式仍生效），错误提示不被后续覆盖。
    setMsg("切换模式失败：" + e, "err");
    setBusy(false);
    return;
  }
  applyMode(m);
  setBusy(false);
  setMsg(
    mode === "official"
      ? "已切到官方模式：第三方代理/沙箱已停，点上方按钮打开你真实的 Claude Science。"
      : "已切到第三方模式：填 key 后点「一键开始」。"
  );
  await refreshStatus();
}

// 官方模式的主按钮：干净打开真实 Claude Science（后端用 open，不注入环境变量）。
async function openOfficial() {
  setBusy(true);
  setMsg("正在打开官方 Claude Science…");
  try {
    await call("open_official");
    setMsg("已打开官方 Claude Science（走你自己的官方登录与订阅）。", "ok");
  } catch (e) {
    setMsg("打开失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

// hero 按钮按当前模式分派。
async function heroClick() {
  if (mode === "official") {
    await openOfficial();
  } else {
    await oneClick();
  }
}

function reflectProvider() {
  const p = els.provider.value;
  els.keyLabel.textContent = KEY_LABELS[p] || "API Key";
  const masked = (window._keys && window._keys[p]) || "";
  els.keyInput.value = "";
  els.keyInput.placeholder = masked
    ? "已存：" + masked
    : p === "relay"
    ? "粘贴中转站 key / token（只存本地）"
    : "粘贴第三方 key（只存本地）";
  updateRelayUI();
}

function currentSettings() {
  const s = {
    provider: els.provider.value,
    proxy_port: parseInt(els.proxyPort.value, 10) || 18991,
    sandbox_port: parseInt(els.sandboxPort.value, 10) || 8990,
  };
  // 只在中转站模式带 base_url（后端据「有无该字段」决定是否改动已存值）。
  if (els.provider.value === "relay") {
    s.base_url = els.relayBase.value.trim();
  }
  return s;
}

// 保存设置：失败会【抛出】，让调用方（起代理 / 一键登录）中止，
// 不再吞掉错误后拿旧配置继续、还误报成功（修 P1-4）。
async function persistSettings() {
  await call("set_config", { cfg: currentSettings() });
}

// 独立 UI 事件（改 provider / 端口）用的兜底版：失败只提示、不抛，避免未捕获拒绝。
async function persistSettingsSafe() {
  try {
    await persistSettings();
  } catch (e) {
    setMsg("保存设置失败：" + e, "err");
  }
}

async function saveKey() {
  const key = els.keyInput.value.trim();
  if (!key) {
    setMsg("请先粘贴 key。", "err");
    return;
  }
  setBusy(true);
  try {
    const masked = await call("save_provider_key", { provider: els.provider.value, key });
    window._keys[els.provider.value] = masked;
    reflectProvider();
    setMsg("已保存，正在启动代理并验证 key…", "ok");
    await persistSettings();
    // 存了 key 就自动起代理 + 用最小请求真验一次这把 key（不是「代理起来了」就当成功）。
    try {
      const v = await call("verify_key");
      if (v && v.ok) {
        setMsg("已保存，key 有效 ✓ 代理已就绪，点「一键开始」即可。", "ok");
      } else {
        setMsg("已保存，代理已起；但 key 未通过验证：" + ((v && v.hint) || "上游未接受") + " 可仍试「一键开始」。", "err");
      }
    } catch (ve) {
      // 代理没起来（缺依赖/端口占用），或验证请求发不出去（网络/上游不通）。
      setMsg("已保存；但未能验证 key：" + ve, "err");
    }
  } catch (e) {
    setMsg("保存失败：" + e, "err");
  } finally {
    setBusy(false);
    await refreshStatus();
  }
}

// 与后端 is_main_list_model 对齐：会平铺进 Science 选择器主列表的 id。
function isMainListModel(id) {
  for (const fam of ["claude-opus-", "claude-sonnet-", "claude-haiku-"]) {
    if (id.startsWith(fam)) {
      const c = id.charAt(fam.length);
      return c >= "0" && c <= "9";
    }
  }
  return false;
}

function renderModels(models) {
  const box = els.modelList;
  box.innerHTML = "";
  if (!models || !models.length) {
    box.hidden = true;
    return;
  }
  for (const id of models) {
    const d = document.createElement("div");
    d.className = "m" + (isMainListModel(id) ? " main" : "");
    d.textContent = id;
    box.appendChild(d);
  }
  box.hidden = false;
}

// 「获取模型」：把 base_url（+可能的新 key）交后端，起 relay 代理并回源拉中转站可用模型。
async function fetchModels() {
  const base = els.relayBase.value.trim();
  if (!base) {
    setMsg("请先填写中转站地址 base_url。", "err");
    return;
  }
  setBusy(true);
  setMsg("获取模型中：起代理 → 回源拉 /v1/models…");
  try {
    const key = els.keyInput.value.trim(); // 有新 key 就带上；为空则后端沿用已存
    const r = await call("fetch_relay_models", { req: { base_url: base, key } });
    const models = (r && r.models) || [];
    renderModels(models);
    if (key) {
      // 后端已把新 key 落盘；刷新掩码占位、清空输入框。
      window._keys.relay = "•".repeat(Math.max(0, key.length - 4)) + key.slice(-4);
      els.keyInput.value = "";
      reflectProvider();
    }
    setMsg(
      "已获取 " + models.length + " 个模型（加粗的会平铺进选择器，其余在「More models」）。点「一键开始」即可在 Science 里选用。",
      "ok"
    );
    await refreshStatus();
  } catch (e) {
    setMsg("获取模型失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function stopAll() {
  setBusy(true);
  setMsg("停止中…");
  try {
    await call("stop_all");
    setMsg("已停止代理与沙箱。", "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("停止失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function oneClick() {
  setBusy(true);
  setMsg("一键开始：起代理 → 起沙箱 → 探活…");
  try {
    // 「粘贴 key → 直接一键开始」也要能走通：输入框里有新 key 就先存下，
    // 不强制用户先点「保存」（修 P1：oneClick 之前不读/不存输入框，导致无 key 起代理失败）。
    const key = els.keyInput.value.trim();
    if (key) {
      const masked = await call("save_provider_key", { provider: els.provider.value, key });
      window._keys[els.provider.value] = masked;
      els.keyInput.value = "";
      reflectProvider();
    }
    await persistSettings();
    const r = await call("one_click_login");
    // 透传后端据实回传的 msg（区分：已重新打开 / 已用新配置重启 / 沿用原有对话 / 已启动 /
    // 打开失败请手动打开），保证提示不谎报。后端未给 msg 时退回中性兜底。
    setMsg((r.msg || "已就绪，正在打开面板…") + "\n" + (r.url || ""), "ok");
    await refreshStatus();
  } catch (e) {
    setMsg("一键开始失败：" + e, "err");
  } finally {
    setBusy(false);
  }
}

async function openBrowser() {
  try {
    await call("open_url", {});
  } catch (e) {
    setMsg("打开浏览器失败：" + e, "err");
  }
}

async function runDoctor() {
  setMsg("自检中…");
  try {
    const out = await call("run_doctor");
    setMsg(out, out.includes("失败 0") ? "ok" : null);
  } catch (e) {
    setMsg("自检失败：" + e, "err");
  }
}

// 简单 semver 比较：a 是否比 b 新。
function isNewer(a, b) {
  const pa = String(a).split(".").map((n) => parseInt(n, 10) || 0);
  const pb = String(b).split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const x = pa[i] || 0, y = pb[i] || 0;
    if (x !== y) return x > y;
  }
  return false;
}

// 轻量检查更新：查 GitHub 最新 Release 版本号，有新版就提示并打开下载页（不自动装）。
async function checkUpdate() {
  setMsg("检查更新中…");
  let cur = "";
  try { cur = await call("app_version"); } catch (e) {}
  try {
    const resp = await fetch(
      "https://api.github.com/repos/SuperJJ007/CSswitch/releases/latest",
      { headers: { Accept: "application/vnd.github+json" } }
    );
    if (!resp.ok) throw new Error("HTTP " + resp.status);
    const data = await resp.json();
    const latest = (data.tag_name || "").replace(/^v/, "");
    if (!latest) throw new Error("无版本信息");
    if (isNewer(latest, cur)) {
      setMsg("发现新版本 v" + latest + "（当前 v" + cur + "）。正在打开下载页…", "ok");
      try { await call("open_release_page"); } catch (_) {}
    } else {
      setMsg("已是最新版本（v" + cur + "）。", "ok");
    }
  } catch (e) {
    setMsg("无法自动检查更新（多为网络或代理限制）。已打开 Releases 页，请手动查看。", "err");
    try { await call("open_release_page"); } catch (_) {}
  }
}

async function refreshStatus() {
  try {
    const s = await call("status");
    setLight(els.ltProxy, s.proxy);
    setLight(els.ltSandbox, s.sandbox);
    setLight(els.ltUpstream, s.upstream);
    const anyGreen = s.proxy === "green" || s.sandbox === "green";
    els.brandDot.className = "dot" + (s.proxy === "green" ? "" : " amber");
  } catch (e) {
    // 状态探测失败不打断，静默降级为黄灯。
    [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((l) => setLight(l, "amber"));
  }
}

function wire() {
  [
    "provider", "keyLabel", "keyInput", "saveKeyBtn", "proxyPort", "sandboxPort",
    "oneClickBtn", "stopBtn", "ltProxy", "ltSandbox", "ltUpstream",
    "msg", "brandDot", "openBrowserBtn", "doctorBtn", "updateBtn", "verLabel",
    "reportBtn", "logsBtn", "quitBtn", "modeSeg",
    "relayBase", "fetchModelsBtn", "modelList",
  ].forEach((id) => (els[id] = $(id)));
  els.panel = document.querySelector(".panel");

  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.addEventListener("click", () => switchMode(b.dataset.mode))
  );

  els.provider.addEventListener("change", async () => {
    reflectProvider();
    await persistSettingsSafe();
  });
  els.proxyPort.addEventListener("change", persistSettingsSafe);
  els.sandboxPort.addEventListener("change", persistSettingsSafe);
  els.relayBase.addEventListener("change", persistSettingsSafe);
  els.fetchModelsBtn.addEventListener("click", fetchModels);
  els.saveKeyBtn.addEventListener("click", saveKey);
  els.stopBtn.addEventListener("click", stopAll);
  els.oneClickBtn.addEventListener("click", heroClick);
  els.openBrowserBtn.addEventListener("click", openBrowser);
  els.doctorBtn.addEventListener("click", runDoctor);
  els.updateBtn.addEventListener("click", checkUpdate);
  els.reportBtn.addEventListener("click", () =>
    call("report_bug").catch((e) => setMsg("打开反馈页失败：" + e, "err"))
  );
  els.logsBtn.addEventListener("click", () =>
    call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"))
  );
  els.quitBtn.addEventListener("click", () => call("quit_app").catch(() => {}));
}

window.addEventListener("DOMContentLoaded", async () => {
  wire();
  await loadConfig();
  try { els.verLabel.textContent = "v" + (await call("app_version")); } catch (e) {}
  await refreshStatus();
  if (PREVIEW) {
    setMsg("预览模式：仅看界面，按钮不连后端（真实 app 里会连进程管家）。");
  } else {
    statusTimer = setInterval(refreshStatus, 2500);
  }
});
