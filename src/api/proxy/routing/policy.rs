//! Request-local policy snapshot. Standbys cannot increase the original budget.
use super::*;

pub(crate) struct RequestAttemptBudget {
    max_attempts: usize,
    pub(in crate::api::proxy) version: u32,
}

impl RequestAttemptBudget {
    pub(crate) fn recovery_wait_deadline(
        &self,
        health: crate::config::UpstreamHealthConfig,
    ) -> tokio::time::Instant {
        let millis = health
            .unavailable_cooldown_millis
            .saturating_add(health.probe_lease_millis)
            .max(0) as u64;
        tokio::time::Instant::now() + std::time::Duration::from_millis(millis)
    }
    pub(crate) fn from_primary(
        route: &ResolvedUpstream,
        request_id: Uuid,
    ) -> Result<Self, AppError> {
        if !codex_transport::is_driver(&route.driver) {
            let budget = Self {
                max_attempts: PROXY_ROUTING_POLICY.max_attempts(),
                version: 1,
            };
            budget.record_snapshot(route, request_id, "host_default");
            return Ok(budget);
        }
        let policy =
            crate::provider::CodexTransportPolicy::parse(route.config.get("transport_policy"))
                .map_err(|_| AppError::BadRequest("invalid Codex transport policy".into()))?;
        let budget = Self {
            max_attempts: policy.candidate_attempts,
            version: policy.version,
        };
        budget.record_snapshot(
            route,
            request_id,
            if route.config.get("transport_policy").is_some() {
                "account_transport_policy"
            } else {
                "codex_default"
            },
        );
        Ok(budget)
    }

    // Only explicit scalar fields cross the logging boundary. In particular,
    // never record route/config/credential Debug output or provider strings.
    fn record_snapshot(&self, route: &ResolvedUpstream, request_id: Uuid, source: &'static str) {
        tracing::info!(
            %request_id,
            route_id = %route.route_id,
            upstream_account_id = %route.account_id,
            transport_revision = route.transport_revision,
            credential_generation = route.credential_generation,
            stage = "request_attempt_budget_snapshot",
            policy_source = source,
            policy_version = self.version,
            candidate_attempts = self.max_attempts,
            "proxy request attempt budget frozen"
        );
    }

