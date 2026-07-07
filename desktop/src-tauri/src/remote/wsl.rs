//! WSL transport for local Windows Linux distributions.
//!
//! WSL targets reuse the same csswitch-helper JSON protocol as SSH targets, but
//! enter Linux through `wsl.exe` instead of `ssh user@host`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use super::ssh;
use super::types::{RemoteError, RemoteHostProfile};

#[cfg(windows)]
const WSL_EXE: &str = "wsl.exe";
#[cfg(not(windows))]
const WSL_EXE: &str = "wsl.exe";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WslDistribution {
    pub name: String,
    pub state: Option<String>,
    pub version: Option<u8>,
    pub is_default: bool,
}

fn decode_wsl_output(bytes: &[u8]) -> String {
    if let Some(boundary) = mixed_wsl_utf16_prefix_boundary(bytes) {
        let mut decoded = decode_utf16le_lossy(&bytes[..boundary]);
        decoded.push_str(&String::from_utf8_lossy(&bytes[boundary..]));
        decoded
    } else if looks_like_utf16le(bytes) {
        decode_utf16le_lossy(bytes)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    bytes.iter().filter(|byte| **byte == 0).count() > bytes.len().saturating_div(8)
}

fn decode_utf16le_lossy(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

fn mixed_wsl_utf16_prefix_boundary(bytes: &[u8]) -> Option<usize> {
    const WSL_PREFIX: &[u8] = b"w\0s\0l\0:\0";
    if !bytes.starts_with(WSL_PREFIX) {
        return None;
    }

    let mut i = 0;
    while i + 1 < bytes.len() {
        let unit = u16::from_le_bytes([bytes[i], bytes[i + 1]]);
        if unit == b'\n' as u16 {
            let next = i + 2;
            if next < bytes.len() && !looks_like_utf16le_fragment(&bytes[next..]) {
                return Some(next);
            }
        }
        i += 2;
    }
    None
}

fn looks_like_utf16le_fragment(bytes: &[u8]) -> bool {
    let pair_count = bytes.len().min(32) / 2;
    if pair_count == 0 {
        return false;
    }
    let zero_high_bytes = (0..pair_count)
        .filter(|idx| bytes[idx * 2 + 1] == 0)
        .count();
    zero_high_bytes * 2 >= pair_count
}

fn clean_wsl_stderr(stderr: &str) -> String {
    let mut cleaned = stderr
        .lines()
        .filter(|line| !is_wsl_localhost_proxy_warning(line))
        .collect::<Vec<_>>()
        .join("\n");
    if stderr.ends_with('\n') && !cleaned.is_empty() {
        cleaned.push('\n');
    }
    cleaned
}

fn is_wsl_localhost_proxy_warning(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.starts_with("wsl:")
        && lower.contains("localhost")
        && lower.contains("nat")
        && lower.contains("wsl")
}

pub fn parse_wsl_list_verbose(raw: &str) -> Vec<WslDistribution> {
    raw.lines()
        .filter_map(parse_wsl_distribution_line)
        .filter(|distro| !is_hidden_wsl_distro(&distro.name))
        .collect()
}

fn parse_wsl_distribution_line(line: &str) -> Option<WslDistribution> {
    let mut trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.contains("NAME") && upper.contains("STATE") {
        return None;
    }

    let is_default = trimmed.starts_with('*');
    if is_default {
        trimmed = trimmed.trim_start_matches('*').trim_start();
    }

    let parts = trimmed.split_whitespace().collect::<Vec<_>>();
    if parts.is_empty() {
        return None;
    }
    let version_idx = parts.iter().rposition(|value| value.parse::<u8>().is_ok());
    let version = version_idx.and_then(|idx| parts[idx].parse::<u8>().ok());
    let state_idx = version_idx.and_then(|idx| idx.checked_sub(1));
    let state = state_idx.map(|idx| parts[idx].to_string());
    let name_end_idx = state_idx.unwrap_or(parts.len());
    let name = parts[..name_end_idx].join(" ");

    Some(WslDistribution {
        name,
        state,
        version,
        is_default,
    })
}

fn is_hidden_wsl_distro(name: &str) -> bool {
    matches!(name, "docker-desktop" | "docker-desktop-data")
}

pub fn list_wsl_distributions() -> Result<Vec<WslDistribution>, RemoteError> {
    if !cfg!(windows) {
        return Err(RemoteError {
            code: "wsl_unsupported_platform".to_string(),
            message: "本机 WSL 仅支持 Windows。".to_string(),
            details: None,
            recoverable: false,
            suggestion: Some("请在 Windows 上使用本机 WSL，或改用远程服务器 SSH。".to_string()),
        });
    }

    let output = ssh::hide_cmd(Command::new(WSL_EXE))
        .args(["--list", "--verbose"])
        .output()
        .map_err(|e| RemoteError {
            code: "wsl_spawn_failed".to_string(),
            message: format!("无法执行 wsl.exe：{e}"),
            details: Some("请确认 Windows Subsystem for Linux 已安装并在 PATH 中。".to_string()),
            recoverable: false,
            suggestion: Some(
                "请先安装 WSL 和 Ubuntu，或在终端运行 wsl.exe --list --verbose 验证。".to_string(),
            ),
        })?;

    if !output.status.success() {
        let stderr = clean_wsl_stderr(&decode_wsl_output(&output.stderr))
            .trim()
            .to_string();
        return Err(map_wsl_error(&stderr, output.status.code(), None));
    }

    let stdout = decode_wsl_output(&output.stdout);
    Ok(parse_wsl_list_verbose(&stdout))
}

pub fn build_wsl_args(profile: &RemoteHostProfile, helper_args: &[String]) -> Vec<String> {
    let helper_cmd = format!(
        "{} --json {}",
        ssh::shell_quote(&profile.helper_path),
        helper_args
            .iter()
            .map(|arg| ssh::shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    build_wsl_shell_args(profile, &helper_cmd)
}

pub fn build_wsl_stdin_args(profile: &RemoteHostProfile) -> Vec<String> {
    let helper_cmd = format!("{} --json serve", ssh::shell_quote(&profile.helper_path));
    build_wsl_shell_args(profile, &helper_cmd)
}

fn build_wsl_shell_args(profile: &RemoteHostProfile, script: &str) -> Vec<String> {
    let distro = profile.distribution.as_deref().unwrap_or_default();
    let mut args = vec!["-d".to_string(), distro.to_string()];
    if !profile.username.trim().is_empty() {
        args.extend(["--user".to_string(), profile.username.clone()]);
    }
    args.extend([
        "--exec".to_string(),
        "sh".to_string(),
        "-lc".to_string(),
        script.to_string(),
    ]);
    args
}

pub fn run_helper_json<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
    retries: u32,
) -> Result<T, RemoteError> {
    let mut last_error: Option<RemoteError> = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            let delay = Duration::from_secs(2u64.saturating_mul(1 << (attempt - 1)));
            std::thread::sleep(delay);
        }

        match try_run_wsl(profile, helper_args, timeout_secs) {
            Ok(stdout) => match ssh::parse_helper_response::<T>(&stdout) {
                Ok(data) => return Ok(data),
                Err(e) => {
                    last_error = Some(e);
                    break;
                }
            },
            Err(e) => {
                let recoverable = e.recoverable;
                last_error = Some(e);
                if !recoverable {
                    break;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| RemoteError {
        code: "wsl_unknown".to_string(),
        message: "未知 WSL 错误".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("请查看日志或在终端运行 wsl.exe 验证。".to_string()),
    }))
}

pub fn run_helper_json_with_retry<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(
        profile,
        helper_args,
        ssh::DEFAULT_CMD_TIMEOUT_SECS,
        ssh::DEFAULT_RETRIES,
    )
}

pub fn run_helper_json_stdin_with_retry<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json_stdin(
        profile,
        helper_args,
        ssh::DEFAULT_CMD_TIMEOUT_SECS,
        ssh::DEFAULT_RETRIES,
    )
}

fn run_helper_json_stdin<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
    retries: u32,
) -> Result<T, RemoteError> {
    let mut last_error: Option<RemoteError> = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            let delay = Duration::from_secs(2u64.saturating_mul(1 << (attempt - 1)));
            std::thread::sleep(delay);
        }

        match try_run_wsl_stdin(profile, helper_args, timeout_secs) {
            Ok(stdout) => match ssh::parse_helper_response::<T>(&stdout) {
                Ok(data) => return Ok(data),
                Err(e) => {
                    last_error = Some(e);
                    break;
                }
            },
            Err(e) => {
                let recoverable = e.recoverable;
                last_error = Some(e);
                if !recoverable {
                    break;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| RemoteError {
        code: "wsl_unknown".to_string(),
        message: "未知 WSL 错误".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("请查看日志或在终端运行 wsl.exe 验证。".to_string()),
    }))
}

