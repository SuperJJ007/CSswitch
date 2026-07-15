use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::auth::{strip_path_secret, AuthResult};
use crate::config::GatewayConfig;
use crate::{
    anthropic_compat::{self, AnthropicMetadata, KimiServerToolFilter},
    connect,
    dsml_shim::{DsmlDetector, DsmlStreamRewriter},
    messages, models, openai_chat, openai_responses, policy,
};

const BRIDGE_REPLAY_WINDOW_SECONDS: u64 = 185;
const BRIDGE_HEARTBEAT_SECONDS: u64 = 2;

#[cfg(unix)]
#[derive(Clone)]
struct BridgeProgress {
    phase: String,
    message: String,
    sequence: u64,
}

struct RequestHead {
    method: String,
    target: String,
    headers: HashMap<String, String>,
}

struct RequestNonceGenerator {
    process_prefix: String,
    request_counter: AtomicU64,
}

impl RequestNonceGenerator {
    fn new() -> Result<Self, String> {
        let mut process_prefix = [0_u8; 16];
        getrandom::getrandom(&mut process_prefix)
            .map_err(|e| format!("request nonce random initialization failed: {e}"))?;
        Ok(Self::with_prefix(process_prefix))
    }

    fn with_prefix(process_prefix: [u8; 16]) -> Self {
        let process_prefix = process_prefix
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            process_prefix,
            request_counter: AtomicU64::new(0),
        }
    }

    fn next_nonce(&self) -> String {
        let request_number = self
            .request_counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("request nonce counter exhausted")
            + 1;
        format!("{}{:016x}", self.process_prefix, request_number)
    }
}

enum StreamFilter {
    Kimi(KimiServerToolFilter),
    DsmlDetect(DsmlDetector),
    DsmlRewrite(DsmlStreamRewriter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamTermination {
    NormalEof,
    UpstreamReadError,
    DownstreamWriteError,
}

impl StreamFilter {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        match self {
            StreamFilter::Kimi(filter) => filter.feed(chunk),
            StreamFilter::DsmlDetect(detector) => {
                detector.feed(chunk);
                chunk.to_vec()
            }
            StreamFilter::DsmlRewrite(rewriter) => rewriter.feed(chunk),
        }
    }

    fn finalize(&mut self) -> Vec<u8> {
        match self {
            StreamFilter::Kimi(filter) => filter.finalize(),
            StreamFilter::DsmlDetect(_) => Vec::new(),
            StreamFilter::DsmlRewrite(rewriter) => rewriter.finalize(),
        }
    }

    fn log_stats(&self) {
        match self {
            StreamFilter::Kimi(filter) if filter.dropped() > 0 => {
                eprintln!(
                    "relay stream rules=tool.kimi.web_search.server-tool-filter dropped={}",
                    filter.dropped()
                );
            }
            StreamFilter::DsmlDetect(detector) if detector.found => {
                eprintln!("deepseek stream DSML detect found=true");
            }
            StreamFilter::DsmlRewrite(rewriter) if rewriter.synthesized => {
                eprintln!("deepseek stream DSML rewrite tool_use={}", rewriter.tool_n);
            }
            _ => {}
        }
    }
}

fn read_head(stream: &mut TcpStream) -> Result<RequestHead, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut byte = [0_u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("empty request".to_string());
        }
        buf.push(byte[0]);
        if buf.len() > 64 * 1024 {
            return Err("request headers too large".to_string());
        }
    }
    let text = std::str::from_utf8(&buf).map_err(|_| "invalid request headers".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

fn content_length(headers: &HashMap<String, String>) -> Result<usize, String> {
    let Some(raw) = headers.get("content-length") else {
        return Ok(0);
    };
    let parsed = raw
        .parse::<i64>()
        .map_err(|_| "invalid Content-Length".to_string())?;
    if parsed < 0 {
        return Err("invalid Content-Length".to_string());
    }
    Ok(parsed as usize)
}

fn read_body(stream: &mut TcpStream, len: usize) -> Result<Vec<u8>, String> {
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(body)
}

fn json_bytes(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec())
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn write_json(stream: &mut TcpStream, status: u16, reason: &str, value: Value) {
    let body = json_bytes(value);
    write_response(stream, status, reason, "application/json", &body);
}

fn typed_error_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    error_type: &str,
    message: &str,
) {
    write_json(
        stream,
        status,
        reason,
        json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": message,
            },
        }),
    );
}

fn forbidden_json(stream: &mut TcpStream) {
    typed_error_json(stream, 403, "Forbidden", "permission_error", "forbidden");
}

fn invalid_request_json(stream: &mut TcpStream, detail: &str) {
    typed_error_json(stream, 400, "Bad Request", "invalid_request_error", detail);
}

fn not_found_json(stream: &mut TcpStream, path: &str) {
    typed_error_json(stream, 404, "Not Found", "not_found_error", path);
}

fn api_error_json(stream: &mut TcpStream, status: u16, detail: &str) {
    typed_error_json(stream, status, status_reason(status), "api_error", detail);
}

fn status_reason(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Error")
}

fn dequery(path: &str) -> &str {
    path.split_once('?').map(|(p, _)| p).unwrap_or(path)
}

fn models_error_json(
    stream: &mut TcpStream,
    status: u16,
    error_kind: &str,
    upstream_status: Option<u16>,
    message: &str,
) {
    write_json(
        stream,
        status,
        status_reason(status),
        json!({
            "error_kind": error_kind,
            "upstream_status": upstream_status,
            "message": message,
        }),
    );
}

