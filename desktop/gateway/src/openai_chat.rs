use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

type HmacSha256 = Hmac<Sha256>;

const REASONING_SIGNATURE_PREFIX: &str = "csswitch.openai-chat-thinking.v1";
const REASONING_SIGNATURE_PURPOSE: &str = "openai-chat-reasoning-round-trip";
const MAX_REASONING_BYTES: usize = 1024 * 1024;
const MAX_SIGNATURE_BYTES: usize = 64 * 1024;
const MAX_TOOL_CALLS: usize = 128;
const MAX_TOOL_ID_BYTES: usize = 256;
const MAX_TOOL_NAME_BYTES: usize = 256;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolBinding {
    id: String,
    name: String,
    arguments_digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReasoningSignaturePayload {
    version: u8,
    purpose: String,
    target_model: String,
    reasoning_digest: String,
    tools: Vec<ToolBinding>,
}

pub struct ReasoningSigner {
    key: [u8; 32],
}

impl ReasoningSigner {
    pub fn new(secret: &str, binding_context: &str) -> Result<Self, String> {
        if secret.is_empty() || binding_context.is_empty() {
            return Err("OpenAI Chat reasoning key is unavailable".into());
        }
        let mut hasher = Sha256::new();
        hasher.update(b"csswitch/openai-chat-thinking/key/v1\0");
        hasher.update(secret.as_bytes());
        hasher.update(b"\0");
        hasher.update(binding_context.as_bytes());
        Ok(Self {
            key: hasher.finalize().into(),
        })
    }

    fn seal(
        &self,
        target_model: &str,
        reasoning: &str,
        tools: &[ToolBinding],
    ) -> Result<String, String> {
        validate_reasoning_context(target_model, reasoning, tools)?;
        let payload = ReasoningSignaturePayload {
            version: 1,
            purpose: REASONING_SIGNATURE_PURPOSE.to_string(),
            target_model: target_model.to_string(),
            reasoning_digest: hex_digest(reasoning.as_bytes()),
            tools: tools.to_vec(),
        };
        let payload = serde_json::to_vec(&payload)
            .map_err(|_| "OpenAI Chat reasoning signature is invalid".to_string())?;
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| "OpenAI Chat reasoning key is unavailable".to_string())?;
        mac.update(REASONING_SIGNATURE_PREFIX.as_bytes());
        mac.update(b".");
        mac.update(encoded.as_bytes());
        let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let signature = format!("{REASONING_SIGNATURE_PREFIX}.{encoded}.{tag}");
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Err("OpenAI Chat reasoning signature is too large".into());
        }
        Ok(signature)
    }

    fn verify(
        &self,
        signature: &str,
        target_model: &str,
        reasoning: &str,
        tools: &[ToolBinding],
    ) -> Result<(), String> {
        validate_reasoning_context(target_model, reasoning, tools)?;
        if signature.len() > MAX_SIGNATURE_BYTES {
            return Err("OpenAI Chat reasoning signature is too large".into());
        }
        let Some(rest) = signature.strip_prefix(&format!("{REASONING_SIGNATURE_PREFIX}.")) else {
            return Err("OpenAI Chat reasoning signature is invalid".into());
        };
        let Some((encoded, tag)) = rest.split_once('.') else {
            return Err("OpenAI Chat reasoning signature is invalid".into());
        };
        if encoded.is_empty() || tag.is_empty() || tag.contains('.') {
            return Err("OpenAI Chat reasoning signature is invalid".into());
        }
        let tag = URL_SAFE_NO_PAD
            .decode(tag)
            .map_err(|_| "OpenAI Chat reasoning signature is invalid".to_string())?;
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| "OpenAI Chat reasoning key is unavailable".to_string())?;
        mac.update(REASONING_SIGNATURE_PREFIX.as_bytes());
        mac.update(b".");
        mac.update(encoded.as_bytes());
        mac.verify_slice(&tag)
            .map_err(|_| "OpenAI Chat reasoning signature is invalid".to_string())?;
        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| "OpenAI Chat reasoning signature is invalid".to_string())?;
        let payload: ReasoningSignaturePayload = serde_json::from_slice(&payload)
            .map_err(|_| "OpenAI Chat reasoning signature is invalid".to_string())?;
        if payload.version != 1
            || payload.purpose != REASONING_SIGNATURE_PURPOSE
            || payload.target_model != target_model
            || payload.reasoning_digest != hex_digest(reasoning.as_bytes())
            || payload.tools != tools
        {
            return Err("OpenAI Chat reasoning signature context changed".into());
        }
        Ok(())
    }
}

impl Drop for ReasoningSigner {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}