pub fn run_helper_json_slow<T: DeserializeOwned>(
    profile: &RemoteHostProfile,
    helper_args: &[String],
) -> Result<T, RemoteError> {
    run_helper_json(
        profile,
        helper_args,
        ssh::SLOW_CMD_TIMEOUT_SECS,
        ssh::DEFAULT_RETRIES,
    )
}

pub fn detect_remote_platform(
    profile: &RemoteHostProfile,
) -> Result<(String, String), RemoteError> {
    let script = "printf '%s %s\\n' \"$(uname -s)\" \"$(uname -m)\"";
    let stdout = run_wsl_shell_script(profile, script, ssh::DEFAULT_CMD_TIMEOUT_SECS)?;
    let mut parts = stdout.split_whitespace();
    let os = parts.next().unwrap_or_default().to_ascii_lowercase();
    let arch = parts.next().unwrap_or_default().to_string();
    if os.is_empty() || arch.is_empty() {
        return Err(RemoteError {
            code: "wsl_platform_parse_failed".to_string(),
            message: "无法识别 WSL 发行版的平台信息".to_string(),
            details: Some(stdout),
            recoverable: false,
            suggestion: Some("请确认该 WSL 发行版可以执行 uname。".to_string()),
        });
    }
    Ok((os, arch))
}

