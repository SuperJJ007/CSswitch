use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use super::credentials::{self, CredentialKind};
use super::prompt::{classify_prompt, PromptKind};

lazy_static::lazy_static! {
    static ref SESSIONS: Mutex<HashMap<String, PathBuf>> = Mutex::new(HashMap::new());
    static ref APP_HANDLE: Mutex<Option<tauri::AppHandle>> = Mutex::new(None);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskpassRequest {
    pub id: String,
    pub prompt: String,
    pub profile_id: String,
    #[serde(default)]
    pub key_path: Option<String>,
}

impl AskpassRequest {
    #[allow(dead_code)]
    pub fn new(prompt: &str, profile_id: &str, key_path: Option<String>) -> Self {
        Self {
            id: new_id(),
            prompt: prompt.to_string(),
            profile_id: profile_id.to_string(),
            key_path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskpassResponse {
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub remember: bool,
}

impl AskpassResponse {
    pub fn secret(secret: &str) -> Self {
        Self {
            secret: Some(secret.to_string()),
            cancelled: false,
            remember: false,
        }
    }

    #[allow(dead_code)]
    pub fn cancelled() -> Self {
        Self {
            secret: None,
            cancelled: true,
            remember: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskpassPromptPayload {
    pub session_id: String,
    pub request_id: String,
    pub profile_id: String,
    pub prompt: String,
    pub kind: String,
    pub remember_allowed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskpassClosePayload {
    pub session_id: String,
}

pub struct AskpassBroker {
    session_id: String,
    app: tauri::AppHandle,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

pub fn set_app_handle(app: tauri::AppHandle) {
    *APP_HANDLE.lock().unwrap() = Some(app);
}

pub fn app_handle() -> Option<tauri::AppHandle> {
    APP_HANDLE.lock().unwrap().clone()
}

pub fn run_cli() -> i32 {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let session_dir = match std::env::var("CSSWITCH_ASKPASS_DIR") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return 1,
    };
    let profile_id = match std::env::var("CSSWITCH_ASKPASS_PROFILE") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => return 1,
    };
    let key_path = std::env::var("CSSWITCH_ASKPASS_KEY_PATH")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let request = AskpassRequest::new(&prompt, &profile_id, key_path);
    if write_request(&session_dir, &request).is_err() {
        return 1;
    }

    match wait_response(&session_dir, &request.id, Duration::from_secs(120)) {
        Ok(response) if response.cancelled => 1,
        Ok(response) => {
            if let Some(secret) = response.secret {
                println!("{secret}");
                0
            } else {
                1
            }
        }
        Err(_) => 1,
    }
}

impl AskpassBroker {
    #[allow(dead_code)]
    pub fn start(app: tauri::AppHandle, session_dir: PathBuf) -> Result<Self, String> {
        ensure_dirs(&session_dir)?;
        let session_id = new_id();
        register_session(&session_id, session_dir.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_session_id = session_id.clone();
        let worker_app = app.clone();
        let worker = thread::spawn(move || {
            poll_requests(worker_app, worker_session_id, session_dir, worker_stop);
        });
        Ok(Self {
            session_id,
            app,
            stop,
            worker: Some(worker),
        })
    }

    #[allow(dead_code)]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for AskpassBroker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let session_dir = unregister_session(&self.session_id);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(session_dir) = session_dir {
            cleanup_session_dir(&session_dir);
        }
        let _ = self.app.emit(
            "remote-auth-prompt-close",
            AskpassClosePayload {
                session_id: self.session_id.clone(),
            },
        );
    }
}

pub fn register_session(session_id: &str, session_dir: PathBuf) {
    SESSIONS
        .lock()
        .unwrap()
        .insert(session_id.to_string(), session_dir);
}

pub fn unregister_session(session_id: &str) -> Option<PathBuf> {
    SESSIONS.lock().unwrap().remove(session_id)
}

#[allow(dead_code)]
pub fn write_request(session_dir: impl AsRef<Path>, req: &AskpassRequest) -> Result<(), String> {
    let session_dir = session_dir.as_ref();
    ensure_dirs(session_dir)?;
    write_json(request_path(session_dir, &req.id), req)
}

pub fn read_request(
    session_dir: impl AsRef<Path>,
    request_id: &str,
) -> Result<AskpassRequest, String> {
    read_json(request_path(session_dir.as_ref(), request_id))
}

pub fn write_response(
    session_dir: impl AsRef<Path>,
    request_id: &str,
    resp: &AskpassResponse,
) -> Result<(), String> {
    let session_dir = session_dir.as_ref();
    ensure_dirs(session_dir)?;
    write_json(response_path(session_dir, request_id), resp)
}

pub fn read_response(
    session_dir: impl AsRef<Path>,
    request_id: &str,
) -> Result<AskpassResponse, String> {
    read_json(response_path(session_dir.as_ref(), request_id))
}

fn consume_response(
    session_dir: impl AsRef<Path>,
    request_id: &str,
) -> Result<AskpassResponse, String> {
    let session_dir = session_dir.as_ref();
    let path = response_path(session_dir, request_id);
    let response = read_json(path.clone())?;
    let _ = fs::remove_file(path);
    Ok(response)
}

#[allow(dead_code)]
pub fn wait_response(
    session_dir: impl AsRef<Path>,
    request_id: &str,
    timeout: Duration,
) -> Result<AskpassResponse, String> {
    let session_dir = session_dir.as_ref();
    let started = Instant::now();
    loop {
        if !session_dir.exists() {
            return Err("登录会话已结束".to_string());
        }
        match consume_response(session_dir, request_id) {
            Ok(resp) => return Ok(resp),
            Err(_) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("等待登录验证超时：{e}")),
        }
    }
}

pub fn respond(
    session_id: &str,
    request_id: &str,
    secret: Option<String>,
    cancelled: bool,
    remember: bool,
) -> Result<(), String> {
    let session_dir = SESSIONS
        .lock()
        .unwrap()
        .get(session_id)
        .cloned()
        .ok_or_else(|| "登录会话已结束，请重新连接。".to_string())?;
    let request = read_request(&session_dir, request_id)?;
    let response = AskpassResponse {
        secret: secret.clone(),
        cancelled,
        remember,
    };
    if remember && !cancelled {
        if let Some(secret) = secret.as_deref() {
            remember_secret(&request, secret)?;
        }
    }
    write_response(session_dir, request_id, &response)
}

fn remember_secret(request: &AskpassRequest, secret: &str) -> Result<(), String> {
    match classify_prompt(&request.prompt) {
        PromptKind::Password => {
            credentials::save_secret(&request.profile_id, CredentialKind::Password, secret)
        }
        PromptKind::KeyPassword => {
            let Some(key_path) = request.key_path.as_deref() else {
                return Ok(());
            };
            credentials::save_secret(
                &request.profile_id,
                CredentialKind::KeyPassword(key_path),
                secret,
            )
        }
        PromptKind::VerificationCode | PromptKind::Unknown => Ok(()),
    }
}

fn poll_requests(
    app: tauri::AppHandle,
    session_id: String,
    session_dir: PathBuf,
    stop: Arc<AtomicBool>,
) {
    let mut seen = HashSet::new();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(entries) = fs::read_dir(requests_dir(&session_dir)) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(request_id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                if !seen.insert(request_id.clone()) {
                    continue;
                }
                if let Ok(request) = read_request(&session_dir, &request_id) {
                    if try_auto_response(&session_dir, &request).is_err() {
                        let _ =
                            app.emit("remote-auth-prompt", prompt_payload(&session_id, &request));
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn try_auto_response(session_dir: &Path, request: &AskpassRequest) -> Result<(), String> {
    let secret = match classify_prompt(&request.prompt) {
        PromptKind::Password => {
            credentials::read_secret(&request.profile_id, CredentialKind::Password)
        }
        PromptKind::KeyPassword => {
            let key_path = request
                .key_path
                .as_deref()
                .ok_or_else(|| "missing key path".to_string())?;
            credentials::read_secret(&request.profile_id, CredentialKind::KeyPassword(key_path))
        }
        PromptKind::VerificationCode | PromptKind::Unknown => None,
    };
    let Some(secret) = secret else {
        return Err("no saved secret".to_string());
    };
    write_response(session_dir, &request.id, &AskpassResponse::secret(&secret))
}

fn prompt_payload(session_id: &str, request: &AskpassRequest) -> AskpassPromptPayload {
    let kind = classify_prompt(&request.prompt);
    AskpassPromptPayload {
        session_id: session_id.to_string(),
        request_id: request.id.clone(),
        profile_id: request.profile_id.clone(),
        prompt: request.prompt.clone(),
        kind: prompt_kind_name(&kind).to_string(),
        remember_allowed: matches!(kind, PromptKind::Password | PromptKind::KeyPassword),
    }
}

fn prompt_kind_name(kind: &PromptKind) -> &'static str {
    match kind {
        PromptKind::Password => "password",
        PromptKind::KeyPassword => "keyPassword",
        PromptKind::VerificationCode => "verificationCode",
        PromptKind::Unknown => "unknown",
    }
}

fn ensure_dirs(session_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(requests_dir(session_dir))
        .map_err(|e| format!("无法创建登录请求目录：{e}"))?;
    fs::create_dir_all(responses_dir(session_dir))
        .map_err(|e| format!("无法创建登录响应目录：{e}"))?;
    Ok(())
}

fn requests_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("requests")
}

fn responses_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("responses")
}

fn request_path(session_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir(session_dir).join(format!("{request_id}.json"))
}

fn response_path(session_dir: &Path, request_id: &str) -> PathBuf {
    responses_dir(session_dir).join(format!("{request_id}.json"))
}

fn cleanup_session_dir(session_dir: &Path) {
    let _ = fs::remove_dir_all(session_dir);
}

fn write_json(path: PathBuf, value: &impl Serialize) -> Result<(), String> {
    let json = serde_json::to_vec(value).map_err(|e| format!("序列化登录验证数据失败：{e}"))?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, json).map_err(|e| format!("写入登录验证数据失败：{e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("保存登录验证数据失败：{e}"))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: PathBuf) -> Result<T, String> {
    let raw = fs::read(&path).map_err(|e| format!("读取登录验证数据失败：{e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("解析登录验证数据失败：{e}"))
}

fn new_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "csswitch-askpass-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn askpass_request_roundtrip_uses_json_files() {
        let dir = temp_session_dir();
        let req = AskpassRequest::new("Password:", "profile-1", None);

        write_request(&dir, &req).unwrap();
        let loaded = read_request(&dir, &req.id).unwrap();
        assert_eq!(loaded.prompt, "Password:");
        assert_eq!(loaded.profile_id, "profile-1");

        write_response(&dir, &req.id, &AskpassResponse::secret("pw")).unwrap();
        let resp = read_response(&dir, &req.id).unwrap();
        assert_eq!(resp.secret.as_deref(), Some("pw"));
        assert!(!resp.cancelled);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn respond_rejects_unknown_session() {
        let err = respond("missing", "req", Some("pw".to_string()), false, false).unwrap_err();
        assert!(err.contains("登录会话"));
    }

    #[test]
    fn consume_response_removes_secret_file() {
        let dir = temp_session_dir();
        let req = AskpassRequest::new("Password:", "profile-1", None);
        write_request(&dir, &req).unwrap();
        write_response(&dir, &req.id, &AskpassResponse::secret("pw")).unwrap();

        let resp = consume_response(&dir, &req.id).unwrap();

        assert_eq!(resp.secret.as_deref(), Some("pw"));
        assert!(!response_path(&dir, &req.id).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cleanup_session_dir_removes_requests_and_responses() {
        let dir = temp_session_dir();
        let req = AskpassRequest::new("Password:", "profile-1", None);
        write_request(&dir, &req).unwrap();
        write_response(&dir, &req.id, &AskpassResponse::secret("pw")).unwrap();

        cleanup_session_dir(&dir);

        assert!(!dir.exists());
    }

    #[test]
    fn wait_response_returns_when_session_dir_disappears() {
        let dir = temp_session_dir();
        let req = AskpassRequest::new("Password:", "profile-1", None);
        write_request(&dir, &req).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let started = Instant::now();
        let result = wait_response(&dir, &req.id, Duration::from_millis(300));

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(150));
    }
}
