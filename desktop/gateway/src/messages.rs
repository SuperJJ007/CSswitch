use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use serde_json::Value;

use crate::config::{GatewayConfig, UPSTREAM_UA};
use crate::provider_contracts::AuthScheme;

#[derive(Debug)]
pub struct UpstreamBody {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub struct UpstreamError {
    pub status: u16,
    pub upstream_status: Option<u16>,
    pub detail: String,
}

#[derive(Debug)]
pub struct UpstreamStream {
    pub response: Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InferenceTimeouts {
    connect: Duration,
    total: Duration,
    read_idle: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelsTimeouts {
    connect: Duration,
    total: Duration,
    read_idle: Duration,
}

// Standalone/test launches retain the legacy 300-second limits. Managed
// launches replace all three values from the validated provider contract.
const INFERENCE_TIMEOUTS: InferenceTimeouts = InferenceTimeouts {
    connect: Duration::from_secs(300),
    total: Duration::from_secs(300),
    read_idle: Duration::from_secs(300),
};

const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

#[derive(Debug, Eq, PartialEq)]
enum BodyReadFailure {
    Deadline,
    Io(String),
}

/// Blocking reqwest exposes a resettable per-operation timeout but cannot
/// shorten an in-flight body read to the request's remaining cumulative
/// deadline. Move the response into a bounded worker and stop waiting exactly
/// at the remaining deadline. Cancellation is observed after the current
/// bounded read, so a timed-out request cannot leave an unbounded reader.
fn read_body_with_deadline(
    mut response: Response,
    limit: Option<u64>,
    started: Instant,
    total: Duration,
) -> Result<Vec<u8>, BodyReadFailure> {
    let remaining = total
        .checked_sub(started.elapsed())
        .ok_or(BodyReadFailure::Deadline)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut body = Vec::new();
        let mut chunk = [0_u8; 8192];
        let result = loop {
            if worker_cancelled.load(Ordering::Relaxed) {
                break Err(BodyReadFailure::Deadline);
            }
            let capacity = limit
                .map(|max| {
                    max.saturating_sub(body.len() as u64)
                        .min(chunk.len() as u64)
                })
                .unwrap_or(chunk.len() as u64) as usize;
            if capacity == 0 {
                break Ok(body);
            }
            match response.read(&mut chunk[..capacity]) {
                Ok(0) => break Ok(body),
                Ok(read) => {
                    body.extend_from_slice(&chunk[..read]);
                    if worker_cancelled.load(Ordering::Relaxed) {
                        break Err(BodyReadFailure::Deadline);
                    }
                }
                Err(error) => break Err(BodyReadFailure::Io(error.to_string())),
            }
        };
        let _ = tx.send(result);
    });
    match rx.recv_timeout(remaining) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            cancelled.store(true, Ordering::Relaxed);
            Err(BodyReadFailure::Deadline)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(BodyReadFailure::Io(
            "upstream body reader terminated unexpectedly".into(),
        )),
    }
}

fn auth_scheme(cfg: &GatewayConfig) -> AuthScheme {
    cfg.provider_contract
        .as_ref()
        .map(|contract| contract.auth_scheme)
        .unwrap_or_else(|| match cfg.provider.as_str() {
            "qwen" | "openai-custom" | "openai-responses" => AuthScheme::Bearer,
            "relay" => AuthScheme::AnthropicDual,
            "codex" => AuthScheme::CsswitchOauth,
            _ => AuthScheme::AnthropicXApiKey,
        })
}

fn models_timeout_secs(provider: &str) -> u64 {
    if provider == "qwen" || provider == "openai-custom" || provider == "openai-responses" {
        300
    } else {
        120
    }
}

fn models_timeouts(cfg: &GatewayConfig) -> ModelsTimeouts {
    cfg.provider_contract
        .as_ref()
        .map(|contract| ModelsTimeouts {
            connect: contract.connect_timeout,
            total: contract.request_timeout,
            read_idle: contract.read_idle_timeout,
        })
        .unwrap_or_else(|| {
            let legacy = Duration::from_secs(models_timeout_secs(&cfg.provider));
            ModelsTimeouts {
                connect: legacy,
                total: legacy,
                read_idle: legacy,
            }
        })
}

