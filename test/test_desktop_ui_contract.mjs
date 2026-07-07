import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";

const html = readFileSync(new URL("../desktop/src/index.html", import.meta.url), "utf8");
const main = readFileSync(new URL("../desktop/src/main.js", import.meta.url), "utf8");
const remoteCommands = readFileSync(new URL("../desktop/src-tauri/src/remote_commands.rs", import.meta.url), "utf8");
const remoteSsh = readFileSync(new URL("../desktop/src-tauri/src/remote/ssh.rs", import.meta.url), "utf8");
const remoteWsl = readFileSync(new URL("../desktop/src-tauri/src/remote/wsl.rs", import.meta.url), "utf8");
const helperCommands = readFileSync(new URL("../desktop/src-tauri/src/cli/commands.rs", import.meta.url), "utf8");
const libTauri = readFileSync(new URL("../desktop/src-tauri/src/lib_tauri.rs", import.meta.url), "utf8");
const tauriConf = readFileSync(new URL("../desktop/src-tauri/tauri.conf.json", import.meta.url), "utf8");
const buildWorkflow = readFileSync(new URL("../.github/workflows/build.yml", import.meta.url), "utf8");
const tauriBuildRs = readFileSync(new URL("../desktop/src-tauri/build.rs", import.meta.url), "utf8");
const crossTomlUrl = new URL("../desktop/src-tauri/Cross.toml", import.meta.url);
const crossToml = existsSync(crossTomlUrl) ? readFileSync(crossTomlUrl, "utf8") : "";

function remoteStartProxyBody() {
  const m = remoteCommands.match(/pub fn remote_start_proxy[\s\S]*?\n}\n\n\/\/\/ 停止远程代理/);
  assert.ok(m, "remote_start_proxy body should be discoverable");
  return m[0];
}

function workflowJob(name) {
  const m = buildWorkflow.match(new RegExp(`\\n  ${name}:\\n[\\s\\S]*?(?=\\n  [a-zA-Z0-9_-]+:\\n|\\n$)`));
  assert.ok(m, `${name} job should exist`);
  return m[0];
}

function frontendFunctionBody(name) {
  const marker = `async function ${name}(`;
  const start = main.indexOf(marker);
  assert.notEqual(start, -1, `${name} should exist`);
  const braceStart = main.indexOf("{", start);
  assert.notEqual(braceStart, -1, `${name} should have a body`);
  let depth = 0;
  for (let i = braceStart; i < main.length; i += 1) {
    if (main[i] === "{") depth += 1;
    if (main[i] === "}") {
      depth -= 1;
      if (depth === 0) return main.slice(start, i + 1);
    }
  }
  assert.fail(`${name} body should close`);
}