fn validate_reasoning_context(
    target_model: &str,
    reasoning: &str,
    tools: &[ToolBinding],
) -> Result<(), String> {
    if target_model.is_empty() || target_model.len() > MAX_TOOL_NAME_BYTES {
        return Err("OpenAI Chat reasoning model is invalid".into());
    }
    if reasoning.is_empty() || reasoning.len() > MAX_REASONING_BYTES {
        return Err("OpenAI Chat reasoning content is invalid".into());
    }
    if tools.len() > MAX_TOOL_CALLS {
        return Err("OpenAI Chat response has too many tool calls".into());
    }
    for tool in tools {
        validate_short_token(&tool.id, MAX_TOOL_ID_BYTES, "tool call id")?;
        validate_short_token(&tool.name, MAX_TOOL_NAME_BYTES, "tool name")?;
    }
    Ok(())
}

fn validate_short_token(value: &str, limit: usize, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > limit
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(format!("OpenAI Chat {label} is invalid"));
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn clamp_qwen_max_tokens(value: Option<u64>) -> Option<u64> {
    value.map(|v| v.min(8192))
}

pub fn map_tool_choice(tool_choice: Option<&Value>, tools: Option<&Value>) -> Option<Value> {
    let tc = tool_choice?.as_object()?;
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => Some(Value::String("auto".to_string())),
        Some("none") => Some(Value::String("none".to_string())),
        Some("tool") => tc
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        Some("any") => {
            let names: Vec<&str> = tools
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                .collect();
            if names.len() == 1 {
                Some(json!({"type": "function", "function": {"name": names[0]}}))
            } else {
                Some(Value::String("required".to_string()))
            }
        }
        _ => None,
    }
}

fn is_science_native_tool(tool: &Value) -> bool {
    matches!(
        (
            tool.get("type").and_then(Value::as_str),
            tool.get("name").and_then(Value::as_str),
        ),
        (Some("web_search_20250305"), Some("web_search"))
            | (Some("web_fetch_20260209"), Some("web_fetch"))
    )
}

fn json_dumps_python_style(value: &Value) -> String {
    let compact = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut out = String::with_capacity(compact.len() + 8);
    let mut in_string = false;
    let mut escaped = false;
    for ch in compact.chars() {
        out.push(ch);
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        } else if ch == ':' || ch == ',' {
            out.push(' ');
        }
    }
    out
}

fn tool_result_content(value: Option<&Value>) -> Result<String, String> {
    match value {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(items)) => {
            let mut text = String::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("text") {
                    return Err("OpenAI Chat tool result content is unsupported".into());
                }
                let part = item
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("OpenAI Chat tool result text is invalid")?;
                text.push_str(part);
            }
            Ok(text)
        }
        Some(other) => Ok(json_dumps_python_style(other)),
        None => Ok(String::new()),
    }
}

pub fn anthropic_to_openai(
    req: &Value,
    target_model: &str,
    signer: &ReasoningSigner,
) -> Result<Value, String> {
    anthropic_to_openai_with_model(req, target_model.to_string(), Some(8192), signer)
}

pub fn anthropic_to_openai_custom(
    req: &Value,
    target_model: &str,
    signer: &ReasoningSigner,
) -> Result<Value, String> {
    anthropic_to_openai_with_model(req, target_model.to_string(), None, signer)
}

