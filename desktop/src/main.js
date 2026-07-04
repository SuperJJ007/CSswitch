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
      return Promise.resolve({ provider: "deepseek", proxy_port: 18991, sandbox_port: 8990, mode: "proxy", keys: { deepseek: "", qwen: "" } });
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
    // 远程命令 mock
    case "remote_list_profiles":
      return Promise.resolve([]);
    case "remote_check_health":
      return Promise.resolve({ reachable: true, helperInstalled: false, compatible: false, desktopVersion: "0.0.0", platform: null, arch: null, capabilities: [], proxyRunning: false, sandboxRunning: false, lastError: "预览模式", lastCheck: 0 });
    case "remote_status":
      return Promise.resolve({ proxy: "amber", sandbox: "amber", upstream: "amber", remote: true });
    case "remote_doctor":
      return Promise.resolve({ checks: [{ name: "预览模式", ok: false, detail: "后端未运行" }] });
    case "remote_logs":
      return Promise.resolve({ content: "(预览模式)", exists: false });
    default:
      return Promise.resolve(null);
  }
}

const $ = (id) => document.getElementById(id);
const els = {};
let statusTimer = null;
let busy = false;
let mode = "proxy"; // "proxy" 第三方 | "official" 官方

// ---- 远程服务器管理状态 ----
let target = "local";    // "local" | "remote"
let currentProfile = null; // RemoteHostProfile | null
let remoteProfiles = [];   // 缓存的 Profile 列表

const KEY_LABELS = { deepseek: "DeepSeek API Key", qwen: "DashScope (通义千问) API Key" };

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
  [els.oneClickBtn, els.stopBtn, els.saveKeyBtn].forEach((b) => (b.disabled = on));
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
    window._keys = cfg.keys || {};
    reflectProvider();
    applyMode(cfg.mode === "official" ? "official" : "proxy");
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
  els.keyInput.placeholder = masked ? "已存：" + masked : "粘贴第三方 key（只存本地）";
}

function currentSettings() {
  return {
    provider: els.provider.value,
    proxy_port: parseInt(els.proxyPort.value, 10) || 18991,
    sandbox_port: parseInt(els.sandboxPort.value, 10) || 8990,
  };
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
    // 远程模式元素
    "targetSeg", "profileSelect", "manageProfilesBtn", "remoteHealthDot",
    "remoteHealthText", "profileModal", "profileList", "addProfileBtn",
    "closeProfileModal", "profileEditModal", "profileEditTitle",
    "editProfileName", "editProfileHost", "editProfilePort", "editProfileUsername",
    "editProfileAuth", "editProfileKeyPath", "editProfileHelperPath",
    "keyFileGroup", "testProfileBtn", "saveProfileBtn", "cancelProfileEditBtn",
    "profileEditMsg",
  ].forEach((id) => (els[id] = $(id)));
  els.panel = document.querySelector(".panel");

  // 已有的事件
  els.modeSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.addEventListener("click", () => switchMode(b.dataset.mode))
  );

  // 远程模式事件
  els.targetSeg.querySelectorAll(".seg-btn").forEach((b) =>
    b.addEventListener("click", () => switchTarget(b.dataset.target))
  );
  els.profileSelect.addEventListener("change", onProfileChange);
  els.manageProfilesBtn.addEventListener("click", openProfileModal);
  els.addProfileBtn.addEventListener("click", () => { closeProfileModal(); openProfileEdit(null); });
  els.closeProfileModal.addEventListener("click", closeProfileModal);
  els.saveProfileBtn.addEventListener("click", saveProfile);
  els.cancelProfileEditBtn.addEventListener("click", closeProfileEdit);
  els.testProfileBtn.addEventListener("click", testProfileConnection);
  els.editProfileAuth.addEventListener("change", toggleKeyFileGroup);

  // 点击弹窗遮罩关闭
  document.querySelectorAll('.modal-overlay').forEach(ov => {
    ov.addEventListener('click', (e) => { if (e.target === ov) { ov.style.display = 'none'; } });
  });

  els.provider.addEventListener("change", async () => {
    reflectProvider();
    await persistSettingsSafe();
  });
  els.proxyPort.addEventListener("change", persistSettingsSafe);
  els.sandboxPort.addEventListener("change", persistSettingsSafe);
  els.saveKeyBtn.addEventListener("click", saveKey);
  els.stopBtn.addEventListener("click", stopAll);
  els.oneClickBtn.addEventListener("click", heroClick);
  els.openBrowserBtn.addEventListener("click", openBrowser);
  els.doctorBtn.addEventListener("click", () => {
    if (target === 'remote' && currentProfile) {
      setBusy(true); setMsg("远程诊断中…");
      call("remote_doctor", { profile: currentProfile })
        .then(out => { setMsg(typeof out === 'string' ? out : JSON.stringify(out.checks || out, null, 2)); setBusy(false); })
        .catch(e => { setMsg("诊断失败：" + e, "err"); setBusy(false); });
    } else {
      runDoctor();
    }
  });
  els.updateBtn.addEventListener("click", checkUpdate);
  els.reportBtn.addEventListener("click", () =>
    call("report_bug").catch((e) => setMsg("打开反馈页失败：" + e, "err"))
  );
  els.logsBtn.addEventListener("click", () => {
    if (target === 'remote' && currentProfile) {
      call("remote_logs", { profile: currentProfile, name: "proxy", lines: 50 })
        .then(out => setMsg(out && out.content ? out.content : '(日志为空)', 'ok'))
        .catch(e => setMsg("获取日志失败：" + e, "err"));
    } else {
      call("open_logs").catch((e) => setMsg("打开日志失败：" + e, "err"));
    }
  });
  els.quitBtn.addEventListener("click", () => call("quit_app").catch(() => {}));
}

