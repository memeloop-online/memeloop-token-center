//! Versioned, bounded policy data: never an authorization or transport hook.
use serde::Deserialize;
use serde_json::Value;

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
            || value.is_some_and(|value| value.get("shared_probe_attempts") == Some(&Value::Null))
        {
            return Err("invalid_transport_policy");
        }
        Ok(policy)
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
            json!({"account_hint": "untrusted"}),
            json!({"retry_503": true}),
            json!({"plugin": "untrusted"}),
        ] {
            assert!(
                CodexTransportPolicy::parse(Some(&invalid)).is_err(),
                "{invalid}"
            );
        }
    }
}
