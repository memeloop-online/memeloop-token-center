use super::*;
use super::{
    chat_sse_usage::{ChatSseDeliveryClass, ChatSseUsageState},
    codex_transport,
    conversation_hints::safe_conversation_hint,
    response_metadata::{
        completed_response_has_billable_result, merge_streaming_usage, trim_ascii_whitespace,
        usage_from_value_checked,
    },
};

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponsesSseEventKind {
    Lifecycle,
    Completed,
    Failed,
    Other,
}

impl ResponsesSseEventKind {
    fn from_name(name: &[u8]) -> Self {
        match trim_ascii_whitespace(name) {
            b"response.created" | b"response.queued" | b"response.in_progress" => Self::Lifecycle,
            b"response.completed" | b"message_stop" => Self::Completed,
            b"response.failed" | b"response.incomplete" | b"error" | b"response.error" => {
                Self::Failed
            }
            _ => Self::Other,
        }
    }

    fn is_response_lifecycle(self) -> bool {
        matches!(self, Self::Lifecycle | Self::Completed | Self::Failed)
    }
}

#[derive(Default)]
pub(super) struct ResponsesSseCapture {
    framer: crate::api::sse::BoundedSseFramer,
    framing_rejection: Option<crate::api::sse::SseFramerRejection>,
    response_id: Option<String>,
    invalid: bool,
    terminal_success: bool,
    terminal_failure: bool,
    usage: Option<TokenUsage>,
    usage_invalid: bool,
    require_explicit_completed: bool,
    responses_delivery: Option<ResponsesDeliveryContract>,
    chat_usage: Option<ChatSseUsageState>,
    delivery: Option<SseDeliveryState>,
    saw_done: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponsesDeliveryContract {
    Compatible,
    Codex,
}

#[derive(Default)]
struct SseDeliveryState {
    frames: Vec<SseDeliveryFrame>,
}

pub(super) struct SseDeliveryFrame {
    pub(super) bytes: Bytes,
    pub(super) billable: bool,
    pub(super) terminal: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResponsesSseOutcome {
    Completed { response_id: Option<String> },
    Failed,
    Incomplete,
}

pub(super) struct ResponsesSseSummary {
    pub(super) outcome: ResponsesSseOutcome,
    pub(super) usage: Option<TokenUsage>,
    pub(super) usage_invalid: bool,
}

impl ResponsesSseCapture {
    pub(super) fn for_responses() -> Self {
        Self {
            require_explicit_completed: true,
            responses_delivery: Some(ResponsesDeliveryContract::Compatible),
            delivery: Some(SseDeliveryState::default()),
            ..Self::default()
        }
    }

    pub(super) fn for_codex_responses() -> Self {
        Self {
            require_explicit_completed: true,
            responses_delivery: Some(ResponsesDeliveryContract::Codex),
            delivery: Some(SseDeliveryState::default()),
            ..Self::default()
        }
    }

    pub(super) fn for_delivery() -> Self {
        Self {
            delivery: Some(SseDeliveryState::default()),
            ..Self::default()
        }
    }