    pub(in crate::api::proxy) fn terminal_reason(&self, attempts: usize) -> Option<&'static str> {
        if attempts >= self.max_attempts {
            Some("upstream_attempts_exhausted")
        } else {
            None
        }
    }

    pub(in crate::api::proxy) async fn send<F>(
        &self,
        send: F,
        _wait_for_compaction: bool,
    ) -> Result<ProxyRouteResponse, ProxySendError>
    where
        F: std::future::Future<Output = Result<ProxyRouteResponse, ProxySendError>>,
    {
        send.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn route(driver: &str, policy: Option<Value>) -> ResolvedUpstream {
        let mut config = json!({"private_metadata": "do-not-log-config"});
        if let Some(policy) = policy {
            config["transport_policy"] = policy;
        }
        ResolvedUpstream {
            route_id: Uuid::from_u128(1),
            account_id: Uuid::from_u128(2),
            transport_revision: 42,
            credential_generation: 7,
            driver: driver.into(),
            base_url: "https://do-not-log-endpoint.invalid".into(),
            config,
            upstream_model: "do-not-log-model".into(),
            credential: crate::provider::UpstreamCredential::ApiKey {
                value: "do-not-log-credential".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
            },
        }
    }

    #[test]
    fn snapshot_logs_effective_budgets_and_only_allowlisted_fields() {
        for (driver, policy, source, attempts) in [
            ("http_json", None, "host_default", 3),
            (codex_transport::DRIVER, None, "codex_default", 3),
            (
                codex_transport::DRIVER,
                Some(json!({"candidate_attempts": 2})),
                "account_transport_policy",
                2,
            ),
        ] {
            let log = LogBuffer::default();
            let writer = log.clone();
            let subscriber = tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let budget =
                    RequestAttemptBudget::from_primary(&route(driver, policy), Uuid::from_u128(3))
                        .unwrap();
                assert_eq!(budget.max_attempts, attempts);
            });
            let bytes = log.0.lock().unwrap();
            let event: Value = serde_json::from_slice(&bytes).unwrap();
            let fields = &event["fields"];
            assert_eq!(fields["policy_source"], source);
            assert_eq!(fields["candidate_attempts"], attempts);
            assert_eq!(fields["policy_version"], 1);
            assert_eq!(fields["transport_revision"], 42);
            assert_eq!(fields["credential_generation"], 7);
            assert_eq!(fields["request_id"], Uuid::from_u128(3).to_string());
            assert_eq!(fields["route_id"], Uuid::from_u128(1).to_string());
            assert_eq!(
                fields["upstream_account_id"],
                Uuid::from_u128(2).to_string()
            );
            assert_eq!(fields["stage"], "request_attempt_budget_snapshot");
            assert_eq!(fields.as_object().unwrap().len(), 10);
            assert!(!String::from_utf8_lossy(&bytes).contains("do-not-log"));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn later_account_policy_does_not_replenish_the_frozen_budget() {
        let mut primary = route(
            codex_transport::DRIVER,
            Some(json!({"candidate_attempts": 1})),
        );
        let budget = RequestAttemptBudget::from_primary(&primary, Uuid::from_u128(3)).unwrap();
        primary.config["transport_policy"] = json!({"candidate_attempts": 8});
        assert_eq!(
            budget.terminal_reason(1),
            Some("upstream_attempts_exhausted")
        );
        tokio::time::advance(std::time::Duration::from_millis(1000)).await;
        assert_eq!(budget.terminal_reason(0), None);
        let next = RequestAttemptBudget::from_primary(&primary, Uuid::from_u128(4)).unwrap();
        assert_eq!(next.max_attempts, 8);
        assert_eq!(next.terminal_reason(1), None);
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_wait_uses_configured_health_cap() {
        let now = tokio::time::Instant::now();
        let mut health = crate::config::UpstreamHealthConfig::DEFAULT;
        health.unavailable_cooldown_millis = 100;
        health.probe_lease_millis = 200;
        let budget = RequestAttemptBudget {
            max_attempts: 2,
            version: 1,
        };
        assert_eq!(
            budget.recovery_wait_deadline(health),
            now + std::time::Duration::from_millis(300)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn elapsed_time_does_not_reset_or_exhaust_attempt_limit() {
        let budget = RequestAttemptBudget {
            max_attempts: 2,
            version: 1,
        };
        assert_eq!(budget.terminal_reason(1), None);
        assert_eq!(budget.terminal_reason(1), None);
        assert_eq!(
            budget.terminal_reason(2),
            Some("upstream_attempts_exhausted")
        );
        assert_eq!(
            budget.terminal_reason(3),
            Some("upstream_attempts_exhausted")
        );
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let result = budget
            .send(
                async {
                    Err(ProxySendError::NonRetryableTransport(
                        TransportFailureKind::Request,
                    ))
                },
                false,
            )
            .await;
        assert!(matches!(
            result,
            Err(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Request
            ))
        ));
        assert_eq!(budget.terminal_reason(1), None);
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_dispatched_send_outlives_selection_without_replenishing_budget() {
        let budget = RequestAttemptBudget::from_primary(
            &route(codex_transport::DRIVER, None),
            Uuid::from_u128(6),
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        let result = budget
            .send(
                async {
                    tokio::time::sleep(std::time::Duration::from_secs(1_300)).await;
                    Err(ProxySendError::NonRetryableTransport(
                        TransportFailureKind::Http2Reset,
                    ))
                },
                false,
            )
            .await;
        assert_eq!(
            result.err(),
            Some(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Http2Reset
            ))
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(1_300));
        assert_eq!(budget.terminal_reason(1), None);
        let result = budget
            .send(
                async {
                    Err(ProxySendError::NonRetryableTransport(
                        TransportFailureKind::Request,
                    ))
                },
                false,
            )
            .await;
        assert!(matches!(
            result,
            Err(ProxySendError::NonRetryableTransport(
                TransportFailureKind::Request
            ))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn dispatched_send_remains_cancellable_after_selection_deadline() {
        let budget = RequestAttemptBudget::from_primary(
            &route(codex_transport::DRIVER, None),
            Uuid::from_u128(7),
        )
        .unwrap();
        let (dropped, mut observed) = tokio::sync::oneshot::channel::<()>();
        let send = budget.send(
            async move {
                let _dropped = dropped;
                std::future::pending().await
            },
            false,
        );
        let mut send = Box::pin(send);
        tokio::select! {
            _ = &mut send => panic!("selection budget must not cancel dispatched send"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(310)) => {}
        }
        assert!(matches!(
            observed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(send);
        assert!(observed.await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_time_does_not_create_a_request_deadline() {
        let budget = RequestAttemptBudget::from_primary(
            &route(codex_transport::DRIVER, None),
            Uuid::from_u128(5),
        )
        .unwrap();
        tokio::time::advance(std::time::Duration::from_secs(1_300)).await;
        assert_eq!(budget.terminal_reason(0), None);
    }
}
