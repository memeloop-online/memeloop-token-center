use super::*;
use futures_util::StreamExt;

struct Fixture {
    state: AppState,
    route: PreparedProxyRoute,
    client: reqwest::Client,
    identity: send_diagnostics::TransportIdentity,
    selected: SelectedEgress,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("egress.db").display()
        );
        let state = AppState::initialize(crate::config::Config::for_test(database_url))
            .await
            .unwrap();
        let route = PreparedProxyRoute {
            route: ResolvedUpstream {
                route_id: Uuid::new_v4(),
                account_id: Uuid::new_v4(),
                credential_generation: 1,
                transport_revision: 1,
                driver: codex_transport::DRIVER.into(),
                base_url: codex_transport::BASE_URL.into(),
                config: Value::Null,
                upstream_model: "synthetic-model".into(),
                credential: UpstreamCredential::OAuth {
                    access_token: "synthetic-access".into(),
                    refresh_token: None,
                    expires_at: Some(i64::MAX),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: Some(serde_json::json!({
                        "schema": "openai-codex-oauth-v1", "account_id": "synthetic-account"
                    })),
                    proxy_url: None,
                    proxy_network_scope: None,
                },
            },
            forwarded_body: bytes::Bytes::from_static(b"{}"),
            upstream_stream: true,
            codex_downstream_stream: true,
            codex_store_disabled: true,
            codex_session_id: Some("synthetic-session".into()),
            component_request: None,
            responses_chat: None,
            upstream_path: codex_transport::RESPONSES_PATH,
            wait_for_compaction: false,
            compact_v2_bridge: false,
            wrap_compact_as_sse: false,
            responses_anthropic: None,
        };
        let client = crate::build_codex_http_client().unwrap();
        let identity = send_diagnostics::TransportIdentity {
            client_instance_id: Uuid::new_v4(),
            cache_hit: false,
            selected_proxy_matches_client_key: Some(true),
            connect_attempt: 1,
            proxy_member_index: None,
            proxy_member_count: 0,
            selection_epoch: 0,
            group_selection_version: None,
            request_local_selection: false,
        };
        let mut selected = SelectedEgress::capture(None).unwrap();
        selected.kind = "direct".into();
        Self {
            state,
            route,
            client,
            identity,
            selected,
            _directory: directory,
        }
    }

    fn context(&self) -> CodexAttemptContext {
        CodexAttemptContext {
            request_id: Uuid::new_v4(),
            candidate_rank: 1,
            outbound_attempt: 1,
            transport_policy: runtime_transport_policy(&Value::Null, 1).unwrap(),
            deadline: CodexRequestDeadline {
                request: None,
                read_inactivity: std::time::Duration::from_secs(30),
            },
        }
    }

    fn recorder(&self, context: CodexAttemptContext) -> AttemptRecorder {
        let body = send_diagnostics::BodyConsumption::default();
        let evidence = send_diagnostics::Http2AttemptEvidence::capture(
            &self.route.route,
            self.identity,
            context,
            false,
            body.clone(),
        );
        AttemptRecorder::begin(
            &self.state,
            &self.route.route,
            Some(self.selected.clone()),
            self.identity,
            context,
            body,
            &evidence,
        )
        .unwrap()
    }

    async fn send(
        &self,
        target: &str,
        context: CodexAttemptContext,
    ) -> Result<
        (
            UpstreamResponse,
            crate::metrics::ActivityGuard,
            Option<FinalSelection>,
        ),
        ProxySendError,
    > {
        let mut h2_recovery_available = true;
        send_codex_attempt_once(
            &self.state,
            &HeaderMap::new(),
            target,
            &self.route,
            "synthetic-session",
            (
                &self.client,
                &self.route.route.credential,
                self.identity,
                Some(self.selected.clone()),
            ),
            context,
            &mut h2_recovery_available,
        )
        .await
    }

    async fn attempts(&self, context: CodexAttemptContext) -> Vec<RequestEgressAttempt> {
        self.state.routing_persistence.drain_for_test().await;
        self.state
            .db
            .request_egress_attempts(context.request_id)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn pending_attempt_updates_coalesce_without_losing_terminal_or_final_selection() {
    let fixture = Fixture::new().await;
    let context = fixture.context();
    let first = fixture.recorder(context);
    let second = fixture.recorder(context);
    let mut releases = Vec::new();
    for _ in 0..2 {
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        releases.push(release);
        assert!(fixture.state.routing_persistence.submit(1, async move {
            let _ = wait.await;
            Ok(())
        }));
    }
    first.0.response_started.store(true, Ordering::Relaxed);
    first.0.response_bytes.store(17, Ordering::Relaxed);
    first
        .0
        .response_body_complete
        .store(true, Ordering::Relaxed);
    second.0.response_started.store(true, Ordering::Relaxed);
    second.0.response_bytes.store(29, Ordering::Relaxed);
    FinalSelection(second.0.clone()).select();
    drop(first);
    drop(second);
    drop(releases);
    let attempts = fixture.attempts(context).await;
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().all(|attempt| attempt.completed));
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.final_response)
            .count(),
        1
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.response_body_complete)
            .count(),
        1
    );
    assert_eq!(
        attempts
            .iter()
            .map(|attempt| attempt.response_payload_bytes.unwrap())
            .sum::<i64>(),
        46
    );
    assert!(
        fixture
            .state
            .routing_persistence
            .render()
            .contains("outcome=\"capacity\"} 0")
    );
}

