use super::*;
use crate::api::proxy::routing::credential_application_error;

async fn expire_current_credential_metadata(fixture: &CodexRouteFixture) {
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE upstream_credentials SET expires_at = $1
         WHERE upstream_account_id = $2
           AND generation = (SELECT credential_generation FROM upstream_accounts WHERE id = $2)",
    )
    .bind(crate::db::unix_millis())
    .bind(fixture.upstream_account_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn expired_primary_credential_is_filtered_before_healthy_standby() {
    let fixture = codex_route_fixture("expired-primary").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-expired-primary", "account-456").await;
    expire_current_credential_metadata(&fixture).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("healthy standby"), "text/event-stream"),
        )
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "expired primary", "stream": false}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, standby.to_string());
    pool.close().await;
    upstream.verify().await;
}

#[tokio::test]
async fn credential_expiring_after_resolution_skips_to_prepared_standby() {
    let fixture = codex_route_fixture("expiry-prepare-race").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-expiry-prepare-race", "account-456").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let request = json!({"model": fixture.model, "input": "expiry race", "stream": false});
    let candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: request_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(candidates[0].account_id, fixture.upstream_account_id);
    expire_current_credential_metadata(&fixture).await;

    let mut snapshot_refresh_used = false;
    let prepared = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
        request: ProxyRequestContext {
            state: &fixture.state,
            key: &key,
            model: &fixture.model,
            protocol: Protocol::OpenAiResponses,
            request_id,
            request_json: &request,
            request_memory: None,
            codex_multi_agent_v2_request: false,
        },
        original_body_length: serde_json::to_vec(&request).unwrap().len(),
        candidates,
        selection: RouteSelectionOptions {
            upstream_account_hint: None,
            avoid_route_account: None,
            selection_seed: request_id,
        },
        snapshot_refresh_used: &mut snapshot_refresh_used,
    })
    .await
    .unwrap();
    assert_eq!(prepared.primary.route.account_id, standby);
}

#[tokio::test]
async fn local_codex_chat_protocol_keeps_priority_over_native_candidate() {
    let fixture = codex_route_fixture("local-protocol-mismatch").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let compatible = fixture
        .state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "codex-route-local-protocol-mismatch".to_owned(),
                name: "compatible-http-standby".to_owned(),
                driver: "http-json".to_owned(),
                config: json!({
                    "base_url": "https://example.com",
                    "network_scope": "public"
                }),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let compatible_route = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "codex-route-local-protocol-mismatch".to_owned(),
            public_model: fixture.model.clone(),
            upstream_account_id: compatible.id,
            upstream_model: "compatible-model".to_owned(),
            protocol: "openai".to_owned(),
            priority: 10,
        })
        .await
        .unwrap();
    fixture
        .state
        .db
        .upsert_model_price("compatible-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at)
         VALUES ($1, $2, $3, NULL, $4)",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(compatible_route.id.to_string())
    .bind(crate::db::unix_millis())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiChat.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: request_id,
            },
        )
        .await
        .unwrap();

    let request = json!({
        "model": fixture.model,
        "messages": [{"role": "user", "content": "local mismatch"}],
        "stream": false
    });
    let mut snapshot_refresh_used = false;
    let prepared = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
        request: ProxyRequestContext {
            state: &fixture.state,
            key: &key,
            model: &fixture.model,
            protocol: Protocol::OpenAiChat,
            request_id,
            request_json: &request,
            request_memory: None,
            codex_multi_agent_v2_request: false,
        },
        original_body_length: serde_json::to_vec(&request).unwrap().len(),
        candidates,
        selection: RouteSelectionOptions {
            upstream_account_hint: None,
            avoid_route_account: None,
            selection_seed: request_id,
        },
        snapshot_refresh_used: &mut snapshot_refresh_used,
    })
    .await
    .unwrap();
    assert_eq!(
        prepared.primary.route.account_id,
        fixture.upstream_account_id
    );
}

#[tokio::test]
async fn changed_transport_revision_invalidates_the_prepared_snapshot() {
    let fixture = codex_route_fixture("transport-snapshot-fence").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let mut candidate = fixture
        .state
        .db
        .resolve_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: request_id,
            },
            fixture.state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap()
        .remove(0);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET updated_at = updated_at + 1 WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    assert_eq!(
        refresh_route_snapshot(&fixture.state, &mut candidate)
            .await
            .unwrap(),
        PreparedRouteReadiness::Unavailable
    );
}