pub fn run_helper_install(profile: &RemoteHostProfile) -> Result<String, RemoteError> {
    let helper_path = ssh::shell_quote(&profile.helper_path);
    let repo = ssh::resolve_helper_release_repo()?;
    let helper_version = env!("CARGO_PKG_VERSION");
    let script = format!(
        r#"set -e
HELPER_PATH={helper_path}
HELPER_DIR=$(dirname "$HELPER_PATH")
mkdir -p "$HELPER_DIR"

download() {{
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    echo "WSL 发行版需要 curl 或 wget 来下载 helper。请手动安装。" >&2
    exit 1
  fi
}}

ARCH_RAW=$(uname -m)
case "$ARCH_RAW" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *)
    echo "不支持的架构: $ARCH_RAW（仅支持 x86_64/aarch64）" >&2
    exit 1
    ;;
esac

OS_RAW=$(uname -s)
case "$OS_RAW" in
  Linux) OS=linux ;;
  *)
    echo "不支持的操作系统: $OS_RAW（仅支持 Linux）" >&2
    exit 1
    ;;
esac

API_URL="https://api.github.com/repos/{repo}/releases/tags/v{helper_version}"
BINARY_NAME="csswitch-helper-${{OS}}-${{ARCH}}"
API_JSON=$(mktemp)
download "$API_URL" "$API_JSON"

