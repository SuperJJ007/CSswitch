use serde_json::{json, Map, Value};

const RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED: &str = "provider.kimi.relay-thinking-enabled";
const RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE: &str = "tool.relay.input-schema-normalize";
const RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_FILTER: &str =
    "tool.kimi.web_search.server-tool-filter";
const RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE: &str = "history.kimi.failed-tail-normalize";
const RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY: &str = "tool.siliconflow.forced-named-to-any";
const SILICONFLOW_API_HOSTS: [&str; 2] = ["api.siliconflow.cn", "api.siliconflow.com"];
const MAX_KIMI_FRAME_BYTES: usize = 1024 * 1024;
const MAX_KIMI_THINKING_BLOCK_BYTES: usize = 2 * 1024 * 1024;
const MAX_KIMI_THINKING_BYTES: usize = 1024 * 1024;
const MAX_KIMI_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_RELAY_HISTORY_MESSAGES: usize = 2048;
const MAX_RELAY_HISTORY_BLOCKS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicMetadata {
    pub target_model: String,
    pub rule_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct KimiServerToolFilter {
    buf: Vec<u8>,
    next_upstream_index: u64,
    next_output_index: u64,
    active_output_block: Option<(u64, u64)>,
    dropped_server_tools: usize,
    dropped_empty_thinking: usize,
    dropped_server_block: Option<u64>,
    thinking: Option<BufferedThinkingBlock>,
}

#[derive(Debug)]
struct BufferedThinkingBlock {
    index: u64,
    frames: Vec<(Option<String>, Value)>,
    buffered_bytes: usize,
    thinking_bytes: usize,
    signature: String,
    signature_structurally_valid: bool,
}

impl KimiServerToolFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((frame, sep_len, rest)) = split_frame(&self.buf) {
            let sep = self.buf[frame.len()..frame.len() + sep_len].to_vec();
            let rewritten = self.rewrite_frame(&frame, &sep)?;
            out.extend_from_slice(&rewritten);
            self.buf = rest;
        }
        if self.buf.len() > MAX_KIMI_FRAME_BYTES {
            return Err("Kimi SSE frame exceeds the bounded buffer".into());
        }
        Ok(out)
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, String> {
        if self.thinking.is_some() {
            return Err("Kimi thinking block ended before content_block_stop".into());
        }
        if self.dropped_server_block.is_some() {
            return Err("Kimi server tool block ended before content_block_stop".into());
        }
        if self.active_output_block.is_some() {
            return Err("Kimi content block ended before content_block_stop".into());
        }
        if self.buf.iter().all(u8::is_ascii_whitespace) {
            self.buf.clear();
            Ok(Vec::new())
        } else {
            Err("Kimi SSE stream ended with a partial frame".into())
        }
    }

    pub fn dropped(&self) -> usize {
        self.dropped_server_tools + self.dropped_empty_thinking
    }

    pub fn dropped_empty_thinking(&self) -> usize {
        self.dropped_empty_thinking
    }

    fn validate_block_start(&self, idx: u64) -> Result<(), String> {
        if idx != self.next_upstream_index || self.active_output_block.is_some() {
            return Err("Kimi content block start index is invalid".into());
        }
        Ok(())
    }

    fn complete_upstream_block(&mut self) -> Result<(), String> {
        self.next_upstream_index = self
            .next_upstream_index
            .checked_add(1)
            .ok_or("Kimi content block index overflow")?;
        Ok(())
    }

    fn allocate_output_index(&mut self) -> Result<u64, String> {
        let mapped = self.next_output_index;
        self.next_output_index = self
            .next_output_index
            .checked_add(1)
            .ok_or("Kimi output block index overflow")?;
        Ok(mapped)
    }

    fn rewrite_frame(&mut self, frame: &[u8], sep: &[u8]) -> Result<Vec<u8>, String> {
        let (event, data) = event_and_data(frame);
        if data.is_empty() {
            if self.thinking.is_some() {
                return Err("Kimi thinking block contains an unsupported SSE frame".into());
            }
            return Ok(passthrough(frame, sep));
        }
        let Ok(mut obj) = serde_json::from_slice::<Value>(&data) else {
            if self.thinking.is_some() {
                return Err("Kimi thinking block contains invalid JSON".into());
            }
            return Ok(passthrough(frame, sep));
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str) else {
            if self.thinking.is_some() {
                return Err("Kimi thinking block event type is missing".into());
            }
            return Ok(passthrough(frame, sep));
        };
        if event.as_deref().is_some_and(|event| event != kind) {
            return Err("Kimi SSE event and JSON type do not match".into());
        }

        if self.thinking.is_some() {
            return self.rewrite_thinking_frame(event, obj, frame.len() + sep.len());
        }
        if self.dropped_server_block.is_some() {
            return self.rewrite_dropped_server_frame(event, obj);
        }
        if self.active_output_block.is_some() {
            return self.rewrite_output_block_frame(event, obj);
        }

        if kind == "content_block_start" {
            let idx = obj
                .get("index")
                .and_then(Value::as_u64)
                .ok_or("Kimi content block start index is invalid")?;
            self.validate_block_start(idx)?;
            let block_type = obj
                .get("content_block")
                .and_then(Value::as_object)
                .and_then(|block| block.get("type"))
                .and_then(Value::as_str);
            if matches!(
                block_type,
                Some("server_tool_use" | "web_search_tool_result")
            ) {
                self.dropped_server_block = Some(idx);
                self.dropped_server_tools += 1;
                return Ok(Vec::new());
            }
            if block_type == Some("thinking") {
                let block = obj
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or("Kimi thinking block start is invalid")?;
                let thinking = optional_kimi_string(block, "thinking")?;
                let signature = optional_kimi_string(block, "signature")?;
                if signature.len() > MAX_KIMI_SIGNATURE_BYTES {
                    return Err("Kimi thinking signature is too large".into());
                }
                let thinking_bytes = thinking.len();
                let signature_structurally_valid = kimi_signature_fragment_is_valid(signature);
                let signature = signature.to_string();
                if thinking_bytes > MAX_KIMI_THINKING_BYTES
                    || frame.len().saturating_add(sep.len()) > MAX_KIMI_THINKING_BLOCK_BYTES
                {
                    return Err("Kimi thinking block exceeds the bounded buffer".into());
                }
                self.thinking = Some(BufferedThinkingBlock {
                    index: idx,
                    frames: vec![(event, obj)],
                    buffered_bytes: frame.len() + sep.len(),
                    thinking_bytes,
                    signature,
                    signature_structurally_valid,
                });
                return Ok(Vec::new());
            }
            if let Some(obj_map) = obj.as_object_mut() {
                let mapped = self.next_output_index;
                obj_map.insert("index".to_string(), Value::Number(mapped.into()));
                self.active_output_block = Some((idx, mapped));
            }
            return Ok(render_sse(event.as_deref(), &obj));
        }
        Ok(passthrough(frame, sep))
    }

    fn rewrite_output_block_frame(
        &mut self,
        event: Option<String>,
        mut obj: Value,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi content block event type is missing")?
            .to_string();
        if kind == "error" {
            self.active_output_block = None;
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "ping" {
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let (upstream, mapped) = self
            .active_output_block
            .ok_or("Kimi content block state is missing")?;
        if obj.get("index").and_then(Value::as_u64) != Some(upstream) {
            return Err("Kimi content block index changed".into());
        }
        if !matches!(kind.as_str(), "content_block_delta" | "content_block_stop") {
            return Err("Kimi content block ended before content_block_stop".into());
        }
        if let Some(obj_map) = obj.as_object_mut() {
            obj_map.insert("index".to_string(), Value::Number(mapped.into()));
        }
        if kind == "content_block_stop" {
            self.active_output_block = None;
            self.complete_upstream_block()?;
            let allocated = self.allocate_output_index()?;
            debug_assert_eq!(allocated, mapped);
        }
        Ok(render_sse(event.as_deref(), &obj))
    }

    fn rewrite_dropped_server_frame(
        &mut self,
        event: Option<String>,
        obj: Value,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi server tool block event type is missing")?;
        if kind == "error" {
            self.dropped_server_block = None;
            return Ok(render_sse(event.as_deref(), &obj));
        }
        if kind == "ping" {
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let index = obj
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("Kimi server tool block index is missing")?;
        if Some(index) != self.dropped_server_block {
            return Err("Kimi server tool block index changed".into());
        }
        match kind {
            "content_block_delta" => {
                if obj.get("delta").and_then(Value::as_object).is_none() {
                    return Err("Kimi server tool delta is invalid".into());
                }
                Ok(Vec::new())
            }
            "content_block_stop" => {
                self.dropped_server_block = None;
                self.complete_upstream_block()?;
                Ok(Vec::new())
            }
            _ => Err("Kimi server tool block ended before content_block_stop".into()),
        }
    }

    fn rewrite_thinking_frame(
        &mut self,
        event: Option<String>,
        obj: Value,
        frame_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        let kind = obj
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Kimi thinking block event type is missing")?;
        if kind == "error" {
            self.thinking = None;
            return Ok(render_sse(event.as_deref(), &obj));
        }
        let thinking = self
            .thinking
            .as_mut()
            .ok_or("Kimi thinking block state is missing")?;
        thinking.buffered_bytes = thinking
            .buffered_bytes
            .checked_add(frame_bytes)
            .ok_or("Kimi thinking block exceeds the bounded buffer")?;
        if thinking.buffered_bytes > MAX_KIMI_THINKING_BLOCK_BYTES {
            return Err("Kimi thinking block exceeds the bounded buffer".into());
        }
        match kind {
            "ping" => {
                thinking.frames.push((event, obj));
                Ok(Vec::new())
            }
            "content_block_delta" => {
                if obj.get("index").and_then(Value::as_u64) != Some(thinking.index) {
                    return Err("Kimi thinking block index changed".into());
                }
                let delta = obj
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or("Kimi thinking delta is invalid")?;
                match delta.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let part = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .ok_or("Kimi thinking delta content is invalid")?;
                        thinking.thinking_bytes = thinking
                            .thinking_bytes
                            .checked_add(part.len())
                            .ok_or("Kimi thinking content is too large")?;
                        if thinking.thinking_bytes > MAX_KIMI_THINKING_BYTES {
                            return Err("Kimi thinking content is too large".into());
                        }
                    }
                    Some("signature_delta") => {
                        let part = delta
                            .get("signature")
                            .and_then(Value::as_str)
                            .ok_or("Kimi thinking signature delta is invalid")?;
                        if thinking.signature.len().saturating_add(part.len())
                            > MAX_KIMI_SIGNATURE_BYTES
                        {
                            return Err("Kimi thinking signature is too large".into());
                        }
                        thinking.signature_structurally_valid &=
                            kimi_signature_fragment_is_valid(part);
                        thinking.signature.push_str(part);
                    }
                    _ => return Err("Kimi thinking delta type is unsupported".into()),
                }
                thinking.frames.push((event, obj));
                Ok(Vec::new())
            }
            "content_block_stop" => {
                if obj.get("index").and_then(Value::as_u64) != Some(thinking.index) {
                    return Err("Kimi thinking block index changed".into());
                }
                thinking.frames.push((event, obj));
                let mut thinking = self
                    .thinking
                    .take()
                    .ok_or("Kimi thinking block state is missing")?;
                self.complete_upstream_block()?;
                let has_valid_signature =
                    thinking.signature_structurally_valid && !thinking.signature.is_empty();
                if thinking.thinking_bytes == 0 && !has_valid_signature {
                    self.dropped_empty_thinking += 1;
                    let mut pings = Vec::new();
                    for (event, frame) in thinking.frames {
                        if frame.get("type").and_then(Value::as_str) == Some("ping") {
                            pings.extend_from_slice(&render_sse(event.as_deref(), &frame));
                        }
                    }
                    return Ok(pings);
                }
                if thinking.thinking_bytes > 0 && !has_valid_signature {
                    return Err("Kimi nonempty thinking has no valid signature".into());
                }
                let mapped = self.allocate_output_index()?;
                let mut out = Vec::new();
                for (event, mut frame) in thinking.frames.drain(..) {
                    if let Some(index) = frame.get_mut("index") {
                        *index = Value::Number(mapped.into());
                    }
                    out.extend_from_slice(&render_sse(event.as_deref(), &frame));
                }
                Ok(out)
            }
            _ => Err("Kimi thinking block ended before content_block_stop".into()),
        }
    }
}

fn optional_kimi_string<'a>(block: &'a Map<String, Value>, field: &str) -> Result<&'a str, String> {
    match block.get(field) {
        Some(Value::String(value)) => Ok(value),
        None => Ok(""),
        Some(_) => Err(format!("Kimi thinking block {field} is invalid")),
    }
}