#[test]
fn credential_expiring_at_header_application_is_typed_unavailable() {
    let now = crate::db::unix_millis();
    let credential = UpstreamCredential::OAuth {
        access_token: "expired".to_owned(),
        refresh_token: None,
        expires_at: Some(now),
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
        adapter_state: None,
        proxy_url: None,
        proxy_network_scope: None,
    };
    assert!(matches!(
        credential_application_error(&credential, now),
        ProxySendError::CredentialUnavailable
    ));
}

#[tokio::test]
async fn header_application_expiry_skips_to_standby_without_breaker_failure() {
    let fixture = codex_route_fixture("header-expiry-standby").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-header-expiry-standby", "account-456").await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-123"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .and(header_matcher("chatgpt-account-id", "account-456"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_codex_sse("standby after header expiry"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = routing::with_test_credential_application_now_once(
        i64::MAX,
        send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model": fixture.model, "input": "header expiry", "stream": false}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    upstream.verify().await;

    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let actual: String = sqlx::query_scalar(
        "SELECT upstream_account_id FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.key_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let primary_failure_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM upstream_account_health
         WHERE upstream_account_id = $1 AND consecutive_failures > 0",
    )
    .bind(fixture.upstream_account_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(actual, standby.to_string());
    assert_eq!(primary_failure_count, 0);
    pool.close().await;
}

#[tokio::test]
async fn unavailable_snapshot_reasons_preserve_fail_closed_materialization() {
    let fixture = codex_route_fixture("snapshot-reasons").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let mut candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: Uuid::now_v7(),
            },
        )
        .await
        .unwrap();
    let candidate = candidates.remove(0);
    let pepper = fixture.state.config.key_pepper.as_bytes();
    let (ready, reason) = fixture
        .state
        .db
        .materialize_authorized_upstream_candidate_diagnostic(&candidate, pepper)
        .await
        .unwrap();
    assert!(ready.is_some());
    assert_eq!(reason, None);
    for (revision, generation, expected) in [
        (
            candidate.transport_revision - 1,
            candidate.credential_generation,
            "transport_revision_changed",
        ),
        (
            candidate.transport_revision,
            candidate.credential_generation - 1,
            "credential_generation_changed",
        ),
    ] {
        let stale = AuthorizedUpstreamCandidate {
            transport_revision: revision,
            credential_generation: generation,
            ..candidate.clone()
        };
        let (route, reason) = fixture
            .state
            .db
            .materialize_authorized_upstream_candidate_diagnostic(&stale, pepper)
            .await
            .unwrap();
        assert!(route.is_none());
        assert_eq!(reason, Some(expected));
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    // Invalid ciphertext must never be opened for a revoked credential.
    sqlx::query("UPDATE upstream_credentials SET revoked_at = $1, credential_ciphertext = 'invalid' WHERE upstream_account_id = $2 AND generation = $3")
        .bind(crate::db::unix_millis()).bind(candidate.account_id.to_string()).bind(candidate.credential_generation).execute(&pool).await.unwrap();
    let (route, reason) = fixture
        .state
        .db
        .materialize_authorized_upstream_candidate_diagnostic(&candidate, pepper)
        .await
        .unwrap();
    assert!(route.is_none());
    assert_eq!(reason, Some("credential_revoked"));
    sqlx::query("UPDATE model_routes SET enabled = 0 WHERE id = $1")
        .bind(candidate.route_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let (route, reason) = fixture
        .state
        .db
        .materialize_authorized_upstream_candidate_diagnostic(&candidate, pepper)
        .await
        .unwrap();
    assert!(route.is_none());
    assert_eq!(reason, Some("route_disabled"));
    pool.close().await;
}

#[tokio::test]
async fn stale_snapshot_refresh_is_request_bounded_and_does_not_restore_revoked_candidates() {
    let fixture = codex_route_fixture("snapshot-one-refresh").await;
    let standby =
        add_codex_standby_route(&fixture, "codex-route-snapshot-one-refresh", "account-456").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let selection = RouteSelectionOptions {
        upstream_account_hint: None,
        avoid_route_account: None,
        selection_seed: request_id,
    };
    let mut candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            selection,
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 2);
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut rotation = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO upstream_credentials (id,upstream_account_id,generation,credential_ciphertext,expires_at,created_at) SELECT $2,upstream_account_id,generation + 1,credential_ciphertext,expires_at,created_at FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = (SELECT credential_generation FROM upstream_accounts WHERE id = $1)")
        .bind(fixture.upstream_account_id.to_string()).execute(&mut *rotation).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET credential_generation = credential_generation + 1, updated_at = updated_at + 1 WHERE id = $1")
        .bind(fixture.upstream_account_id.to_string()).execute(&mut *rotation).await.unwrap();
    rotation.commit().await.unwrap();
    sqlx::query("UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2")
        .bind(crate::db::unix_millis())
        .bind(standby.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let request =
        json!({"model": fixture.model, "input": "fresh snapshot contract", "stream": false});
    let context = ProxyRequestContext {
        state: &fixture.state,
        key: &key,
        model: &fixture.model,
        protocol: Protocol::OpenAiResponses,
        request_id,
        request_json: &request,
        request_memory: None,
        codex_multi_agent_v2_request: false,
    };
    let mut used = false;
    let plan = prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
        request: context,
        original_body_length: 100,
        candidates: candidates.clone(),
        selection,
        snapshot_refresh_used: &mut used,
    })
    .await
    .unwrap();
    assert!(used);
    assert_eq!(plan.primary.route.account_id, fixture.upstream_account_id);
    assert!(
        plan.remaining_candidates
            .as_slice()
            .iter()
            .all(|candidate| candidate.account_id != standby)
    );
    // The second preparation (including group-policy replan) cannot acquire a
    // second refresh allowance, even if the same stale tuple is supplied.
    assert!(
        prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
            request: context,
            original_body_length: 100,
            candidates,
            selection,
            snapshot_refresh_used: &mut used
        })
        .await
        .is_err()
    );
    pool.close().await;
}

#[tokio::test]
async fn stale_snapshot_refresh_rechecks_revoked_grants() {
    let fixture = codex_route_fixture("snapshot-grant-revoked").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let selection = RouteSelectionOptions {
        upstream_account_hint: None,
        avoid_route_account: None,
        selection_seed: request_id,
    };
    let mut candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            selection,
        )
        .await
        .unwrap();
    for candidate in &mut candidates {
        candidate.transport_revision -= 1;
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DELETE FROM routing_grants WHERE key_id = $1")
        .bind(key.key_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let request =
        json!({"model": fixture.model, "input": "revoked grant contract", "stream": false});
    let mut used = false;
    assert!(
        prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
            request: ProxyRequestContext {
                state: &fixture.state,
                key: &key,
                model: &fixture.model,
                protocol: Protocol::OpenAiResponses,
                request_id,
                request_json: &request,
                request_memory: None,
                codex_multi_agent_v2_request: false
            },
            original_body_length: 100,
            candidates,
            selection,
            snapshot_refresh_used: &mut used
        })
        .await
        .is_err()
    );
    assert!(used);
    pool.close().await;
}