fn handle_get(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    relay_models: &models::RelayModelCache,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    match path.as_str() {
        "/health" => write_json(
            stream,
            200,
            "OK",
            json!({
                "status": "ok",
                "gateway": "rust",
                "provider": cfg.provider,
                "shim": cfg.shim_mode,
                "launch_id": cfg.launch_id,
            }),
        ),
        "/v1/models" if cfg.provider == "qwen" => {
            write_json(stream, 200, "OK", models::qwen_models_response())
        }
        "/v1/models"
            if cfg.provider == "openai-custom"
                || cfg.provider == "openai-responses"
                || cfg.provider == "relay" =>
        {
            if let Some(model) = cfg.forced_model.as_deref() {
                write_json(stream, 200, "OK", models::force_shell_response(model));
                return;
            }
            let Some(models_url) = cfg.models_url.as_deref() else {
                models_error_json(stream, 502, "network", None, "missing models URL");
                return;
            };
            match messages::get(cfg, models_url) {
                Ok(resp) => match serde_json::from_slice::<Value>(&resp.body) {
                    Ok(raw) => {
                        let (body, ids) = models::normalize_live_models(&raw);
                        relay_models.update_from_live_models(&cfg.provider, &ids);
                        write_json(stream, 200, "OK", body);
                    }
                    Err(e) => models_error_json(
                        stream,
                        502,
                        "protocol",
                        None,
                        &format!("upstream models JSON parse failed: {e}"),
                    ),
                },
                Err(e) => models_error_json(
                    stream,
                    e.status,
                    if e.upstream_status.is_some() {
                        "upstream"
                    } else {
                        "network"
                    },
                    e.upstream_status,
                    &e.detail,
                ),
            }
        }
        "/v1/models" => write_json(stream, 200, "OK", models::deepseek_models_response()),
        _ => not_found_json(stream, &path),
    }
}

fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", chunk.len())?;
    stream.write_all(chunk)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn stream_error_event(detail: &str) -> Vec<u8> {
    format!(
        "event: error\ndata: {}\n\n",
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": detail,
            },
        })
    )
    .into_bytes()
}

fn sse_event(event: &str, data: &Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string())
    )
    .into_bytes()
}

fn forward_stream_body<R, F>(
    upstream: &mut R,
    first: &[u8],
    filter: &mut Option<StreamFilter>,
    mut emit: F,
) -> StreamTermination
where
    R: Read,
    F: FnMut(&[u8]) -> std::io::Result<()>,
{
    let first = if let Some(filter) = filter.as_mut() {
        filter.feed(first)
    } else {
        first.to_vec()
    };
    if !first.is_empty() && emit(&first).is_err() {
        return StreamTermination::DownstreamWriteError;
    }

    let mut buf = [0_u8; 8192];
    loop {
        match upstream.read(&mut buf) {
            Ok(0) => {
                if let Some(filter) = filter.as_mut() {
                    let tail = filter.finalize();
                    if !tail.is_empty() && emit(&tail).is_err() {
                        return StreamTermination::DownstreamWriteError;
                    }
                }
                return StreamTermination::NormalEof;
            }
            Ok(n) => {
                let chunk = if let Some(filter) = filter.as_mut() {
                    filter.feed(&buf[..n])
                } else {
                    buf[..n].to_vec()
                };
                if !chunk.is_empty() && emit(&chunk).is_err() {
                    return StreamTermination::DownstreamWriteError;
                }
            }
            Err(e) => {
                if emit(&stream_error_event(&e.to_string())).is_err() {
                    return StreamTermination::DownstreamWriteError;
                }
                return StreamTermination::UpstreamReadError;
            }
        }
    }
}

fn handle_stream(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    mut filter: Option<StreamFilter>,
) {
    if write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
    )
    .and_then(|_| stream.flush())
    .is_err()
    {
        return;
    }
    let (tx, rx) = mpsc::channel();
    let cfg_for_open = cfg.clone();
    thread::spawn(move || {
        let _ = tx.send(messages::open_stream(&cfg_for_open, body));
    });
    let upstream = loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => break result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if write_chunk(stream, b": csswitch-keepalive\n\n").is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(messages::UpstreamError {
                    status: 502,
                    upstream_status: None,
                    detail: "upstream stream failed".to_string(),
                });
            }
        }
    };
    match upstream {
        Ok(mut upstream) => {
            let termination = forward_stream_body(
                &mut upstream.response,
                &upstream.first,
                &mut filter,
                |chunk| write_chunk(stream, chunk),
            );
            match termination {
                StreamTermination::NormalEof => {
                    if let Some(filter) = filter.as_ref() {
                        filter.log_stats();
                    }
                }
                StreamTermination::UpstreamReadError => {}
                StreamTermination::DownstreamWriteError => return,
            }
        }
        Err(e) => {
            if write_chunk(stream, &stream_error_event(&e.detail)).is_err() {
                return;
            }
        }
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
}

fn log_relay_metadata(metadata: &AnthropicMetadata, is_stream: bool, message_count: usize) {
    let rules = if metadata.rule_ids.is_empty() {
        "-".to_string()
    } else {
        metadata.rule_ids.join(",")
    };
    eprintln!(
        "POST /v1/messages relay target={} stream={} msgs={} rules={}",
        metadata.target_model, is_stream, message_count, rules
    );
}

