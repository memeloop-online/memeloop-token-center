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
    ProviderDefault,
    /// Accept only controls whose value is neutral for the Codex transport.
    #[default]
    Strict,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    pub max_sse_event_bytes: usize,
    pub max_sse_framed_bytes: usize,
    pub max_sse_terminal_hold_bytes: usize,
    pub chat_controls: CodexChatControlPolicy,
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
            max_sse_event_bytes: SseFramingLimits::DEFAULT_EVENT_BYTES,
            max_sse_framed_bytes: SseFramingLimits::DEFAULT_FRAMED_BYTES,
            max_sse_terminal_hold_bytes: SseFramingLimits::DEFAULT_TERMINAL_HOLD_BYTES,
            chat_controls: CodexChatControlPolicy::Strict,
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
            || !(SseFramingLimits::MIN_EVENT_BYTES..=SseFramingLimits::MAX_EVENT_BYTES)
                .contains(&policy.max_sse_event_bytes)
            || !(policy.max_sse_event_bytes..=SseFramingLimits::MAX_BUFFER_BYTES)
                .contains(&policy.max_sse_framed_bytes)
            || !(policy.max_sse_event_bytes..=SseFramingLimits::MAX_BUFFER_BYTES)
                .contains(&policy.max_sse_terminal_hold_bytes)
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
        assert_eq!(policy.chat_controls, CodexChatControlPolicy::Strict);
        let provider_default = CodexTransportPolicy::parse(Some(&json!({
            "chat_controls": "provider_default"
        })))
        .unwrap();
        assert_eq!(
            provider_default.chat_controls,
            CodexChatControlPolicy::ProviderDefault
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
            json!({"account_hint": "untrusted"}),
            json!({"retry_503": true}),
            json!({"plugin": "untrusted"}),
            json!({"connect_timeout_millis": 99}),
            json!({"connect_timeout_millis": 60001}),
            json!({"read_timeout_millis": 999}),
            json!({"request_timeout_millis": 1260001}),
            json!({"memory_admission_wait_millis": 99}),
            json!({"memory_admission_wait_millis": 300001}),
            json!({"max_sse_event_bytes": 262143}),
            json!({"max_sse_event_bytes": 16777217}),
            json!({"max_sse_event_bytes": 1048576, "max_sse_framed_bytes": 1048575}),
            json!({"max_sse_event_bytes": 1048576, "max_sse_terminal_hold_bytes": 1048575}),
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
