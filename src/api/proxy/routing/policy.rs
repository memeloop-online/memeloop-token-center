//! Request-local policy snapshot. Standbys cannot increase the original budget.
use super::*;

pub(in crate::api::proxy) struct RequestAttemptBudget {
    max_attempts: usize,
    deadline: Option<tokio::time::Instant>,
    pub(in crate::api::proxy) version: u32,
}

impl RequestAttemptBudget {
    pub(in crate::api::proxy) fn from_primary(route: &ResolvedUpstream) -> Result<Self, AppError> {
        if !codex_transport::is_driver(&route.driver) {
            return Ok(Self {
                max_attempts: PROXY_ROUTING_POLICY.max_attempts(),
                deadline: None,
                version: 1,
            });
        }
        let policy =
            crate::provider::CodexTransportPolicy::parse(route.config.get("transport_policy"))
                .map_err(|_| AppError::BadRequest("invalid Codex transport policy".into()))?;
        Ok(Self {
            max_attempts: policy.candidate_attempts,
            deadline: Some(
                tokio::time::Instant::now()
                    + std::time::Duration::from_millis(policy.failover_deadline_millis),
            ),
            version: policy.version,
        })
    }

    pub(in crate::api::proxy) fn terminal_reason(&self, attempts: usize) -> Option<&'static str> {
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            Some("upstream_failover_deadline")
        } else if attempts >= self.max_attempts {
            Some("upstream_attempts_exhausted")
        } else {
            None
        }
    }

    pub(in crate::api::proxy) async fn send<F>(
        &self,
        send: F,
    ) -> Result<ProxyRouteResponse, ProxySendError>
    where
        F: std::future::Future<Output = Result<ProxyRouteResponse, ProxySendError>>,
    {
        match self.deadline {
            // Cancellation after dispatch is ambiguous. It never grants another send.
            Some(deadline) => tokio::time::timeout_at(deadline, send)
                .await
                .unwrap_or(Err(ProxySendError::NonRetryableTransport)),
            None => send.await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn absolute_deadline_and_attempt_limit_cannot_be_reset_by_failover() {
        let budget = RequestAttemptBudget {
            max_attempts: 2,
            deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1)),
            version: 1,
        };
        assert_eq!(budget.terminal_reason(1), None);
        assert_eq!(
            budget.terminal_reason(2),
            Some("upstream_attempts_exhausted")
        );
        assert_eq!(
            budget.terminal_reason(3),
            Some("upstream_attempts_exhausted")
        );
        let result = budget.send(std::future::pending()).await;
        assert!(matches!(result, Err(ProxySendError::NonRetryableTransport)));
        assert_eq!(
            budget.terminal_reason(1),
            Some("upstream_failover_deadline")
        );
    }
}
