//! Retry boundary exclusively for verified, read-only supplier quota endpoints.
//! In particular, reset consumption and OAuth exchange/refresh must not call this.
use super::*;
use backon::{BackoffBuilder, ExponentialBuilder};
use std::{future::Future, sync::Mutex as StdMutex};

#[derive(Clone, Debug, Serialize)]
pub(super) struct Attempt {
    endpoint_kind: &'static str,
    attempt: usize,
    limit: usize,
    failure_stage: &'static str,
    outcome: &'static str,
    error_code: Option<&'static str>,
    elapsed_ms: u64,
    retry_delay_ms: Option<u64>,
    trigger: &'static str,
    cache_hit: bool,
}

pub(super) struct Failure {
    code: &'static str,
    stage: &'static str,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl Failure {
    pub(super) fn terminal(code: &'static str, stage: &'static str) -> Self {
        Self {
            code,
            stage,
            retryable: false,
            retry_after: None,
        }
    }

    pub(super) fn wreq(error: wreq::Error) -> Self {
        let stage = wreq_stage(
            error.is_proxy_connect(),
            error.is_dns(),
            error.is_tls(),
            error.is_connect(),
            error.is_timeout(),
        );
        Self {
            code: quota_transport_error_code(error.is_timeout()),
            stage,
            retryable: stage != "transport",
            retry_after: None,
        }
    }

    pub(super) fn reqwest(error: reqwest::Error) -> Self {
        Self {
            code: quota_reqwest_error_code(error.is_timeout()),
            stage: quota_reqwest_error_kind(
                error.is_timeout(),
                error.is_connect(),
                error.is_body(),
                error.is_request(),
            ),
            retryable: error.is_connect() || error.is_timeout(),
            retry_after: None,
        }
    }

    // Only explicit supplier throttling/unavailability with a valid wait hint.
    // Generic 5xx, auth errors and response-body failures are never replayed.
    pub(super) fn status(status: u16, headers: &http::HeaderMap) -> Option<Self> {
        if (200..300).contains(&status) {
            return None;
        }
        let retry_after = matches!(status, 429 | 503)
            .then(|| retry_after(headers))
            .flatten();
        Some(Self {
            code: match status {
                401 | 403 => "quota_not_authorized",
                429 => "quota_rate_limited",
                _ => "quota_upstream_error",
            },
            stage: "headers",
            retryable: retry_after.is_some(),
            retry_after,
        })
    }
}

fn wreq_stage(proxy: bool, dns: bool, tls: bool, connect: bool, timeout: bool) -> &'static str {
    if proxy {
        "proxy_connect"
    } else if dns {
        "dns"
    } else if tls {
        "tls"
    } else if connect {
        "connect"
    } else if timeout {
        "timeout"
    } else {
        "transport"
    }
}

fn retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp_millis();
    Some(Duration::from_millis(
        at.saturating_sub(unix_millis()).max(0) as u64,
    ))
}

pub(super) struct ReadSession<'a> {
    state: Option<&'a AppState>,
    pub(super) deadline: tokio::time::Instant,
    pub(super) budget: QuotaBudget,
    attempts: StdMutex<Vec<Attempt>>,
}

impl<'a> ReadSession<'a> {
    #[cfg(test)]
    pub(super) fn testing(budget: QuotaBudget) -> Self {
        Self::new(None, tokio::time::Instant::now() + budget.total, budget)
    }

    pub(super) fn new(
        state: Option<&'a AppState>,
        deadline: tokio::time::Instant,
        budget: QuotaBudget,
    ) -> Self {
        Self {
            state,
            deadline,
            budget,
            attempts: StdMutex::new(Vec::new()),
        }
    }

    pub(super) fn attempts(&self) -> Vec<Attempt> {
        self.attempts
            .lock()
            .expect("quota diagnostics lock")
            .iter()
            .cloned()
            .map(|mut entry| {
                // The outer supplier deadline may cancel a request/body read before
                // this loop can finalize it. Never lose that attempt from the response.
                if entry.outcome == "started" {
                    entry.outcome = "error";
                    entry.failure_stage = "deadline";
                    entry.error_code = Some("quota_timeout");
                }
                entry
            })
            .collect()
    }