// =========================================================================
// 远程服务器管理
// =========================================================================

/// 切换本地/远程模式。
async function switchTarget(t) {
  if (t === target) return;
  target = t;
  // 更新 UI 类
  const panel = document.querySelector('.panel');
  if (t === 'remote') {
    panel.classList.add('target-remote');
  } else {
    panel.classList.remove('target-remote');
  }
  // 更新分段按钮
  document.querySelectorAll('#targetSeg .seg-btn').forEach(b =>
    b.classList.toggle('active', b.dataset.target === t)
  );
  if (t === 'remote') {
    await loadRemoteProfiles();
    setMsg('已切换到远程模式。请选择服务器。');
  } else {
    setMsg('已切换到本地模式。');
  }
  await refreshStatus();
}

/// 加载远程 Profile 列表。
async function loadRemoteProfiles() {
  try {
    remoteProfiles = await call("remote_list_profiles");
    const sel = $('#profileSelect');
    sel.innerHTML = '<option value="">-- 选择服务器 --</option>' +
      remoteProfiles.map(p =>
        `<option value="${p.id}">${p.name} (${p.username}@${p.host}:${p.port})</option>`
      ).join('');
    // 恢复之前选择的
    if (currentProfile && remoteProfiles.find(p => p.id === currentProfile.id)) {
      sel.value = currentProfile.id;
    }
    updateRemoteHealthUI();
  } catch (e) {
    setMsg("加载服务器列表失败：" + e, "err");
  }
}

/// Profile 变更时。
async function onProfileChange() {
  const id = $('#profileSelect').value;
  currentProfile = remoteProfiles.find(p => p.id === id) || null;
  if (currentProfile) {
    setMsg(`已选择 ${currentProfile.name}，正在检查连接…`, null);
    await checkRemoteHealth();
  } else {
    updateRemoteHealthUI();
    setMsg('请选择远程服务器。');
  }
}