fn kimi_signature_fragment_is_valid(signature: &str) -> bool {
    !signature
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
}

pub fn filter_kimi_nonstream_response(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut response: Value = serde_json::from_slice(body)
        .map_err(|_| "Kimi nonstream response is not valid JSON".to_string())?;
    let content = response
        .get_mut("content")
        .and_then(Value::as_array_mut)
        .ok_or("Kimi nonstream response content is invalid")?;
    if content.len() > 4096 {
        return Err("Kimi nonstream response has too many content blocks".into());
    }
    let mut kept = Vec::with_capacity(content.len());
    for block in content.drain(..) {
        let object = block
            .as_object()
            .ok_or("Kimi nonstream content block is invalid")?;
        if object.get("type").and_then(Value::as_str) != Some("thinking") {
            kept.push(block);
            continue;
        }
        let thinking = optional_kimi_string(object, "thinking")?;
        let signature = optional_kimi_string(object, "signature")?;
        if thinking.len() > MAX_KIMI_THINKING_BYTES {
            return Err("Kimi thinking content is too large".into());
        }
        if signature.len() > MAX_KIMI_SIGNATURE_BYTES {
            return Err("Kimi thinking signature is too large".into());
        }
        let has_valid_signature =
            !signature.is_empty() && kimi_signature_fragment_is_valid(signature);
        if thinking.is_empty() && !has_valid_signature {
            continue;
        }
        if !thinking.is_empty() && !has_valid_signature {
            return Err("Kimi nonempty thinking has no valid signature".into());
        }
        kept.push(block);
    }
    *content = kept;
    serde_json::to_vec(&response).map_err(|_| "Kimi nonstream response serialization failed".into())
}