fn anthropic_to_openai_with_model(
    req: &Value,
    target_model: String,
    max_token_cap: Option<u64>,
    signer: &ReasoningSigner,
) -> Result<Value, String> {
    let obj = req
        .as_object()
        .ok_or("request body must be a JSON object with a 'messages' array")?;
    if !obj.get("messages").map(Value::is_array).unwrap_or(false) {
        return Err("request body must be a JSON object with a 'messages' array".to_string());
    }

    let mut msgs = Vec::new();
    if let Some(system) = obj.get("system") {
        let sys_prompt = match system {
            Value::Array(items) => {
                let mut parts = Vec::new();
                for item in items {
                    if item.get("type").and_then(Value::as_str) != Some("text") {
                        return Err("OpenAI Chat system content is unsupported".into());
                    }
                    parts.push(
                        item.get("text")
                            .and_then(Value::as_str)
                            .ok_or("OpenAI Chat system text is invalid")?,
                    );
                }
                parts.join("\n")
            }
            Value::String(s) => s.clone(),
            _ => return Err("OpenAI Chat system content is invalid".into()),
        };
        if !sys_prompt.is_empty() {
            msgs.push(json!({"role": "system", "content": sys_prompt}));
        }
    }

    let mut pending_tool_calls = std::collections::HashSet::new();
    let mut seen_tool_calls = std::collections::HashSet::new();
    for message in obj.get("messages").and_then(Value::as_array).unwrap() {
        let message = message
            .as_object()
            .ok_or("OpenAI Chat history message is invalid")?;
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .filter(|role| matches!(*role, "user" | "assistant"))
            .ok_or("OpenAI Chat history role is invalid")?;
        let must_resolve_tool_calls = !pending_tool_calls.is_empty();
        if must_resolve_tool_calls && role != "user" {
            return Err("OpenAI Chat tool results must immediately follow tool calls".into());
        }
        let content = message
            .get("content")
            .ok_or("OpenAI Chat history content is missing")?;
        if let Some(text) = content.as_str() {
            if must_resolve_tool_calls {
                return Err("OpenAI Chat tool results must immediately follow tool calls".into());
            }
            msgs.push(json!({"role": role, "content": text}));
            continue;
        }
        let content = content
            .as_array()
            .ok_or("OpenAI Chat history content is invalid")?;

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        let mut tool_bindings = Vec::new();
        let mut reasoning: Option<(&str, &str)> = None;
        for block in content {
            let block = block
                .as_object()
                .ok_or("OpenAI Chat history content block is invalid")?;
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text_parts.push(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or("OpenAI Chat history text block is invalid")?,
                    );
                }
                Some("thinking") => {
                    if role != "assistant" || reasoning.is_some() {
                        return Err("OpenAI Chat thinking history is invalid".into());
                    }
                    let thinking = block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .ok_or("OpenAI Chat thinking content is invalid")?;
                    let signature = block
                        .get("signature")
                        .and_then(Value::as_str)
                        .ok_or("OpenAI Chat reasoning signature is missing")?;
                    reasoning = Some((thinking, signature));
                }
                Some("tool_use") => {
                    if role != "assistant" || tool_bindings.len() >= MAX_TOOL_CALLS {
                        return Err("OpenAI Chat tool history is invalid".into());
                    }
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or("OpenAI Chat tool call id is invalid")?;
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or("OpenAI Chat tool name is invalid")?;
                    validate_short_token(id, MAX_TOOL_ID_BYTES, "tool call id")?;
                    validate_short_token(name, MAX_TOOL_NAME_BYTES, "tool name")?;
                    if !seen_tool_calls.insert(id.to_string()) {
                        return Err("OpenAI Chat tool call id is duplicated".into());
                    }
                    pending_tool_calls.insert(id.to_string());
                    let input = block
                        .get("input")
                        .filter(|input| input.is_object())
                        .cloned()
                        .ok_or("OpenAI Chat tool input is invalid")?;
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": json_dumps_python_style(&input)
                        }
                    }));
                    tool_bindings.push(ToolBinding {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments_digest: json_value_digest(&input)?,
                    });
                }
                Some("tool_result") => {
                    if role != "user" {
                        return Err("OpenAI Chat tool result history is invalid".into());
                    }
                    let tool_use_id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .ok_or("OpenAI Chat tool result id is invalid")?;
                    validate_short_token(tool_use_id, MAX_TOOL_ID_BYTES, "tool result id")?;
                    if !pending_tool_calls.remove(tool_use_id) {
                        return Err("OpenAI Chat tool result has no matching tool call".into());
                    }
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": tool_result_content(block.get("content"))?
                    }));
                }
                Some(_) => return Err("OpenAI Chat history content block is unsupported".into()),
                None => return Err("OpenAI Chat history content block type is missing".into()),
            }
        }
        let joined_text = text_parts.join("");
        let resolved_any_tool_call = !tool_results.is_empty();
        if must_resolve_tool_calls && !resolved_any_tool_call {
            return Err("OpenAI Chat tool results must immediately follow tool calls".into());
        }
        if must_resolve_tool_calls && !pending_tool_calls.is_empty() && !joined_text.is_empty() {
            return Err("OpenAI Chat user text cannot precede pending tool results".into());
        }
        if role == "assistant" {
            if !tool_results.is_empty() {
                return Err("OpenAI Chat assistant history contains tool results".into());
            }
            let mut assistant = Map::new();
            assistant.insert("role".into(), Value::String("assistant".into()));
            assistant.insert(
                "content".into(),
                if joined_text.is_empty() {
                    Value::Null
                } else {
                    Value::String(joined_text)
                },
            );
            if !tool_calls.is_empty() {
                assistant.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            if let Some((thinking, signature)) = reasoning {
                signer.verify(signature, &target_model, thinking, &tool_bindings)?;
                assistant.insert(
                    "reasoning_content".into(),
                    Value::String(thinking.to_string()),
                );
            }
            if assistant.get("content") == Some(&Value::Null)
                && !assistant.contains_key("tool_calls")
                && !assistant.contains_key("reasoning_content")
            {
                return Err("OpenAI Chat assistant history has no content".into());
            }
            msgs.push(Value::Object(assistant));
        } else if !tool_results.is_empty() {
            if reasoning.is_some() || !tool_calls.is_empty() {
                return Err("OpenAI Chat user history contains assistant blocks".into());
            }
            msgs.extend(tool_results);
            if !joined_text.is_empty() {
                msgs.push(json!({"role": role, "content": joined_text}));
            }
        } else {
            if reasoning.is_some() || !tool_calls.is_empty() {
                return Err("OpenAI Chat user history contains assistant blocks".into());
            }
            msgs.push(json!({"role": role, "content": joined_text}));
        }
    }
    if !pending_tool_calls.is_empty() {
        return Err("OpenAI Chat history ends with incomplete tool calls".into());
    }

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(target_model));
    out.insert("messages".to_string(), Value::Array(msgs));
    out.insert("stream".to_string(), Value::Bool(false));
    if let Some(max_tokens) = obj.get("max_tokens").and_then(Value::as_u64) {
        let value = max_token_cap
            .map(|cap| max_tokens.min(cap))
            .unwrap_or(max_tokens);
        out.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(value)),
        );
    }
    if let Some(temperature) = obj.get("temperature") {
        if !temperature.is_null() {
            out.insert("temperature".to_string(), temperature.clone());
        }
    }
    let mut forwarded_tool_declarations = Vec::new();
    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            if tools.len() > MAX_TOOL_CALLS {
                return Err("OpenAI Chat request has too many tools".into());
            }
            let mut mapped = Vec::with_capacity(tools.len());
            for tool in tools {
                if is_science_native_tool(tool) {
                    continue;
                }
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or("OpenAI Chat tool name is invalid")?;
                validate_short_token(name, MAX_TOOL_NAME_BYTES, "tool name")?;
                let description = match tool.get("description") {
                    Some(Value::String(description)) => description.as_str(),
                    Some(_) => return Err("OpenAI Chat tool description is invalid".into()),
                    None => "",
                };
                let parameters = tool
                    .get("input_schema")
                    .filter(|schema| schema.is_object())
                    .cloned()
                    .ok_or("OpenAI Chat tool input schema is invalid")?;
                mapped.push(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }
                }));
                forwarded_tool_declarations.push(tool.clone());
            }
            if !mapped.is_empty() {
                out.insert("tools".to_string(), Value::Array(mapped));
            }
        }
    }
    if obj.contains_key("tool_choice") {
        let forwarded_tools = Value::Array(forwarded_tool_declarations.clone());
        let choice_type = obj
            .get("tool_choice")
            .and_then(Value::as_object)
            .and_then(|choice| choice.get("type"))
            .and_then(Value::as_str)
            .ok_or("OpenAI Chat tool choice is invalid")?;
        if forwarded_tool_declarations.is_empty() {
            match choice_type {
                "auto" | "none" => {}
                "any" | "tool" => return Err(
                    "OpenAI Chat tool choice requires a function tool supported by this transport"
                        .into(),
                ),
                _ => return Err("OpenAI Chat tool choice is invalid".into()),
            }
        } else {
            let mapped = map_tool_choice(obj.get("tool_choice"), Some(&forwarded_tools))
                .ok_or("OpenAI Chat tool choice is invalid")?;
            if let Some(name) = mapped
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
            {
                let declared = obj.get("tools").and_then(Value::as_array).is_some_and(|_| {
                    forwarded_tool_declarations
                        .iter()
                        .any(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                });
                if !declared {
                    return Err("OpenAI Chat forced tool is not declared".into());
                }
            }
            out.insert("tool_choice".to_string(), mapped);
        }
    }
    if let Some(stop) = obj.get("stop_sequences") {
        out.insert("stop".to_string(), stop.clone());
    }
    if let Some(top_p) = obj.get("top_p") {
        if !top_p.is_null() {
            out.insert("top_p".to_string(), top_p.clone());
        }
    }
    Ok(Value::Object(out))
}