fn log_responses_metadata(
    transformed: &Value,
    metadata: &openai_responses::ResponsesMetadata,
    is_stream: bool,
) {
    let rules = if metadata.rule_ids.is_empty() {
        "-".to_string()
    } else {
        metadata.rule_ids.join(",")
    };
    let target_model = transformed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input_count = transformed
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let tool_count = transformed
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    eprintln!(
        "POST /v1/messages provider=openai-responses target={} stream={} input={} tools={} rules={}",
        target_model, is_stream, input_count, tool_count, rules
    );
}

fn known_tools_from_request(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(tools) = raw.get("tools").and_then(Value::as_array) else {
        return out;
    };
    for tool in tools {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let schema = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({}));
        out.insert(name.to_string(), schema);
    }
    out
}

fn dsml_stream_filter(
    cfg: &GatewayConfig,
    known_tools: &Map<String, Value>,
    request_nonce: Option<&str>,
) -> Option<StreamFilter> {
    if cfg.provider != "deepseek" || known_tools.is_empty() {
        return None;
    }
    match cfg.shim_mode.as_str() {
        "detect" => Some(StreamFilter::DsmlDetect(DsmlDetector::new())),
        "rewrite" => request_nonce.map(|nonce| {
            StreamFilter::DsmlRewrite(DsmlStreamRewriter::new(known_tools.clone(), nonce))
        }),
        _ => None,
    }
}

fn apply_dsml_nonstream(
    cfg: &GatewayConfig,
    known_tools: &Map<String, Value>,
    body: Vec<u8>,
    request_nonce: Option<&str>,
) -> Vec<u8> {
    if cfg.provider != "deepseek" || known_tools.is_empty() {
        return body;
    }
    match cfg.shim_mode.as_str() {
        "detect" => {
            let mut detector = DsmlDetector::new();
            detector.feed(&body);
            if detector.found {
                eprintln!("deepseek nonstream DSML detect found=true");
            }
            body
        }
        "rewrite" => {
            let Some(request_nonce) = request_nonce else {
                return body;
            };
            let rewritten =
                crate::dsml_shim::rewrite_nonstream_body(&body, known_tools, request_nonce);
            if rewritten != body {
                eprintln!("deepseek nonstream DSML rewrite applied=true");
            }
            rewritten
        }
        _ => body,
    }
}

fn handle_messages(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    body: Vec<u8>,
    request_nonces: Option<&RequestNonceGenerator>,
    relay_models: &models::RelayModelCache,
) {
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            invalid_request_json(stream, &e.to_string());
            return;
        }
    };
    let known_tools = known_tools_from_request(&raw);
    let is_stream = raw.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let dsml_request_nonce =
        (cfg.provider == "deepseek" && cfg.shim_mode == "rewrite" && !known_tools.is_empty())
            .then(|| request_nonces.map(RequestNonceGenerator::next_nonce))
            .flatten();
    if cfg.provider == "qwen"
        || cfg.provider == "openai-custom"
        || cfg.provider == "openai-responses"
    {
        let model_id = raw
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("claude-sonnet-5")
            .to_string();
        let transformed = if cfg.provider == "openai-responses" {
            openai_responses::anthropic_to_openai(
                &raw,
                cfg.forced_model.as_deref(),
                openai_responses::is_dashscope_responses_endpoint(&cfg.provider, &cfg.upstream_url),
            )
            .map(|(body, metadata)| (body, Some(metadata)))
        } else if cfg.provider == "openai-custom" {
            openai_chat::anthropic_to_openai_custom(&raw, cfg.forced_model.as_deref())
                .map(|body| (body, None))
        } else {
            openai_chat::anthropic_to_openai(&raw).map(|body| (body, None))
        };
        let (transformed, responses_metadata) = match transformed {
            Ok(result) => result,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        };
        let body = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        if let Some(metadata) = responses_metadata.as_ref() {
            log_responses_metadata(&transformed, metadata, is_stream);
        }
        match messages::post_nonstream(cfg, body) {
            Ok(resp) => {
                let openai_resp: Value = match serde_json::from_slice(&resp.body) {
                    Ok(v) => v,
                    Err(e) => {
                        api_error_json(stream, 502, &e.to_string());
                        return;
                    }
                };
                let anthropic_resp = if cfg.provider == "openai-responses" {
                    openai_responses::openai_to_anthropic(&openai_resp, &model_id)
                } else {
                    openai_chat::openai_to_anthropic(&openai_resp, &model_id)
                };
                if is_stream {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
                    );
                    for (event, data) in openai_chat::replay_as_sse_events(&anthropic_resp) {
                        if write_chunk(stream, &sse_event(&event, &data)).is_err() {
                            return;
                        }
                    }
                    let _ = stream.write_all(b"0\r\n\r\n");
                    let _ = stream.flush();
                } else {
                    write_json(stream, 200, "OK", anthropic_resp);
                }
            }
            Err(e) => api_error_json(stream, e.status, &e.detail),
        }
        return;
    }
    if cfg.provider == "relay" {
        let relay_models = relay_models.snapshot();
        let (transformed, metadata) = match anthropic_compat::transform_relay_request(
            raw,
            cfg.forced_model.as_deref(),
            &relay_models,
            cfg.relay_thinking.as_deref(),
            &cfg.upstream_url,
        ) {
            Ok(result) => result,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        };
        let message_count = transformed
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        log_relay_metadata(&metadata, is_stream, message_count);
        let transformed = match serde_json::to_vec(&transformed) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e.to_string());
                return;
            }
        };
        if is_stream {
            let filter = if metadata.target_model.to_ascii_lowercase().contains("kimi") {
                Some(StreamFilter::Kimi(KimiServerToolFilter::new()))
            } else {
                None
            };
            handle_stream(stream, cfg, transformed, filter);
            return;
        }
        match messages::post_nonstream(cfg, transformed) {
            Ok(resp) => write_response(
                stream,
                resp.status,
                status_reason(resp.status),
                &resp.content_type,
                &resp.body,
            ),
            Err(e) => api_error_json(stream, e.status, &e.detail),
        }
        return;
    }
    let transformed = match policy::transform_request(raw) {
        Ok(body) => body,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    if is_stream {
        let filter = dsml_stream_filter(cfg, &known_tools, dsml_request_nonce.as_deref());
        handle_stream(stream, cfg, transformed, filter);
        return;
    }
    match messages::post_nonstream(cfg, transformed) {
        Ok(resp) => {
            let body =
                apply_dsml_nonstream(cfg, &known_tools, resp.body, dsml_request_nonce.as_deref());
            write_response(
                stream,
                resp.status,
                status_reason(resp.status),
                &resp.content_type,
                &body,
            )
        }
        Err(e) => api_error_json(stream, e.status, &e.detail),
    }
}