fn split_frame(buf: &[u8]) -> Option<(Vec<u8>, usize, Vec<u8>)> {
    let lf = buf.windows(2).position(|window| window == b"\n\n");
    let crlf = buf.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (None, None) => None,
        (Some(i), None) => Some((buf[..i].to_vec(), 2, buf[i + 2..].to_vec())),
        (None, Some(i)) => Some((buf[..i].to_vec(), 4, buf[i + 4..].to_vec())),
        (Some(lf_i), Some(crlf_i)) if lf_i <= crlf_i => {
            Some((buf[..lf_i].to_vec(), 2, buf[lf_i + 2..].to_vec()))
        }
        (Some(_), Some(crlf_i)) => Some((buf[..crlf_i].to_vec(), 4, buf[crlf_i + 4..].to_vec())),
    }
}

fn event_and_data(frame: &[u8]) -> (Option<String>, Vec<u8>) {
    let normalized = String::from_utf8_lossy(frame).replace("\r\n", "\n");
    let mut event = None;
    let mut data = Vec::new();
    for line in normalized.split('\n') {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().as_bytes().to_vec());
        }
    }
    (event, data.join(b"\n".as_slice()))
}

fn render_sse(event: Option<&str>, obj: &Value) -> Vec<u8> {
    let data = serde_json::to_vec(obj).unwrap_or_else(|_| b"{}".to_vec());
    let mut out = Vec::new();
    if let Some(event) = event {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.extend_from_slice(b"\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&data);
    out.extend_from_slice(b"\n\n");
    out
}

fn passthrough(frame: &[u8], sep: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + sep.len());
    out.extend_from_slice(frame);
    out.extend_from_slice(sep);
    out
}

fn append_rule_id(rule_ids: &mut Vec<String>, rule_id: &str) {
    if !rule_ids.iter().any(|existing| existing == rule_id) {
        rule_ids.push(rule_id.to_string());
    }
}

fn is_siliconflow_anthropic_endpoint(endpoint: &str) -> bool {
    let raw_authority = endpoint
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if raw_authority.contains('@') {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = if let Some(without_dot) = host.strip_suffix('.') {
        if without_dot.ends_with('.') {
            return false;
        }
        without_dot
    } else {
        host
    };
    SILICONFLOW_API_HOSTS
        .iter()
        .any(|official| host.eq_ignore_ascii_case(official))
}

fn enabled_budget(max_tokens: Option<u64>) -> u64 {
    let default = 1024;
    match max_tokens {
        Some(value) if value > 0 => default.min(value.saturating_sub(1)).max(1),
        _ => default,
    }
}

fn is_forced_tool_choice(body: &Value) -> bool {
    body.get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        .map(|kind| kind == "any" || kind == "tool")
        .unwrap_or(false)
}

fn normalize_relay_thinking(body: &mut Value, relay_thinking: Option<&str>) {
    if relay_thinking == Some("enabled") {
        if is_forced_tool_choice(body) {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("tool_choice");
            }
        }
        let already_enabled = body
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            == Some("enabled");
        if !already_enabled {
            let budget = enabled_budget(body.get("max_tokens").and_then(Value::as_u64));
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        return;
    }

    if body
        .get("thinking")
        .and_then(Value::as_object)
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("auto")
    {
        if let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) {
            thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
        }
    }
}

fn normalize_relay_input_schema(schema: Option<&Value>) -> Value {
    let Some(Value::Object(obj)) = schema else {
        return json!({"type": "object", "properties": {}});
    };
    if obj.is_empty() {
        return json!({"type": "object", "properties": {}});
    }
    let mut out = obj.clone();
    let has_properties = out.get("properties").map(Value::is_object).unwrap_or(false);
    match out.get("type").and_then(Value::as_str) {
        None if has_properties => {
            out.insert("type".to_string(), Value::String("object".to_string()));
        }
        Some("object") => {}
        _ => return json!({"type": "object", "properties": {}}),
    }
    if !out.get("properties").map(Value::is_object).unwrap_or(false) {
        out.insert("properties".to_string(), json!({}));
    }
    if out.get("required").map(Value::is_array) == Some(false) {
        out.remove("required");
    }
    Value::Object(out)
}

fn degrade_missing_tool_choice(body: &mut Value) {
    let Some(choice) = body.get("tool_choice").and_then(Value::as_object) else {
        return;
    };
    if choice.get("type").and_then(Value::as_str) != Some("tool") {
        return;
    }
    let choice_name = choice.get("name").and_then(Value::as_str).unwrap_or("");
    let exists = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(choice_name));
    if !exists {
        body["tool_choice"] = json!({"type": "auto"});
    }
}