pub fn openai_to_anthropic(
    resp: &Value,
    model_id: &str,
    target_model: &str,
    signer: &ReasoningSigner,
) -> Result<Value, String> {
    let response = resp
        .as_object()
        .ok_or("OpenAI Chat response is not a JSON object")?;
    let choices = response
        .get("choices")
        .and_then(Value::as_array)
        .ok_or("OpenAI Chat response choices are invalid")?;
    if choices.len() != 1 {
        return Err("OpenAI Chat response must contain exactly one choice".into());
    }
    let choice = choices[0]
        .as_object()
        .ok_or("OpenAI Chat response choice is invalid")?;
    let msg = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or("OpenAI Chat response message is invalid")?;
    if msg.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err("OpenAI Chat response role is invalid".into());
    }
    if msg.get("refusal").is_some_and(|refusal| !refusal.is_null()) {
        return Err("OpenAI Chat response was refused".into());
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .ok_or("OpenAI Chat finish reason is invalid")?;
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        _ => return Err("OpenAI Chat finish reason is unsupported".into()),
    };

    let mut blocks = Vec::new();
    let reasoning = match msg.get("reasoning_content") {
        Some(Value::String(reasoning)) if !reasoning.is_empty() => {
            if reasoning.len() > MAX_REASONING_BYTES {
                return Err("OpenAI Chat reasoning content is too large".into());
            }
            Some(reasoning.as_str())
        }
        Some(Value::String(_)) | Some(Value::Null) | None => None,
        Some(_) => return Err("OpenAI Chat reasoning content is invalid".into()),
    };
    let content = match msg.get("content") {
        Some(Value::String(content)) => Some(content.as_str()),
        Some(Value::Null) | None => None,
        Some(_) => return Err("OpenAI Chat response content is invalid".into()),
    };

    let tool_calls = match msg.get("tool_calls") {
        Some(Value::Array(tool_calls)) => tool_calls.as_slice(),
        Some(Value::Null) | None => &[],
        Some(_) => return Err("OpenAI Chat response tool calls are invalid".into()),
    };
    if tool_calls.len() > MAX_TOOL_CALLS {
        return Err("OpenAI Chat response has too many tool calls".into());
    }
    let mut tool_bindings = Vec::with_capacity(tool_calls.len());
    let mut seen_ids = std::collections::HashSet::new();
    let mut mapped_tools = Vec::with_capacity(tool_calls.len());
    for tool_call in tool_calls {
        let tool_call = tool_call
            .as_object()
            .ok_or("OpenAI Chat response tool call is invalid")?;
        if tool_call.get("type").and_then(Value::as_str) != Some("function") {
            return Err("OpenAI Chat response tool type is unsupported".into());
        }
        let id = tool_call
            .get("id")
            .and_then(Value::as_str)
            .ok_or("OpenAI Chat tool call id is invalid")?;
        validate_short_token(id, MAX_TOOL_ID_BYTES, "tool call id")?;
        if !seen_ids.insert(id) {
            return Err("OpenAI Chat tool call id is duplicated".into());
        }
        let function = tool_call
            .get("function")
            .and_then(Value::as_object)
            .ok_or("OpenAI Chat response tool function is invalid")?;
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .ok_or("OpenAI Chat tool name is invalid")?;
        validate_short_token(name, MAX_TOOL_NAME_BYTES, "tool name")?;
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or("OpenAI Chat tool arguments are invalid")?;
        if arguments.len() > MAX_REASONING_BYTES {
            return Err("OpenAI Chat tool arguments are too large".into());
        }
        let input: Value = serde_json::from_str(arguments)
            .map_err(|_| "OpenAI Chat tool arguments are invalid".to_string())?;
        if !input.is_object() {
            return Err("OpenAI Chat tool arguments must be a JSON object".into());
        }
        tool_bindings.push(ToolBinding {
            id: id.to_string(),
            name: name.to_string(),
            arguments_digest: json_value_digest(&input)?,
        });
        mapped_tools.push(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }));
    }
    let finish_expects_tool_calls = finish_reason == "tool_calls";
    let response_has_tool_calls = !mapped_tools.is_empty();
    if finish_expects_tool_calls != response_has_tool_calls {
        return Err("OpenAI Chat finish reason and tool calls disagree".into());
    }

    if let Some(reasoning) = reasoning {
        let signature = signer.seal(target_model, reasoning, &tool_bindings)?;
        blocks.push(json!({
            "type": "thinking",
            "thinking": reasoning,
            "signature": signature,
        }));
    }
    if let Some(content) = content.filter(|content| !content.is_empty()) {
        blocks.push(json!({"type": "text", "text": content}));
    }
    blocks.extend(mapped_tools);
    if blocks.is_empty() {
        return Err("OpenAI Chat response message has no content".into());
    }

    let id = response
        .get("id")
        .and_then(Value::as_str)
        .ok_or("OpenAI Chat response id is invalid")?;
    validate_short_token(id, MAX_TOOL_ID_BYTES, "response id")?;
    let (input_tokens, output_tokens) = match response.get("usage") {
        Some(Value::Object(usage)) => (
            usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .ok_or("OpenAI Chat prompt token usage is invalid")?,
            usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .ok_or("OpenAI Chat completion token usage is invalid")?,
        ),
        Some(Value::Null) | None => (0, 0),
        Some(_) => return Err("OpenAI Chat usage is invalid".into()),
    };
    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}