    pub(super) fn validated<T>(
        &self,
        endpoint: &'static str,
        result: Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        if let Err(code) = &result {
            if let Some(entry) = self
                .attempts
                .lock()
                .expect("quota diagnostics lock")
                .iter_mut()
                .rev()
                .find(|entry| entry.endpoint_kind == endpoint && entry.outcome == "success")
            {
                entry.outcome = "error";
                entry.failure_stage = "payload";
                entry.error_code = Some(*code);
                tracing::info!(
                    operation = "quota_supplier_read",
                    endpoint_kind = endpoint,
                    attempt = entry.attempt,
                    limit = entry.limit,
                    outcome = "error",
                    failure_stage = "payload",
                    error_code = *code,
                    trigger = entry.trigger,
                    cache_hit = false,
                    "quota payload validation failed without retry"
                );
            }
        }
        result
    }

    fn record(
        &self,
        context: QuotaRequestContext,
        attempt: usize,
        started: tokio::time::Instant,
        failure: Option<&Failure>,
        outcome: &'static str,
        delay: Option<Duration>,
    ) {
        let entry = Attempt {
            endpoint_kind: context.endpoint_kind,
            attempt,
            limit: self.budget.attempt_limit,
            failure_stage: failure.map_or("none", |f| f.stage),
            outcome,
            error_code: failure.map(|f| f.code),
            elapsed_ms: started.elapsed().as_millis() as u64,
            retry_delay_ms: delay.map(|d| d.as_millis() as u64),
            trigger: context.trigger.as_str(),
            cache_hit: false,
        };
        if outcome != "started" {
            tracing::info!(operation = "quota_supplier_read", upstream_account_id = %context.account_id,
            credential_generation = context.credential_generation, endpoint_kind = entry.endpoint_kind,
            attempt, limit = entry.limit, failure_stage = entry.failure_stage, outcome,
            error_code = entry.error_code.unwrap_or("none"), elapsed_ms = entry.elapsed_ms,
            retry_delay_ms = entry.retry_delay_ms, trigger = entry.trigger, cache_hit = false,
            "quota supplier attempt completed");
        }
        let mut attempts = self.attempts.lock().expect("quota diagnostics lock");
        if let Some(existing) = attempts
            .iter_mut()
            .find(|entry| entry.endpoint_kind == context.endpoint_kind && entry.attempt == attempt)
        {
            *existing = entry;
        } else {
            attempts.push(entry);
        }
    }

    pub(super) async fn run<T, F, Fut>(
        &self,
        context: QuotaRequestContext,
        mut send: F,
    ) -> Result<T, &'static str>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, Failure>>,
    {
        let mut backoff = ExponentialBuilder::default()
            .with_min_delay(self.budget.retry_delay)
            .with_max_delay(Duration::from_secs(2))
            .with_factor(2.0)
            .with_jitter()
            .with_max_times(self.budget.attempt_limit.saturating_sub(1))
            .build();
        for attempt in 1..=self.budget.attempt_limit {
            let started = tokio::time::Instant::now();
            self.record(context, attempt, started, None, "started", None);
            let result = tokio::time::timeout_at(self.deadline, async {
                if tokio::time::Instant::now() >= self.deadline {
                    return Err(Failure::terminal("quota_timeout", "deadline"));
                }
                if let Some(state) = self.state {
                    match state
                        .db
                        .quota_read_generation_current(
                            context.account_id,
                            context.credential_generation,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(Failure::terminal(
                                "credential_generation_changed",
                                "credential",
                            ));
                        }
                        Err(_) => {
                            return Err(Failure::terminal(
                                "quota_credential_check_failed",
                                "credential",
                            ));
                        }
                    }
                }
                if tokio::time::Instant::now() >= self.deadline {
                    return Err(Failure::terminal("quota_timeout", "deadline"));
                }
                send().await
            })
            .await
            .unwrap_or_else(|_| Err(Failure::terminal("quota_timeout", "deadline")));
            match result {
                Ok(value) => {
                    self.record(context, attempt, started, None, "success", None);
                    return Ok(value);
                }
                Err(failure) => {
                    let delay = if failure.retryable {
                        backoff
                            .next()
                            .map(|delay| delay.max(failure.retry_after.unwrap_or_default()))
                            .filter(|delay| {
                                started.checked_add(*delay).is_some()
                                    && tokio::time::Instant::now()
                                        .checked_add(*delay)
                                        .is_some_and(|at| at < self.deadline)
                            })
                    } else {
                        None
                    };
                    self.record(
                        context,
                        attempt,
                        started,
                        Some(&failure),
                        if delay.is_some() { "retry" } else { "error" },
                        delay,
                    );
                    match delay {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => return Err(failure.code),
                    }
                }
            }
        }
        unreachable!("backon yields at most limit minus one retry delays")
    }
}

