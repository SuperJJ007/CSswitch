use std::fmt;

use serde_json::Value;

const MAX_PENDING_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtocolError(&'static str);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ValidatedChunk {
    pub(crate) bytes: Vec<u8>,
    pub(crate) terminal_error: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitMessageStart,
    Streaming,
    AfterMessageDelta,
    Complete,
    Failed,
}

/// Incremental validator for the Anthropic Messages SSE lifecycle.
///
/// Frames are released only after their event name, JSON `type`, block index,
/// and lifecycle transition have been validated. `message_stop` is withheld
/// until a clean EOF, so a truncated or trailing-corrupt stream can never look
/// successful to Science.
pub(crate) struct Validator {
    pending: Vec<u8>,
    phase: Phase,
    open_block: Option<u64>,
    next_block: u64,
    pending_message_stop: Option<Vec<u8>>,
}

impl Default for Validator {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            phase: Phase::AwaitMessageStart,
            open_block: None,
            next_block: 0,
            pending_message_stop: None,
        }
    }
}

impl Validator {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<ValidatedChunk, ProtocolError> {
        if matches!(self.phase, Phase::Complete | Phase::Failed) && !chunk.is_empty() {
            return Err(ProtocolError("SSE data followed a terminal event"));
        }
        if self.pending.len().saturating_add(chunk.len()) > MAX_PENDING_BYTES {
            return Err(ProtocolError("SSE frame exceeds the bounded buffer"));
        }
        self.pending.extend_from_slice(chunk);
        let mut output = ValidatedChunk::default();
        while let Some(end) = frame_end(&self.pending) {
            let frame = self.pending.drain(..end).collect::<Vec<_>>();
            match self.validate_frame(&frame)? {
                FrameDisposition::Emit => output.bytes.extend_from_slice(&frame),
                FrameDisposition::WithholdMessageStop => {
                    self.pending_message_stop = Some(frame);
                }
                FrameDisposition::TerminalError => {
                    output.bytes.extend_from_slice(&frame);
                    output.terminal_error = true;
                    if !self.pending.is_empty() {
                        return Err(ProtocolError("SSE data followed a terminal error"));
                    }
                    break;
                }
            }
        }
        Ok(output)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<u8>, ProtocolError> {
        if !self.pending.is_empty() {
            if self.pending.iter().all(u8::is_ascii_whitespace) {
                self.pending.clear();
            } else {
                return Err(ProtocolError("SSE stream ended with a partial frame"));
            }
        }
        match self.phase {
            Phase::Complete => self
                .pending_message_stop
                .take()
                .ok_or(ProtocolError("SSE message_stop frame is unavailable")),
            Phase::Failed => Ok(Vec::new()),
            _ => Err(ProtocolError("SSE stream ended before message_stop")),
        }
    }