fn handle_post(
    stream: &mut TcpStream,
    cfg: &GatewayConfig,
    target: &str,
    head: &RequestHead,
    request_nonces: Option<&RequestNonceGenerator>,
    relay_models: &models::RelayModelCache,
) {
    let path = match strip_path_secret(dequery(target), cfg.auth_secret.as_deref()) {
        AuthResult::Ok(path) => path,
        AuthResult::Forbidden => {
            forbidden_json(stream);
            return;
        }
    };
    let len = match content_length(&head.headers) {
        Ok(len) => len,
        Err(e) => {
            invalid_request_json(stream, &e);
            return;
        }
    };
    let body = if len == 0 {
        b"{}".to_vec()
    } else {
        match read_body(stream, len) {
            Ok(body) => body,
            Err(e) => {
                invalid_request_json(stream, &e);
                return;
            }
        }
    };
    if path != "/v1/messages" {
        not_found_json(stream, &path);
        return;
    }
    handle_messages(stream, cfg, body, request_nonces, relay_models);
}

#[cfg(unix)]
fn valid_bridge_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(unix)]
fn acquire_bridge_host_lock(bridge: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let lock_path = bridge.join(".csswitch-host.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("refusing unsafe Skill bridge host lock".into());
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Skill bridge already has an active host".into());
    }
    Ok(file)
}

#[cfg(unix)]
fn bridge_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(unix)]
fn bridge_operation_timeout(operation: &str) -> u64 {
    if operation == "install" {
        crate::skill_install::BRIDGE_INSTALL_RESPONSE_TIMEOUT_SECONDS
    } else {
        60
    }
}

#[cfg(unix)]
fn write_bridge_status(
    bridge: &std::path::Path,
    id: &str,
    operation: &str,
    started_at: u64,
    timeout_seconds: u64,
    progress: &BridgeProgress,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let now = bridge_now();
    let payload = json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status": "PROCESSING",
        "request_id": id,
        "operation": operation,
        "phase": progress.phase,
        "message": progress.message,
        "sequence": progress.sequence,
        "started_at": started_at,
        "updated_at": now,
        "elapsed_seconds": now.saturating_sub(started_at),
        "timeout_seconds": timeout_seconds,
        "deadline_at": started_at.saturating_add(timeout_seconds),
        "terminal_grace_seconds": 5,
        "poll_after_seconds": 3,
        "response_filename": format!("{id}.response.json")
    });
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = bridge.join(format!(
        ".{id}.status.{}.{}.{}.tmp",
        std::process::id(),
        progress.sequence,
        nonce
    ));
    let target = bridge.join(format!("{id}.status.json"));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, &payload).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(&temp, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        error.to_string()
    })
}

#[cfg(unix)]
fn bridge_final_response_exists(path: &std::path::Path) -> Result<bool, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0 =>
        {
            Ok(true)
        }
        Ok(_) => Err("invalid existing Skill bridge response".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(unix)]
fn write_bridge_response_once(
    bridge: &std::path::Path,
    id: &str,
    response: &Value,
) -> Result<bool, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let target = bridge.join(format!("{id}.response.json"));
    if bridge_final_response_exists(&target)? {
        return Ok(false);
    }
    let temp = bridge.join(format!(".{id}.response.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, response).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let linked = match std::fs::hard_link(&temp, &target) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error.to_string());
        }
    };
    std::fs::remove_file(&temp).map_err(|error| error.to_string())?;
    Ok(linked)
}

#[cfg(unix)]
fn cleanup_bridge_processing(bridge: &std::path::Path, id: &str) {
    let _ = std::fs::remove_file(bridge.join(format!("{id}.processing")));
    let _ = std::fs::remove_file(bridge.join(format!("{id}.status.json")));
}

#[cfg(unix)]
fn finalize_bridge_processing(
    bridge: &std::path::Path,
    id: &str,
    response: &Value,
) -> Result<(), String> {
    write_bridge_response_once(bridge, id, response)?;
    cleanup_bridge_processing(bridge, id);
    Ok(())
}