fn normalize_relay_tools(body: &mut Value, rule_ids: &mut Vec<String>) {
    let Some(tools) = body.get("tools") else {
        return;
    };
    let Some(tool_items) = tools.as_array() else {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
        degrade_missing_tool_choice(body);
        return;
    };

    let mut normalized = Vec::new();
    for tool in tool_items {
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut clean = match tool {
            Value::Object(obj) => obj.clone(),
            _ => Map::new(),
        };
        clean.insert(
            "input_schema".to_string(),
            normalize_relay_input_schema(tool.get("input_schema")),
        );
        normalized.push(Value::Object(clean));
    }
    append_rule_id(rule_ids, RULE_TOOL_RELAY_INPUT_SCHEMA_NORMALIZE);
    if normalized.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(normalized);
    }
    degrade_missing_tool_choice(body);
}

fn filter_kimi_server_tools(body: &mut Value, target_model: &str, rule_ids: &mut Vec<String>) {
    normalize_relay_tools(body, rule_ids);
    if !target_model.to_ascii_lowercase().contains("kimi") {
        return;
    }
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return;
    };
    let filtered: Vec<Value> = tools
        .iter()
        .filter(|tool| tool.get("name").and_then(Value::as_str) != Some("web_search"))
        .cloned()
        .collect();
    if filtered.len() == tools.len() {
        return;
    }
    append_rule_id(rule_ids, RULE_TOOL_KIMI_WEB_SEARCH_SERVER_TOOL_FILTER);
    if filtered.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
        }
    } else {
        body["tools"] = Value::Array(filtered);
    }
    degrade_missing_tool_choice(body);
}

