use axum::body::Bytes;
use serde_json::Value;

use super::{
    BoundedSseEvent, BoundedSseFramer, SAFE_SSE_HEARTBEAT_COMMENT, SseIdleControl,
    parse_unique_json,
};
use crate::api::{limits::MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES, proxy::safe_response_id};

const SAFE_FAILURE_EVENT: &[u8] = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"upstream request failed\",\"type\":\"upstream_error\"}}\n\n";

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamTerminal {
    Completed,
    Failed,
}

/// Binds every event in one successful Responses lifecycle to its first
/// canonical response identifier. Output events are invalid until a lifecycle
/// event establishes that identity.
#[derive(Default)]
pub(in crate::api) struct ResponseIdentityGate {
    response_id: Option<String>,
}

impl ResponseIdentityGate {
    pub(in crate::api) fn observe(
        &mut self,
        payload_name: &str,
        value: &Value,
    ) -> Result<(), &'static str> {
        let lifecycle = matches!(
            payload_name,
            "response.created"
                | "response.queued"
                | "response.in_progress"
                | "response.completed"
                | "response.failed"
                | "response.incomplete"
                | "response.error"
        );
        if !lifecycle {
            if payload_name.starts_with("response.") && self.response_id.is_none() {
                return Err("upstream_invalid_response");
            }
            return Ok(());
        }
        let id_required = matches!(
            payload_name,
            "response.queued" | "response.created" | "response.in_progress" | "response.completed"
        );
        let response_id = match value.pointer("/response/id").or_else(|| value.get("id")) {
            Some(Value::String(response_id)) => {
                Some(safe_response_id(response_id).ok_or("upstream_invalid_response")?)
            }
            Some(_) => return Err("upstream_invalid_response"),
            None if id_required => return Err("upstream_incomplete_response"),
            None => None,
        };
        if let Some(response_id) = response_id {
            match self.response_id.as_deref() {
                None => self.response_id = Some(response_id),
                Some(current) if current == response_id => {}
                Some(_) => return Err("upstream_invalid_response"),
            }
        }
        Ok(())
    }
}

/// A successful terminal stays private until EOF validates all framing. This
/// bound is intentionally tied to the per-network-chunk delivery ceiling so
/// EOF validation cannot turn a completed response into an unbounded buffer.
#[derive(Default)]
enum ResponseTerminalHold {
    #[default]
    Idle,
    Holding(Vec<u8>),
}

impl ResponseTerminalHold {
    fn begin(&mut self) {
        *self = Self::Holding(Vec::new());
    }

    fn is_active(&self) -> bool {
        matches!(self, Self::Holding(_))
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        let Self::Holding(held) = self else {
            return Ok(());
        };
        if held.len().saturating_add(bytes.len()) > MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES {
            return Err("upstream_response_terminal_too_large");
        }
        held.extend_from_slice(bytes);
        Ok(())
    }

    fn release(&mut self) -> Bytes {
        match std::mem::take(self) {
            Self::Idle => Bytes::new(),
            Self::Holding(held) => Bytes::from(held),
        }
    }
}

/// Validates and redacts the standard Responses SSE protocol before delivery.
/// It shares the raw SSE framer with capture and headerless admission so every
/// path observes identical CR/LF/CRLF and EOF boundaries.
#[derive(Default)]
pub(in crate::api) struct ResponsesStreamingSanitizer {
    framer: BoundedSseFramer,
    terminal: Option<StreamTerminal>,
    saw_protocol_event: bool,
    forward_crlf_continuation: bool,
    identity: ResponseIdentityGate,
    terminal_hold: ResponseTerminalHold,
}