fn inference_client(
    timeouts: InferenceTimeouts,
    enforce_total: bool,
) -> Result<Client, UpstreamError> {
    // Blocking reqwest reapplies `timeout` to request send and every Response::read.
    // Finite requests therefore use the tighter per-operation bound and also enforce
    // the cumulative total deadline while consuming the body. Active SSE streams use
    // only the resettable read-idle bound and intentionally have no cumulative limit.
    let operation_timeout = if enforce_total {
        timeouts.total.min(timeouts.read_idle)
    } else {
        timeouts.read_idle
    };
    let builder = Client::builder()
        .connect_timeout(timeouts.connect)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(operation_timeout);
    builder.build().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })
}

fn models_client(timeouts: ModelsTimeouts) -> Result<Client, UpstreamError> {
    Client::builder()
        .connect_timeout(timeouts.connect)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeouts.total.min(timeouts.read_idle))
        .build()
        .map_err(|e| UpstreamError {
            status: 502,
            upstream_status: None,
            detail: e.to_string(),
        })
}

fn post_with_timeouts(
    cfg: &GatewayConfig,
    body: Vec<u8>,
    timeouts: InferenceTimeouts,
    enforce_total: bool,
) -> Result<Response, UpstreamError> {
    let api_key = cfg.api_key.as_deref().unwrap_or("");
    let request = inference_client(timeouts, enforce_total)?
        .post(&cfg.upstream_url)
        .header("content-type", "application/json")
        .header("user-agent", UPSTREAM_UA);
    let request = match auth_scheme(cfg) {
        AuthScheme::Bearer => request.header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicDual => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key)
            .header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicXApiKey => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key),
        // Codex requests use codex_transport and never reach this generic API-key path.
        AuthScheme::CsswitchOauth => request,
    };
    request.body(body).send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })
}

fn inference_timeouts(cfg: &GatewayConfig) -> InferenceTimeouts {
    cfg.provider_contract
        .as_ref()
        .map(|contract| InferenceTimeouts {
            connect: contract.connect_timeout,
            total: contract.request_timeout,
            read_idle: contract.read_idle_timeout,
        })
        .unwrap_or(INFERENCE_TIMEOUTS)
}

fn get_once(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    let timeouts = models_timeouts(cfg);
    let started = Instant::now();
    let api_key = cfg.api_key.as_deref().unwrap_or("");
    let request = models_client(timeouts)?
        .get(url)
        .header("user-agent", UPSTREAM_UA);
    let request = match auth_scheme(cfg) {
        AuthScheme::Bearer => request.header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicDual => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key)
            .header("authorization", format!("Bearer {api_key}")),
        AuthScheme::AnthropicXApiKey => request
            .header("anthropic-version", "2023-06-01")
            .header("x-api-key", api_key),
        AuthScheme::CsswitchOauth => request,
    };
    let resp = request.send().map_err(|e| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: e.to_string(),
    })?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    if !(200..300).contains(&status) {
        let body = bounded_redacted_error_body(resp, api_key, started, timeouts.total);
        let detail = if body.is_empty() {
            format!("upstream {status}")
        } else {
            format!("upstream {status}: {body}")
        };
        return Err(UpstreamError {
            status: if (400..=599).contains(&status) {
                status
            } else {
                502
            },
            upstream_status: Some(status),
            detail,
        });
    }
    let body = read_body_with_deadline(resp, None, started, timeouts.total).map_err(|error| {
        let detail = match error {
            BodyReadFailure::Deadline => {
                "upstream models response exceeded the total timeout".into()
            }
            BodyReadFailure::Io(error) => {
                format!("upstream response body read failed: {error}")
            }
        };
        UpstreamError {
            status: 502,
            upstream_status: None,
            detail,
        }
    })?;
    Ok(UpstreamBody {
        status,
        content_type,
        body,
    })
}

fn retry_delay(attempt: usize) {
    std::thread::sleep(Duration::from_millis(800 * attempt as u64));
}