#[tokio::test]
async fn stale_snapshot_refresh_does_not_admit_real_quota_exhaustion() {
    let fixture = codex_route_fixture("snapshot-quota-exhausted").await;
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let selection = RouteSelectionOptions {
        upstream_account_hint: None,
        avoid_route_account: None,
        selection_seed: request_id,
    };
    let mut candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            key.key_id,
            key.tenant_id,
            &fixture.model,
            Protocol::OpenAiResponses.name(),
            selection,
        )
        .await
        .unwrap();
    for candidate in &mut candidates {
        candidate.transport_revision -= 1;
    }
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("INSERT INTO upstream_account_health (upstream_account_id,consecutive_failures,cooldown_until,probe_lease_until,probe_lease_token,credential_generation,transport_revision,last_failure_kind,updated_at) SELECT id,1,$1,0,'',credential_generation,updated_at,'quota_exhausted',$2 FROM upstream_accounts WHERE id = $3")
        .bind(crate::db::unix_millis() + 3_600_000)
        .bind(crate::db::unix_millis())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let request =
        json!({"model": fixture.model, "input": "revoked grant contract", "stream": false});
    let mut used = false;
    assert!(
        prepare_authorized_proxy_routes(AuthorizedProxyRoutesInput {
            request: ProxyRequestContext {
                state: &fixture.state,
                key: &key,
                model: &fixture.model,
                protocol: Protocol::OpenAiResponses,
                request_id,
                request_json: &request,
                request_memory: None,
                codex_multi_agent_v2_request: false
            },
            original_body_length: 100,
            candidates,
            selection,
            snapshot_refresh_used: &mut used
        })
        .await
        .is_err()
    );
    assert!(used);
    pool.close().await;
}