impl ResponsesStreamingSanitizer {
    pub(in crate::api) fn push(&mut self, chunk: &[u8]) -> Result<Bytes, &'static str> {
        let mut output = Vec::new();
        let batch = self.framer.push(chunk);
        if let Some(rejection) = batch.rejection {
            return Err(rejection.error_code());
        }
        for event in batch.events {
            self.sanitize_event(event, &mut output)?;
        }
        Ok(Bytes::from(output))
    }

    #[cfg(test)]
    pub(in crate::api) fn is_complete(&self) -> bool {
        self.framer.is_complete()
    }

    /// Release a successful terminal only after upstream EOF confirms there
    /// is no unterminated trailing data.
    pub(in crate::api) fn finish(&mut self) -> Result<Bytes, &'static str> {
        if !self.framer.is_complete() {
            return Err("upstream_incomplete_response");
        }
        Ok(self.terminal_hold.release())
    }

    pub(in crate::api) fn saw_protocol_event(&self) -> bool {
        self.saw_protocol_event
    }

    fn sanitize_event(
        &mut self,
        event: BoundedSseEvent,
        output: &mut Vec<u8>,
    ) -> Result<(), &'static str> {
        if event.is_line_ending_continuation {
            if self.forward_crlf_continuation {
                // The framer emitted the preceding CR-terminated event at a
                // network boundary. This LF contains no provider payload,
                // but it must follow that forwarded CR exactly so headerless
                // admission can retain a bare-CR prefix without swallowing a
                // later CRLF pair.
                self.append_output(output, &event.bytes)?;
            }
            self.forward_crlf_continuation = false;
            return Ok(());
        }
        // A non-continuation event proves that a preceding CR was a complete
        // bare-CR separator. No later LF may be attached to it.
        self.forward_crlf_continuation = false;
        let (event_name, data) = parse_sse_event(&event)?;
        if data.is_none()
            && event_name
                .as_deref()
                .is_some_and(is_response_lifecycle_event_name)
        {
            // A named lifecycle without a JSON envelope must never let the
            // downstream capture discover a forged terminal after emit.
            return Err("upstream_invalid_response");
        }
        if data.as_deref() == Some(b"[DONE]") {
            if self.terminal.is_none() {
                return Err("upstream_incomplete_response");
            }
            // Keep a standard DONE event's original CR/LF spelling. A named
            // provider event is normalized so its untrusted event name never
            // escapes the sanitizer.
            if event_name.is_none() {
                self.append_safe_sse_fields(&event, output)?;
            } else {
                self.append_output(output, b"data: [DONE]\n\n")?;
            }
            return Ok(());
        }
        if self.terminal == Some(StreamTerminal::Failed) {
            // Once a framed failure terminal is delivered, all later
            // provider data is untrusted diagnostic tail. Drop it before
            // JSON parsing or identity checks so a secret-bearing or forged
            // post-terminal event cannot turn a safe failure into a second
            // downstream error.
            return Ok(());
        }
        let Some(data) = data else {
            if self.terminal.is_some() {
                // Nothing without a data envelope is meaningful after a
                // terminal. In particular, never retain an untrusted bare
                // event name in the downstream or archive representation.
                return Ok(());
            }
            if event.idle_control == Some(SseIdleControl::Comment) {
                self.append_safe_sse_fields(&event, output)?;
            }
            return Ok(());
        };
        let value: Value = parse_unique_json(&data)?;
        let payload_name = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or("upstream_invalid_response")?;
        if payload_name != "error" && !payload_name.starts_with("response.") {
            return Err("upstream_invalid_response");
        }
        if event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_name)
        {
            return Err("upstream_invalid_response");
        }
        self.identity.observe(payload_name, &value)?;
        self.saw_protocol_event = true;
        let failure = matches!(terminal_kind(payload_name), Some(StreamTerminal::Failed))
            || value.get("error").is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null());
        let terminal = if failure {
            Some(StreamTerminal::Failed)
        } else {
            terminal_kind(payload_name)
        };
        match self.terminal {
            Some(StreamTerminal::Failed) => return Ok(()),
            Some(StreamTerminal::Completed) => return Err("upstream_invalid_response"),
            None => {}
        }
        self.terminal = terminal;
        if matches!(terminal, Some(StreamTerminal::Completed)) {
            self.terminal_hold.begin();
        }
        if failure {
            self.append_output(output, SAFE_FAILURE_EVENT)?;
        } else {
            self.append_safe_sse_fields(&event, output)?;
        }
        Ok(())
    }

    fn append_safe_sse_fields(
        &mut self,
        event: &BoundedSseEvent,
        output: &mut Vec<u8>,
    ) -> Result<(), &'static str> {
        self.append_output(output, &safe_sse_fields(event))?;
        self.forward_crlf_continuation = event.bytes.last() == Some(&b'\r');
        Ok(())
    }

    fn append_output(&mut self, output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), &'static str> {
        if self.terminal_hold.is_active() {
            self.terminal_hold.append(bytes)?;
        } else {
            output.extend_from_slice(bytes);
        }
        Ok(())
    }
}

fn safe_sse_fields(event: &BoundedSseEvent) -> Vec<u8> {
    let mut output = Vec::with_capacity(event.bytes.len());
    for line in &event.lines {
        if line.value.starts_with(b":") {
            output.extend_from_slice(SAFE_SSE_HEARTBEAT_COMMENT);
            output.extend_from_slice(&line.ending);
        } else if is_sse_field_line(&line.value, b"event")
            || is_sse_field_line(&line.value, b"data")
        {
            output.extend_from_slice(&line.value);
            output.extend_from_slice(&line.ending);
        }
    }
    output.extend_from_slice(&event.terminator);
    output
}

fn is_response_lifecycle_event_name(name: &str) -> bool {
    matches!(
        name,
        "response.created"
            | "response.queued"
            | "response.in_progress"
            | "response.completed"
            | "response.failed"
            | "response.incomplete"
            | "response.error"
            | "error"
    )
}

pub(in crate::api) fn parse_sse_event(
    event: &BoundedSseEvent,
) -> Result<(Option<String>, Option<Vec<u8>>), &'static str> {
    let mut event_name = None;
    let mut data = None::<Vec<u8>>;
    for raw_line in &event.lines {
        let line = raw_line.value.as_slice();
        if line == b"event" || line.starts_with(b"event:") {
            if event_name.is_some() {
                return Err("upstream_invalid_response");
            }
            let value = if line == b"event" {
                &[][..]
            } else {
                trim_ascii(&line[6..])
            };
            if value.len() > 128 {
                return Err("upstream_invalid_response");
            }
            event_name = Some(
                std::str::from_utf8(value)
                    .map_err(|_| "upstream_invalid_response")?
                    .to_owned(),
            );
        } else if line == b"data" || line.starts_with(b"data:") {
            let value = if line == b"data" {
                &[][..]
            } else {
                line[5..].strip_prefix(b" ").unwrap_or(&line[5..])
            };
            let append_separator = data.is_some();
            let data = data.get_or_insert_with(Vec::new);
            if append_separator {
                data.push(b'\n');
            }
            data.extend_from_slice(value);
        }
    }
    Ok((event_name, data))
}

pub(in crate::api) fn is_sse_field_line(line: &[u8], field: &[u8]) -> bool {
    line == field
        || line
            .strip_prefix(field)
            .is_some_and(|rest| rest.starts_with(b":"))
}

pub(in crate::api) fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn terminal_kind(name: &str) -> Option<StreamTerminal> {
    match name {
        "response.completed" => Some(StreamTerminal::Completed),
        "response.failed" | "response.incomplete" | "response.error" | "error" => {
            Some(StreamTerminal::Failed)
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;