pub fn get(cfg: &GatewayConfig, url: &str) -> Result<UpstreamBody, UpstreamError> {
    let mut last_error = None;
    for attempt in 1..=3 {
        match get_once(cfg, url) {
            Ok(resp) => return Ok(resp),
            Err(e) if e.upstream_status.is_some() => return Err(e),
            Err(e) => {
                last_error = Some(e);
                if attempt < 3 {
                    retry_delay(attempt);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| UpstreamError {
        status: 502,
        upstream_status: None,
        detail: "upstream models request failed".to_string(),
    }))
}

fn sensitive_error_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "authorization",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
        "credential",
        "cookie",
        "secret",
        "path_secret",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn redact_json(value: &mut Value, api_key: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if sensitive_error_key(key) {
                    *value = Value::String("[REDACTED]".into());
                } else {
                    redact_json(value, api_key);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                redact_json(value, api_key);
            }
        }
        Value::String(text) => {
            let exact_redacted = if api_key.is_empty() {
                std::mem::take(text)
            } else {
                text.replace(api_key, "[REDACTED]")
            };
            *text = redact_ascii_token(redact_ascii_token(exact_redacted, "bearer "), "sk-");
        }
        _ => {}
    }
}

fn redact_ascii_token(mut text: String, marker: &str) -> String {
    let mut offset = 0;
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(relative_start) = lower[offset..].find(marker) else {
            return text;
        };
        let start = offset + relative_start;
        let token_start = start + marker.len();
        let token_len = text[token_start..]
            .bytes()
            .take_while(|byte| {
                !byte.is_ascii_whitespace()
                    && !matches!(*byte, b'"' | b'\'' | b',' | b'}' | b']' | b';')
            })
            .count();
        if token_len == 0 {
            offset = token_start;
            continue;
        }
        text.replace_range(token_start..token_start + token_len, "[REDACTED]");
        offset = token_start + "[REDACTED]".len();
    }
}

fn redact_error_body(mut bytes: Vec<u8>, api_key: &str) -> String {
    let truncated = bytes.len() as u64 > MAX_ERROR_BODY_BYTES;
    bytes.truncate(MAX_ERROR_BODY_BYTES as usize);
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let mut safe = match serde_json::from_str::<Value>(&raw) {
        Ok(mut value) => {
            redact_json(&mut value, api_key);
            serde_json::to_string(&value).unwrap_or_else(|_| "upstream error".into())
        }
        Err(_) => {
            let text = if api_key.is_empty() {
                raw
            } else {
                raw.replace(api_key, "[REDACTED]")
            };
            redact_ascii_token(redact_ascii_token(text, "bearer "), "sk-")
        }
    };
    if truncated {
        safe.push_str("…[truncated]");
    }
    safe
}

fn bounded_redacted_error_body(
    resp: Response,
    api_key: &str,
    started: Instant,
    total: Duration,
) -> String {
    match read_body_with_deadline(resp, Some(MAX_ERROR_BODY_BYTES + 1), started, total) {
        Ok(bytes) => redact_error_body(bytes, api_key),
        Err(BodyReadFailure::Deadline) => "upstream error body exceeded the total timeout".into(),
        Err(BodyReadFailure::Io(_)) => "upstream error body could not be read".into(),
    }
}

fn map_http_error(
    resp: Response,
    api_key: &str,
    started: Instant,
    total: Duration,
) -> UpstreamError {
    let status = resp.status().as_u16();
    let body = bounded_redacted_error_body(resp, api_key, started, total);
    let mapped = if (400..=599).contains(&status) {
        status
    } else {
        502
    };
    let detail = if body.is_empty() {
        format!("upstream {status}")
    } else {
        format!("upstream {status}: {body}")
    };
    UpstreamError {
        status: mapped,
        upstream_status: Some(status),
        detail,
    }
}

pub fn post_nonstream(cfg: &GatewayConfig, body: Vec<u8>) -> Result<UpstreamBody, UpstreamError> {
    let timeouts = inference_timeouts(cfg);
    let started = Instant::now();
    let resp = post_with_timeouts(cfg, body, timeouts, true)?;
    if !resp.status().is_success() {
        return Err(map_http_error(
            resp,
            cfg.api_key.as_deref().unwrap_or_default(),
            started,
            timeouts.total,
        ));
    }
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let response_body =
        read_body_with_deadline(resp, None, started, timeouts.total).map_err(|error| {
            let detail = match error {
                BodyReadFailure::Deadline => "upstream response exceeded the total timeout".into(),
                BodyReadFailure::Io(error) => {
                    format!("upstream response body read failed: {error}")
                }
            };
            UpstreamError {
                status: 502,
                upstream_status: Some(status),
                detail,
            }
        })?;
    Ok(UpstreamBody {
        status,
        content_type,
        body: response_body,
    })
}