/// 检查远程健康状态。
async function checkRemoteHealth() {
  if (!currentProfile) return;
  const dot = $('#remoteHealthDot');
  const txt = $('#remoteHealthText');
  dot.className = 'lt a pulsing';
  txt.textContent = '连接中…';
  try {
    const health = await call("remote_check_health", { profile: currentProfile });
    if (health.reachable && health.helperInstalled && health.compatible) {
      dot.className = 'lt g';
      txt.textContent = `已连接 | ${health.platform || '?'} ${health.arch || '?'} | Helper ${health.helperVersion || '?'}`;
    } else if (health.reachable && !health.helperInstalled) {
      dot.className = 'lt a';
      txt.textContent = '已连接，Helper 未安装。点击下方「安装 Helper」。';
    } else if (health.reachable && !health.compatible) {
      dot.className = 'lt a';
      txt.textContent = `Helper 版本不兼容：${health.lastError || '请升级'}`;
    } else {
      dot.className = 'lt r';
      txt.textContent = health.lastError || '连接失败';
    }
    // 如果远程代理/沙箱在运行，更新状态灯
    if (health.proxyRunning || health.sandboxRunning) {
      setLight(els.ltProxy, health.proxyRunning ? 'green' : 'amber');
      setLight(els.ltSandbox, health.sandboxRunning ? 'green' : 'amber');
      setLight(els.ltUpstream, 'green');
    }
  } catch (e) {
    dot.className = 'lt r';
    txt.textContent = '检查失败：' + e;
  }
}

/// 更新远程健康 UI（初始/断开状态）。
function updateRemoteHealthUI() {
  const dot = $('#remoteHealthDot');
  const txt = $('#remoteHealthText');
  if (currentProfile) {
    dot.className = 'lt a';
    txt.textContent = `已选：${currentProfile.name}`;
  } else {
    dot.className = 'lt a';
    txt.textContent = '未连接';
  }
}

/// 安装远程 Helper。
async function installRemoteHelper() {
  if (!currentProfile) { setMsg('请先选择服务器', 'err'); return; }
  setBusy(true); setMsg('正在安装 Helper，可能需要 1-2 分钟…');
  try {
    const health = await call("remote_install_helper", { profile: currentProfile });
    if (health.helperInstalled) {
      setMsg(`Helper ${health.helperVersion} 安装成功！`, 'ok');
      await checkRemoteHealth();
    } else {
      setMsg('安装失败：' + (health.lastError || '未知错误'), 'err');
    }
  } catch (e) { setMsg('安装失败：' + e, 'err'); }
  finally { setBusy(false); }
}

// =========================================================================
// Profile 管理弹窗
// =========================================================================

/// 打开 Profile 管理弹窗。
async function openProfileModal() {
  // 确保 profiles 已加载
  try { await loadRemoteProfiles(); } catch(e) { /* 忽略加载错误 */ }
  const modal = $('#profileModal');
  const list = $('#profileList');
  if (!modal || !list) { console.error('profileModal/profileList not found'); return; }
  // 渲染列表
  list.innerHTML = remoteProfiles.length === 0
    ? '<div class="hint">暂无服务器。点击「+ 添加」。</div>'
    : remoteProfiles.map(p => `
      <div class="profile-item">
        <div>
          <div class="pi-name">${escHtml(p.name)}</div>
          <div class="pi-detail">${escHtml(p.username)}@${escHtml(p.host)}:${p.port} · ${p.authMethod.type === 'SshAgent' ? 'SSH Agent' : 'KeyFile'}</div>
        </div>
        <div class="pi-actions">
          <span class="pi-act" data-action="edit" data-id="${p.id}">编辑</span>
          <span class="pi-act del" data-action="delete" data-id="${p.id}">删除</span>
        </div>
      </div>
    `).join('');
  // 绑定事件
  list.querySelectorAll('.pi-act').forEach(el => {
    el.addEventListener('click', async () => {
      const id = el.dataset.id;
      if (el.dataset.action === 'edit') {
        openProfileEdit(id);
      } else if (el.dataset.action === 'delete') {
        if (confirm('确定删除此服务器配置？')) {
          await call("remote_delete_profile", { id });
          await loadRemoteProfiles();
          if (currentProfile && currentProfile.id === id) currentProfile = null;
          openProfileModal(); // 刷新列表
        }
      }
    });
  });
  modal.style.display = 'flex';
}