    fn validate_frame(&mut self, frame: &[u8]) -> Result<FrameDisposition, ProtocolError> {
        let text = std::str::from_utf8(frame)
            .map_err(|_| ProtocolError("SSE frame is not valid UTF-8"))?;
        let mut event = None;
        let mut data_lines = Vec::new();
        let mut has_comment = false;
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if line.starts_with(':') {
                has_comment = true;
                continue;
            }
            if let Some(value) = line.strip_prefix("event:") {
                if event.is_some() {
                    return Err(ProtocolError("SSE frame contains duplicate event fields"));
                }
                event = Some(value.trim());
                continue;
            }
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.strip_prefix(' ').unwrap_or(value));
                continue;
            }
            return Err(ProtocolError("SSE frame contains an unsupported field"));
        }
        if event.is_none() && data_lines.is_empty() && has_comment {
            if matches!(self.phase, Phase::Complete | Phase::Failed) {
                return Err(ProtocolError("SSE comment followed a terminal event"));
            }
            return Ok(FrameDisposition::Emit);
        }
        if event.is_some_and(str::is_empty) || data_lines.is_empty() {
            return Err(ProtocolError("SSE event or data field is empty"));
        }
        let data = data_lines.join("\n");
        let value: Value =
            serde_json::from_str(&data).map_err(|_| ProtocolError("SSE data is not valid JSON"))?;
        let json_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ProtocolError("SSE JSON type is missing"))?;
        if event.is_some_and(|event| event != json_type) {
            return Err(ProtocolError("SSE event and JSON type do not match"));
        }

        match json_type {
            "ping" => {
                if matches!(self.phase, Phase::Complete | Phase::Failed) {
                    return Err(ProtocolError("SSE ping followed a terminal event"));
                }
                Ok(FrameDisposition::Emit)
            }
            "message_start" => {
                if self.phase != Phase::AwaitMessageStart
                    || value.get("message").and_then(Value::as_object).is_none()
                {
                    return Err(ProtocolError("invalid message_start lifecycle"));
                }
                self.phase = Phase::Streaming;
                Ok(FrameDisposition::Emit)
            }
            "content_block_start" => {
                let index = event_index(&value)?;
                if self.phase != Phase::Streaming
                    || self.open_block.is_some()
                    || index != self.next_block
                    || value
                        .get("content_block")
                        .and_then(Value::as_object)
                        .is_none()
                {
                    return Err(ProtocolError("invalid content_block_start lifecycle"));
                }
                self.open_block = Some(index);
                Ok(FrameDisposition::Emit)
            }
            "content_block_delta" => {
                let index = event_index(&value)?;
                if self.phase != Phase::Streaming
                    || self.open_block != Some(index)
                    || value.get("delta").and_then(Value::as_object).is_none()
                {
                    return Err(ProtocolError("invalid content_block_delta lifecycle"));
                }
                Ok(FrameDisposition::Emit)
            }
            "content_block_stop" => {
                let index = event_index(&value)?;
                if self.phase != Phase::Streaming || self.open_block != Some(index) {
                    return Err(ProtocolError("invalid content_block_stop lifecycle"));
                }
                self.open_block = None;
                self.next_block = self
                    .next_block
                    .checked_add(1)
                    .ok_or(ProtocolError("SSE block index overflow"))?;
                Ok(FrameDisposition::Emit)
            }
            "message_delta" => {
                if self.phase != Phase::Streaming
                    || self.open_block.is_some()
                    || value.get("delta").and_then(Value::as_object).is_none()
                    || value.get("usage").and_then(Value::as_object).is_none()
                {
                    return Err(ProtocolError("invalid message_delta lifecycle"));
                }
                self.phase = Phase::AfterMessageDelta;
                Ok(FrameDisposition::Emit)
            }
            "message_stop" => {
                if self.phase != Phase::AfterMessageDelta {
                    return Err(ProtocolError("invalid message_stop lifecycle"));
                }
                self.phase = Phase::Complete;
                Ok(FrameDisposition::WithholdMessageStop)
            }
            "error" => {
                if matches!(self.phase, Phase::Complete | Phase::Failed)
                    || value.get("error").and_then(Value::as_object).is_none()
                {
                    return Err(ProtocolError("invalid terminal error lifecycle"));
                }
                self.phase = Phase::Failed;
                Ok(FrameDisposition::TerminalError)
            }
            _ => Err(ProtocolError("unsupported Anthropic SSE event")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameDisposition {
    Emit,
    WithholdMessageStop,
    TerminalError,
}

fn event_index(value: &Value) -> Result<u64, ProtocolError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or(ProtocolError("SSE block index is missing or invalid"))
}

fn frame_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer
        .windows(2)
        .position(|part| part == b"\n\n")
        .map(|i| i + 2);
    let crlf = buffer
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Validator;

    fn frame(event: &str, data: &str) -> Vec<u8> {
        format!("event: {event}\ndata: {data}\n\n").into_bytes()
    }

    fn data_frame(data: &str) -> Vec<u8> {
        format!("data: {data}\n\n").into_bytes()
    }

    fn complete_stream() -> Vec<u8> {
        [
            frame(
                "message_start",
                r#"{"type":"message_start","message":{"id":"m","type":"message"}}"#,
            ),
            frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            frame(
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            frame(
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            frame(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
            ),
            frame("message_stop", r#"{"type":"message_stop"}"#),
        ]
        .concat()
    }

    #[test]
    fn message_stop_is_released_only_after_clean_eof() {
        let bytes = complete_stream();
        let mut validator = Validator::default();
        let split = bytes.len() - frame("message_stop", r#"{"type":"message_stop"}"#).len();
        let first = validator.feed(&bytes[..split]).unwrap();
        assert!(!String::from_utf8_lossy(&first.bytes).contains("message_stop"));
        let second = validator.feed(&bytes[split..]).unwrap();
        assert!(second.bytes.is_empty());
        let terminal = validator.finish().unwrap();
        assert!(String::from_utf8_lossy(&terminal).contains("message_stop"));
    }

    #[test]
    fn data_only_frames_use_the_json_type_for_the_lifecycle() {
        let stream = [
            data_frame(r#"{"type":"message_start","message":{"id":"m","type":"message"}}"#),
            data_frame(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
            data_frame(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#),
            data_frame(r#"{"type":"content_block_stop","index":0}"#),
            data_frame(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#),
            data_frame(r#"{"type":"message_stop"}"#),
        ]
        .concat();
        let mut validator = Validator::default();
        let validated = validator.feed(&stream).unwrap();
        assert!(!validated
            .bytes
            .windows(12)
            .any(|part| part == b"message_stop"));
        let terminal = validator.finish().unwrap();
        assert!(terminal.windows(12).any(|part| part == b"message_stop"));
    }

    #[test]
    fn missing_stop_partial_frame_and_trailing_data_fail_closed() {
        let complete = complete_stream();
        let stop = frame("message_stop", r#"{"type":"message_stop"}"#);

        let mut missing = Validator::default();
        missing
            .feed(&complete[..complete.len() - stop.len()])
            .unwrap();
        assert!(missing.finish().is_err());

        let mut partial = Validator::default();
        partial.feed(b"event: message_start\n").unwrap();
        assert!(partial.finish().is_err());

        let mut trailing = Validator::default();
        trailing.feed(&complete).unwrap();
        assert!(trailing.feed(b": late\n\n").is_err());
    }

    #[test]
    fn mismatched_indexes_and_event_types_fail_closed() {
        let mut validator = Validator::default();
        validator
            .feed(&frame(
                "message_start",
                r#"{"type":"message_start","message":{}}"#,
            ))
            .unwrap();
        assert!(validator
            .feed(&frame(
                "content_block_start",
                r#"{"type":"content_block_delta","index":0,"content_block":{}}"#,
            ))
            .is_err());

        let mut validator = Validator::default();
        validator
            .feed(&frame(
                "message_start",
                r#"{"type":"message_start","message":{}}"#,
            ))
            .unwrap();
        assert!(validator
            .feed(&frame(
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{}}"#,
            ))
            .is_err());
    }

    #[test]
    fn upstream_error_is_the_only_terminal_frame() {
        let mut validator = Validator::default();
        let output = validator
            .feed(&frame(
                "error",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
            ))
            .unwrap();
        assert!(output.terminal_error);
        assert_eq!(
            String::from_utf8_lossy(&output.bytes)
                .matches("event: error")
                .count(),
            1
        );
        assert!(validator.finish().unwrap().is_empty());
    }
}