if command -v jq >/dev/null 2>&1; then
  DOWNLOAD_URL=$(jq -r ".assets[] | select(.name==\"$BINARY_NAME\") | .browser_download_url" "$API_JSON" 2>/dev/null || true)
elif command -v python3 >/dev/null 2>&1; then
  DOWNLOAD_URL=$(python3 -c "
import json,sys
data=json.load(open('$API_JSON'))
for a in data.get('assets',[]):
    if a.get('name')=='$BINARY_NAME':
        print(a['browser_download_url'])
        break
" 2>/dev/null || true)
else
  DOWNLOAD_URL=$(awk -v name="\"$BINARY_NAME\"" '
    $0 ~ name {{ found=1 }}
    found && /browser_download_url/ {{
      if (match($0, /https:[^"]+/)) {{
        print substr($0, RSTART, RLENGTH)
        exit
      }}
    }}
  ' "$API_JSON" || true)
fi
rm -f "$API_JSON"

if [ -z "$DOWNLOAD_URL" ]; then
  echo "无法从 GitHub Releases 获取 $BINARY_NAME 下载链接。" >&2
  echo "手动安装: wget <url> -O $HELPER_PATH && chmod +x $HELPER_PATH" >&2
  exit 1
fi

TMP=$(mktemp)
download "$DOWNLOAD_URL" "$TMP"
chmod +x "$TMP"
mv "$TMP" "$HELPER_PATH"
"$HELPER_PATH" --json status
"#,
        helper_path = helper_path,
        repo = repo,
        helper_version = helper_version,
    );
    run_wsl_shell_script(profile, &script, ssh::SLOW_CMD_TIMEOUT_SECS)
}

pub fn install_helper_from_stdin(
    profile: &RemoteHostProfile,
    helper_bytes: &[u8],
) -> Result<String, RemoteError> {
    let helper_path = ssh::shell_quote(&profile.helper_path);
    let script = format!(
        concat!(
            "set -e; ",
            "helper_path={helper_path}; ",
            "helper_dir=$(dirname \"$helper_path\"); ",
            "mkdir -p \"$helper_dir\"; ",
            "helper_tmp=$(mktemp); ",
            "cat > \"$helper_tmp\"; ",
            "chmod +x \"$helper_tmp\"; ",
            "mv \"$helper_tmp\" \"$helper_path\"; ",
            "\"$helper_path\" --json status"
        ),
        helper_path = helper_path
    );
    run_wsl_shell_script_with_stdin(profile, &script, helper_bytes, ssh::SLOW_CMD_TIMEOUT_SECS)
}

fn try_run_wsl(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let args = build_wsl_args(profile, helper_args);
    let mut command = ssh::hide_cmd(Command::new(WSL_EXE));
    command.args(&args);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let child = command.spawn().map_err(|e| RemoteError {
        code: "wsl_spawn_failed".to_string(),
        message: format!("无法启动 wsl.exe：{e}"),
        details: None,
        recoverable: false,
        suggestion: Some("请确认 Windows Subsystem for Linux 已安装。".to_string()),
    })?;
    collect_wsl_output(profile, child, timeout_secs)
}

fn try_run_wsl_stdin(
    profile: &RemoteHostProfile,
    helper_args: &[String],
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let args = build_wsl_stdin_args(profile);
    let payload = ssh::helper_stdin_payload(helper_args)?;
    let mut command = ssh::hide_cmd(Command::new(WSL_EXE));
    command.args(&args);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| RemoteError {
        code: "wsl_spawn_failed".to_string(),
        message: format!("无法启动 wsl.exe：{e}"),
        details: None,
        recoverable: false,
        suggestion: Some("请确认 Windows Subsystem for Linux 已安装。".to_string()),
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload).map_err(|e| RemoteError {
            code: "wsl_stdin_failed".to_string(),
            message: format!("写入 WSL 命令 stdin 失败：{e}"),
            details: None,
            recoverable: true,
            suggestion: Some("请确认 WSL 发行版、Linux 用户和 Helper 路径权限正确。".to_string()),
        })?;
    }
    collect_wsl_output(profile, child, timeout_secs)
}