/// 关闭 Profile 管理弹窗。
function closeProfileModal() {
  $('#profileModal').style.display = 'none';
}

/// 打开 Profile 编辑弹窗（新增或编辑）。
function openProfileEdit(id) {
  const modal = $('#profileEditModal');
  const profile = id ? remoteProfiles.find(p => p.id === id) : null;
  $('#profileEditTitle').textContent = profile ? '编辑服务器' : '添加服务器';
  $('#editProfileName').value = profile ? profile.name : '';
  $('#editProfileHost').value = profile ? profile.host : '';
  $('#editProfilePort').value = profile ? profile.port : 22;
  $('#editProfileUsername').value = profile ? profile.username : '';
  $('#editProfileAuth').value = profile
    ? (profile.authMethod.type === 'SshAgent' ? 'ssh_agent' : 'key_file')
    : 'ssh_agent';
  $('#editProfileKeyPath').value = (profile && profile.authMethod.path) ? profile.authMethod.path : '~/.ssh/id_ed25519';
  $('#editProfileHelperPath').value = profile ? profile.helperPath : '~/.csswitch/bin/csswitch-helper';
  $('#profileEditMsg').textContent = '';
  // 切换认证方式显示
  toggleKeyFileGroup();
  // 存储当前编辑的 ID
  modal.dataset.editId = id || '';
  modal.style.display = 'flex';
}

/// 关闭编辑弹窗。
function closeProfileEdit() {
  $('#profileEditModal').style.display = 'none';
}

/// 认证方式切换时显示/隐藏密钥路径。
function toggleKeyFileGroup() {
  $('#keyFileGroup').style.display = $('#editProfileAuth').value === 'key_file' ? '' : 'none';
}

/// 保存 Profile。
async function saveProfile() {
  const id = $('#profileEditModal').dataset.editId || crypto.randomUUID ? crypto.randomUUID() : 'p_' + Date.now();
  const authType = $('#editProfileAuth').value;
  const profile = {
    id: id,
    name: $('#editProfileName').value.trim() || '未命名',
    host: $('#editProfileHost').value.trim(),
    port: parseInt($('#editProfilePort').value) || 22,
    username: $('#editProfileUsername').value.trim(),
    authMethod: authType === 'key_file'
      ? { type: 'KeyFile', path: $('#editProfileKeyPath').value.trim() }
      : { type: 'SshAgent' },
    helperPath: $('#editProfileHelperPath').value.trim() || '~/.csswitch/bin/csswitch-helper',
  };
  if (!profile.host || !profile.username) {
    $('#profileEditMsg').textContent = '服务器地址和用户名不能为空。';
    $('#profileEditMsg').className = 'msg err';
    return;
  }
  try {
    await call("remote_save_profile", { profile });
    await loadRemoteProfiles();
    closeProfileEdit();
    openProfileModal(); // 刷新管理列表
  } catch (e) {
    $('#profileEditMsg').textContent = '保存失败：' + e;
    $('#profileEditMsg').className = 'msg err';
  }
}

/// 测试连接。
async function testProfileConnection() {
  const btn = $('#testProfileBtn');
  btn.disabled = true;
  $('#profileEditMsg').textContent = '正在测试连接…';
  $('#profileEditMsg').className = 'msg';
  try {
    // 构建临时 Profile 用于测试
    const authType = $('#editProfileAuth').value;
    const tmpProfile = {
      id: '_test_',
      name: 'test',
      host: $('#editProfileHost').value.trim(),
      port: parseInt($('#editProfilePort').value) || 22,
      username: $('#editProfileUsername').value.trim(),
      authMethod: authType === 'key_file'
        ? { type: 'KeyFile', path: $('#editProfileKeyPath').value.trim() }
        : { type: 'SshAgent' },
      helperPath: $('#editProfileHelperPath').value.trim() || '~/.csswitch/bin/csswitch-helper',
    };
    const health = await call("remote_check_health", { profile: tmpProfile });
    if (health.reachable) {
      let msg = `✅ 连接成功！平台：${health.platform || '?'} ${health.arch || '?'}`;
      if (health.helperInstalled) {
        msg += ` | Helper：${health.helperVersion || '?'}`;
      } else {
        msg += ' | Helper 未安装（保存后可在主面板安装）';
      }
      $('#profileEditMsg').textContent = msg;
      $('#profileEditMsg').className = 'msg ok';
    } else {
      $('#profileEditMsg').textContent = `❌ ${health.lastError || '连接失败'}`;
      $('#profileEditMsg').className = 'msg err';
    }
  } catch (e) {
    $('#profileEditMsg').textContent = '❌ ' + e;
    $('#profileEditMsg').className = 'msg err';
  } finally {
    btn.disabled = false;
  }
}

