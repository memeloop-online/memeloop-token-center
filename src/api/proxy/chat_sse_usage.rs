use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::TokenUsage;

/// An account or route opt-in for the exact Chat SSE usage dialect it supports.
/// This stays separate from a generic HTTP JSON driver: compatible upstreams
/// vary in when and whether they emit a final usage-only frame.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ChatSseUsageContract {
    None,
    OpenAiUsageOnly,
}

impl ChatSseUsageContract {
    pub(super) fn from_route_config(config: &Value) -> Self {
        match config.get("stream_usage_contract").and_then(Value::as_str) {
            Some("openai-chat-usage-only") => Self::OpenAiUsageOnly,
            _ => Self::None,
        }
    }

    pub(super) fn requires_terminal_usage(self) -> bool {
        matches!(self, Self::OpenAiUsageOnly)
    }
}

/// Complete SSE events, not arbitrary network chunks, are the delivery unit.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ChatSseDeliveryClass {
    Billable,
    Control,
}

/// Validation state for the OpenAI Chat `include_usage` extension. Framing is
/// deliberately owned by the generic SSE capture; this type receives a single
/// complete JSON data frame at a time.
pub(super) struct ChatSseUsageState {
    model: Option<String>,
    response_id: Option<String>,
    service_tier: Option<String>,
    expected_choice_indices: BTreeSet<i64>,
    seen_choice_indices: BTreeSet<i64>,
    finished_choice_indices: BTreeSet<i64>,
    usage: Option<TokenUsage>,
    done: bool,
    invalid: bool,
}

impl Default for ChatSseUsageState {
    fn default() -> Self {
        Self {
            model: None,
            response_id: None,
            service_tier: None,
            // Admission rejects `n != 1`, so an enabled route has exactly one
            // expected choice. Keeping the expected set explicit prevents a
            // stream from silently dropping a second choice.
            expected_choice_indices: [0].into_iter().collect(),
            seen_choice_indices: BTreeSet::new(),
            finished_choice_indices: BTreeSet::new(),
            usage: None,
            done: false,
            invalid: false,
        }
    }
}

impl ChatSseUsageState {
    pub(super) fn observe_data(&mut self, data: &[u8]) -> ChatSseDeliveryClass {
        let Ok(chunk) = serde_json::from_slice::<CanonicalChatChunk>(data) else {
            self.invalid = true;
            return ChatSseDeliveryClass::Billable;
        };
        if self.done || !self.observe_envelope(&chunk) {
            self.invalid = true;
            return ChatSseDeliveryClass::Billable;
        }
        if chunk.choices.is_empty() {
            if chunk.moderation.is_some() && chunk.usage.is_none() {
                // Chat moderation arrives as a separate, non-billable chunk
                // with no choices and no terminal usage. It is neither model
                // output nor a replacement for the required usage-only frame.
            } else {
                self.observe_usage_only(chunk.usage);
            }
            ChatSseDeliveryClass::Control
        } else {
            let class = if chunk.choices.iter().all(|choice| choice.is_control()) {
                ChatSseDeliveryClass::Control
            } else {
                ChatSseDeliveryClass::Billable
            };
            self.observe_choices(chunk.usage, &chunk.choices);
            class
        }
    }

    pub(super) fn observe_done(&mut self) {
        if self.done
            || self.usage.is_none()
            || self.seen_choice_indices != self.expected_choice_indices
            || self.finished_choice_indices != self.expected_choice_indices
        {
            self.invalid = true;
        }
        self.done = true;
    }

    pub(super) fn usage(&self) -> Option<TokenUsage> {
        self.usage.clone()
    }

    pub(super) fn usage_invalid(&self) -> bool {
        self.invalid || !self.done || self.usage.is_none()
    }

    pub(super) fn is_done(&self) -> bool {
        self.done
    }

    fn observe_envelope(&mut self, chunk: &CanonicalChatChunk) -> bool {
        if chunk.object != "chat.completion.chunk"
            || !checked_identifier(&chunk.model)
            || !checked_identifier(&chunk.id)
        {
            return false;
        }
        let model_matches = match self.model.as_deref() {
            None => {
                self.model = Some(chunk.model.clone());
                true
            }
            Some(current) => current == chunk.model,
        };
        let id_matches = match self.response_id.as_deref() {
            None => {
                self.response_id = Some(chunk.id.clone());
                true
            }
            Some(current) => current == chunk.id,
        };
        let tier_matches = match (self.service_tier.as_deref(), chunk.service_tier.as_deref()) {
            (_, Some(tier)) if !super::is_supported_service_tier(tier) => false,
            (None, Some(tier)) => {
                self.service_tier = Some(tier.to_owned());
                true
            }
            (Some(current), Some(next)) => current == next,
            (_, None) => true,
        };
        model_matches && id_matches && tier_matches
    }

