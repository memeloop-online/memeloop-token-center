use super::*;
use futures_util::FutureExt;

#[tokio::test]
async fn optional_routing_sql_never_holds_forwarding_or_lifecycle_capacity() {
    for mode in ["pool_starvation", "pool_closed", "saturation"] {
        for stream in [false, true] {
            let fixture = codex_route_fixture(&format!("optional-routing-{mode}-{stream}")).await;
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
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
            sqlx::query("UPDATE upstream_account_health SET cooldown_until=0, probe_lease_until=0 WHERE upstream_account_id=$1")
                .bind(fixture.upstream_account_id.to_string()).execute(&pool).await.unwrap();
            let holders = if mode == "pool_starvation" {
                fixture
                    .state
                    .routing_persistence_db
                    .hold_group_snapshot_pool_for_tests()
                    .await
            } else {
                Vec::new()
            };
            if mode == "pool_closed" {
                fixture.state.routing_persistence_db.close().await;
            }
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            if mode == "saturation" {
                for _ in 0..4 {
                    let release = release.clone();
                    assert!(fixture.state.routing_persistence.submit(1, async move {
                        release.notified().await;
                        Ok(())
                    }));
                }
                tokio::task::yield_now().await;
            }
            let lifecycle_capacity = fixture.state.proxy_lifecycle_permits.available_permits();
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(
                    completed_codex_sse("optional SQL cannot hold output"),
                    "text/event-stream",
                ))
                .expect(1)
                .mount(&upstream)
                .await;
            tokio::time::timeout(Duration::from_secs(1), async {
                let response = send_codex_route(
                    &fixture,
                    &upstream,
                    "/v1/responses",
                    json!({
                        "model": fixture.model, "input": "isolation", "stream": stream,
                        "metadata": {"session_id": "optional-sql-session"}
                    }),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK, "{mode}, stream={stream}");
                let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                    .await
                    .unwrap();
                assert!(String::from_utf8_lossy(&body).contains("optional SQL cannot hold output"));
                wait_for_request_settlement(&fixture, 1).await;
                while fixture.state.proxy_lifecycle_permits.available_permits()
                    != lifecycle_capacity
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("output, settlement and lifecycle capacity must precede optional SQL release");
            if mode == "saturation" {
                assert!(
                    !fixture
                        .state
                        .routing_persistence
                        .render_routing()
                        .contains("outcome=\"capacity\"} 0\n")
                );
            }
            drop(holders);
            release.notify_waiters();
            fixture.state.routing_persistence.drain_for_test().await;
            if mode == "pool_closed" {
                assert!(
                    !fixture
                        .state
                        .routing_persistence
                        .render_routing()
                        .contains("outcome=\"failed\"} 0\n")
                );
            }
            let rows = fixture
                .state
                .db
                .list_requests(fixture.key_id, 10)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_exactly_once_side_effects(&fixture, rows[0].request_id, Some("resp-codex"))
                .await;
            upstream.verify().await;
            pool.close().await;
        }
    }
}

#[tokio::test]
async fn health_publication_returns_ready_without_polling_sql() {
    let fixture = codex_route_fixture("health-publication-ready").await;
    let holders = fixture
        .state
        .routing_persistence_db
        .hold_group_snapshot_pool_for_tests()
        .await;
    let mut guard = UpstreamAttemptGuard::new(
        &fixture.state,
        Uuid::new_v4(),
        fixture.route_id,
        fixture.upstream_account_id,
        1,
        0,
        UpstreamAttemptAdmission::SharedProbe {
            lease_token: Uuid::new_v4(),
        },
        None,
    );
    assert!(guard.delivered_validated_output().now_or_never().is_some());
    assert!(
        guard
            .complete(UpstreamAttemptTerminal::Succeeded)
            .now_or_never()
            .is_some()
    );
    drop(holders);
    fixture.state.routing_persistence.drain_for_test().await;
}

#[tokio::test]
async fn session_preferences_are_scoped_cached_and_explicit_hints_win_without_sql() {
    let fixture = codex_route_fixture("session-preference-cache").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let hints = crate::conversation::ConversationHints {
        session_id: Some("cache-session".into()),
        ..Default::default()
    };
    let request_id = Uuid::new_v4();
    fixture.state.session_preferences.observe(
        &fixture.state,
        crate::db::SessionRoutingTerminalInput {
            key: &key,
            request_id,
            explicit_session_id: "cache-session",
            model: &fixture.model,
            protocol: "openai-responses",
            status_code: 502,
            error_code: Some("upstream_timeout"),
            model_route_id: Some(fixture.route_id),
            upstream_account_id: Some(fixture.upstream_account_id),
            observed_at: crate::db::unix_millis(),
        },
    );
    fixture.state.routing_persistence_db.close().await;
    let lookup = |key: &AuthenticatedKey, model: &str, hint| {
        session_route_account_to_avoid(
            &fixture.state,
            key,
            request_id,
            &hints,
            model,
            "openai-responses",
            hint,
        )
    };
    assert_eq!(
        lookup(&key, &fixture.model, None),
        Some((fixture.route_id, fixture.upstream_account_id))
    );
    assert_eq!(
        lookup(&key, &fixture.model, Some(fixture.upstream_account_id)),
        None
    );
    assert_eq!(lookup(&key, "other-model", None), None);
    for dimension in ["tenant", "principal", "key", "generation"] {
        let mut other = key.clone();
        match dimension {
            "tenant" => other.tenant_id = Uuid::new_v4(),
            "principal" => other.principal_id = Uuid::new_v4(),
            "key" => other.key_id = Uuid::new_v4(),
            _ => other.credential_generation += 1,
        }
        assert_eq!(lookup(&other, &fixture.model, None), None);
    }
    fixture.state.routing_persistence.drain_for_test().await;
}