fn zero_information_kimi_block(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("server_tool_use" | "web_search_tool_result") => true,
        Some("text") => block.get("text").and_then(Value::as_str) == Some(""),
        Some("thinking") => {
            block.get("thinking").and_then(Value::as_str) == Some("")
                && block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
        }
        _ => false,
    }
}

fn message_has_no_information(message: &Value) -> bool {
    match message.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.is_empty(),
        Some(Value::Array(blocks)) => blocks.is_empty(),
        _ => false,
    }
}

fn normalize_kimi_failed_history_tail(
    body: &mut Value,
    target_model: &str,
    rule_ids: &mut Vec<String>,
) -> Result<(), String> {
    if !target_model.to_ascii_lowercase().contains("kimi") {
        return Ok(());
    }
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if messages.len() > MAX_RELAY_HISTORY_MESSAGES {
        return Err("Kimi history has too many messages".into());
    }
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if blocks.len() > MAX_RELAY_HISTORY_BLOCKS {
            return Err("Kimi history message has too many content blocks".into());
        }
        let before = blocks.len();
        blocks.retain(|block| !zero_information_kimi_block(block));
        changed |= blocks.len() != before;
    }

    // A failed Science turn can leave an empty assistant placeholder either at
    // the tail or immediately before the user's edited resend. Remove only that
    // zero-information placeholder; successful assistant turns are untouched.
    let trailing_empty_assistant = messages.last().is_some_and(|message| {
        message.get("role").and_then(Value::as_str) == Some("assistant")
            && message_has_no_information(message)
    });
    if trailing_empty_assistant {
        messages.pop();
        changed = true;
    }
    if messages.len() >= 2
        && messages
            .last()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user")
    {
        let index = messages.len() - 2;
        if messages[index].get("role").and_then(Value::as_str) == Some("assistant")
            && message_has_no_information(&messages[index])
        {
            messages.remove(index);
            changed = true;
        }
    }
    if changed && messages.is_empty() {
        return Err("Kimi history is empty after removing a failed placeholder".into());
    }
    if changed {
        append_rule_id(rule_ids, RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE);
    }
    Ok(())
}

fn validate_relay_tool_history(body: &Value) -> Result<(), String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = std::collections::BTreeSet::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or("relay history message role is invalid")?;
        if !matches!(role, "user" | "assistant") {
            return Err("relay history message role is invalid".into());
        }
        let blocks = match message.get("content") {
            Some(Value::Array(blocks)) => blocks.as_slice(),
            Some(Value::String(_)) | Some(Value::Null) | None => &[],
            _ => return Err("relay history message content is invalid".into()),
        };
        if blocks.len() > MAX_RELAY_HISTORY_BLOCKS {
            return Err("relay history message has too many content blocks".into());
        }
        if role == "assistant" {
            if !pending.is_empty() {
                return Err(
                    "relay history has unresolved tool calls before an assistant turn".into(),
                );
            }
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        return Err("relay history tool_result must have the user role".into())
                    }
                    Some("tool_use") => {}
                    _ => continue,
                }
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or("relay history tool_use id is invalid")?;
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && name.len() <= 256)
                    .ok_or("relay history tool_use name is invalid")?;
                if !block.get("input").is_some_and(Value::is_object) {
                    return Err("relay history tool_use input is invalid".into());
                }
                if !seen.insert(id.to_string()) {
                    return Err("relay history contains a duplicate tool_use id".into());
                }
                pending.insert(id.to_string());
            }
        } else {
            let mut results = std::collections::BTreeSet::new();
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        return Err("relay history tool_use must have the assistant role".into())
                    }
                    Some("tool_result") => {}
                    _ => continue,
                }
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && id.len() <= 256)
                    .ok_or("relay history tool_result id is invalid")?;
                if !pending.contains(id) || !results.insert(id.to_string()) {
                    return Err("relay history contains an orphan or duplicate tool_result".into());
                }
            }
            if !pending.is_empty() {
                if results != pending {
                    return Err("relay history has incomplete tool results".into());
                }
                pending.clear();
            } else if !results.is_empty() {
                return Err("relay history contains an orphan tool_result".into());
            }
        }
    }
    if !pending.is_empty() {
        return Err("relay history ends with unresolved tool calls".into());
    }
    Ok(())
}