fn run_wsl_shell_script(
    profile: &RemoteHostProfile,
    script: &str,
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    run_wsl_shell_script_with_stdin(profile, script, &[], timeout_secs)
}

fn run_wsl_shell_script_with_stdin(
    profile: &RemoteHostProfile,
    script: &str,
    stdin_bytes: &[u8],
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let args = build_wsl_shell_args(profile, script);

    let mut command = ssh::hide_cmd(Command::new(WSL_EXE));
    command.args(&args);
    command.stdin(if stdin_bytes.is_empty() {
        Stdio::null()
    } else {
        Stdio::piped()
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|e| RemoteError {
        code: "wsl_spawn_failed".to_string(),
        message: format!("无法启动 wsl.exe：{e}"),
        details: None,
        recoverable: false,
        suggestion: Some("请确认 Windows Subsystem for Linux 已安装。".to_string()),
    })?;

    if !stdin_bytes.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(write_error) = stdin.write_all(stdin_bytes) {
                drop(stdin);
                let stderr = ssh::wait_with_timeout(child, Duration::from_secs(timeout_secs))
                    .ok()
                    .flatten()
                    .map(|output| clean_wsl_stderr(&decode_wsl_output(&output.stderr)))
                    .unwrap_or_default();
                return Err(RemoteError {
                    code: "wsl_stdin_failed".to_string(),
                    message: wsl_stdin_failed_message(&write_error, &stderr),
                    details: if stderr.trim().is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                    recoverable: true,
                    suggestion: Some(
                        "请确认 WSL 发行版、Linux 用户和 Helper 路径权限正确。".to_string(),
                    ),
                });
            }
        }
    }

    collect_wsl_output(profile, child, timeout_secs)
}

fn wsl_stdin_failed_message(write_error: &std::io::Error, stderr: &str) -> String {
    let stderr = clean_wsl_stderr(stderr);
    if stderr.trim().is_empty() {
        format!("写入 WSL 命令 stdin 失败：{write_error}")
    } else {
        format!(
            "写入 WSL 命令 stdin 失败：{write_error}；WSL 错误：{}",
            stderr.trim()
        )
    }
}