fn json_value_digest(value: &Value) -> Result<String, String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| "OpenAI Chat tool arguments are invalid".to_string())?;
    Ok(hex_digest(&bytes))
}

pub fn replay_as_sse_events(aresp: &Value) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let usage = aresp
        .get("usage")
        .cloned()
        .unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0}));
    events.push((
        "message_start".to_string(),
        json!({"type": "message_start", "message": {
            "id": aresp.get("id").and_then(Value::as_str).unwrap_or("msg_proxy"),
            "type": "message",
            "role": "assistant",
            "model": aresp.get("model").cloned().unwrap_or(Value::Null),
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": usage
        }}),
    ));
    events.push(("ping".to_string(), json!({"type": "ping"})));

    let default_blocks = vec![json!({"type": "text", "text": ""})];
    let blocks = aresp
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&default_blocks);
    for (idx, block) in blocks.iter().enumerate() {
        let index = Value::Number(serde_json::Number::from(idx));
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                events.push((
                    "content_block_start".to_string(),
                    json!({"type": "content_block_start", "index": index, "content_block": {
                        "type": "tool_use",
                        "id": block.get("id").cloned().unwrap_or(Value::Null),
                        "name": block.get("name").cloned().unwrap_or(Value::Null),
                        "input": {}
                    }}),
                ));
                events.push((
                    "content_block_delta".to_string(),
                    json!({"type": "content_block_delta", "index": idx, "delta": {
                        "type": "input_json_delta",
                        "partial_json": json_dumps_python_style(block.get("input").unwrap_or(&json!({})))
                    }}),
                ));
            }
            Some("thinking") => {
                events.push((
                    "content_block_start".to_string(),
                    json!({"type": "content_block_start", "index": idx, "content_block": {
                        "type": "thinking",
                        "thinking": "",
                        "signature": "",
                    }}),
                ));
                events.push((
                    "content_block_delta".to_string(),
                    json!({"type": "content_block_delta", "index": idx, "delta": {
                        "type": "thinking_delta",
                        "thinking": block.get("thinking").and_then(Value::as_str).unwrap_or(""),
                    }}),
                ));
                events.push((
                    "content_block_delta".to_string(),
                    json!({"type": "content_block_delta", "index": idx, "delta": {
                        "type": "signature_delta",
                        "signature": block.get("signature").and_then(Value::as_str).unwrap_or(""),
                    }}),
                ));
            }
            _ => {
                events.push((
                    "content_block_start".to_string(),
                    json!({"type": "content_block_start", "index": idx, "content_block": {
                        "type": "text",
                        "text": ""
                    }}),
                ));
                events.push((
                    "content_block_delta".to_string(),
                    json!({"type": "content_block_delta", "index": idx, "delta": {
                        "type": "text_delta",
                        "text": block.get("text").and_then(Value::as_str).unwrap_or("")
                    }}),
                ));
            }
        }
        events.push((
            "content_block_stop".to_string(),
            json!({"type": "content_block_stop", "index": idx}),
        ));
    }
    events.push((
        "message_delta".to_string(),
        json!({"type": "message_delta", "delta": {
            "stop_reason": aresp.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn"),
            "stop_sequence": null
        }, "usage": {
            "output_tokens": aresp.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_u64).unwrap_or(0)
        }}),
    ));
    events.push(("message_stop".to_string(), json!({"type": "message_stop"})));
    events
}