#[tokio::test]
async fn unpolled_expired_and_build_failed_sends_do_not_create_egress_attempts() {
    let fixture = Fixture::new().await;
    let upstream = wiremock::MockServer::start().await;
    let target = upstream.uri();
    let unpolled = fixture.context();
    drop(fixture.send(&target, unpolled));
    assert!(fixture.attempts(unpolled).await.is_empty());
    let mut expired = fixture.context();
    expired.deadline.request =
        Some(tokio::time::Instant::now() - std::time::Duration::from_secs(1));
    assert!(matches!(
        fixture.send(&target, expired).await,
        Err(ProxySendError::AmbiguousResponse(
            upstream_response::UPSTREAM_REQUEST_TIMEOUT
        ))
    ));
    assert!(fixture.attempts(expired).await.is_empty());
    let invalid = fixture.context();
    assert!(fixture.send("http://[invalid", invalid).await.is_err());
    assert!(fixture.attempts(invalid).await.is_empty());
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn actual_send_records_before_stream_completion_and_transfers_drop_ownership() {
    let fixture = Fixture::new().await;
    let upstream = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("response payload"))
        .expect(2)
        .mount(&upstream)
        .await;
    for consume in [false, true] {
        let context = fixture.context();
        let (response, _activity, selection) =
            fixture.send(&upstream.uri(), context).await.unwrap();
        let attempts = fixture.attempts(context).await;
        assert_eq!(attempts.len(), 1);
        assert!(!attempts[0].completed);
        selection.unwrap().select();
        if consume {
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                chunk.unwrap();
            }
        } else {
            drop(response);
        }
        let attempts = fixture.attempts(context).await;
        assert_eq!(attempts.len(), 1);
        assert!(attempts[0].completed && attempts[0].final_response);
        assert_eq!(attempts[0].response_body_complete, consume);
        assert_eq!(attempts[0].request_payload_bytes, Some(2));
        assert_eq!(
            attempts[0].response_payload_bytes,
            Some(if consume { 16 } else { 0 })
        );
    }
    upstream.verify().await;
}

#[tokio::test]
async fn cancelling_a_polled_send_before_headers_completes_only_the_actual_attempt() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = format!("http://{}", listener.local_addr().unwrap());
    let context = fixture.context();
    let mut send = Box::pin(fixture.send(&target, context));
    let peer = async {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut prefix = [0; 4096];
        assert!(socket.read(&mut prefix).await.unwrap() > 0);
        socket
    };
    let socket = tokio::select! {
        result = &mut send => panic!("send finished before response headers: {}", result.is_ok()),
        socket = peer => socket,
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("actual send did not reach peer"),
    };
    drop(send);
    drop(socket);
    let attempts = fixture.attempts(context).await;
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].completed);
    assert!(!attempts[0].final_response);
    assert_eq!(attempts[0].response_payload_bytes, None);
    assert!(!attempts[0].response_body_complete);
}