#[cfg(unix)]
fn recover_orphaned_bridge_processing(bridge: &std::path::Path) -> Result<(), String> {
    for entry in std::fs::read_dir(bridge).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".processing") else {
            continue;
        };
        if !valid_bridge_id(id) {
            continue;
        }
        let response = json!({
            "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
            "status": "REQUEST_INTERRUPTED",
            "request_id": id,
            "retryable": true,
            "request_terminal": true,
            "automatic_retry_allowed": false,
            "directory_commit": null,
            "attach_state": "UNKNOWN",
            "restart_required": false,
            "message": "CSSwitch 在处理该请求期间中断。宿主已清理遗留 .processing；请重新调用相同工具，让 CSSwitch 验证真实落盘和绑定状态后安全恢复。"
        });
        finalize_bridge_processing(bridge, id, &response)?;
    }
    Ok(())
}

#[cfg(unix)]
fn start_skill_install_bridge(cfg: &GatewayConfig) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let (Some(bridge), Some(data_dir), Some(bridge_token)) = (
        &cfg.skill_bridge_dir,
        &cfg.skill_data_dir,
        &cfg.skill_bridge_token,
    ) else {
        return Ok(());
    };
    let name = bridge
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !bridge.is_absolute() || !name.starts_with("CSSwitch-Skill-Bridge-") {
        return Err("refusing unsafe Skill install bridge path".into());
    }
    reject_bridge_symlinks(bridge)?;
    match std::fs::symlink_metadata(bridge) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err("refusing non-directory Skill install bridge path".into())
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(bridge).map_err(|error| error.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    }
    std::fs::set_permissions(bridge, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(bridge).map_err(|error| error.to_string())?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err("refusing Skill install bridge owned by another user".into());
    }
    let host_lock = acquire_bridge_host_lock(bridge)?;
    recover_orphaned_bridge_processing(bridge)?;
    let bridge = bridge.clone();
    let data_dir = data_dir.clone();
    let bridge_token = bridge_token.clone();
    let science_host_context = cfg.science_host_context.clone();
    thread::spawn(move || {
        let _host_lock = host_lock;
        let mut used_request_ids = HashMap::new();
        loop {
            let entries = match std::fs::read_dir(&bridge) {
                Ok(entries) => entries,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".request.json") else {
                    continue;
                };
                if !valid_bridge_id(id) {
                    continue;
                }
                let processing = bridge.join(format!("{id}.processing"));
                if std::fs::rename(entry.path(), &processing).is_err() {
                    continue;
                }
                let target = bridge.join(format!("{id}.response.json"));
                match bridge_final_response_exists(&target) {
                    Ok(true) => {
                        cleanup_bridge_processing(&bridge, id);
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("Skill bridge refused existing response for {id}: {error}");
                        let progress = BridgeProgress {
                            phase: "finalization_failed".into(),
                            message:
                                "检测到非法的既有最终响应；请求未执行，.processing 已保留供安全恢复"
                                    .into(),
                            sequence: 0,
                        };
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            "unknown",
                            bridge_now(),
                            bridge_operation_timeout("unknown"),
                            &progress,
                        );
                        continue;
                    }
                }
                let request = read_regular_bridge_request(&processing)
                    .ok()
                    .and_then(|body| serde_json::from_slice::<Value>(&body).ok());
                let operation = request
                    .as_ref()
                    .and_then(|request| request.get("operation"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let started_at = bridge_now();
                let timeout_seconds = bridge_operation_timeout(&operation);
                let progress = Arc::new(Mutex::new(BridgeProgress {
                    phase: "accepted".into(),
                    message: "宿主已接收唯一请求，正在开始处理".into(),
                    sequence: 0,
                }));
                if let Ok(snapshot) = progress.lock() {
                    let _ = write_bridge_status(
                        &bridge,
                        id,
                        &operation,
                        started_at,
                        timeout_seconds,
                        &snapshot,
                    );
                }
                let (stop_tx, stop_rx) = mpsc::channel();
                let heartbeat_bridge = bridge.clone();
                let heartbeat_id = id.to_string();
                let heartbeat_operation = operation.clone();
                let heartbeat_progress = Arc::clone(&progress);
                let heartbeat = thread::spawn(move || loop {
                    match stop_rx.recv_timeout(Duration::from_secs(BRIDGE_HEARTBEAT_SECONDS)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Ok(snapshot) = heartbeat_progress.lock() {
                                let _ = write_bridge_status(
                                    &heartbeat_bridge,
                                    &heartbeat_id,
                                    &heartbeat_operation,
                                    started_at,
                                    timeout_seconds,
                                    &snapshot,
                                );
                            }
                        }
                    }
                });
                let mut report_progress = |phase: &str, message: &str| {
                    if let Ok(mut state) = progress.lock() {
                        state.phase = phase.to_string();
                        state.message = message.to_string();
                        state.sequence = state.sequence.saturating_add(1);
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            &operation,
                            started_at,
                            timeout_seconds,
                            &state,
                        );
                    }
                };
                let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    request.map_or_else(bridge_request_failed, |request| {
                        if crate::skill_install::validate_bridge_request(
                            &bridge_token,
                            id,
                            &request,
                        )
                        .is_err()
                            || bridge_request_is_replay(
                                &mut used_request_ids,
                                id,
                                request
                                    .get("issued_at")
                                    .and_then(Value::as_u64)
                                    .unwrap_or_default(),
                                bridge_now(),
                            )
                        {
                            bridge_request_failed()
                        } else {
                            crate::skill_install::handle_bridge_request_with_progress(
                                &data_dir,
                                science_host_context.as_ref(),
                                &request,
                                &mut report_progress,
                            )
                        }
                    })
                }))
                .unwrap_or_else(|_| bridge_request_internal_failed());
                let _ = stop_tx.send(());
                let _ = heartbeat.join();
                match finalize_bridge_processing(&bridge, id, &response) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("Skill bridge final response write failed for {id}: {error}");
                        let snapshot = BridgeProgress {
                            phase: "finalization_failed".into(),
                            message: "最终响应写入失败；.processing 已保留，gateway 重启后会恢复"
                                .into(),
                            sequence: u64::MAX,
                        };
                        let _ = write_bridge_status(
                            &bridge,
                            id,
                            &operation,
                            started_at,
                            timeout_seconds,
                            &snapshot,
                        );
                    }
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    Ok(())
}