    fn observe_choices(
        &mut self,
        usage: Option<CanonicalChatUsage>,
        choices: &[CanonicalChatChoice],
    ) {
        if self.usage.is_some() || usage.is_some() {
            self.invalid = true;
            return;
        }
        let mut frame_indices = BTreeSet::new();
        for choice in choices {
            if !self.expected_choice_indices.contains(&choice.index)
                || !frame_indices.insert(choice.index)
            {
                self.invalid = true;
                return;
            }
            match choice.finish_reason.as_deref() {
                None if !self.finished_choice_indices.contains(&choice.index) => {
                    self.seen_choice_indices.insert(choice.index);
                }
                Some(reason)
                    if is_terminal_finish_reason(reason)
                        && self.seen_choice_indices.contains(&choice.index)
                        && self.finished_choice_indices.insert(choice.index) => {}
                _ => {
                    self.invalid = true;
                    return;
                }
            }
        }
    }

    fn observe_usage_only(&mut self, usage: Option<CanonicalChatUsage>) {
        if self.usage.is_some()
            || self.seen_choice_indices != self.expected_choice_indices
            || self.finished_choice_indices != self.expected_choice_indices
        {
            self.invalid = true;
            return;
        }
        match usage.and_then(|usage| canonical_chat_usage(usage, self.service_tier.clone())) {
            Some(usage) => self.usage = Some(usage),
            None => self.invalid = true,
        }
    }
}

// These typed structs reject duplicate known fields and unknown aliases. The
// top-level optional fields cover the standard Chat chunk metadata without
// accepting a provider-specific payload beside a canonical terminal usage.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatChunk {
    id: String,
    object: String,
    model: String,
    choices: Vec<CanonicalChatChoice>,
    #[serde(default, rename = "created")]
    _created: Option<i64>,
    #[serde(default, rename = "system_fingerprint")]
    _system_fingerprint: Option<String>,
    #[serde(default, rename = "obfuscation")]
    _obfuscation: Option<Value>,
    #[serde(default)]
    moderation: Option<Value>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    usage: Option<CanonicalChatUsage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatChoice {
    index: i64,
    delta: Map<String, Value>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    logprobs: Option<CanonicalChatLogprobs>,
}

impl CanonicalChatChoice {
    fn is_control(&self) -> bool {
        self.logprobs
            .as_ref()
            .is_none_or(CanonicalChatLogprobs::is_empty)
            && ((self
                .finish_reason
                .as_deref()
                .is_some_and(is_terminal_finish_reason)
                && self.delta_is_control_preamble())
                || (self.finish_reason.is_none() && self.delta_is_control_preamble()))
    }

    fn delta_is_control_preamble(&self) -> bool {
        self.delta
            .iter()
            .all(|(field, value)| match field.as_str() {
                "role" => value.as_str() == Some("assistant"),
                "content" | "refusal" => value.is_null() || value.as_str() == Some(""),
                "tool_calls" => value.as_array().is_some_and(Vec::is_empty),
                "function_call" | "audio" => value.is_null(),
                // Any unknown or populated semantic delta is potential user-visible
                // output and must start durable delivery before it is forwarded.
                _ => false,
            })
    }
}

/// The OpenAI Chat logprobs payload is part of a choice delta. Empty
/// containers commonly accompany role-only preambles, while a token or byte
/// sequence is user-observable output and must start durable delivery. Keep
/// the schema typed and closed so an unrecognized output-bearing field cannot
/// be mistaken for an empty preamble.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatLogprobs {
    #[serde(default)]
    content: Option<Vec<CanonicalChatLogprob>>,
    #[serde(default)]
    refusal: Option<Vec<CanonicalChatLogprob>>,
}