fn collect_wsl_output(
    profile: &RemoteHostProfile,
    child: std::process::Child,
    timeout_secs: u64,
) -> Result<String, RemoteError> {
    let output = ssh::wait_with_timeout(child, Duration::from_secs(timeout_secs)).map_err(|e| {
        RemoteError {
            code: "wsl_io_error".to_string(),
            message: format!("WSL 进程 I/O 错误：{e}"),
            details: None,
            recoverable: true,
            suggestion: Some("请重试。如持续出现，请检查系统资源。".to_string()),
        }
    })?;

    let Some(output) = output else {
        return Err(RemoteError {
            code: "wsl_timeout".to_string(),
            message: format!("WSL 命令执行超时（{}秒）", timeout_secs),
            details: profile.distribution.clone(),
            recoverable: true,
            suggestion: Some("该 WSL 发行版可能正在启动或命令卡住，请稍后重试。".to_string()),
        });
    };

    if !output.status.success() {
        let stderr = clean_wsl_stderr(&decode_wsl_output(&output.stderr))
            .trim()
            .to_string();
        return Err(map_wsl_error(&stderr, output.status.code(), Some(profile)));
    }

    const MAX_OUTPUT_SIZE: usize = 1024 * 1024;
    if output.stdout.len() > MAX_OUTPUT_SIZE {
        return Err(RemoteError {
            code: "output_too_large".to_string(),
            message: format!("WSL 输出过大（{} 字节）", output.stdout.len()),
            details: None,
            recoverable: false,
            suggestion: Some("请在 WSL 中查看 Helper 日志排查问题。".to_string()),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| RemoteError {
        code: "invalid_utf8".to_string(),
        message: "WSL 返回了无效的 UTF-8 数据".to_string(),
        details: None,
        recoverable: false,
        suggestion: Some("请检查 WSL 命令输出。".to_string()),
    })
}

fn map_wsl_error(
    stderr: &str,
    exit_code: Option<i32>,
    profile: Option<&RemoteHostProfile>,
) -> RemoteError {
    let lower = stderr.to_lowercase();
    if lower.contains("helper not installed") {
        return RemoteError {
            code: "helper_not_found".to_string(),
            message: "WSL Helper 未安装或路径不正确".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some("请点击“安装 / 更新 Helper”，或检查 Helper 路径。".to_string()),
        };
    }
    if lower.contains("wslregisterdistribution failed")
        || lower.contains("windows subsystem for linux has no installed distributions")
        || lower.contains("wsl 2 requires an update")
    {
        return RemoteError {
            code: "wsl_not_installed".to_string(),
            message: "未找到可用的 WSL 环境".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some("请先安装 Windows Subsystem for Linux 和 Ubuntu。".to_string()),
        };
    }
    if lower.contains("there is no distribution")
        || lower.contains("distribution") && lower.contains("not found")
        || lower.contains("the specified distribution")
    {
        let distro = profile
            .and_then(|p| p.distribution.as_deref())
            .unwrap_or("所选发行版");
        return RemoteError {
            code: "wsl_distribution_not_found".to_string(),
            message: format!("找不到 WSL 发行版：{distro}"),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some("请点击“重新扫描”后选择列表中的发行版。".to_string()),
        };
    }
    if lower.contains("user") && (lower.contains("not found") || lower.contains("does not exist")) {
        let user = profile.map(|p| p.username.as_str()).unwrap_or("所选用户");
        return RemoteError {
            code: "wsl_user_not_found".to_string(),
            message: format!("WSL 用户不存在或无法启动：{user}"),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some(
                "请确认该 Linux 用户存在，或在 WSL 中运行 whoami 查看用户名。".to_string(),
            ),
        };
    }
    if lower.contains("no such file")
        || lower.contains("not found")
        || stderr.contains("没有那个文件或目录")
    {
        return RemoteError {
            code: "helper_not_found".to_string(),
            message: "WSL Helper 未安装或路径不正确".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some("请点击“安装 / 更新 Helper”，或检查 Helper 路径。".to_string()),
        };
    }
    if lower.contains("permission denied") {
        return RemoteError {
            code: "permission_denied".to_string(),
            message: "WSL 内权限不足".to_string(),
            details: Some(stderr.to_string()),
            recoverable: false,
            suggestion: Some("请确认 WSL 用户对 Helper 路径有读写和执行权限。".to_string()),
        };
    }

    RemoteError {
        code: exit_code
            .map(|code| format!("wsl_exit_{code}"))
            .unwrap_or_else(|| "wsl_failed".to_string()),
        message: if stderr.trim().is_empty() {
            "WSL 命令执行失败".to_string()
        } else {
            stderr.trim().to_string()
        },
        details: Some(stderr.to_string()),
        recoverable: false,
        suggestion: Some("请在终端运行 wsl.exe 验证该发行版和用户是否可用。".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::types::{RemoteAuthMethod, RemoteSshAdvancedOptions, RemoteTargetKind};

    fn wsl_profile() -> RemoteHostProfile {
        RemoteHostProfile {
            id: "wsl".to_string(),
            name: "Ubuntu".to_string(),
            kind: RemoteTargetKind::Wsl,
            host: String::new(),
            port: 0,
            distribution: Some("Ubuntu".to_string()),
            username: "zhawei".to_string(),
            auth_method: RemoteAuthMethod::Recommended {
                use_saved_keys: true,
                use_default_key_files: true,
                allow_password: true,
                allow_verification_code: true,
                remember_connection: true,
                strict: false,
            },
            helper_path: "~/.csswitch/bin/csswitch-helper".to_string(),
            last_connected: None,
            ssh_options: RemoteSshAdvancedOptions::default(),
            transient_password: None,
        }
    }

    #[test]
    fn parses_wsl_list_verbose_output() {
        let raw = "  NAME                  STATE           VERSION\n* Ubuntu 22.04 LTS     Running         2\n  Debian               Stopped         2\n  docker-desktop       Running         2\n";
        let distros = parse_wsl_list_verbose(raw);
        assert_eq!(
            distros.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec!["Ubuntu 22.04 LTS", "Debian"]
        );
        assert!(distros[0].is_default);
        assert_eq!(distros[0].state.as_deref(), Some("Running"));
        assert_eq!(distros[0].version, Some(2));
    }

    #[test]
    fn decodes_utf16le_wsl_output() {
        let raw =
            "  NAME            STATE           VERSION\n* Ubuntu          Running         2\n";
        let bytes = raw
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect::<Vec<_>>();
        let decoded = decode_wsl_output(&bytes);
        assert!(decoded.contains("Ubuntu"));
    }

    #[test]
    fn decodes_mixed_wsl_warning_and_utf8_process_stderr() {
        let warning = "wsl: 检测到 localhost 代理配置，但未镜像到 WSL。NAT 模式下的 WSL 不支持 localhost 代理。\r\n";
        let mut bytes = warning
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect::<Vec<_>>();
        bytes.extend_from_slice(
            "mkdir: cannot create directory '': No such file or directory\n".as_bytes(),
        );

        let decoded = decode_wsl_output(&bytes);

        assert!(decoded.contains("mkdir: cannot create directory"));
        assert!(decoded.contains("No such file or directory"));
    }

    #[test]
    fn builds_wsl_helper_args() {
        let profile = wsl_profile();
        let args = build_wsl_args(&profile, &["status".to_string()]);
        assert_eq!(
            args,
            vec![
                "-d",
                "Ubuntu",
                "--user",
                "zhawei",
                "--exec",
                "sh",
                "-lc",
                "~/.csswitch/bin/csswitch-helper --json status"
            ]
        );
    }

    #[test]
    fn stdin_helper_args_do_not_put_payload_on_wsl_command_line() {
        let profile = wsl_profile();
        let args = build_wsl_stdin_args(&profile);
        let joined = args.join(" ");
        assert!(joined.contains("--json serve"));
        assert!(!joined.contains("sk-secret"));
        assert!(!joined.contains("config set"));
    }

    #[test]
    fn maps_helper_not_installed_to_helper_not_found() {
        let err = map_wsl_error("Helper not installed", Some(127), None);

        assert_eq!(err.code, "helper_not_found");
        assert!(err.message.contains("Helper"));
    }

    #[test]
    fn maps_real_wsl_not_installed_to_wsl_not_installed() {
        let err = map_wsl_error("WslRegisterDistribution failed with error", Some(1), None);

        assert_eq!(err.code, "wsl_not_installed");
    }

    #[test]
    fn wsl_stdin_failed_message_includes_stderr_when_available() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe ended");
        let message = wsl_stdin_failed_message(&err, "user zhawei not found\n");

        assert!(message.contains("pipe ended"));
        assert!(message.contains("user zhawei not found"));
    }

    #[test]
    fn wsl_stdin_failed_message_filters_wsl_localhost_proxy_warning() {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe ended");
        let stderr = "wsl: 检测到 localhost 代理配置，但未镜像到 WSL。NAT 模式下的 WSL 不支持 localhost 代理。\nmkdir: cannot create directory '': No such file or directory\n";
        let message = wsl_stdin_failed_message(&err, stderr);

        assert!(!message.contains("localhost"));
        assert!(message.contains("mkdir: cannot create directory"));
    }
}