#[cfg(unix)]
fn bridge_request_is_replay(
    used: &mut HashMap<String, u64>,
    id: &str,
    issued_at: u64,
    now: u64,
) -> bool {
    used.retain(|_, issued| now.saturating_sub(*issued) <= BRIDGE_REPLAY_WINDOW_SECONDS);
    used.insert(id.to_string(), issued_at).is_some()
}

#[cfg(unix)]
fn reject_bridge_symlinks(path: &std::path::Path) -> Result<(), String> {
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("refusing symlink in Skill install bridge path".into())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_regular_bridge_request(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "invalid Skill bridge request")?;
    let metadata = file
        .metadata()
        .map_err(|_| "invalid Skill bridge request")?;
    if !metadata.is_file()
        || metadata.len() > 1024 * 1024
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("invalid Skill bridge request".into());
    }
    let mut body = Vec::with_capacity(metadata.len() as usize);
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "invalid Skill bridge request")?;
    if body.len() > 1024 * 1024 {
        return Err("invalid Skill bridge request".into());
    }
    Ok(body)
}

#[cfg(unix)]
fn bridge_request_failed() -> Value {
    json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status":"REQUEST_FAILED",
        "message":"本地 Skill 请求非法或已处理",
        "directory_commit":false,
        "restart_required":false
    })
}

#[cfg(unix)]
fn bridge_request_internal_failed() -> Value {
    json!({
        "schema_version": csswitch_skill_install_core::SCHEMA_VERSION,
        "status":"REQUEST_FAILED",
        "message":"本地 Skill 请求处理异常；宿主已停止该请求，可安全重试相同工具",
        "retryable":true,
        "directory_commit":null,
        "restart_required":false
    })
}

#[cfg(not(unix))]
fn start_skill_install_bridge(_cfg: &GatewayConfig) -> Result<(), String> {
    Ok(())
}

fn handle_one(
    cfg: GatewayConfig,
    mut stream: TcpStream,
    request_nonces: Option<Arc<RequestNonceGenerator>>,
    relay_models: Arc<models::RelayModelCache>,
) {
    let head = match read_head(&mut stream) {
        Ok(head) => head,
        Err(e) => {
            invalid_request_json(&mut stream, &e);
            return;
        }
    };
    match head.method.as_str() {
        "CONNECT" => connect::handle_connect(&head.target, stream),
        "GET" => handle_get(&mut stream, &cfg, &head.target, &relay_models),
        "POST" => {
            let target = head.target.clone();
            handle_post(
                &mut stream,
                &cfg,
                &target,
                &head,
                request_nonces.as_deref(),
                &relay_models,
            )
        }
        _ => not_found_json(&mut stream, &head.target),
    }
}

