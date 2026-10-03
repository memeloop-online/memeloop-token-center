use super::*;

#[tokio::test]
async fn queued_delivery_recovery_cannot_clear_a_newer_health_epoch() {
    let fixture = codex_route_fixture("queued-stale-health").await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.upstream_account_id,
            1,
            UpstreamFailureKind::InvalidResponse,
        )
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0 WHERE upstream_account_id = $1")
        .bind(fixture.upstream_account_id.to_string()).execute(&pool).await.unwrap();
    let revision: i64 =
        sqlx::query_scalar("SELECT updated_at FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let admission = fixture
        .state
        .db
        .claim_upstream_account_attempt_at_revision_with_health_config(
            fixture.upstream_account_id,
            1,
            revision,
            fixture.state.config.upstream_health,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(admission, UpstreamAttemptAdmission::Probe { .. }));
    let mut guard = UpstreamAttemptGuard::new(
        &fixture.state,
        Uuid::new_v4(),
        fixture.route_id,
        fixture.upstream_account_id,
        1,
        revision,
        admission,
        None,
    );
    let holders = fixture
        .state
        .observation_db
        .hold_group_snapshot_pool_for_tests()
        .await;
    guard.delivered_validated_output();
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.upstream_account_id,
            1,
            UpstreamFailureKind::Authentication,
        )
        .await
        .unwrap();
    let completion = tokio::spawn(async move {
        guard.complete(UpstreamAttemptTerminal::Inconclusive).await;
    });
    tokio::task::yield_now().await;
    assert!(
        !completion.is_finished(),
        "terminal health owner must consume the queued acknowledgement first"
    );
    drop(holders);
    tokio::time::timeout(Duration::from_secs(3), completion)
        .await
        .unwrap()
        .unwrap();
    let failure: String = sqlx::query_scalar(
        "SELECT last_failure_kind FROM upstream_account_health WHERE upstream_account_id = $1",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(failure, UpstreamFailureKind::Authentication.as_str());
    assert!(
        !fixture
            .state
            .metrics
            .render(&crate::metrics::RuntimeMetrics::default())
            .contains("event=\"recovered\"")
    );
}

#[tokio::test]
async fn delivery_health_pool_starvation_and_queue_saturation_do_not_hold_eof() {
    for mode in ["pool", "queue", "closed"] {
        let fixture = codex_route_fixture(&format!("delivery-health-{mode}")).await;
        fixture
            .state
            .db
            .record_upstream_account_failure(
                fixture.upstream_account_id,
                1,
                UpstreamFailureKind::InvalidResponse,
            )
            .await
            .unwrap();
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        sqlx::query("UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0 WHERE upstream_account_id = $1")
            .bind(fixture.upstream_account_id.to_string()).execute(&pool).await.unwrap();
        let holders = if mode == "pool" {
            fixture
                .state
                .observation_db
                .hold_group_snapshot_pool_for_tests()
                .await
        } else {
            Vec::new()
        };
        if mode == "closed" {
            fixture.state.observation_db.close().await;
        }
        let mut releases = Vec::new();
        let mut completions = Vec::new();
        if mode == "queue" {
            for _ in 0..16 {
                let (release, wait) = tokio::sync::oneshot::channel();
                releases.push(release);
                completions.push(
                    fixture
                        .state
                        .observations
                        .submit_health(async move {
                            wait.await.unwrap();
                            false
                        })
                        .unwrap(),
                );
            }
            assert!(
                fixture
                    .state
                    .observations
                    .submit_health(async { panic!("rejected work must not run") })
                    .is_none()
            );
        }
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                completed_codex_sse("health publication is independent"),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        let body = tokio::time::timeout(Duration::from_secs(2), async {
            let response = send_codex_route(
                &fixture,
                &upstream,
                "/v1/responses",
                json!({"model":fixture.model,"input":"bounded health","stream":true}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap()
        })
        .await
        .expect("all frames and EOF precede optional health release");
        assert!(String::from_utf8_lossy(&body).contains("health publication is independent"));
        if mode == "pool" {
            assert!(
                fixture
                    .state
                    .observations
                    .render()
                    .contains("delivery_health_jobs 1\n")
            );
        }
        if mode == "queue" {
            assert!(
                fixture
                    .state
                    .observations
                    .render()
                    .contains("delivery_health_rejected_total 2\n")
            );
        }
        drop(holders);
        for release in releases {
            release.send(()).unwrap();
        }
        for completion in completions {
            assert!(!completion.await.unwrap());
        }
        wait_for_request_settlement(&fixture, 1).await;
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex")).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let failures: i64 = sqlx::query_scalar("SELECT consecutive_failures FROM upstream_account_health WHERE upstream_account_id = $1")
                    .bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
                if failures == 0 && fixture.state.observations.render().contains("delivery_health_jobs 0\n") { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("terminal health owner converges after release, including rejected early recovery");
        upstream.verify().await;
    }
}

#[tokio::test]
async fn session_preference_cold_miss_is_nonblocking_and_local_terminal_fences_refresh() {
    use crate::api::proxy::observations::sessions::SessionCache;
    let fixture = codex_route_fixture("session-preference-pool").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let holders = fixture
        .state
        .observation_db
        .hold_group_snapshot_pool_for_tests()
        .await;
    assert_eq!(
        SessionCache::lookup(
            &fixture.state,
            &key,
            "session",
            &fixture.model,
            "openai-responses"
        ),
        None
    );
    let input = crate::db::SessionRoutingTerminalInput {
        key: &key,
        request_id: Uuid::new_v4(),
        explicit_session_id: "session",
        model: &fixture.model,
        protocol: "openai-responses",
        status_code: 502,
        error_code: Some("upstream_transport_connection_reset"),
        model_route_id: Some(fixture.route_id),
        upstream_account_id: Some(fixture.upstream_account_id),
        observed_at: crate::db::unix_millis(),
    };
    fixture.state.observations.sessions.publish(input);
    let route = Some((fixture.route_id, fixture.upstream_account_id));
    assert_eq!(
        SessionCache::lookup(
            &fixture.state,
            &key,
            "session",
            &fixture.model,
            "openai-responses"
        ),
        route
    );
    let mut other_key = key.clone();
    other_key.key_id = Uuid::new_v4();
    assert_eq!(
        SessionCache::lookup(
            &fixture.state,
            &other_key,
            "session",
            &fixture.model,
            "openai-responses"
        ),
        None
    );
    drop(holders);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture
            .state
            .observations
            .render()
            .contains("session_preference_refresh_jobs 0\n")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        SessionCache::lookup(
            &fixture.state,
            &key,
            "session",
            &fixture.model,
            "openai-responses"
        ),
        route
    );
    fixture
        .state
        .observations
        .sessions
        .publish(crate::db::SessionRoutingTerminalInput {
            status_code: 200,
            error_code: None,
            observed_at: input.observed_at + 1,
            ..input
        });
    fixture.state.observations.sessions.publish(input);
    assert_eq!(
        SessionCache::lookup(
            &fixture.state,
            &key,
            "session",
            &fixture.model,
            "openai-responses"
        ),
        None
    );
}
