//! Versioned, bounded policy data: never an authorization or transport hook.
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SseFramingLimits {
    pub event_bytes: usize,
    pub framed_bytes: usize,
    pub terminal_hold_bytes: usize,
}

impl SseFramingLimits {
    pub const DEFAULT_EVENT_BYTES: usize = 8 * 1024 * 1024;
    pub const DEFAULT_FRAMED_BYTES: usize = Self::DEFAULT_EVENT_BYTES + 64 * 1024;
    pub const DEFAULT_TERMINAL_HOLD_BYTES: usize = Self::DEFAULT_FRAMED_BYTES;
    pub const MIN_EVENT_BYTES: usize = 256 * 1024;
    pub const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
    pub const MAX_BUFFER_BYTES: usize = Self::MAX_EVENT_BYTES + 64 * 1024;
}

impl Default for SseFramingLimits {
    fn default() -> Self {
        Self {
            event_bytes: Self::DEFAULT_EVENT_BYTES,
            framed_bytes: Self::DEFAULT_FRAMED_BYTES,
            terminal_hold_bytes: Self::DEFAULT_TERMINAL_HOLD_BYTES,
        }
    }
}

/// How the native Codex adapter handles OpenAI Chat controls which the
/// upstream Responses transport cannot represent exactly.
///
/// Keep this account-owned and versioned.  Client names and model slugs are
/// deliberately not part of the decision, so operators can change upstream
/// compatibility without another client-specific gateway patch.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexChatControlPolicy {
    /// Validate the OpenAI value shape, then let the Codex upstream apply its
    /// own sampling and output-length policy.
    #[default]
    ProviderDefault,
    /// Accept only controls whose value is neutral for the Codex transport.
    Strict,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexResponsesOutputLimitPolicy {
    #[default]
    ProviderDefault,
    Strict,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default)]
pub(crate) struct CodexTransportPolicy {
    pub version: u32,
    pub connect_attempts: usize,
    pub connect_retry_delay_millis: u64,
    pub shared_probe_attempts: Option<u32>,
    pub candidate_attempts: usize,
    /// Absolute selection/send budget, not the successful stream lifetime.
    pub failover_deadline_millis: u64,
    pub connect_timeout_millis: u64,
    pub read_timeout_millis: u64,
    pub request_timeout_millis: u64,
    /// Maximum local memory queue time; does not extend the request deadline.
    pub memory_admission_wait_millis: u64,
    pub dispatch_max_in_flight: usize,
    pub dispatch_max_queued: usize,
    pub dispatch_queue_timeout_millis: u64,
    pub max_sse_event_bytes: usize,
    pub max_sse_framed_bytes: usize,
    pub max_sse_terminal_hold_bytes: usize,
    pub chat_controls: CodexChatControlPolicy,
    pub responses_output_limits: CodexResponsesOutputLimitPolicy,
}

impl Default for CodexTransportPolicy {
    fn default() -> Self {
        Self {
            version: 1,
            connect_attempts: 2,
            connect_retry_delay_millis: 150,
            shared_probe_attempts: None,
            candidate_attempts: super::types::PROXY_ROUTING_POLICY.max_attempts(),
            failover_deadline_millis: 300_000,
            connect_timeout_millis: 5_000,
            read_timeout_millis: 600_000,
            request_timeout_millis: 1_260_000,
            memory_admission_wait_millis: 30_000,
            dispatch_max_in_flight: 0,
            dispatch_max_queued: 32,
            dispatch_queue_timeout_millis: 30_000,
            max_sse_event_bytes: SseFramingLimits::DEFAULT_EVENT_BYTES,
            max_sse_framed_bytes: SseFramingLimits::DEFAULT_FRAMED_BYTES,
            max_sse_terminal_hold_bytes: SseFramingLimits::DEFAULT_TERMINAL_HOLD_BYTES,
            chat_controls: CodexChatControlPolicy::ProviderDefault,
            responses_output_limits: CodexResponsesOutputLimitPolicy::ProviderDefault,
        }
    }
}