pub fn serve(cfg: GatewayConfig) -> Result<(), String> {
    let request_nonces = if cfg.provider == "deepseek" && cfg.shim_mode == "rewrite" {
        Some(Arc::new(RequestNonceGenerator::new()?))
    } else {
        None
    };
    let relay_models = Arc::new(models::RelayModelCache::default());
    let listener = TcpListener::bind(("127.0.0.1", cfg.port)).map_err(|e| e.to_string())?;
    if let Err(error) = start_skill_install_bridge(&cfg) {
        eprintln!("Skill install host unavailable (gateway continues): {error}");
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cfg = cfg.clone();
                let request_nonces = request_nonces.clone();
                let relay_models = Arc::clone(&relay_models);
                thread::spawn(move || handle_one(cfg, stream, request_nonces, relay_models));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::io::{Cursor, Error, ErrorKind, Read};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use serde_json::{json, Map, Value};

    #[cfg(unix)]
    use super::{
        acquire_bridge_host_lock, bridge_request_is_replay, finalize_bridge_processing,
        read_regular_bridge_request, recover_orphaned_bridge_processing,
        write_bridge_response_once, write_bridge_status, BridgeProgress,
    };
    use super::{
        forward_stream_body, stream_error_event, KimiServerToolFilter, RequestNonceGenerator,
        StreamFilter, StreamTermination,
    };
    use crate::dsml_shim::DsmlStreamRewriter;

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(Error::new(ErrorKind::ConnectionReset, "mock read failure"))
        }
    }

    struct CountingEofReader {
        reads: usize,
    }

    #[cfg(unix)]
    #[test]
    fn bridge_replay_window_rejects_duplicates_and_prunes_expired_ids() {
        let mut used = HashMap::new();
        assert!(!bridge_request_is_replay(&mut used, "first", 100, 100));
        assert!(bridge_request_is_replay(&mut used, "first", 100, 101));
        assert!(!bridge_request_is_replay(&mut used, "second", 286, 286));
        assert_eq!(used.len(), 1, "expired replay ids must not grow forever");
    }

    #[cfg(unix)]
    fn bridge_temp_dir(label: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::path::PathBuf::from("/private/tmp").join(format!(
            "csswitch-bridge-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn bridge_status_has_bounded_heartbeat_contract() {
        let bridge = bridge_temp_dir("status");
        let id = "1".repeat(32);
        let progress = BridgeProgress {
            phase: "download".into(),
            message: "downloading".into(),
            sequence: 4,
        };
        write_bridge_status(&bridge, &id, "install", 100, 1_800, &progress).unwrap();
        let status: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.status.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(status["status"], "PROCESSING");
        assert_eq!(status["phase"], "download");
        assert_eq!(status["sequence"], 4);
        assert_eq!(status["deadline_at"], 1_900);
        assert_eq!(status["poll_after_seconds"], 3);
        std::fs::remove_dir_all(bridge).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bridge_finalization_cleans_processing_for_success_failure_and_timeout() {
        let bridge = bridge_temp_dir("finalize");
        for (digit, status) in [
            ('2', "BUNDLE_INSTALLED_ATTACHED"),
            ('3', "INSTALL_FAILED"),
            ('4', "GITHUB_TIMEOUT"),
        ] {
            let id = digit.to_string().repeat(32);
            std::fs::write(bridge.join(format!("{id}.processing")), b"{}").unwrap();
            std::fs::write(bridge.join(format!("{id}.status.json")), b"{}").unwrap();
            finalize_bridge_processing(&bridge, &id, &json!({"status": status})).unwrap();
            assert!(!bridge.join(format!("{id}.processing")).exists());
            assert!(!bridge.join(format!("{id}.status.json")).exists());
            let response: Value = serde_json::from_slice(
                &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(response["status"], status);
        }
        std::fs::remove_dir_all(bridge).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_bridge_id_never_overwrites_first_final_response() {
        let bridge = bridge_temp_dir("duplicate");
        let id = "5".repeat(32);
        assert!(write_bridge_response_once(&bridge, &id, &json!({"status":"FIRST"})).unwrap());
        assert!(!write_bridge_response_once(&bridge, &id, &json!({"status":"SECOND"})).unwrap());
        let response: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(response["status"], "FIRST");
        std::fs::remove_dir_all(bridge).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn gateway_restart_recovers_orphaned_processing_to_final_response() {
        let bridge = bridge_temp_dir("recover");
        let id = "6".repeat(32);
        std::fs::write(bridge.join(format!("{id}.processing")), b"{}").unwrap();
        std::fs::write(bridge.join(format!("{id}.status.json")), b"{}").unwrap();
        recover_orphaned_bridge_processing(&bridge).unwrap();
        assert!(!bridge.join(format!("{id}.processing")).exists());
        assert!(!bridge.join(format!("{id}.status.json")).exists());
        let response: Value = serde_json::from_slice(
            &std::fs::read(bridge.join(format!("{id}.response.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(response["status"], "REQUEST_INTERRUPTED");
        assert_eq!(response["retryable"], true);
        assert_eq!(response["request_terminal"], true);
        assert_eq!(response["automatic_retry_allowed"], false);
        assert_eq!(response["attach_state"], "UNKNOWN");
        std::fs::remove_dir_all(bridge).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bridge_host_lock_allows_only_one_recovery_owner() {
        let bridge = bridge_temp_dir("host-lock");
        let first = acquire_bridge_host_lock(&bridge).unwrap();
        assert!(acquire_bridge_host_lock(&bridge).is_err());
        drop(first);
        let second = acquire_bridge_host_lock(&bridge).unwrap();
        drop(second);
        std::fs::remove_dir_all(bridge).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bridge_reader_rejects_symlink_and_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::time::{Instant, SystemTime, UNIX_EPOCH};

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::path::PathBuf::from("/private/tmp").join(format!(
            "csswitch-bridge-reader-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        let regular = root.join("regular");
        std::fs::write(&regular, b"{}").unwrap();
        std::fs::set_permissions(&regular, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_regular_bridge_request(&regular).unwrap(), b"{}");

        let link = root.join("link");
        symlink(&regular, &link).unwrap();
        assert!(read_regular_bridge_request(&link).is_err());

        let fifo = root.join("fifo");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(read_regular_bridge_request(&fifo).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn request_nonce_generator_is_sequential_and_id_safe() {
        let generator = RequestNonceGenerator::with_prefix([0xab; 16]);
        let first = generator.next_nonce();
        let second = generator.next_nonce();

        assert_eq!(first, format!("{}0000000000000001", "ab".repeat(16)));
        assert_eq!(second, format!("{}0000000000000002", "ab".repeat(16)));
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(second.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn request_nonce_generator_is_unique_under_concurrency() {
        const THREADS: usize = 16;
        const PER_THREAD: usize = 128;

        let generator = Arc::new(RequestNonceGenerator::with_prefix([0x3c; 16]));
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles = (0..THREADS)
            .map(|_| {
                let generator = Arc::clone(&generator);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    (0..PER_THREAD)
                        .map(|_| generator.next_nonce())
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();

        let nonces = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        let unique = nonces.iter().collect::<HashSet<_>>();

        assert_eq!(nonces.len(), THREADS * PER_THREAD);
        assert_eq!(unique.len(), nonces.len());
        assert!(nonces.iter().all(|nonce| nonce.len() == 48));
        assert!(nonces.iter().all(|nonce| nonce
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())));
    }

    impl Read for CountingEofReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            Ok(0)
        }
    }

    fn kimi_complete_then_partial() -> Vec<u8> {
        concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"buffered\"}}"
        )
        .as_bytes()
        .to_vec()
    }

    fn dsml_complete_start_then_partial_tool_delta() -> Vec<u8> {
        let start = format!(
            "event: content_block_start\ndata: {}\n\n",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            })
        );
        let leak = concat!(
            "<｜｜DSML｜｜tool_calls>",
            "<｜｜DSML｜｜invoke name=\"web_search\">",
            "<｜｜DSML｜｜parameter name=\"query\" string=\"true\">cats",
            "</｜｜DSML｜｜parameter>",
            "</｜｜DSML｜｜invoke>",
            "</｜｜DSML｜｜tool_calls>"
        );
        let delta = format!(
            "event: content_block_delta\ndata: {}",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": leak},
            })
        );
        format!("{start}{delta}").into_bytes()
    }

    fn dsml_filter() -> Option<StreamFilter> {
        let mut tools = Map::<String, Value>::new();
        tools.insert(
            "web_search".to_string(),
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            }),
        );
        Some(StreamFilter::DsmlRewrite(DsmlStreamRewriter::new(
            tools, "test",
        )))
    }

    #[test]
    fn kimi_read_error_is_terminal_and_does_not_finalize_buffer() {
        let first = b"event: content_block_start\ndata: {\"type\":\"content_block_start\"}";
        let mut upstream = FailingReader;
        let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
        let mut output = Vec::new();

        let termination = forward_stream_body(&mut upstream, first, &mut filter, |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        });

        assert_eq!(termination, StreamTermination::UpstreamReadError);
        assert_eq!(output, stream_error_event("mock read failure"));
        let StreamFilter::Kimi(filter) = filter.as_mut().unwrap() else {
            panic!("expected Kimi filter");
        };
        assert!(
            !filter.finalize().is_empty(),
            "buffer must remain unflushed"
        );
    }

    #[test]
    fn dsml_read_error_is_terminal_and_does_not_synthesize_buffered_tool() {
        let first = dsml_complete_start_then_partial_tool_delta();
        let mut upstream = FailingReader;
        let mut filter = dsml_filter();
        let mut output = Vec::new();

        let termination = forward_stream_body(&mut upstream, &first, &mut filter, |chunk| {
            output.extend_from_slice(chunk);
            Ok(())
        });

        let error = stream_error_event("mock read failure");
        assert_eq!(termination, StreamTermination::UpstreamReadError);
        assert!(output.ends_with(&error));
        assert!(!String::from_utf8_lossy(&output).contains("tool_use"));
        let StreamFilter::DsmlRewrite(filter) = filter.as_mut().unwrap() else {
            panic!("expected DSML rewrite filter");
        };
        let withheld = filter.finalize();
        assert!(
            String::from_utf8_lossy(&withheld).contains("tool_use"),
            "the buffered tool must still be present, proving the error path did not finalize it"
        );
    }

    #[test]
    fn normal_eof_finalizes_kimi_and_dsml_buffers() {
        let kimi_first = kimi_complete_then_partial();
        let mut kimi_upstream = Cursor::new(Vec::<u8>::new());
        let mut kimi_filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));
        let mut kimi_output = Vec::new();
        let kimi_termination =
            forward_stream_body(&mut kimi_upstream, &kimi_first, &mut kimi_filter, |chunk| {
                kimi_output.extend_from_slice(chunk);
                Ok(())
            });
        assert_eq!(kimi_termination, StreamTermination::NormalEof);
        assert!(String::from_utf8_lossy(&kimi_output).contains("buffered"));

        let dsml_first = dsml_complete_start_then_partial_tool_delta();
        let mut dsml_upstream = Cursor::new(Vec::<u8>::new());
        let mut dsml_filter = dsml_filter();
        let mut dsml_output = Vec::new();
        let dsml_termination =
            forward_stream_body(&mut dsml_upstream, &dsml_first, &mut dsml_filter, |chunk| {
                dsml_output.extend_from_slice(chunk);
                Ok(())
            });
        assert_eq!(dsml_termination, StreamTermination::NormalEof);
        assert!(String::from_utf8_lossy(&dsml_output).contains("tool_use"));
        assert!(!String::from_utf8_lossy(&dsml_output).contains("event: error"));
    }

    #[test]
    fn downstream_write_error_stops_before_more_reads_or_finalize() {
        let first = kimi_complete_then_partial();
        let mut upstream = CountingEofReader { reads: 0 };
        let mut filter = Some(StreamFilter::Kimi(KimiServerToolFilter::new()));

        let termination = forward_stream_body(&mut upstream, &first, &mut filter, |_chunk| {
            Err(Error::new(ErrorKind::BrokenPipe, "mock client closed"))
        });

        assert_eq!(termination, StreamTermination::DownstreamWriteError);
        assert_eq!(
            upstream.reads, 0,
            "must stop reading after client write failure"
        );
        let StreamFilter::Kimi(filter) = filter.as_mut().unwrap() else {
            panic!("expected Kimi filter");
        };
        assert!(
            String::from_utf8_lossy(&filter.finalize()).contains("buffered"),
            "buffer must remain unflushed after client write failure"
        );
    }
}
