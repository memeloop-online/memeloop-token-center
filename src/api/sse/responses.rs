use axum::body::Bytes;
use serde_json::{Value, json};

use super::{
    BoundedSseEvent, BoundedSseFramer, SAFE_SSE_HEARTBEAT_COMMENT, SseIdleControl,
    parse_unique_json,
};
use crate::api::{limits::MAX_RESPONSES_SSE_TERMINAL_HOLD_BYTES, proxy::safe_response_id};

// Codex records `response.failed` as a terminal Responses error. A generic
// `error` event is valid SSE but is ignored by the Codex Responses parser, so
// a following clean EOF is surfaced as "stream closed before
// response.completed" and loses the explicit upstream failure. Keep this
// envelope fixed and redacted, and do not mint a response id or a successful
// terminal for a lifecycle we could not validate.
const SAFE_FAILURE_EVENT: &[u8] = b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"message\":\"upstream request failed\",\"type\":\"server_error\",\"code\":\"server_error\"}}}\n\n";

/// A fixed terminal error frame for a downstream Responses SSE stream.  It
/// deliberately contains no provider detail and lets a stream that has
/// already started end with a valid body instead of a transport decode error.
pub(in crate::api) fn safe_failure_event() -> Bytes {
    Bytes::from_static(SAFE_FAILURE_EVENT)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StreamTerminal {
    Completed,
    Incomplete,
    Failed,
}

#[derive(Clone, Copy)]
struct SanitizerRejection {
    code: &'static str,
    stage: &'static str,
}

impl SanitizerRejection {
    const fn new(code: &'static str, stage: &'static str) -> Self {
        Self { code, stage }
    }
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
                Some(_)
                    if matches!(
                        payload_name,
                        "response.completed"
                            | "response.failed"
                            | "response.incomplete"
                            | "response.error"
                    ) =>
                {
                    return Err("upstream_response_terminal_conflict");
                }
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
    // This is derived only from the validated lifecycle identity. It is used
    // by the Codex Responses proxy to make an otherwise quiet, long-running
    // response observable to the downstream client without replaying an
    // upstream payload.
    progress_heartbeat: Option<Bytes>,
    // A fixed, low-cardinality operator diagnosis. It is deliberately never
    // derived from an upstream event, identifier, or payload.
    last_rejection_stage: Option<&'static str>,
}

impl ResponsesStreamingSanitizer {
    pub(in crate::api) fn push(&mut self, chunk: &[u8]) -> Result<Bytes, &'static str> {
        let mut output = Vec::new();
        let batch = self.framer.push(chunk);
        if let Some(rejection) = batch.rejection {
            self.last_rejection_stage = Some("framing");
            return Err(rejection.error_code());
        }
        for event in batch.events {
            if let Err(rejection) = self.sanitize_event(event, &mut output) {
                self.last_rejection_stage = Some(rejection.stage);
                return Err(rejection.code);
            }
        }
        Ok(Bytes::from(output))
    }

    pub(in crate::api) fn is_complete(&self) -> bool {
        self.framer.is_complete()
    }

    /// Whether a receiver cancellation may still be followed by immediately
    /// ready bytes that complete an event or release a validated success at
    /// EOF. This never authorizes waiting for another provider byte.
    pub(in crate::api) fn has_pending_delivery(&self) -> bool {
        !self.is_complete() || self.terminal_hold.is_active()
    }

    /// Release a successful terminal only after upstream EOF confirms there
    /// is no unterminated trailing data.
    pub(in crate::api) fn finish(&mut self) -> Result<Bytes, &'static str> {
        if !self.framer.is_complete() {
            self.last_rejection_stage = Some("eof_incomplete");
            return Err("upstream_incomplete_response");
        }
        if self.terminal.is_none() {
            self.last_rejection_stage = Some("eof_without_terminal");
            return Err("upstream_eof_without_terminal");
        }
        let terminal = self.terminal_hold.release();
        self.progress_heartbeat = None;
        Ok(terminal)
    }

    pub(in crate::api) fn saw_protocol_event(&self) -> bool {
        self.saw_protocol_event
    }

    /// A natural failure terminal was already emitted as the fixed safe error
    /// frame. A later transport error must close the body without adding a
    /// second terminal error.
    pub(in crate::api) fn has_failed_terminal(&self) -> bool {
        self.terminal == Some(StreamTerminal::Failed)
    }

    /// A canonical, non-terminal Responses lifecycle event while downstream
    /// still awaits its terminal. The caller sends this directly to the body:
    /// it is intentionally excluded from capture, archival, usage, and billing.
    pub(in crate::api) fn progress_heartbeat(&self) -> Option<Bytes> {
        self.progress_heartbeat.clone()
    }

    /// A static parser boundary suitable for operator logs and metric labels.
    /// It never includes any upstream bytes, headers, identifiers, or errors.
    pub(in crate::api) fn last_rejection_stage(&self) -> &'static str {
        self.last_rejection_stage.unwrap_or("unknown")
    }

    fn sanitize_event(
        &mut self,
        event: BoundedSseEvent,
        output: &mut Vec<u8>,
    ) -> Result<(), SanitizerRejection> {
        if event.is_line_ending_continuation {
            if self.forward_crlf_continuation {
                // The framer emitted the preceding CR-terminated event at a
                // network boundary. This LF contains no provider payload,
                // but it must follow that forwarded CR exactly so headerless
                // admission can retain a bare-CR prefix without swallowing a
                // later CRLF pair.
                self.append_output(output, &event.bytes)
                    .map_err(|code| SanitizerRejection::new(code, "terminal_hold"))?;
            }
            self.forward_crlf_continuation = false;
            return Ok(());
        }
        // A non-continuation event proves that a preceding CR was a complete
        // bare-CR separator. No later LF may be attached to it.
        self.forward_crlf_continuation = false;
        let (event_name, data) = parse_sse_event(&event)
            .map_err(|code| SanitizerRejection::new(code, "sse_metadata"))?;
        if data.is_none()
            && event_name
                .as_deref()
                .is_some_and(is_response_lifecycle_event_name)
        {
            // A named lifecycle without a JSON envelope must never let the
            // downstream capture discover a forged terminal after emit.
            return Err(SanitizerRejection::new(
                "upstream_invalid_response",
                "lifecycle_missing_data",
            ));
        }
        if data.as_deref() == Some(b"[DONE]") {
            if self.terminal.is_none() {
                return Err(SanitizerRejection::new(
                    "upstream_incomplete_response",
                    "done_before_terminal",
                ));
            }
            // Keep a standard DONE event's original CR/LF spelling. A named
            // provider event is normalized so its untrusted event name never
            // escapes the sanitizer.
            if event_name.is_none() {
                self.append_safe_sse_fields(&event, output)?;
            } else {
                self.append_output(output, b"data: [DONE]\n\n")
                    .map_err(|code| SanitizerRejection::new(code, "terminal_hold"))?;
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
        let value: Value =
            parse_unique_json(&data).map_err(|code| SanitizerRejection::new(code, "json"))?;
        let payload = value
            .as_object()
            .ok_or_else(|| SanitizerRejection::new("upstream_invalid_response", "payload_shape"))?;
        let payload_name = payload.get("type").and_then(Value::as_str).ok_or_else(|| {
            SanitizerRejection::new("upstream_invalid_response", "payload_schema")
        })?;
        if event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_name)
        {
            if event_name.as_deref().and_then(terminal_kind).is_some()
                || terminal_kind(payload_name).is_some()
            {
                return Err(SanitizerRejection::new(
                    "upstream_response_terminal_conflict",
                    "terminal_conflict",
                ));
            }
            return Err(SanitizerRejection::new(
                "upstream_invalid_response",
                "event_type_mismatch",
            ));
        }
        if is_response_metadata_event(payload_name) {
            // Codex may send this opaque envelope before the response
            // lifecycle. It has a top-level response_id rather than the
            // lifecycle response.id that binds identity, so drop only this
            // exact type before it can reach the identity gate.
            return Ok(());
        }
        if payload_name != "error" && !payload_name.starts_with("response.") {
            // Unknown provider events are opaque metadata. Do not let them
            // establish identity, affect lifecycle/accounting state, or
            // escape into the downstream/archive representation.
            return Ok(());
        }
        self.identity
            .observe(payload_name, &value)
            .map_err(|code| SanitizerRejection::new(code, "response_identity"))?;
        self.saw_protocol_event = true;
        let failure = matches!(terminal_kind(payload_name), Some(StreamTerminal::Failed))
            || (payload_name == "response.incomplete"
                && (value.pointer("/response/status").and_then(Value::as_str)
                    != Some("incomplete")
                    || value
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .is_none()
                    || value.get("response").is_none_or(|response| {
                        crate::api::proxy::codex_transport::canonical_responses_usage(response)
                            .is_err()
                    })))
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
            Some(StreamTerminal::Completed | StreamTerminal::Incomplete) => {
                return Err(SanitizerRejection::new(
                    "upstream_response_terminal_conflict",
                    "terminal_conflict",
                ));
            }
            None => {}
        }
        self.terminal = terminal;
        if (matches!(payload_name, "response.created" | "response.in_progress")
            && self.terminal.is_none()
            || matches!(
                terminal,
                Some(StreamTerminal::Completed | StreamTerminal::Incomplete)
            ))
            && let Some(response_id) = self.identity.response_id.as_deref()
        {
            self.progress_heartbeat = Some(progress_heartbeat_event(response_id));
        }
        if self.terminal == Some(StreamTerminal::Failed) {
            self.progress_heartbeat = None;
        }
        if matches!(
            terminal,
            Some(StreamTerminal::Completed | StreamTerminal::Incomplete)
        ) {
            self.terminal_hold.begin();
        }
        if failure {
            self.append_output(output, SAFE_FAILURE_EVENT)
                .map_err(|code| SanitizerRejection::new(code, "terminal_hold"))?;
        } else {
            self.append_safe_sse_fields(&event, output)?;
        }
        Ok(())
    }

    fn append_safe_sse_fields(
        &mut self,
        event: &BoundedSseEvent,
        output: &mut Vec<u8>,
    ) -> Result<(), SanitizerRejection> {
        self.append_output(output, &safe_sse_fields(event))
            .map_err(|code| SanitizerRejection::new(code, "terminal_hold"))?;
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

fn progress_heartbeat_event(response_id: &str) -> Bytes {
    // Serialize the identifier rather than interpolating it into SSE. The
    // identity gate already bounds and validates it, and JSON serialization
    // preserves that guarantee if the accepted character set changes.
    let data = serde_json::to_vec(&json!({
        "type": "response.in_progress",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "in_progress",
            "output": [],
        },
    }))
    .expect("a fixed Responses progress heartbeat is serializable");
    let mut event =
        Vec::with_capacity(b"event: response.in_progress\ndata: \n\n".len() + data.len());
    event.extend_from_slice(b"event: response.in_progress\ndata: ");
    event.extend_from_slice(&data);
    event.extend_from_slice(b"\n\n");
    Bytes::from(event)
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

/// `response.metadata` is an opaque Codex envelope, not a Responses
/// lifecycle event. Keep this exact so other `response.*` events retain the
/// normal identity and terminal validation.
pub(in crate::api) fn is_response_metadata_event(name: &str) -> bool {
    name == "response.metadata"
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
        "response.incomplete" => Some(StreamTerminal::Incomplete),
        "response.failed" | "response.error" | "error" => Some(StreamTerminal::Failed),
        _ => None,
    }
}

#[cfg(test)]
#[path = "responses/tests.rs"]
mod tests;