pub(super) fn cached(mut snapshot: QuotaSnapshot, trigger: QuotaReadTrigger) -> QuotaSnapshot {
    snapshot.cache_hit = true;
    tracing::info!(operation = "quota_supplier_read", upstream_account_id = %snapshot.upstream_account_id,
        trigger = trigger.as_str(), cache_hit = true, outcome = "cache_hit", "quota cache read completed");
    // Attempts remain the original supplier observation, not invented attempts
    // made by this cache consumer. Their trigger and cache_hit remain accurate.
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    fn context() -> QuotaRequestContext {
        QuotaRequestContext {
            account_id: Uuid::from_u128(1),
            credential_generation: 1,
            endpoint_kind: "usage",
            trigger: QuotaReadTrigger::Bulk,
        }
    }

    fn policy() -> QuotaBudget {
        codex_quota_budget(&json!({})).unwrap()
    }

    #[test]
    fn wreq_classification_covers_disjoint_pre_header_failures() {
        for (flags, expected) in [
            ([true, false, false, false, false], "proxy_connect"),
            ([false, true, false, false, false], "dns"),
            ([false, false, true, false, false], "tls"),
            ([false, false, false, true, false], "connect"),
            ([false, false, false, false, true], "timeout"),
            ([false; 5], "transport"),
            ([true; 5], "proxy_connect"),
        ] {
            assert_eq!(
                wreq_stage(flags[0], flags[1], flags[2], flags[3], flags[4]),
                expected
            );
        }
    }

    #[test]
    fn runtime_quota_policy_is_bounded_and_rejects_inference_names() {
        assert_eq!(policy().attempt_limit, 3);
        for value in [
            json!({"max_attempts":0}),
            json!({"max_attempts":5}),
            json!({"initial_delay_millis":0}),
            json!({"total_timeout_millis":30001}),
            json!({"connect_attempts":3}),
            Value::Null,
        ] {
            assert!(codex_quota_budget(&json!({"quota_read_policy":value})).is_err());
        }
        let budget = codex_quota_budget(&json!({"quota_read_policy":{
            "max_attempts":4, "initial_delay_millis":500, "total_timeout_millis":30000
        }}))
        .unwrap();
        assert_eq!(budget.attempt_limit, 4);
        assert_eq!(budget.total, Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_have_bounded_jittered_attempt_diagnostics() {
        let session = ReadSession::testing(policy());
        let count = AtomicUsize::new(0);
        let result = session
            .run(context(), || async {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(Failure {
                    code: "quota_transport_failed",
                    stage: "proxy_connect",
                    retryable: true,
                    retry_after: None,
                })
            })
            .await;
        assert_eq!(result.unwrap_err(), "quota_transport_failed");
        assert_eq!(count.load(Ordering::SeqCst), 3);
        let attempts = session.attempts();
        assert_eq!(attempts.len(), 3);
        assert_eq!(attempts[0].outcome, "retry");
        assert_eq!(attempts[1].outcome, "retry");
        assert_eq!(attempts[2].outcome, "error");
        assert!((200..400).contains(&attempts[0].retry_delay_ms.unwrap()));
        assert!((400..800).contains(&attempts[1].retry_delay_ms.unwrap()));
        assert!(
            attempts
                .iter()
                .all(|a| a.limit == 3 && a.trigger == "bulk" && !a.cache_hit)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_failures_and_expired_deadline_never_retry() {
        for code in [
            "quota_not_authorized",
            "quota_invalid_payload",
            "credential_generation_changed",
            "quota_response_too_large",
        ] {
            let session = ReadSession::testing(policy());
            let count = AtomicUsize::new(0);
            let result = session
                .run(context(), || async {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(Failure::terminal(code, "body"))
                })
                .await;
            assert_eq!(result.unwrap_err(), code);
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
        let session = ReadSession::new(None, tokio::time::Instant::now(), policy());
        assert_eq!(
            session
                .run(context(), || async {
                    panic!("expired session must not send");
                    #[allow(unreachable_code)]
                    Ok::<(), Failure>(())
                })
                .await
                .unwrap_err(),
            "quota_timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_cannot_extend_the_absolute_deadline() {
        let session = ReadSession::testing(policy());
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("21"),
        );
        let result = session
            .run(context(), || async {
                Err::<(), _>(Failure::status(503, &headers).unwrap())
            })
            .await;
        assert_eq!(result.unwrap_err(), "quota_upstream_error");
        assert_eq!(session.attempts().len(), 1);
        let session = ReadSession::testing(policy());
        assert_eq!(
            session
                .run(context(), || std::future::pending::<Result<(), Failure>>())
                .await
                .unwrap_err(),
            "quota_timeout"
        );
        assert_eq!(session.attempts()[0].failure_stage, "deadline");
    }

    #[test]
    fn only_explicit_safe_retry_after_statuses_are_replayable() {
        let mut headers = http::HeaderMap::new();
        for status in [401, 403, 429, 500, 503] {
            assert!(!Failure::status(status, &headers).unwrap().retryable);
        }
        headers.insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("0"),
        );
        for status in [401, 403, 500] {
            assert!(!Failure::status(status, &headers).unwrap().retryable);
        }
        for status in [429, 503] {
            assert!(Failure::status(status, &headers).unwrap().retryable);
        }
        headers.insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("invalid-secret"),
        );
        assert!(!Failure::status(503, &headers).unwrap().retryable);
    }

    #[tokio::test]
    async fn codex_retry_after_recovers_but_auth_and_empty_body_are_single_requests() {
        for (status, body, expected_count) in [
            (503, "supplier-secret", 3),
            (401, "supplier-secret", 1),
            (403, "supplier-secret", 1),
            (200, "", 1),
            (200, "not-json", 1),
        ] {
            let server = MockServer::start().await;
            let count = AtomicUsize::new(0);
            Mock::given(method("GET"))
                .respond_with(move |_: &wiremock::Request| {
                    let attempt = count.fetch_add(1, Ordering::SeqCst);
                    if status == 503 && attempt == 2 {
                        ResponseTemplate::new(200).set_body_json(json!({"plan_type":"pro"}))
                    } else {
                        ResponseTemplate::new(status)
                            .insert_header("Retry-After", "0")
                            .set_body_string(body)
                    }
                })
                .expect(expected_count)
                .mount(&server)
                .await;
            let http = crate::build_codex_http_client_with_policy(
                crate::provider::CodexTransportPolicy::default(),
            )
            .unwrap();
            let budget = policy();
            let session = ReadSession::testing(budget);
            let result = get_codex_json(
                &http,
                CodexQuotaAuth {
                    credential_header: http::header::AUTHORIZATION,
                    credential_value: http::HeaderValue::from_static("Bearer fixture-secret"),
                    account: http::HeaderValue::from_static("fixture-account"),
                    proxy_url: None,
                },
                &server.uri(),
                context(),
                budget,
                &session,
            )
            .await;
            assert_eq!(result.is_ok(), status == 503);
            assert_eq!(session.attempts().len(), expected_count as usize);
            let serialized = serde_json::to_string(&session.attempts()).unwrap();
            assert!(!serialized.contains("secret"));
            assert!(!serialized.contains(&server.uri()));
        }
    }

    #[tokio::test]
    async fn credential_rotation_between_attempts_prevents_the_next_send() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("quota-retry.db").display()
        )))
        .await
        .unwrap();
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-old".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let account = state
            .db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: "quota-retry".into(),
                    name: "fixture".into(),
                    driver: "kimi-oauth".into(),
                    config: json!({"base_url":"https://api.kimi.com"}),
                    credential: credential.clone(),
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let budget = policy();
        let session = ReadSession::new(
            Some(&state),
            tokio::time::Instant::now() + budget.total,
            budget,
        );
        let count = AtomicUsize::new(0);
        let result = session
            .run(
                QuotaRequestContext::for_account(&account, "usage", QuotaReadTrigger::Manual),
                || async {
                    count.fetch_add(1, Ordering::SeqCst);
                    state
                        .db
                        .update_upstream_account(
                            account.id,
                            "quota-retry",
                            crate::db::UpdateUpstreamAccountInput {
                                name: account.name.clone(),
                                config: account.config.clone(),
                                expected_updated_at: account.updated_at,
                                expected_credential_generation: Some(account.credential_generation),
                                credential: Some(credential.clone()),
                            },
                            state.config.key_pepper.as_bytes(),
                        )
                        .await
                        .unwrap();
                    Err::<(), _>(Failure {
                        code: "quota_transport_failed",
                        stage: "connect",
                        retryable: true,
                        retry_after: None,
                    })
                },
            )
            .await;
        assert_eq!(result.unwrap_err(), "credential_generation_changed");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(session.attempts()[1].failure_stage, "credential");
    }
}