fn apply_siliconflow_tool_choice_compat(
    body: &mut Value,
    upstream_url: &str,
    rule_ids: &mut Vec<String>,
) {
    if !is_siliconflow_anthropic_endpoint(upstream_url) {
        return;
    }
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| !tools.is_empty())
        .unwrap_or(false);
    if !has_tools {
        return;
    }
    let is_forced_named = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("tool");
    if !is_forced_named {
        return;
    }
    body["tool_choice"] = json!({"type": "any"});
    append_rule_id(rule_ids, RULE_TOOL_SILICONFLOW_FORCED_NAMED_TO_ANY);
}

pub fn transform_relay_request(
    mut body: Value,
    target_model: &str,
    relay_thinking: Option<&str>,
    upstream_url: &str,
) -> Result<(Value, AnthropicMetadata), String> {
    let obj = body
        .as_object_mut()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }

    if target_model.is_empty() {
        return Err("resolved upstream model is required".into());
    }
    let target_model = target_model.to_string();
    let mut rule_ids = Vec::new();
    if relay_thinking == Some("enabled") && target_model.to_ascii_lowercase().contains("kimi") {
        append_rule_id(&mut rule_ids, RULE_PROVIDER_KIMI_RELAY_THINKING_ENABLED);
    }
    obj.insert("model".to_string(), Value::String(target_model.clone()));
    normalize_kimi_failed_history_tail(&mut body, &target_model, &mut rule_ids)?;
    validate_relay_tool_history(&body)?;
    normalize_relay_thinking(&mut body, relay_thinking);
    filter_kimi_server_tools(&mut body, &target_model, &mut rule_ids);
    apply_siliconflow_tool_choice_compat(&mut body, upstream_url, &mut rule_ids);
    Ok((
        body,
        AnthropicMetadata {
            target_model,
            rule_ids,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        filter_kimi_nonstream_response, is_siliconflow_anthropic_endpoint, transform_relay_request,
        KimiServerToolFilter, MAX_KIMI_FRAME_BYTES,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/relay_anthropic.json")).unwrap()
    }

    #[test]
    fn kimi_history_removes_only_failed_placeholders_and_preserves_complete_tool_rounds() {
        let request = json!({
            "messages": [
                {"role": "user", "content": "round one"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "inspect", "signature": "opaque"},
                    {"type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {"q": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "found"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "round one done"}]},
                {"role": "user", "content": "round two"},
                {"role": "assistant", "content": [
                    {"type": "server_tool_use", "name": "web_search"},
                    {"type": "web_search_tool_result", "content": []},
                    {"type": "thinking", "thinking": "", "signature": ""},
                    {"type": "text", "text": ""}
                ]},
                {"role": "user", "content": "round two edited and resent"}
            ]
        });
        let (mapped, metadata) = transform_relay_request(
            request,
            "kimi-k3",
            Some("enabled"),
            "https://example.invalid/v1/messages",
        )
        .unwrap();
        let messages = mapped["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[1]["content"][1]["id"], "toolu_1");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[3]["content"][0]["text"], "round one done");
        assert_eq!(messages[5]["content"], "round two edited and resent");
        assert!(metadata
            .rule_ids
            .iter()
            .any(|rule| rule == super::RULE_HISTORY_KIMI_FAILED_TAIL_NORMALIZE));
    }

    #[test]
    fn relay_history_rejects_orphan_duplicate_and_unresolved_tools() {
        for messages in [
            json!([{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "missing", "content": "x"}]}]),
            json!([{"role": "user", "content": [{"type": "tool_use", "id": "wrong_role", "name": "lookup", "input": {}}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_result", "tool_use_id": "wrong_role", "content": "x"}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_use", "id": "missing_name", "input": {}}]}]),
            json!([{"role": "assistant", "content": [{"type": "tool_use", "id": "missing_input", "name": "lookup"}]}]),
            json!([
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "dup", "name": "a", "input": {}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "dup", "content": "x"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "dup", "name": "b", "input": {}}]},
            ]),
            json!([
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "pending", "name": "a", "input": {}}]},
            ]),
        ] {
            assert!(transform_relay_request(
                json!({"messages": messages}),
                "kimi-k3",
                Some("enabled"),
                "https://example.invalid/v1/messages",
            )
            .is_err());
        }
    }

    #[test]
    fn siliconflow_endpoint_matching_is_exact_and_url_parsed() {
        for endpoint in [
            "https://api.siliconflow.cn",
            "https://API.SILICONFLOW.CN/v1/messages",
            "http://api.siliconflow.com./anthropic/v1/messages",
        ] {
            assert!(is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "ftp://api.siliconflow.cn/v1/messages",
            "https://sub.api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn.evil/v1/messages",
            "https://api.siliconflow.com.evil/v1/messages",
            "https://api.siliconflow.cn@evil.example/v1/messages",
            "https://evil@api.siliconflow.cn/v1/messages",
            "https://@api.siliconflow.cn/v1/messages",
            "https://:pass@api.siliconflow.cn/v1/messages",
            "https://user:@api.siliconflow.cn/v1/messages",
            "https://api.siliconflow.cn../v1/messages",
            "https://evil.example/api.siliconflow.cn/v1/messages",
            "not a url api.siliconflow.cn",
        ] {
            assert!(!is_siliconflow_anthropic_endpoint(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn siliconflow_tool_choice_fixture_matrix_matches_python() {
        let fixture = fixture();
        let cases = fixture["siliconflow_tool_choice_cases"].as_array().unwrap();
        for case in cases {
            let (mapped, metadata) = transform_relay_request(
                case["request"].clone(),
                case["mapped"]["model"].as_str().unwrap(),
                None,
                case["endpoint"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(mapped, case["mapped"], "{}", case["name"]);
            let expected_rules: Vec<String> =
                serde_json::from_value(case["rule_ids"].clone()).unwrap();
            assert_eq!(metadata.rule_ids, expected_rules, "{}", case["name"]);
        }
    }

    #[test]
    fn relay_snaps_bare_model_and_preserves_max_tokens() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["plain_request"].clone(),
            fixture["plain_target_model"].as_str().unwrap(),
            None,
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["plain_mapped"]);
        assert_eq!(metadata.target_model, fixture["plain_target_model"]);
        assert_eq!(metadata.rule_ids, Vec::<String>::new());
    }

    #[test]
    fn relay_force_model_overrides_shell() {
        let fixture = fixture();
        let (mapped, metadata) =
            transform_relay_request(fixture["force_request"].clone(), "MiniMax-M2", None, "")
                .unwrap();
        assert_eq!(mapped, fixture["force_mapped"]);
        assert_eq!(metadata.target_model, fixture["force_target_model"]);
        assert_eq!(metadata.rule_ids, Vec::<String>::new());
    }

    #[test]
    fn relay_kimi_thinking_and_tool_quirks_match_python_fixture() {
        let fixture = fixture();
        let (mapped, metadata) = transform_relay_request(
            fixture["kimi_request"].clone(),
            "kimi-k2.7-code",
            Some("enabled"),
            "",
        )
        .unwrap();
        assert_eq!(mapped, fixture["kimi_mapped"]);
        assert_eq!(metadata.target_model, fixture["kimi_target_model"]);
        assert_eq!(
            metadata.rule_ids,
            vec![
                "provider.kimi.relay-thinking-enabled".to_string(),
                "tool.relay.input-schema-normalize".to_string(),
                "tool.kimi.web_search.server-tool-filter".to_string(),
            ]
        );
    }

    #[test]
    fn relay_kimi_removed_tool_choice_degrades_to_auto_without_enabled_thinking() {
        let (mapped, _metadata) = transform_relay_request(
            json!({
                "model": "claude-opus-4-8",
                "messages": [],
                "tool_choice": {"type": "tool", "name": "web_search"},
                "tools": [{"name": "web_search", "input_schema": {"type": "object"}}],
            }),
            "kimi-k2.7-code",
            None,
            "",
        )
        .unwrap();
        assert!(mapped.get("tools").is_none());
        assert_eq!(mapped["tool_choice"], json!({"type": "auto"}));
    }

    #[test]
    fn kimi_stream_filter_drops_server_tool_blocks_and_compacts_indexes() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"web_search_tool_result\",\"content\":[]}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":3}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n"
        );
        let mut filter = KimiServerToolFilter::new();
        let midpoint = sse.len() / 2;
        let mut out = filter.feed(&sse.as_bytes()[..midpoint]).unwrap();
        out.extend(filter.feed(&sse.as_bytes()[midpoint..]).unwrap());
        out.extend(filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("server_tool_use"));
        assert!(!text.contains("web_search_tool_result"));
        assert!(!text.contains("\"type\":\"thinking\""));
        assert!(text.contains("\"index\":1"));
        assert!(text.contains("\"text\":\"OK\""));
        assert_eq!(filter.dropped(), 3);
        assert_eq!(filter.dropped_empty_thinking(), 1);
    }

    #[test]
    fn kimi_stream_filter_preserves_signed_thinking_and_original_frame_order() {
        let sse = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"seed\",\"signature\":\"sig-a\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"thought\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"-tail\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        let mut out = Vec::new();
        for chunk in sse.as_bytes().chunks(13) {
            out.extend(filter.feed(chunk).unwrap());
        }
        out.extend(filter.finalize().unwrap());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"thinking\":\"seed\""));
        assert!(text.contains("\"thinking\":\"thought\""));
        assert!(text.contains("\"signature\":\"-tail\""));
        assert!(text.contains("\"index\":0"));
        assert!(text.contains("\"index\":1"));
        assert!(text.find("content_block_start").unwrap() < text.find("event: ping").unwrap());
        assert_eq!(filter.dropped(), 0);
    }

    #[test]
    fn kimi_stream_filter_drops_only_zero_information_thinking_and_fails_nonempty_unsigned() {
        let empty_invalid = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"bad signature\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        let output = filter.feed(empty_invalid.as_bytes()).unwrap();
        assert!(!String::from_utf8_lossy(&output).contains("thinking"));
        assert!(String::from_utf8_lossy(&output).contains("event: ping"));
        assert_eq!(filter.dropped_empty_thinking(), 1);

        let unsigned = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"secret\",\"signature\":\"\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut filter = KimiServerToolFilter::new();
        assert_eq!(
            filter.feed(unsigned.as_bytes()).unwrap_err(),
            "Kimi nonempty thinking has no valid signature"
        );
    }

    #[test]
    fn kimi_stream_filter_requires_dropped_server_tool_blocks_to_close() {
        let start = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"name\":\"web_search\"}}\n\n";
        let mut missing_stop = KimiServerToolFilter::new();
        assert!(missing_stop.feed(start.as_bytes()).unwrap().is_empty());
        assert_eq!(
            missing_stop.finalize().unwrap_err(),
            "Kimi server tool block ended before content_block_stop"
        );

        let mut wrong_index = KimiServerToolFilter::new();
        wrong_index.feed(start.as_bytes()).unwrap();
        assert_eq!(
            wrong_index
                .feed(b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":4}\n\n")
                .unwrap_err(),
            "Kimi server tool block index changed"
        );

        let mut early_terminal = KimiServerToolFilter::new();
        early_terminal.feed(start.as_bytes()).unwrap();
        assert_eq!(
            early_terminal
                .feed(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n")
                .unwrap_err(),
            "Kimi server tool block index is missing"
        );

        let mut malformed_delta = KimiServerToolFilter::new();
        malformed_delta.feed(start.as_bytes()).unwrap();
        assert_eq!(
            malformed_delta
                .feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0}\n\n")
                .unwrap_err(),
            "Kimi server tool delta is invalid"
        );
    }

    #[test]
    fn kimi_stream_filter_rejects_nonsequential_original_indexes_without_state_growth() {
        for invalid_start in [77_i64, -1] {
            let mut filter = KimiServerToolFilter::new();
            let frame = format!(
                "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{invalid_start},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n"
            );
            assert_eq!(
                filter.feed(frame.as_bytes()).unwrap_err(),
                "Kimi content block start index is invalid"
            );
        }

        let mut filter = KimiServerToolFilter::new();
        filter
            .feed(
                b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            )
            .unwrap();
        assert_eq!(filter.next_upstream_index, 1);
        assert_eq!(filter.next_output_index, 1);
        assert!(filter.active_output_block.is_none());
        assert_eq!(
            filter
                .feed(b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n")
                .unwrap_err(),
            "Kimi content block start index is invalid"
        );
    }

    #[test]
    fn kimi_stream_filter_bounds_incomplete_frames_without_bounding_the_whole_stream() {
        let frame = "event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let complete = frame.repeat((MAX_KIMI_FRAME_BYTES / frame.len()) + 2);
        let mut filter = KimiServerToolFilter::new();
        let output_bytes = complete
            .as_bytes()
            .chunks(8192)
            .map(|chunk| filter.feed(chunk).unwrap().len())
            .sum::<usize>();
        assert_eq!(output_bytes, complete.len());
        assert!(filter.finalize().unwrap().is_empty());

        let mut filter = KimiServerToolFilter::new();
        let incomplete = vec![b'x'; MAX_KIMI_FRAME_BYTES + 1];
        assert_eq!(
            filter.feed(&incomplete).unwrap_err(),
            "Kimi SSE frame exceeds the bounded buffer"
        );
    }

    #[test]
    fn kimi_nonstream_filter_preserves_signed_thinking_and_rejects_unsigned_content() {
        let body = json!({
            "id": "msg",
            "type": "message",
            "content": [
                {"type": "thinking", "thinking": "", "signature": ""},
                {"type": "thinking", "thinking": "kept", "signature": "opaque"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let filtered = filter_kimi_nonstream_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        let parsed: Value = serde_json::from_slice(&filtered).unwrap();
        assert_eq!(parsed["content"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["content"][0]["thinking"], "kept");
        assert_eq!(parsed["stop_reason"], "end_turn");
        assert_eq!(parsed["usage"]["output_tokens"], 2);

        let invalid = json!({
            "content": [{"type": "thinking", "thinking": "secret", "signature": ""}]
        });
        assert_eq!(
            filter_kimi_nonstream_response(&serde_json::to_vec(&invalid).unwrap()).unwrap_err(),
            "Kimi nonempty thinking has no valid signature"
        );
    }
}