impl CanonicalChatLogprobs {
    fn is_empty(&self) -> bool {
        !self.content.as_ref().is_some_and(|entries| {
            entries
                .iter()
                .any(CanonicalChatLogprob::has_reconstructable_output)
        }) && !self.refusal.as_ref().is_some_and(|entries| {
            entries
                .iter()
                .any(CanonicalChatLogprob::has_reconstructable_output)
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatLogprob {
    token: String,
    #[serde(default)]
    bytes: Option<Vec<i64>>,
    #[serde(default, rename = "logprob")]
    _logprob: Option<Value>,
    #[serde(default)]
    top_logprobs: Option<Vec<CanonicalChatLogprobAlternative>>,
}

impl CanonicalChatLogprob {
    fn has_reconstructable_output(&self) -> bool {
        !self.token.is_empty()
            || self.bytes.as_ref().is_some_and(|bytes| !bytes.is_empty())
            || self.top_logprobs.as_ref().is_some_and(|alternatives| {
                alternatives
                    .iter()
                    .any(CanonicalChatLogprobAlternative::has_reconstructable_output)
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatLogprobAlternative {
    token: String,
    #[serde(default)]
    bytes: Option<Vec<i64>>,
    #[serde(default, rename = "logprob")]
    _logprob: Option<Value>,
}

impl CanonicalChatLogprobAlternative {
    fn has_reconstructable_output(&self) -> bool {
        !self.token.is_empty() || self.bytes.as_ref().is_some_and(|bytes| !bytes.is_empty())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: Option<CanonicalPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CanonicalCompletionTokensDetails>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
    #[serde(default)]
    cache_write_tokens: Option<i64>,
    #[serde(default)]
    audio_tokens: Option<i64>,
    #[serde(default)]
    image_tokens: Option<i64>,
    #[serde(default)]
    text_tokens: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalCompletionTokensDetails {
    #[serde(default)]
    accepted_prediction_tokens: Option<i64>,
    #[serde(default)]
    audio_tokens: Option<i64>,
    #[serde(default)]
    reasoning_tokens: Option<i64>,
    #[serde(default)]
    rejected_prediction_tokens: Option<i64>,
    #[serde(default)]
    text_tokens: Option<i64>,
}

fn checked_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 500 && !value.chars().any(char::is_control)
}

fn is_terminal_finish_reason(reason: &str) -> bool {
    matches!(
        reason,
        "stop" | "length" | "tool_calls" | "content_filter" | "function_call"
    )
}

fn canonical_chat_usage(
    usage: CanonicalChatUsage,
    service_tier: Option<String>,
) -> Option<TokenUsage> {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    let cache_write = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_write_tokens)
        .unwrap_or(0);
    let prompt_details_are_valid = usage.prompt_tokens_details.as_ref().is_none_or(|details| {
        [
            details.cached_tokens,
            details.cache_write_tokens,
            details.audio_tokens,
            details.image_tokens,
            details.text_tokens,
        ]
        .into_iter()
        .flatten()
        .all(|tokens| (0..=usage.prompt_tokens).contains(&tokens))
    });
    let completion_details_are_valid =
        usage
            .completion_tokens_details
            .as_ref()
            .is_none_or(|details| {
                [
                    details.accepted_prediction_tokens,
                    details.audio_tokens,
                    details.reasoning_tokens,
                    details.rejected_prediction_tokens,
                    details.text_tokens,
                ]
                .into_iter()
                .flatten()
                .all(|tokens| (0..=usage.completion_tokens).contains(&tokens))
            });
    if usage.prompt_tokens < 0
        || usage.completion_tokens < 0
        || usage.total_tokens <= 0
        || cached < 0
        || cache_write < 0
        || cached > usage.prompt_tokens
        || cached
            .checked_add(cache_write)
            .is_none_or(|cached_and_written| cached_and_written > usage.prompt_tokens)
        || !prompt_details_are_valid
        || !completion_details_are_valid
        || usage.total_tokens != usage.prompt_tokens.checked_add(usage.completion_tokens)?
    {
        return None;
    }
    let input_tokens = usage
        .prompt_tokens
        .checked_sub(cached)?
        .checked_sub(cache_write)?;
    [input_tokens, cached, cache_write, usage.completion_tokens]
        .into_iter()
        .all(|tokens| (0..=super::MAX_REPORTED_TOKENS).contains(&tokens))
        .then_some(TokenUsage {
            input_tokens,
            cached_input_tokens: cached,
            cache_write_tokens: cache_write,
            output_tokens: usage.completion_tokens,
            service_tier,
        })
}

#[cfg(test)]
pub(super) fn canonical_chat_chunk_is_accepted(data: &[u8]) -> bool {
    serde_json::from_slice::<CanonicalChatChunk>(data).is_ok()
}