    pub(super) fn for_openai_chat_usage() -> Self {
        Self {
            chat_usage: Some(ChatSseUsageState::default()),
            delivery: Some(SseDeliveryState::default()),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(super) fn push(&mut self, chunk: &[u8]) {
        let _ = self.push_framed(chunk);
    }

    fn push_framed(&mut self, chunk: &[u8]) -> Result<(), crate::api::sse::SseFramerRejection> {
        if let Some(rejection) = self.framing_rejection {
            return Err(rejection);
        }
        let batch = self.framer.push(chunk);
        if let Some(rejection) = batch.rejection {
            // Any framing limit invalidates the delivery capture permanently.
            // A later blank line must not let an oversized event recover into
            // a complete-looking terminal stream.
            self.invalid = true;
            self.framing_rejection = Some(rejection);
            return Err(rejection);
        }
        for event in batch.events {
            if self.saw_done {
                if event.is_line_ending_continuation {
                    let bytes = self.delivery_event_bytes(&event);
                    self.finish_delivery_event(bytes, ChatSseDeliveryClass::Control);
                }
                continue;
            }
            let has_data = event
                .lines
                .iter()
                .any(|line| crate::api::sse::is_sse_field_line(line.value.as_slice(), b"data"));
            if !has_data {
                if self.terminal_success || self.terminal_failure {
                    continue;
                }
                if self.chat_usage.is_some()
                    && event.lines.iter().any(|line| {
                        crate::api::sse::is_sse_field_line(line.value.as_slice(), b"event")
                    })
                {
                    let class = self.dispatch_event(&event);
                    let bytes = Self::strict_chat_named_control_bytes(&event);
                    self.finish_delivery_event(bytes, class);
                    continue;
                }
                // Only fixed, redacted heartbeats and field-less framing are
                // safe to forward without a data envelope. Drop arbitrary
                // event/id/retry metadata so it cannot carry provider secrets.
                if event.idle_control != Some(crate::api::sse::SseIdleControl::Comment)
                    && !event.lines.is_empty()
                {
                    continue;
                }
                let bytes = self.delivery_event_bytes(&event);
                self.finish_delivery_event(bytes, ChatSseDeliveryClass::Control);
                continue;
            }
            let class = self.dispatch_event(&event);
            let bytes = self.delivery_event_bytes(&event);
            self.finish_delivery_event(bytes, class);
        }
        Ok(())
    }

    pub(super) fn push_delivery_frames(
        &mut self,
        chunk: &[u8],
    ) -> Result<Vec<SseDeliveryFrame>, crate::api::sse::SseFramerRejection> {
        self.push_framed(chunk)?;
        Ok(self
            .delivery
            .as_mut()
            .map_or_else(Vec::new, |delivery| std::mem::take(&mut delivery.frames)))
    }

    pub(super) fn finish_summary(mut self) -> ResponsesSseSummary {
        if !self.framer.is_complete() {
            self.invalid = true;
        }
        if let Some(chat_usage) = self.chat_usage.as_ref() {
            self.usage = chat_usage.usage();
            let usage_invalid = chat_usage.usage_invalid();
            self.usage_invalid |= usage_invalid;
            self.invalid |= usage_invalid;
        }
        let outcome = if self.terminal_failure {
            ResponsesSseOutcome::Failed
        } else if self.invalid {
            ResponsesSseOutcome::Incomplete
        } else if self.terminal_success {
            ResponsesSseOutcome::Completed {
                response_id: self.response_id,
            }
        } else {
            ResponsesSseOutcome::Incomplete
        };
        ResponsesSseSummary {
            outcome,
            usage: self.usage,
            usage_invalid: self.usage_invalid,
        }
    }

    pub(super) fn strict_chat_terminal_ready(&self) -> bool {
        self.chat_usage
            .as_ref()
            .is_some_and(ChatSseUsageState::is_done)
            && self.saw_done
            && !self.framer.has_pending_crlf_continuation()
    }

    pub(super) fn can_confirm_probe_delivery(&self) -> bool {
        !self.invalid && !self.terminal_failure && !self.usage_invalid
    }

    #[cfg(test)]
    pub(super) fn saw_done(&self) -> bool {
        self.saw_done
    }

    #[cfg(test)]
    #[cfg(test)]
    pub(super) fn has_pending_crlf_continuation(&self) -> bool {
        self.framer.has_pending_crlf_continuation()
    }

    #[cfg(test)]
    pub(super) fn finish(self) -> ResponsesSseOutcome {
        self.finish_summary().outcome
    }

    fn finish_delivery_event(&mut self, bytes: Bytes, class: ChatSseDeliveryClass) {
        let Some(delivery) = self.delivery.as_mut() else {
            return;
        };
        if !bytes.is_empty() {
            delivery.frames.push(SseDeliveryFrame {
                bytes,
                billable: matches!(class, ChatSseDeliveryClass::Billable),
                terminal: !self.terminal_failure && (self.terminal_success || self.saw_done),
            });
        }
    }

    fn delivery_event_bytes(&self, event: &crate::api::sse::BoundedSseEvent) -> Bytes {
        let metadata_policy = if Self::event_name_matches_payload(event)
            || (self.chat_usage.is_some() && Self::strict_chat_safe_event_name(event).is_some())
        {
            crate::api::sse::SseEventMetadataPolicy::ValidatedEventNames
        } else {
            crate::api::sse::SseEventMetadataPolicy::DataOnly
        };
        crate::api::sse::redacted_sse_event_bytes(event, metadata_policy)
    }

    fn strict_chat_named_control_bytes(event: &crate::api::sse::BoundedSseEvent) -> Bytes {
        let Ok((_, None)) = crate::api::sse::parse_sse_event(event) else {
            return Bytes::new();
        };
        let Some(safe_name) = Self::strict_chat_safe_event_name(event) else {
            return Bytes::new();
        };
        Bytes::from(format!("event: {safe_name}\n\n"))
    }

    fn strict_chat_safe_event_name(
        event: &crate::api::sse::BoundedSseEvent,
    ) -> Option<&'static str> {
        let (Some(event_name), _) = crate::api::sse::parse_sse_event(event).ok()? else {
            return None;
        };
        match event_name.as_str() {
            "message" => Some("message"),
            "error" => Some("error"),
            "response.failed" => Some("response.failed"),
            _ => None,
        }
    }

    fn event_name_matches_payload(event: &crate::api::sse::BoundedSseEvent) -> bool {
        let Ok((Some(event_name), Some(data))) = crate::api::sse::parse_sse_event(event) else {
            return false;
        };
        serde_json::from_slice::<Value>(&data)
            .ok()
            .is_some_and(|value| {
                value.get("type").and_then(Value::as_str) == Some(event_name.as_str())
            })
    }

    fn dispatch_event(&mut self, event: &crate::api::sse::BoundedSseEvent) -> ChatSseDeliveryClass {
        let mut data = None::<Vec<u8>>;
        let mut event_kind = None;
        for line in &event.lines {
            let line = line.value.as_slice();
            if line == b"data" || line.starts_with(b"data:") {
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
            } else if line == b"event" || line.starts_with(b"event:") {
                let value = if line == b"event" {
                    &[][..]
                } else {
                    line[6..].strip_prefix(b" ").unwrap_or(&line[6..])
                };
                event_kind = Some(ResponsesSseEventKind::from_name(value));
            }
        }
        // Explicit failure names remain authoritative, while unrelated event
        // metadata is redacted and the typed Chat payload is still validated.
        // This prevents provider metadata from changing settlement semantics.
        if self.chat_usage.is_some() && matches!(event_kind, Some(ResponsesSseEventKind::Failed)) {
            self.invalid = true;
            self.terminal_failure = true;
        }
        let Some(data) = data else {
            if self.chat_usage.is_some() && event_kind.is_some() {
                self.usage_invalid = true;
            }
            match event_kind {
                Some(ResponsesSseEventKind::Completed) if self.require_explicit_completed => {
                    self.invalid = true;
                }
                Some(ResponsesSseEventKind::Completed) => self.terminal_success = true,
                Some(ResponsesSseEventKind::Failed) => self.terminal_failure = true,
                Some(ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other) | None => {}
            }
            return ChatSseDeliveryClass::Control;
        };
        if self.chat_usage.is_some() && trim_ascii_whitespace(&data).is_empty() {
            self.usage_invalid = true;
            self.invalid = true;
            return ChatSseDeliveryClass::Control;
        }
        if data == b"[DONE]" {
            self.saw_done = true;
            if let Some(chat_usage) = self.chat_usage.as_mut() {
                chat_usage.observe_done();
            }
            if matches!(event_kind, Some(ResponsesSseEventKind::Failed)) {
                if self.require_explicit_completed && self.terminal_failure {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            } else if !self.require_explicit_completed {
                self.terminal_success = true;
            }
            return ChatSseDeliveryClass::Control;
        }
        if let Some(chat_usage) = self.chat_usage.as_mut() {
            return chat_usage.observe_data(&data);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&data) else {
            self.invalid = true;
            return ChatSseDeliveryClass::Billable;
        };
        let payload_kind = value
            .get("type")
            .and_then(Value::as_str)
            .map(|name| ResponsesSseEventKind::from_name(name.as_bytes()));
        let usage = if self.responses_delivery == Some(ResponsesDeliveryContract::Codex) {
            if payload_kind == Some(ResponsesSseEventKind::Completed) {
                value
                    .get("response")
                    .filter(|response| response.is_object())
                    .ok_or(())
                    .and_then(codex_transport::canonical_responses_usage)
                    .map(Some)
            } else {
                Ok(None)
            }
        } else {
            usage_from_value_checked(&value)
        };
        match usage {
            Err(()) => self.usage_invalid = true,
            Ok(None) => {}
            Ok(Some(next)) => {
                let current = self.usage.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    self.usage_invalid = true;
                }
            }
        }
        if value.get("error").is_some_and(|error| !error.is_null())
            || value
                .pointer("/response/error")
                .is_some_and(|error| !error.is_null())
        {
            self.terminal_failure = true;
        }
        if self.require_explicit_completed
            && let (Some(event_kind), Some(payload_kind)) = (event_kind, payload_kind)
            && event_kind != payload_kind
            && (event_kind.is_response_lifecycle() || payload_kind.is_response_lifecycle())
        {
            self.invalid = true;
            if matches!(event_kind, ResponsesSseEventKind::Failed)
                || matches!(payload_kind, ResponsesSseEventKind::Failed)
            {
                self.terminal_failure = true;
            }
            return ChatSseDeliveryClass::Billable;
        }
        let kind = payload_kind
            .or(event_kind)
            .unwrap_or(ResponsesSseEventKind::Other);
        let event_response_id = kind.is_response_lifecycle().then(|| {
            value
                .pointer("/response/id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint)
        });
        if let Some(Some(response_id)) = event_response_id.as_ref() {
            match self.response_id.as_deref() {
                None => self.response_id = Some(response_id.clone()),
                Some(current) if current == response_id.as_str() => {}
                Some(_) => self.invalid = true,
            }
        }
        match kind {
            ResponsesSseEventKind::Completed => {
                if self.require_explicit_completed
                    && (self.terminal_success
                        || self.terminal_failure
                        || !matches!(event_response_id, Some(Some(_))))
                {
                    self.invalid = true;
                }
                self.terminal_success = true;
            }
            ResponsesSseEventKind::Failed => {
                if self.require_explicit_completed
                    && (self.terminal_success || self.terminal_failure)
                {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            }
            ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other => {}
        }
        if self.response_delivery_is_billable(kind, &value) {
            ChatSseDeliveryClass::Billable
        } else {
            ChatSseDeliveryClass::Control
        }
    }

    fn response_delivery_is_billable(&self, kind: ResponsesSseEventKind, value: &Value) -> bool {
        match (self.responses_delivery, kind) {
            // Compatible Responses routes have historically charged a safe
            // failure after it is delivered. Direct Codex failure envelopes
            // remain a non-billable control frame unless output was already
            // delivered in a preceding event.
            (Some(ResponsesDeliveryContract::Compatible), ResponsesSseEventKind::Failed) => true,
            (_, ResponsesSseEventKind::Other) => true,
            (_, ResponsesSseEventKind::Completed) => completed_response_has_billable_result(value),
            (_, ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Failed) => false,
        }
    }
}