impl CodexTransportPolicy {
    pub(crate) fn parse(value: Option<&Value>) -> Result<Self, &'static str> {
        let policy: Self = match value {
            None => Self::default(),
            Some(value) => {
                serde_json::from_value(value.clone()).map_err(|_| "invalid_transport_policy")?
            }
        };
        if policy.version != 1
            || !(1..=4).contains(&policy.connect_attempts)
            || policy.connect_retry_delay_millis > 2_000
            || policy.shared_probe_attempts.is_some_and(|attempts| {
                attempts > crate::config::MAX_UPSTREAM_SHARED_PROBE_ATTEMPTS
            })
            || !(1..=8).contains(&policy.candidate_attempts)
            || !(1_000..=300_000).contains(&policy.failover_deadline_millis)
            || !(100..=60_000).contains(&policy.connect_timeout_millis)
            || !(1_000..=1_260_000).contains(&policy.read_timeout_millis)
            || !(1_000..=1_260_000).contains(&policy.request_timeout_millis)
            || !(100..=300_000).contains(&policy.memory_admission_wait_millis)
            || policy.dispatch_max_in_flight > 64
            || policy.dispatch_max_queued > 1024
            || !(1..=300_000).contains(&policy.dispatch_queue_timeout_millis)
            || !(SseFramingLimits::MIN_EVENT_BYTES..=SseFramingLimits::MAX_EVENT_BYTES)
                .contains(&policy.max_sse_event_bytes)
            || !(policy.max_sse_event_bytes..=SseFramingLimits::MAX_BUFFER_BYTES)
                .contains(&policy.max_sse_framed_bytes)
            || !(policy.max_sse_event_bytes..=SseFramingLimits::MAX_BUFFER_BYTES)
                .contains(&policy.max_sse_terminal_hold_bytes)
            || policy.max_sse_terminal_hold_bytes > policy.max_sse_framed_bytes
            || policy.connect_timeout_millis >= policy.request_timeout_millis
            || policy.read_timeout_millis > policy.request_timeout_millis
            || value.is_some_and(|value| value.get("shared_probe_attempts") == Some(&Value::Null))
        {
            return Err("invalid_transport_policy");
        }
        Ok(policy)
    }

    pub(crate) const fn sse_framing_limits(self) -> SseFramingLimits {
        SseFramingLimits {
            event_bytes: self.max_sse_event_bytes,
            framed_bytes: self.max_sse_framed_bytes,
            terminal_hold_bytes: self.max_sse_terminal_hold_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_defaults_and_versioned_budgets_are_bounded() {
        assert_eq!(
            CodexTransportPolicy::parse(None)
                .unwrap()
                .candidate_attempts,
            3
        );
        let policy = CodexTransportPolicy::parse(Some(&json!({
            "version": 1, "candidate_attempts": 8, "failover_deadline_millis": 1000,
            "connect_attempts": 1, "shared_probe_attempts": 0
        })))
        .unwrap();
        assert_eq!(policy.candidate_attempts, 8);
        assert_eq!(policy.failover_deadline_millis, 1000);
        assert_eq!(policy.shared_probe_attempts, Some(0));
        assert_eq!(policy.connect_timeout_millis, 5_000);
        assert_eq!(policy.read_timeout_millis, 600_000);
        assert_eq!(policy.request_timeout_millis, 1_260_000);
        assert_eq!(policy.memory_admission_wait_millis, 30_000);
        assert_eq!(policy.sse_framing_limits(), SseFramingLimits::default());
        assert_eq!(
            policy.chat_controls,
            CodexChatControlPolicy::ProviderDefault
        );
        assert_eq!(
            policy.responses_output_limits,
            CodexResponsesOutputLimitPolicy::ProviderDefault
        );
        let provider_default = CodexTransportPolicy::parse(Some(&json!({
            "chat_controls": "provider_default", "responses_output_limits": "provider_default"
        })))
        .unwrap();
        assert_eq!(
            provider_default.chat_controls,
            CodexChatControlPolicy::ProviderDefault
        );
        assert_eq!(
            provider_default.responses_output_limits,
            CodexResponsesOutputLimitPolicy::ProviderDefault
        );
        let independent_phases = CodexTransportPolicy::parse(Some(&json!({
            "connect_timeout_millis": 5_000,
            "read_timeout_millis": 1_000,
            "request_timeout_millis": 6_000
        })))
        .unwrap();
        assert_eq!(independent_phases.connect_timeout_millis, 5_000);
        assert_eq!(independent_phases.read_timeout_millis, 1_000);
        let framing = CodexTransportPolicy::parse(Some(&json!({
            "max_sse_event_bytes": 1_048_576,
            "max_sse_framed_bytes": 1_114_112,
            "max_sse_terminal_hold_bytes": 1_114_112,
        })))
        .unwrap();
        assert_eq!(
            framing.sse_framing_limits(),
            SseFramingLimits {
                event_bytes: 1_048_576,
                framed_bytes: 1_114_112,
                terminal_hold_bytes: 1_114_112,
            }
        );
    }

    #[test]
    fn omitted_controls_use_provider_defaults_but_explicit_strict_is_preserved() {
        assert_eq!(
            CodexChatControlPolicy::default(),
            CodexChatControlPolicy::ProviderDefault
        );
        assert_eq!(
            CodexResponsesOutputLimitPolicy::default(),
            CodexResponsesOutputLimitPolicy::ProviderDefault
        );
        for value in [None, Some(json!({})), Some(json!({"connect_attempts":1}))] {
            let policy = CodexTransportPolicy::parse(value.as_ref()).unwrap();
            assert_eq!(
                policy.chat_controls,
                CodexChatControlPolicy::ProviderDefault
            );
            assert_eq!(
                policy.responses_output_limits,
                CodexResponsesOutputLimitPolicy::ProviderDefault
            );
        }
        for (config, chat, responses) in [
            (
                json!({"chat_controls":"strict"}),
                CodexChatControlPolicy::Strict,
                CodexResponsesOutputLimitPolicy::ProviderDefault,
            ),
            (
                json!({"responses_output_limits":"strict"}),
                CodexChatControlPolicy::ProviderDefault,
                CodexResponsesOutputLimitPolicy::Strict,
            ),
            (
                json!({"chat_controls":"strict","responses_output_limits":"strict"}),
                CodexChatControlPolicy::Strict,
                CodexResponsesOutputLimitPolicy::Strict,
            ),
        ] {
            let policy = CodexTransportPolicy::parse(Some(&config)).unwrap();
            assert_eq!(policy.chat_controls, chat);
            assert_eq!(policy.responses_output_limits, responses);
        }
    }

    #[test]
    fn unknown_policy_fields_are_ignored_without_changing_known_values() {
        let policy = CodexTransportPolicy::parse(Some(&json!({
            "connect_attempts": 3,
            "future_option": {"nested": true},
            "account_hint": "untrusted",
            "retry_503": true,
            "plugin": "untrusted"
        })))
        .unwrap();
        let defaults = CodexTransportPolicy::default();
        assert_eq!(policy.connect_attempts, 3);
        assert_eq!(policy.candidate_attempts, defaults.candidate_attempts);
        assert_eq!(
            policy.dispatch_max_in_flight,
            defaults.dispatch_max_in_flight
        );
        assert_eq!(policy.sse_framing_limits(), defaults.sse_framing_limits());
        assert!(
            CodexTransportPolicy::parse(Some(&json!({
                "connect_attempts": 0,
                "future_option": true
            })))
            .is_err()
        );
    }

    #[test]
    fn dispatch_limits_require_an_explicit_positive_setting() {
        for config in [
            json!({}),
            json!({"connect_attempts": 3}),
            json!({"dispatch_max_in_flight": 0}),
        ] {
            assert_eq!(
                CodexTransportPolicy::parse(Some(&config))
                    .unwrap()
                    .dispatch_max_in_flight,
                0
            );
        }
        assert_eq!(
            CodexTransportPolicy::parse(None)
                .unwrap()
                .dispatch_max_in_flight,
            0
        );
        assert_eq!(
            CodexTransportPolicy::parse(Some(&json!({"dispatch_max_in_flight": 4})))
                .unwrap()
                .dispatch_max_in_flight,
            4
        );
    }

    #[test]
    fn invalid_policy_never_silently_enables_defaults() {
        for invalid in [
            json!(null),
            json!("policy"),
            json!({"version": 2}),
            json!({"candidate_attempts": 0}),
            json!({"candidate_attempts": 9}),
            json!({"failover_deadline_millis": 999}),
            json!({"failover_deadline_millis": 300001}),
            json!({"connect_attempts": -1}),
            json!({"connect_attempts": 5}),
            json!({"connect_retry_delay_millis": 2001}),
            json!({"shared_probe_attempts": 5}),
            json!({"shared_probe_attempts": null}),
            json!({"chat_controls": "unknown"}),
            json!({"responses_output_limits": "unknown"}),
            json!({"chat_controls": null}),
            json!({"responses_output_limits": null}),
            json!({"chat_controls": true}),
            json!({"responses_output_limits": 1}),
            json!({"connect_timeout_millis": 99}),
            json!({"connect_timeout_millis": 60001}),
            json!({"read_timeout_millis": 999}),
            json!({"request_timeout_millis": 1260001}),
            json!({"memory_admission_wait_millis": 99}),
            json!({"memory_admission_wait_millis": 300001}),
            json!({"dispatch_max_in_flight": -1}),
            json!({"dispatch_max_in_flight": 65}),
            json!({"dispatch_max_queued": 1025}),
            json!({"dispatch_queue_timeout_millis": 0}),
            json!({"dispatch_queue_timeout_millis": 300001}),
            json!({"max_sse_event_bytes": 262143}),
            json!({"max_sse_event_bytes": 16777217}),
            json!({"max_sse_event_bytes": 1048576, "max_sse_framed_bytes": 1048575}),
            json!({"max_sse_event_bytes": 1048576, "max_sse_terminal_hold_bytes": 1048575}),
            json!({"max_sse_event_bytes": 1048576, "max_sse_framed_bytes": 1114112, "max_sse_terminal_hold_bytes": 1179648}),
            json!({"max_sse_framed_bytes": 16842753}),
            json!({"max_sse_terminal_hold_bytes": 16842753}),
            json!({"read_timeout_millis": 2000, "request_timeout_millis": 1000}),
            json!({"connect_timeout_millis": 2000, "read_timeout_millis": 1000, "request_timeout_millis": 1000}),
            json!({"connect_timeout_millis": 1000, "read_timeout_millis": 1000, "request_timeout_millis": 1000}),
        ] {
            assert!(
                CodexTransportPolicy::parse(Some(&invalid)).is_err(),
                "{invalid}"
            );
        }
    }
}