#[cfg(test)]
mod tests {
    use super::{
        anthropic_to_openai, anthropic_to_openai_custom, map_tool_choice, openai_to_anthropic,
        replay_as_sse_events, ReasoningSigner,
    };
    use serde_json::{json, Value};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test/golden/qwen_openai_chat.json")).unwrap()
    }

    fn signer() -> ReasoningSigner {
        signer_for("test-contract\0https://example.invalid/v1/chat/completions")
    }

    fn signer_for(context: &str) -> ReasoningSigner {
        ReasoningSigner::new("test-only-openai-chat-signing-secret", context).unwrap()
    }

    #[test]
    fn qwen_transform_matches_python_fixture() {
        let fx = fixture();
        let got = anthropic_to_openai(&fx["request"], "qwen-turbo", &signer()).unwrap();
        assert_eq!(got, fx["openai_request"]);
    }

    #[test]
    fn qwen_response_mapping_matches_python_fixture() {
        let fx = fixture();
        let got = openai_to_anthropic(
            &fx["openai_response"],
            "claude-haiku-4-5",
            "qwen-turbo",
            &signer(),
        )
        .unwrap();
        assert_eq!(got, fx["anthropic_response"]);
    }

    #[test]
    fn qwen_tool_choice_contract_matches_python() {
        assert_eq!(
            map_tool_choice(
                Some(&json!({"type": "any"})),
                Some(&json!([{"name": "only"}]))
            ),
            Some(json!({"type": "function", "function": {"name": "only"}}))
        );
        assert_eq!(
            map_tool_choice(
                Some(&json!({"type": "any"})),
                Some(&json!([{"name": "a"}, {"name": "b"}]))
            ),
            Some(json!("required"))
        );
        assert_eq!(
            map_tool_choice(Some(&json!({"type": "none"})), Some(&json!([]))),
            Some(json!("none"))
        );
    }

    #[test]
    fn qwen_token_cap_preserves_resolved_target() {
        let got = anthropic_to_openai(
            &json!({
                "model": "claude-opus-4-8",
                "max_tokens": 100000,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "qwen3.7-max",
            &signer(),
        )
        .unwrap();
        assert_eq!(got["model"], "qwen3.7-max");
        assert_eq!(got["max_tokens"], 8192);
    }

    #[test]
    fn custom_openai_forces_model_without_generic_token_clamp() {
        let got = anthropic_to_openai_custom(
            &json!({
                "model": "claude-opus-4-8",
                "max_tokens": 1000000,
                "messages": [{"role": "user", "content": "hi"}]
            }),
            "glm-4.5",
            &signer(),
        )
        .unwrap();
        assert_eq!(got["model"], "glm-4.5");
        assert_eq!(got["max_tokens"], 1000000);
    }

    #[test]
    fn custom_openai_filters_science_native_tools_and_recomputes_tool_choice() {
        let native_search = json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 3,
        });
        let native_fetch = json!({
            "type": "web_fetch_20260209",
            "name": "web_fetch",
        });
        let function = json!({
            "name": "lookup",
            "description": "lookup",
            "input_schema": {"type": "object", "properties": {}},
        });
        let native_only = anthropic_to_openai_custom(
            &json!({
                "messages": [{"role": "user", "content": "search"}],
                "tools": [native_search.clone(), native_fetch.clone()],
                "tool_choice": {"type": "auto"},
            }),
            "provider-model",
            &signer(),
        )
        .unwrap();
        assert!(native_only.get("tools").is_none());
        assert!(native_only.get("tool_choice").is_none());

        let mixed = anthropic_to_openai_custom(
            &json!({
                "messages": [{"role": "user", "content": "lookup"}],
                "tools": [native_search, function, native_fetch],
                "tool_choice": {"type": "any"},
            }),
            "provider-model",
            &signer(),
        )
        .unwrap();
        assert_eq!(mixed["tools"].as_array().unwrap().len(), 1);
        assert_eq!(mixed["tools"][0]["function"]["name"], "lookup");
        assert_eq!(
            mixed["tool_choice"],
            json!({"type": "function", "function": {"name": "lookup"}})
        );
    }

    #[test]
    fn custom_openai_rejects_forced_or_malformed_native_tool_declarations() {
        let forced_native = anthropic_to_openai_custom(
            &json!({
                "messages": [{"role": "user", "content": "search"}],
                "tools": [{"type": "web_search_20250305", "name": "web_search"}],
                "tool_choice": {"type": "tool", "name": "web_search"},
            }),
            "provider-model",
            &signer(),
        );
        assert!(forced_native
            .unwrap_err()
            .contains("requires a function tool"));

        let malformed = anthropic_to_openai_custom(
            &json!({
                "messages": [{"role": "user", "content": "search"}],
                "tools": [{"type": "web_search_20250305", "name": "not_web_search"}],
            }),
            "provider-model",
            &signer(),
        );
        assert!(malformed.unwrap_err().contains("input schema"));
    }

    #[test]
    fn qwen_sse_replay_events_match_python_sequence_shape() {
        let fx = fixture();
        let events = replay_as_sse_events(&fx["anthropic_response"]);
        let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[3].1["delta"]["text"], "answer");
        assert_eq!(events[5].1["content_block"]["type"], "tool_use");
        assert_eq!(events[6].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[8].1["delta"]["stop_reason"], "tool_use");
        assert_eq!(events[8].1["usage"]["output_tokens"], 8);
    }

    #[test]
    fn k3_reasoning_and_tool_call_round_trip_is_signed_and_deterministic() {
        let signer = signer();
        let response = json!({
            "id": "chatcmpl_k3_roundtrip",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "inspect the repository before editing",
                    "tool_calls": [{
                        "id": "call_k3_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/main.rs\"}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 20},
        });
        let anthropic =
            openai_to_anthropic(&response, "claude-opus-4-8", "kimi-k3", &signer).unwrap();
        assert_eq!(anthropic["content"][0]["type"], "thinking");
        assert_eq!(
            anthropic["content"][0]["thinking"],
            "inspect the repository before editing"
        );
        assert!(anthropic["content"][0]["signature"]
            .as_str()
            .unwrap()
            .starts_with(super::REASONING_SIGNATURE_PREFIX));
        assert_eq!(anthropic["content"][1]["type"], "tool_use");
        assert_eq!(anthropic["stop_reason"], "tool_use");

        let events = replay_as_sse_events(&anthropic);
        assert!(events.iter().any(|(_, event)| {
            event["delta"]["type"] == "thinking_delta"
                && event["delta"]["thinking"] == "inspect the repository before editing"
        }));
        assert!(events
            .iter()
            .any(|(_, event)| event["delta"]["type"] == "signature_delta"));

        let request = json!({
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "user", "content": "inspect it"},
                {"role": "assistant", "content": anthropic["content"]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_k3_1",
                    "content": "fn main() {}",
                }]},
            ],
        });
        let restored = anthropic_to_openai_custom(&request, "kimi-k3", &signer).unwrap();
        assert_eq!(
            restored["messages"][1]["reasoning_content"],
            "inspect the repository before editing"
        );
        assert_eq!(
            restored["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\": \"src/main.rs\"}"
        );
        assert_eq!(restored["messages"][2]["role"], "tool");
        assert_eq!(restored["messages"][2]["tool_call_id"], "call_k3_1");
        let restarted_signer =
            signer_for("test-contract\0https://example.invalid/v1/chat/completions");
        assert!(anthropic_to_openai_custom(&request, "kimi-k3", &restarted_signer).is_ok());
        let foreign_profile =
            signer_for("other-contract\0https://example.invalid/v1/chat/completions");
        assert!(anthropic_to_openai_custom(&request, "kimi-k3", &foreign_profile).is_err());

        let mut tampered = request.clone();
        tampered["messages"][1]["content"][0]["thinking"] = json!("changed");
        assert!(anthropic_to_openai_custom(&tampered, "kimi-k3", &signer).is_err());
        let mut tampered_arguments = request;
        tampered_arguments["messages"][1]["content"][1]["input"]["path"] = json!("src/changed.rs");
        assert!(anthropic_to_openai_custom(&tampered_arguments, "kimi-k3", &signer).is_err());
        assert!(anthropic_to_openai_custom(
            &json!({
                "model": "claude-opus-4-8",
                "messages": [
                    {"role": "user", "content": "inspect it"},
                    {"role": "assistant", "content": anthropic["content"]},
                    {"role": "user", "content": [{
                        "type": "tool_result",
                        "tool_use_id": "call_k3_1",
                        "content": "ok",
                    }]},
                ],
            }),
            "different-model",
            &signer,
        )
        .is_err());
    }

    #[test]
    fn malformed_openai_chat_responses_never_synthesize_success() {
        let signer = signer();
        let valid = json!({
            "id": "chatcmpl_valid",
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1},
        });
        for invalid in [
            json!({"id": "x", "choices": []}),
            json!({"id": "x", "choices": [{"finish_reason": "stop"}]}),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"content": "accepted without role"},
                    "finish_reason": "stop",
                }],
            }),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"role": "assistant", "content": null},
                    "finish_reason": "length",
                }],
            }),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "mystery"
                }],
            }),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call",
                        "type": "function",
                        "function": {"name": "tool", "arguments": "not-json"},
                    }]},
                    "finish_reason": "tool_calls",
                }],
            }),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "call",
                        "type": "function",
                        "function": {"name": "tool", "arguments": "[]"},
                    }]},
                    "finish_reason": "tool_calls",
                }],
            }),
            json!({
                "id": "x",
                "choices": [{
                    "message": {"role": "assistant", "content": "ok", "tool_calls": [{
                        "id": "call",
                        "type": "function",
                        "function": {"name": "tool", "arguments": "{}"},
                    }]},
                    "finish_reason": "stop",
                }],
            }),
        ] {
            assert!(
                openai_to_anthropic(&invalid, "claude", "kimi-k3", &signer).is_err(),
                "{invalid}"
            );
        }
        assert!(openai_to_anthropic(&valid, "claude", "kimi-k3", &signer).is_ok());
    }

    #[test]
    fn malformed_openai_chat_history_fails_before_transport() {
        let signer = signer();
        for request in [
            json!({
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "thinking",
                        "thinking": "not signed",
                        "signature": "foreign",
                    }],
                }],
            }),
            json!({
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "call",
                        "name": "tool",
                        "input": {},
                    }],
                }],
            }),
            json!({
                "messages": [{
                    "role": "user",
                    "content": [{"type": "image", "source": {}}],
                }],
            }),
            json!({
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "missing",
                        "content": "x",
                    }],
                }],
            }),
            json!({
                "messages": [
                    {"role": "assistant", "content": [{
                        "type": "tool_use", "id": "call", "name": "tool", "input": {},
                    }]},
                    {"role": "assistant", "content": "continued without a tool result"},
                ],
            }),
            json!({
                "messages": [
                    {"role": "assistant", "content": [{
                        "type": "tool_use", "id": "call", "name": "tool", "input": {},
                    }]},
                    {"role": "user", "content": "continued without a tool result"},
                ],
            }),
        ] {
            assert!(
                anthropic_to_openai_custom(&request, "kimi-k3", &signer).is_err(),
                "{request}"
            );
        }
    }
}