test("desktop profile UI script matches the v2 profile HTML", () => {
  assert.match(html, /id="profileList"/);
  assert.match(html, /id="newBtn"/);
  assert.match(html, /id="wizSec"/);

  assert.doesNotMatch(main, /els\.(provider|keyInput|saveKeyBtn)\b/);
  assert.doesNotMatch(main, /save_provider_key/);

  for (const command of [
    "create_profile",
    "update_profile_metadata",
    "update_profile_connection",
    "clear_profile_key",
    "delete_profile",
    "set_active_profile",
  ]) {
    assert.match(main, new RegExp(`["']${command}["']`));
  }

  assert.match(main, /newBtn\.addEventListener\(["']click["']/);
});

test("remote server modal uses its own list instead of the local profile list", () => {
  assert.match(html, /id="remoteProfileList"/);
  assert.match(main, /getElementById\(["']remoteProfileList["']\)/);
  assert.doesNotMatch(main, /const\s+list\s*=\s*document\.getElementById\(["']profileList["']\)/);
});

test("remote start uploads the active local profile before starting helper proxy", () => {
  assert.match(remoteCommands, /remote_active_config_for_start/);
  assert.match(remoteCommands, /config::load_from\(&config::default_dir\(\)\)/);
  assert.match(remoteCommands, /"config"\.to_string\(\),\s*"set"\.to_string\(\)/s);
  assert.match(remoteCommands, /serde_json::to_string\(&remote_cfg\)/);
});

test("remote start stops stale helper proxy before starting with the new secret", () => {
  const body = remoteStartProxyBody();
  assert.match(
    body,
    /stop_remote_proxy\(&profile\)[\s\S]*"proxy"\.to_string\(\),\s*"start"\.to_string\(\)/,
  );
  assert.ok(
    body.indexOf("stop_remote_proxy(&profile)") < body.indexOf('"config".to_string()'),
    "remote_start_proxy must stop the old configured proxy before writing the new port",
  );
});

test("remote one-click frontend calls the full remote stack command", () => {
  const body = main.match(/async function remoteOneClick\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "remoteOneClick body should be discoverable");
  assert.match(body[0], /call\(["']remote_one_click["']/);
  assert.match(body[0], /proxyPort/);
  assert.match(body[0], /sandboxPort/);
  assert.doesNotMatch(body[0], /call\(["']remote_start_proxy["']/);
});

test("remote profile test prepares helper instead of only checking health", () => {
  const body = main.match(/async function testProfileConnection\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "testProfileConnection body should be discoverable");
  assert.match(body[0], /call\(["']remote_prepare_helper["']/);
  assert.doesNotMatch(body[0], /call\(["']remote_check_health["']/);
});

test("remote profile save prepares helper before saving the server", () => {
  const body = main.match(/async function saveProfile\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "saveProfile body should be discoverable");
  assert.match(body[0], /call\(["']remote_prepare_helper["']/);
  assert.match(body[0], /call\(["']remote_save_profile["']/);
  assert.ok(
    body[0].indexOf('call("remote_prepare_helper"') < body[0].indexOf('call("remote_save_profile"'),
    "save should prepare helper before persisting the remote server",
  );
});

test("remote password login uses a transient password instead of requiring system storage", () => {
  assert.match(html, /id="passwordGroup"/);
  assert.match(html, /<input(?=[^>]*id="editProfilePassword")(?=[^>]*type="password")[^>]*>/);
  assert.match(html, /id="editProfileRememberPassword"/);
  assert.match(html, /id="keyFileGroup"/);

  assert.match(main, /"editProfilePassword"/);
  assert.match(main, /"editProfileRememberPassword"/);
  assert.match(main, /"passwordGroup"/);
  assert.match(main, /function toggleAuthFields\(\)/);
  assert.match(main, /passwordGroup\.style\.display\s*=\s*method === "password"/);
  assert.match(main, /keyFileGroup\.style\.display\s*=\s*method === "key_file"/);
  assert.match(main, /function withTransientPassword\(/);
  assert.match(main, /function stripTransientPassword\(/);
  assert.match(main, /function rememberPasswordAfterConnection\(/);
  assert.match(main, /editProfilePassword\.value\s*=\s*""/);
  assert.doesNotMatch(main, /authMethod,\s*password/i);
  assert.doesNotMatch(main, /password:\s*els\.editProfilePassword/);

  const saveBody = frontendFunctionBody("saveProfile");
  const testBody = frontendFunctionBody("testProfileConnection");
  const rememberBody = main.match(/async function rememberPasswordAfterConnection\([\s\S]*?\n\}/);
  assert.ok(rememberBody, "rememberPasswordAfterConnection body should be discoverable");
  assert.match(saveBody, /withTransientPassword\(profile,\s*loginSecret\)/);
  assert.match(saveBody, /stripTransientPassword\(profileForConnection\)/);
  assert.match(testBody, /withTransientPassword\(profile,\s*loginSecret\)/);
  assert.match(saveBody, /rememberPasswordAfterConnection\(profile\.id,\s*profile\.authMethod,\s*loginSecret\)/);
  assert.match(testBody, /rememberPasswordAfterConnection\(profile\.id,\s*profile\.authMethod,\s*loginSecret\)/);
  assert.match(rememberBody[0], /saveRemoteLoginSecret/);
  assert.match(rememberBody[0], /deleteRemoteLoginSecret/);
  assert.ok(
    saveBody.indexOf('call("remote_prepare_helper"') < saveBody.indexOf("rememberPasswordAfterConnection"),
    "password should be remembered only after SSH connection succeeds",
  );
  assert.ok(
    testBody.indexOf('call("remote_prepare_helper"') < testBody.indexOf("rememberPasswordAfterConnection"),
    "test connection should remember the password only after SSH connection succeeds",
  );
});

test("remote one-click does not repeat helper preparation", () => {
  const body = main.match(/async function remoteOneClick\(\) \{[\s\S]*?\n\}/);
  assert.ok(body, "remoteOneClick body should be discoverable");
  assert.doesNotMatch(body[0], /remote_prepare_helper/);
});

test("backend exposes explicit remote helper preparation command", () => {
  assert.match(remoteCommands, /pub fn remote_prepare_helper/);
  assert.match(main, /case "remote_prepare_helper"/);
});

test("remote helper preparation installs only when health is not ready and falls back to bundled upload", () => {
  const m = remoteCommands.match(/pub fn remote_prepare_helper[\s\S]*?\n}\n\n\/\/ ============================================================================/);
  assert.ok(m, "remote_prepare_helper body should be discoverable");
  const body = m[0];
  assert.match(body, /helper_ready_for_profile/);
  assert.match(body, /install_or_update_helper/);
});

test("remote helper preparation uses target-specific install ordering", () => {
  const start = remoteCommands.indexOf("fn install_or_update_helper");
  assert.notEqual(start, -1, "install_or_update_helper body should be discoverable");
  const end = remoteCommands.indexOf("\n\n/// 安装/升级远程 Helper", start);
  assert.notEqual(end, -1, "install_or_update_helper body should end before remote_install_helper");
  const body = remoteCommands.slice(start, end);

  assert.match(body, /RemoteTargetKind::Wsl/);
  assert.match(body, /RemoteTargetKind::Ssh/);

  const wslBranch = body.match(/RemoteTargetKind::Wsl[\s\S]*?RemoteTargetKind::Ssh/)[0];
  assert.ok(
    wslBranch.indexOf("install_helper_from_bundle") < wslBranch.indexOf("install_helper_from_github"),
    "WSL should prefer bundled upload before GitHub download",
  );

  const sshBranch = body.match(/RemoteTargetKind::Ssh[\s\S]*?$/)[0];
  assert.ok(
    sshBranch.indexOf("install_helper_from_github") < sshBranch.indexOf("install_helper_from_bundle"),
    "SSH should prefer GitHub download before bundled upload",
  );
});

test("remote bundled helper upload uses SSH stdin and installs atomically", () => {
  assert.match(remoteSsh, /pub fn install_helper_from_stdin/);
  assert.match(remoteSsh, /stdin\(Stdio::piped\(\)\)/);
  assert.match(remoteSsh, /cat > "\$TMP"/);
  assert.match(remoteSsh, /chmod \+x "\$TMP"/);
  assert.match(remoteSsh, /mv "\$TMP" "\$HELPER_PATH"/);
});

test("remote github helper installer extracts browser download url after matching asset name", () => {
  assert.match(remoteSsh, /releases\/tags\/v\{helper_version\}/);
  assert.doesNotMatch(remoteSsh, /releases\/latest/);
  assert.match(remoteSsh, /BINARY_NAME="csswitch-helper-\$\{\{OS\}\}-\$\{\{ARCH\}\}"/);
  assert.match(remoteSsh, /browser_download_url/);
  assert.match(remoteSsh, /awk -v name=/);
});

test("wsl github helper installer downloads release asset instead of only checking status", () => {
  assert.match(remoteWsl, /API_URL="https:\/\/api\.github\.com\/repos\/\{repo\}\/releases\/tags\/v\{helper_version\}"/);
  assert.match(remoteWsl, /BINARY_NAME="csswitch-helper-\$\{\{OS\}\}-\$\{\{ARCH\}\}"/);
  assert.match(remoteWsl, /browser_download_url/);
  const installer = remoteWsl.match(/pub fn run_helper_install[\s\S]*?\n}\n\npub fn install_helper_from_stdin/);
  assert.ok(installer, "WSL helper installer should be discoverable");
  assert.doesNotMatch(installer[0], /Helper not installed/);
});

test("helper release repo is detected without a static default", () => {
  assert.match(remoteSsh, /CSSWITCH_HELPER_RELEASE_REPO/);
  assert.doesNotMatch(remoteSsh, /const\s+HELPER_RELEASE_REPO\s*:/);
  assert.doesNotMatch(remoteSsh, /"bfzha\/CSswitch"/);
  assert.match(remoteSsh, /resolve_helper_release_repo_from/);
  assert.match(remoteSsh, /option_env!\("GITHUB_REPOSITORY"\)/);
  assert.match(remoteSsh, /git_origin_remote/);
  assert.match(remoteSsh, /helper_release_repo_unknown/);
});

test("saving remote profile fails before persisting when helper preparation fails", () => {
  const body = frontendFunctionBody("saveProfile");
  assert.ok(
    body.indexOf('call("remote_prepare_helper"') < body.indexOf('call("remote_save_profile"'),
    "helper preparation must happen before profile persistence",
  );
});

test("manual helper install command reuses target-specific install ordering", () => {
  const m = remoteCommands.match(/pub fn remote_install_helper[\s\S]*?\n}\n\n#\[tauri::command\]\npub fn remote_prepare_helper/);
  assert.ok(m, "remote_install_helper body should be discoverable");
  assert.match(m[0], /app: tauri::AppHandle/);
  assert.match(m[0], /detect_remote_platform/);
  assert.match(m[0], /install_or_update_helper\(&app, &profile, &arch\)/);
});

test("github helper installers avoid latest so helper version matches desktop", () => {
  assert.doesNotMatch(remoteSsh, /releases\/latest/);
  assert.doesNotMatch(remoteWsl, /releases\/latest/);
  assert.match(remoteSsh, /releases\/tags\/v\{helper_version\}/);
  assert.match(remoteWsl, /releases\/tags\/v\{helper_version\}/);
});

test("desktop bundle and release workflow provide linux helper assets for upload fallback", () => {
  assert.match(tauriConf, /helper-assets/);
  assert.match(buildWorkflow, /csswitch-helper-linux-\$\{\{ matrix\.asset_arch \}\}/);
  assert.match(buildWorkflow, /helper-assets/);
});

test("desktop release workflow uses the Tauri v2 bundles argument", () => {
  assert.doesNotMatch(buildWorkflow, /--bundler\b/);
  assert.match(buildWorkflow, /npx tauri build --target \$\{\{ matrix\.target \}\} --bundles nsis/);
  assert.match(buildWorkflow, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
});

test("desktop workflow publishes Windows installers for x64 and arm64", () => {
  const body = workflowJob("build-windows");
  assert.match(body, /fail-fast: false/);
  assert.match(body, /target: x86_64-pc-windows-msvc[\s\S]*artifact: CSSwitch-Windows-x64/);
  assert.match(body, /target: aarch64-pc-windows-msvc[\s\S]*artifact: CSSwitch-Windows-arm64/);
  assert.match(body, /targets: \$\{\{ matrix\.target \}\}/);
  assert.match(body, /npx tauri build --target \$\{\{ matrix\.target \}\} --bundles nsis/);
  assert.match(body, /name: \$\{\{ matrix\.artifact \}\}/);
  assert.match(body, /target\/\$\{\{ matrix\.target \}\}\/release\/bundle\/nsis\/\*\.exe/);
});

test("desktop workflow keeps macOS packaging aligned with upstream arm64 DMG releases", () => {
  const body = workflowJob("build-macos");
  assert.match(body, /runs-on: macos-15/);
  assert.match(body, /targets: aarch64-apple-darwin/);
  assert.match(body, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
  assert.match(body, /name: CSSwitch-macOS-arm64/);
  assert.match(body, /target\/aarch64-apple-darwin\/release\/bundle\/dmg\/\*\.dmg/);
  assert.doesNotMatch(body, /macos-15-intel/);
  assert.doesNotMatch(body, /x86_64-apple-darwin/);
  assert.doesNotMatch(body, /universal-apple-darwin/);
});

test("release job uploads all public release assets", () => {
  const body = workflowJob("release");
  assert.match(body, /CSSwitch-Windows-\*\/\*\.exe/);
  assert.match(body, /CSSwitch-macOS-arm64\/\*\.dmg/);
  assert.match(body, /csswitch-helper-\*\/\*/);
});

test("release job prefers curated release notes when present", () => {
  const body = workflowJob("release");
  assert.match(body, /Resolve Release Notes/);
  assert.match(body, /docs\/release-notes\/\$\{tag\}\.md/);
  assert.match(body, /docs\/release-notes\/\$\{version\}\.md/);
  assert.match(body, /body_path: \$\{\{ steps\.release_notes\.outputs\.path \}\}/);
  assert.match(body, /generate_release_notes: true/);
});

test("linux helper release workflow uses cross for musl target builds", () => {
  const body = workflowJob("build-helper");
  assert.match(body, /fail-fast: false/);
  assert.match(body, /taiki-e\/install-action@v2/);
  assert.match(body, /tool: cross/);
  assert.match(body, /CSSWITCH_BUNDLED_PROXY_DIR: \$\{\{ github\.workspace \}\}\/proxy/);
  assert.match(body, /cross build --bin csswitch-helper --no-default-features --release --target \$\{\{ matrix\.target \}\}/);
  assert.doesNotMatch(body, /cargo build --bin csswitch-helper --no-default-features --release --target \$\{\{ matrix\.target \}\}/);
  assert.doesNotMatch(body, /apt-get install -y musl-tools/);
});

test("linux helper cross build mounts the bundled proxy directory into the container", () => {
  assert.match(crossToml, /\[build\.env\]/);
  assert.match(crossToml, /volumes\s*=\s*\[[\s\S]*"CSSWITCH_BUNDLED_PROXY_DIR"[\s\S]*\]/);
  assert.match(crossToml, /passthrough\s*=\s*\[[\s\S]*"CSSWITCH_BUNDLED_PROXY_DIR"[\s\S]*\]/);
  assert.match(tauriBuildRs, /CSSWITCH_BUNDLED_PROXY_DIR/);
});

test("tauri build script validates bundled proxy resources with a clear error", () => {
  assert.match(tauriBuildRs, /fn require_bundled_proxy_file/);
  assert.match(tauriBuildRs, /csswitch_proxy\.py/);
  assert.match(tauriBuildRs, /dsml_shim\.py/);
  assert.match(tauriBuildRs, /\.is_file\(\)/);
  assert.match(tauriBuildRs, /panic!\([\s\S]*CSSWITCH_BUNDLED_PROXY_DIR/);
});

test("linux test workflow installs Tauri system dependencies before cargo tests", () => {
  const body = workflowJob("test");
  assert.match(body, /Install Linux desktop dependencies/);
  assert.ok(
    body.indexOf("Install Linux desktop dependencies") < body.indexOf("Run Tests"),
    "Linux desktop dependencies should be installed before Rust tests compile Tauri crates",
  );
  for (const pkg of [
    "libwebkit2gtk-4.1-dev",
    "build-essential",
    "curl",
    "wget",
    "file",
    "libxdo-dev",
    "libssl-dev",
    "libayatana-appindicator3-dev",
    "librsvg2-dev",
  ]) {
    assert.match(body, new RegExp(pkg.replaceAll(".", "\\.")));
  }
});

test("macOS desktop builds bundle the linux helper assets too", () => {
  const body = workflowJob("build-macos");
  assert.match(body, /needs: build-helper/);
  assert.match(body, /Download Linux Helper Assets/);
  assert.match(body, /pattern: csswitch-helper-linux-\*/);
  assert.match(body, /path: desktop\/src-tauri\/helper-assets/);
  assert.match(body, /npx tauri build --target aarch64-apple-darwin --bundles dmg/);
  assert.match(buildWorkflow, /CSSwitch-macOS-arm64/);
});

test("macOS one-click command keeps app and state names available under cfg", () => {
  const m = libTauri.match(/fn one_click_login\([\s\S]*?\n}\n\n\/\/\/ 从/);
  assert.ok(m, "one_click_login body should be discoverable");
  const body = m[0];
  assert.match(body, /app: tauri::AppHandle/);
  assert.match(body, /state: State<'_, Mutex<AppState>>/);
  assert.doesNotMatch(body, /_app: tauri::AppHandle/);
  assert.doesNotMatch(body, /_state: State<'_, Mutex<AppState>>/);
  assert.match(body, /ensure_proxy\(&app, &state, &lifecycle\)/);
  assert.match(body, /stop_sandbox_inner\(&app, &mut st\)/);
  assert.match(body, /asset_root\(&app\)/);
});

test("macOS sandbox stopper keeps app handle name available under cfg", () => {
  const m = libTauri.match(/fn stop_sandbox_inner\([\s\S]*?\n}\n\n\/\/ ----------/);
  assert.ok(m, "stop_sandbox_inner body should be discoverable");
  const body = m[0];
  assert.match(body, /app: &tauri::AppHandle/);
  assert.doesNotMatch(body, /_app: &tauri::AppHandle/);
  assert.match(body, /asset_root\(app\)/);
});

test("remote one-click backend starts proxy and sandbox and returns access info", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.match(body, /remote_active_config_for_start/);
  assert.match(body, /stop_remote_proxy\(&profile\)[\s\S]*"proxy"\.to_string\(\),\s*"start"\.to_string\(\)/);
  assert.match(body, /stop_remote_sandbox\(&profile\)[\s\S]*"sandbox"\.to_string\(\),\s*"start"\.to_string\(\)/);
  assert.match(body, /stop_remote_proxy\(&profile\)/);
  assert.ok(
    body.indexOf("stop_remote_sandbox(&profile)") < body.indexOf('"config".to_string()'),
    "remote_one_click must stop the old sandbox before writing new ports",
  );
  assert.ok(
    body.indexOf("stop_remote_proxy(&profile)") < body.indexOf('"config".to_string()'),
    "remote_one_click must stop the old proxy before writing new ports",
  );
  assert.match(body, /proxy_url/);
  assert.match(body, /tunnel_hint/);
  assert.match(body, /local_url/);
});

test("remote one-click keeps the requested proxy port instead of drifting", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.doesNotMatch(body, /for candidate_proxy_port in proxy_port\.\.=proxy_port\.saturating_add\(20\)/);
  assert.doesNotMatch(body, /selected_proxy_port/);
  assert.match(body, /"proxy_port": proxy_port/);
  assert.match(body, /启动远程代理失败/);
});

test("remote one-click returns the fresh Science URL from sandbox start", () => {
  const m = remoteCommands.match(/pub fn remote_one_click[\s\S]*?\n}\n\n\/\/ ==========================================================================/);
  assert.ok(m, "remote_one_click body should be discoverable");
  const body = m[0];
  assert.match(body, /sandbox_result\["url"\]\s*\.as_str\(\)/);
  assert.match(body, /"local_url": local_url/);
});

test("remote one-click frontend renders a clickable local URL without auto-opening", () => {
  const body = frontendFunctionBody("remoteOneClick");
  assert.doesNotMatch(body, /await openLocalUrl\(localUrl\)/);
  assert.match(body, /setMsgHtml/);
  assert.match(body, /data-url=/);
  assert.match(main, /function setMsgHtml/);
  assert.match(main, /async function openLocalUrl/);
  assert.match(main, /call\("open_url", url \? \{ url \} : \{\}\)/);
  assert.match(main, /els\.msg\.addEventListener\("click"/);
});

test("browser opener reuses open_url and only accepts local sandbox URLs", () => {
  const m = libTauri.match(/fn open_url[\s\S]*?\n}\n\n\/\/\/ 运行诊断脚本/);
  assert.ok(m, "open_url body should be discoverable");
  const body = m[0];
  assert.match(body, /url: Option<String>/);
  assert.match(body, /http:\/\/127\.0\.0\.1:/);
  assert.match(body, /http:\/\/localhost:/);
  assert.match(body, /http:\/\/\[::1\]:/);
  assert.match(body, /只允许打开本地沙箱 URL/);
  assert.doesNotMatch(libTauri, /fn open_browser_url/);
});

test("remote stop button stops remote sandbox and proxy", () => {
  const body = frontendFunctionBody("stopAll");
  assert.match(body, /target === "remote" && currentProfile/);
  assert.match(body, /call\("remote_stop_all", \{ profile: currentProfile \}\)/);
  assert.doesNotMatch(body, /call\("remote_stop_proxy", \{ profile: currentProfile \}\)/);
  assert.match(body, /远程代理与沙箱已停止/);
});

test("remote stop-all backend stops sandbox and proxy without serial delay", () => {
  const m = remoteCommands.match(/pub fn remote_stop_all[\s\S]*?\n}\n\n\/\/\/ 查询远程代理状态/);
  assert.ok(m, "remote_stop_all body should be discoverable");
  const body = m[0];
  assert.match(body, /std::thread::spawn[\s\S]*stop_remote_sandbox/);
  assert.match(body, /let proxy_res = stop_remote_proxy\(&profile\)/);
  assert.match(body, /远程代理已停；但停止远程沙箱失败/);
});

test("remote helper status reports the configured sandbox state", () => {
  assert.match(helperCommands, /fn sandbox_is_running/);
  assert.match(helperCommands, /fn get_configured_sandbox_port/);
  assert.match(helperCommands, /"sandbox_running": sandbox_is_running\(\)/);
});

test("remote helper sandbox stop is idempotent before requiring Science", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_stop[\s\S]*?\n}\n\n\/\/\/ `logs/);
  assert.ok(m, "cmd_sandbox_stop body should be discoverable");
  const body = m[0];
  assert.match(body, /if !sandbox_is_running\(\)[\s\S]*CliEnvelope::ok/);
  assert.match(body, /find_cmd\("claude-science"\)/);
  assert.ok(
    body.indexOf("if !sandbox_is_running()") < body.indexOf('find_cmd("claude-science")'),
    "not-running sandbox should return ok before requiring the binary",
  );
});

test("remote helper sandbox start returns a fresh claude-science url", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(body, /sandbox_fresh_url/);
  assert.match(body, /\.args\(\["url", "--data-dir"\]\)/);
  assert.match(body, /"url": url/);
});

test("remote helper clears stale Science processes and waits for a usable sandbox url", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(helperCommands, /fn wait_for_sandbox_ready/);
  assert.match(helperCommands, /fn terminate_sandbox_processes/);
  assert.match(helperCommands, /fn matching_sandbox_pids/);
  assert.ok(
    body.indexOf("terminate_sandbox_processes") < body.indexOf(".spawn()"),
    "sandbox start must clear stale same-data-dir Science processes before spawning",
  );
  assert.match(helperCommands, /let url = sandbox_fresh_url/);
  assert.match(helperCommands, /http_health\(port, None/);
  assert.match(body, /sandbox\.log/);
  assert.doesNotMatch(
    body,
    /stdout\(std::process::Stdio::null\(\)\)[\s\S]*stderr\(std::process::Stdio::null\(\)\)[\s\S]*\.spawn\(\)/,
    "serve startup output should not be discarded while diagnosing daemon startup failures",
  );
});

test("remote helper sandbox start binds Science to loopback for SSH tunnel access", () => {
  const m = helperCommands.match(/pub fn cmd_sandbox_start[\s\S]*?\n}\n\n\/\/\/ `sandbox stop/);
  assert.ok(m, "cmd_sandbox_start body should be discoverable");
  const body = m[0];
  assert.match(body, /\.arg\("--host"\)\s*\.arg\("127\.0\.0\.1"\)/);
  assert.doesNotMatch(body, /\.arg\("0\.0\.0\.0"\)/);
});

test("remote helper carries and installs the managed proxy script", () => {
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/csswitch_proxy\.py"[\s\S]*\)\)/,
  );
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/dsml_shim\.py"[\s\S]*\)\)/,
  );
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/provider_policy\.py"[\s\S]*\)\)/,
  );
  assert.match(
    helperCommands,
    /include_str!\(concat!\([\s\S]*env!\("CSSWITCH_BUNDLED_PROXY_DIR"\)[\s\S]*"\/anthropic_compat\.py"[\s\S]*\)\)/,
  );
  assert.match(helperCommands, /fn ensure_managed_proxy_script\(\) -> Result<PathBuf, String>/);
  assert.match(helperCommands, /"~\/\.csswitch\/proxy\/csswitch_proxy\.py"/);
  assert.match(helperCommands, /dsml_shim\.py/);
  assert.match(helperCommands, /provider_policy\.py/);
  assert.match(helperCommands, /anthropic_compat\.py/);
  assert.match(helperCommands, /BUNDLED_PROXY/);
});

test("remote helper status advertises the current proxy bundle capability", () => {
  assert.match(helperCommands, /"proxy-bundle-v2"/);
  assert.match(remoteCommands, /"proxy-bundle-v2"/);
});

test("desktop build requires every Python module imported by the managed proxy", () => {
  for (const file of ["csswitch_proxy.py", "dsml_shim.py", "provider_policy.py", "anthropic_compat.py"]) {
    assert.match(tauriBuildRs, new RegExp(`"${file.replace(".", "\\.")}"`));
  }
});

test("remote helper prefers the self-healed managed proxy bundle after explicit overrides", () => {
  const m = helperCommands.match(/fn proxy_script_path[\s\S]*?\n}\n\n\/\/ ==========/);
  assert.ok(m, "proxy_script_path body should be discoverable");
  const body = m[0];
  assert.ok(
    body.indexOf('std::env::var("CSSWITCH_PROXY_DIR")') < body.indexOf("ensure_managed_proxy_script()"),
    "explicit CSSWITCH_PROXY_DIR override should remain first",
  );
  assert.doesNotMatch(body, /current_exe\(\)[\s\S]*ensure_managed_proxy_script\(\)/);
});

test("remote helper searches user-local binary directories for Science", () => {
  const m = helperCommands.match(/fn find_cmd[\s\S]*?\n}/);
  assert.ok(m, "find_cmd body should be discoverable");
  const body = m[0];
  assert.match(body, /\.local"\)\.join\("bin"\)/);
  assert.match(body, /miniconda3"\)\.join\("bin"\)/);
  assert.match(body, /anaconda3"\)\.join\("bin"\)/);
});

test("remote helper injects relay profile connection fields into proxy env", () => {
  assert.match(helperCommands, /fn proxy_launch_from_config/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_KEY"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_BASE_URL"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_MODEL"/);
  assert.match(helperCommands, /"CSSWITCH_RELAY_THINKING"/);
  assert.doesNotMatch(helperCommands, /_ => "DEEPSEEK_API_KEY"/);
});

test("remote helper clears an unhealthy proxy port before spawning a replacement", () => {
  assert.match(helperCommands, /fn clear_unhealthy_proxy_port/);
  assert.match(helperCommands, /fn stop_recorded_proxy/);
  assert.match(helperCommands, /pid_looks_like_recorded_proxy/);
  assert.match(helperCommands, /stop_recorded_proxy\(port\)/);
  assert.match(helperCommands, /clear_unhealthy_proxy_port\(port\)/);
  assert.match(helperCommands, /port_in_use/);
});

test("remote helper proxy start detaches and waits for health", () => {
  const m = helperCommands.match(/pub fn cmd_proxy_start[\s\S]*?\n}\n\n\/\/\/ `proxy status`/);
  assert.ok(m, "cmd_proxy_start body should be discoverable");
  const body = m[0];
  assert.match(body, /proxy\.log/);
  assert.match(body, /stdin\(Stdio::null\(\)\)/);
  assert.match(body, /process_group\(0\)/);
  assert.match(body, /proxy_health\(port, secret\)/);
  assert.match(body, /try_wait\(\)/);
  assert.match(body, /proxy_start_timeout/);
});