/// HTML 转义。
function escHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// =========================================================================
// 重写关键操作以支持远程模式分派
// =========================================================================

/// 保存 Key（本地或远程）。
const _saveKeyOrig = saveKey;
saveKey = async function() {
  if (target === 'remote' && currentProfile) {
    const key = els.keyInput.value.trim();
    if (!key) { setMsg("请先粘贴 key。", "err"); return; }
    setBusy(true);
    try {
      const masked = await call("remote_save_provider_key", { profile: currentProfile, provider: els.provider.value, key });
      if (!window._keys) window._keys = {};
      window._keys[els.provider.value] = masked;
      reflectProvider();
      setMsg("已保存到远程服务器。", "ok");
    } catch (e) { setMsg("保存失败：" + e, "err"); }
    finally { setBusy(false); await refreshStatus(); }
    return;
  }
  return _saveKeyOrig();
};

/// 一键开始（本地或远程）。
const _oneClickOrig = oneClick;
oneClick = async function() {
  if (target === 'remote' && currentProfile) {
    setBusy(true); setMsg('远程一键开始：保存 Key → 起代理…');
    try {
      const key = els.keyInput.value.trim();
      const r = await call("remote_one_click", {
        profile: currentProfile,
        provider: els.provider.value,
        key: key,
        proxyPort: parseInt(els.proxyPort.value) || 18991,
        sandboxPort: parseInt(els.sandboxPort.value) || 8990,
      });
      setMsg("远程代理已启动！端口：" + (r && r.port) + "。请在浏览器中访问 Science。", "ok");
      await refreshStatus();
    } catch (e) { setMsg("远程一键开始失败：" + e, "err"); }
    finally { setBusy(false); }
    return;
  }
  return _oneClickOrig();
};

/// 全部停止（本地或远程）。
const _stopAllOrig = stopAll;
stopAll = async function() {
  if (target === 'remote' && currentProfile) {
    setBusy(true); setMsg('停止远程服务…');
    try {
      await call("remote_stop_proxy", { profile: currentProfile });
      setMsg("远程代理已停止。", "ok");
      await refreshStatus();
    } catch (e) { setMsg("停止失败：" + e, "err"); }
    finally { setBusy(false); }
    return;
  }
  return _stopAllOrig();
};

/// 刷新状态（本地或远程）。
const _refreshStatusOrig = refreshStatus;
refreshStatus = async function() {
  if (target === 'remote' && currentProfile) {
    try {
      const s = await call("remote_status", { profile: currentProfile });
      setLight(els.ltProxy, s.proxy);
      setLight(els.ltSandbox, s.sandbox);
      setLight(els.ltUpstream, s.upstream);
      const anyGreen = s.proxy === "green" || s.sandbox === "green";
      els.brandDot.className = "dot" + (s.proxy === "green" ? "" : " amber");
    } catch (e) {
      [els.ltProxy, els.ltSandbox, els.ltUpstream].forEach((l) => setLight(l, "amber"));
    }
    return;
  }
  return _refreshStatusOrig();
};

/// Hero 按钮（官方/本地/远程分派）。
const _heroClickOrig = heroClick;
heroClick = async function() {
  if (target === 'remote') {
    if (mode === 'official') {
      setMsg('远程模式不支持官方 Claude。请用第三方模型。', 'err');
    } else {
      await oneClick();
    }
    return;
  }
  return _heroClickOrig();
};

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