#[cfg(test)]
fn read_first_line(resp: &mut Response) -> Result<Vec<u8>, UpstreamError> {
    let mut first = Vec::new();
    let mut byte = [0_u8; 1];
    while first.len() < 65_536 {
        match resp.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                first.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(e) => {
                return Err(UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: e.to_string(),
                });
            }
        }
    }
    if first.is_empty() {
        return Err(UpstreamError {
            status: 502,
            upstream_status: Some(200),
            detail: "upstream 200 but empty body".to_string(),
        });
    }
    Ok(first)
}

pub fn open_stream(cfg: &GatewayConfig, body: Vec<u8>) -> Result<UpstreamStream, UpstreamError> {
    let timeouts = inference_timeouts(cfg);
    let started = Instant::now();
    let resp = post_with_timeouts(cfg, body, timeouts, false)?;
    if !resp.status().is_success() {
        return Err(map_http_error(
            resp,
            cfg.api_key.as_deref().unwrap_or_default(),
            started,
            timeouts.total,
        ));
    }
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"))
    {
        return Err(UpstreamError {
            status: 502,
            upstream_status: Some(status),
            detail: "upstream 200 returned a non-SSE Content-Type".into(),
        });
    }
    Ok(UpstreamStream { response: resp })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        models_timeout_secs, models_timeouts, post_with_timeouts, read_body_with_deadline,
        read_first_line, redact_error_body, BodyReadFailure, InferenceTimeouts, ModelsTimeouts,
        INFERENCE_TIMEOUTS,
    };
    use crate::config::GatewayConfig;

    fn bind_loopback() -> TcpListener {
        loop {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock upstream");
            if listener.local_addr().expect("mock address").port() != 8765 {
                return listener;
            }
        }
    }

    fn read_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set mock read timeout");
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buf).expect("read mock request");
            assert!(read > 0, "gateway closed before request body completed");
            request.extend_from_slice(&buf[..read]);
            if expected_len.is_none() {
                if let Some(head_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..head_end]);
                    let body_len = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("content length"))
                        })
                        .unwrap_or(0);
                    expected_len = Some(head_end + 4 + body_len);
                }
            }
            if expected_len.is_some_and(|len| request.len() >= len) {
                return;
            }
        }
    }

    fn spawn_stream(chunks: Vec<(Duration, &'static [u8])>) -> (String, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let total_len = chunks.iter().map(|(_, chunk)| chunk.len()).sum::<usize>();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            read_request(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {total_len}\r\n\r\n"
            )
            .expect("write mock response head");
            stream.flush().expect("flush mock response head");
            for (delay, chunk) in chunks {
                thread::sleep(delay);
                if stream.write_all(chunk).is_err() {
                    return;
                }
                if stream.flush().is_err() {
                    return;
                }
            }
        });
        (format!("http://{address}/v1/messages"), handle)
    }

    fn spawn_counted_response(
        response: Vec<u8>,
    ) -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_thread = Arc::clone(&count);
        let handle = thread::spawn(move || {
            let serve = |mut stream: TcpStream| {
                count_for_thread.fetch_add(1, Ordering::SeqCst);
                read_request(&mut stream);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            };
            let (stream, _) = listener.accept().expect("accept first request");
            serve(stream);
            listener.set_nonblocking(true).expect("set nonblocking");
            let deadline = Instant::now() + Duration::from_millis(1_500);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{address}/v1/messages"), count, handle)
    }

    fn spawn_redirect_response() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_thread = Arc::clone(&count);
        let handle = thread::spawn(move || {
            let serve = |mut stream: TcpStream, redirect: bool| {
                count_for_thread.fetch_add(1, Ordering::SeqCst);
                read_request(&mut stream);
                let response = if redirect {
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .into()
                };
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            };
            let (stream, _) = listener.accept().expect("accept redirect request");
            serve(stream, true);
            listener.set_nonblocking(true).expect("set nonblocking");
            let deadline = Instant::now() + Duration::from_millis(1_500);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, false),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{address}/v1/messages"), count, handle)
    }

    struct ServerRelease(Option<mpsc::Sender<()>>);

    impl ServerRelease {
        fn release(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    impl Drop for ServerRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn spawn_blocked_stream(
        first: Option<&'static [u8]>,
    ) -> (
        String,
        ServerRelease,
        mpsc::Receiver<()>,
        thread::JoinHandle<()>,
    ) {
        let listener = bind_loopback();
        let address = listener.local_addr().expect("mock address");
        let (release_tx, release_rx) = mpsc::channel();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept gateway request");
            read_request(&mut stream);
            let declared_len = first.map_or(1, |chunk| chunk.len() + 1);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_len}\r\n\r\n"
            )
            .expect("write blocked response head");
            if let Some(chunk) = first {
                stream.write_all(chunk).expect("write first stream line");
            }
            stream.flush().expect("flush blocked response");
            blocked_tx.send(()).expect("signal blocked response");
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        (
            format!("http://{address}/v1/messages"),
            ServerRelease(Some(release_tx)),
            blocked_rx,
            handle,
        )
    }

    fn test_config(upstream_url: String) -> GatewayConfig {
        GatewayConfig {
            provider: "deepseek".to_string(),
            port: 0,
            auth_secret: None,
            api_key: Some("fake-key".to_string()),
            upstream_url,
            models_url: None,
            relay_thinking: None,
            provider_contract: None,
            intent: crate::config::GatewayIntent::Formal,
            static_model_resolver: None,
            shim_mode: "off".to_string(),
            codex_state_root: None,
            codex_contract: None,
            launch_id: "timeout-test".to_string(),
            skill_data_dir: None,
            skill_bridge_dir: None,
            skill_bridge_token: None,
            science_host_context: None,
        }
    }

    #[test]
    fn inference_and_models_timeout_contracts_are_separate() {
        assert_eq!(INFERENCE_TIMEOUTS.connect, Duration::from_secs(300));
        assert_eq!(INFERENCE_TIMEOUTS.total, Duration::from_secs(300));
        assert_eq!(INFERENCE_TIMEOUTS.read_idle, Duration::from_secs(300));

        assert_eq!(models_timeout_secs("qwen"), 300);
        assert_eq!(models_timeout_secs("openai-custom"), 300);
        assert_eq!(models_timeout_secs("openai-responses"), 300);
        assert_eq!(models_timeout_secs("deepseek"), 120);
        assert_eq!(models_timeout_secs("relay"), 120);

        let mut managed = test_config("http://127.0.0.1:9/v1/messages".into());
        managed.provider_contract =
            Some(crate::provider_contracts::load_runtime_contract("deepseek", None, None).unwrap());
        assert_eq!(
            models_timeouts(&managed),
            ModelsTimeouts {
                connect: Duration::from_secs(10),
                total: Duration::from_secs(30),
                read_idle: Duration::from_secs(300),
            }
        );
    }

    #[test]
    fn nonstream_body_failure_and_empty_stream_handshake_post_exactly_once() {
        let incomplete_json = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}".to_vec();
        let (url, count, upstream) = spawn_counted_response(incomplete_json);
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec()).unwrap_err();
        assert_eq!(error.status, 502);
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let empty_sse = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (url, count, upstream) = spawn_counted_response(empty_sse);
        let opened = super::open_stream(&test_config(url), b"{}".to_vec()).unwrap();
        drop(opened);
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn inference_redirect_is_not_followed_or_reposted() {
        let (url, count, upstream) = spawn_redirect_response();
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec()).unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, Some(307));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn finite_body_read_obeys_the_remaining_cumulative_deadline() {
        let (url, mut release, blocked, upstream) = spawn_blocked_stream(None);
        let timeouts = InferenceTimeouts {
            connect: Duration::from_secs(1),
            total: Duration::from_millis(120),
            read_idle: Duration::from_millis(500),
        };
        let started = Instant::now();
        let response = post_with_timeouts(&test_config(url), b"{}".to_vec(), timeouts, true)
            .expect("response headers");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("upstream blocked after headers");
        thread::sleep(Duration::from_millis(70));
        let read_started = Instant::now();
        let error = read_body_with_deadline(response, None, started, timeouts.total).unwrap_err();
        assert_eq!(error, BodyReadFailure::Deadline);
        assert!(read_started.elapsed() < Duration::from_millis(100));
        release.release();
        upstream.join().unwrap();
    }

    #[test]
    fn json_error_strings_redact_generic_tokens_without_truncation() {
        let safe = redact_error_body(
            br#"{"error":{"message":"Bearer other-bearer and sk-other-key and configured-key","detail":"ok"}}"#
                .to_vec(),
            "configured-key",
        );
        assert!(!safe.contains("other-bearer"));
        assert!(!safe.contains("other-key"));
        assert!(!safe.contains("configured-key"));
        assert!(safe.contains("Bearer [REDACTED]"));
        assert!(safe.contains("sk-[REDACTED]"));
    }

    #[test]
    fn stream_requires_sse_content_type_before_success() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 3\r\nConnection: close\r\n\r\n{}\n".to_vec();
        let (url, count, upstream) = spawn_counted_response(response);
        let error = super::open_stream(&test_config(url), b"{}".to_vec()).unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, Some(200));
        assert!(error.detail.contains("non-SSE"));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn upstream_http_status_is_preserved_and_error_body_is_bounded_and_redacted() {
        let key = "fake-key";
        let body = format!(
            "{{\"error\":{{\"message\":\"Bearer other-secret and sk-other-secret and Bearer {key}\",\"api_key\":\"{key}\",\"padding\":\"{}\"}}}}",
            "x".repeat(20_000)
        );
        let response = format!(
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (url, count, upstream) = spawn_counted_response(response);
        let error = super::post_nonstream(&test_config(url), b"{}".to_vec()).unwrap_err();
        assert_eq!(error.status, 405);
        assert_eq!(error.upstream_status, Some(405));
        assert!(!error.detail.contains(key));
        assert!(!error.detail.contains("other-secret"));
        assert!(error.detail.len() < 17_000);
        assert!(error.detail.contains("truncated"));
        upstream.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn active_stream_can_outlive_read_idle_timeout() {
        let read_idle = Duration::from_millis(500);
        let (url, upstream) = spawn_stream(vec![
            (Duration::ZERO, b"event: message_start\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
            (Duration::from_millis(80), b"data: tick\n"),
        ]);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
        )
        .expect("open active stream");
        let first = read_first_line(&mut response).expect("read first stream line");
        let started = Instant::now();
        let mut remaining = Vec::new();
        response
            .read_to_end(&mut remaining)
            .expect("active stream must not hit a total deadline");
        let elapsed = started.elapsed();

        assert_eq!(first, b"event: message_start\n");
        assert_eq!(
            remaining,
            b"data: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\ndata: tick\n"
        );
        assert!(
            elapsed > read_idle,
            "stream should run longer than one idle window: {elapsed:?}"
        );
        upstream.join().expect("join active mock upstream");
    }

    #[test]
    fn stalled_stream_exceeding_read_idle_timeout_fails() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) =
            spawn_blocked_stream(Some(b"event: message_start\n"));
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
        )
        .expect("open stalled stream");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled stall");
        assert_eq!(
            read_first_line(&mut response).expect("read first stream line"),
            b"event: message_start\n"
        );

        let started = Instant::now();
        let error = response
            .read_to_end(&mut Vec::new())
            .expect_err("stalled stream must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert!(
            !error.to_string().is_empty(),
            "stalled-stream error detail must not be empty"
        );
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join stalled mock upstream");
    }

    #[test]
    fn first_byte_idle_timeout_keeps_upstream_error_contract() {
        let read_idle = Duration::from_millis(250);
        let (url, mut release, blocked, upstream) = spawn_blocked_stream(None);
        let cfg = test_config(url);
        let mut response = post_with_timeouts(
            &cfg,
            b"{}".to_vec(),
            InferenceTimeouts {
                connect: Duration::from_secs(1),
                total: Duration::from_secs(1),
                read_idle,
            },
            false,
        )
        .expect("receive mock response headers");
        blocked
            .recv_timeout(Duration::from_secs(1))
            .expect("mock upstream must enter the controlled first-byte stall");

        let started = Instant::now();
        let error = read_first_line(&mut response)
            .expect_err("missing first byte must hit the read-idle timeout");
        let elapsed = started.elapsed();
        assert_eq!(error.status, 502);
        assert_eq!(error.upstream_status, None);
        assert!(!error.detail.is_empty());
        assert!(elapsed >= read_idle, "timeout fired too early: {elapsed:?}");
        release.release();
        upstream.join().expect("join first-byte mock upstream");
    }
}
