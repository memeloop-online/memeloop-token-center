use super::*;

#[tokio::test]
async fn provider429_publishes_exact_model_and_next_request_uses_alternate_without_probe_post() {
    let exhausted = MockServer::start().await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_json(json!({"error":{"type":"usage_limit_reached"}})),
        )
        .expect(1)
        .mount(&exhausted)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(2)
        .mount(&healthy)
        .await;
    let fixture = resilient_route_fixture(
        "scoped-provider429",
        &[(exhausted.uri(), 0), (healthy.uri(), 10)],
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "INSERT INTO upstream_service_probes (upstream_account_id, status, error_code, latency_ms, checked_at)
         VALUES ($1, 'unhealthy', 'probe_fixture', 17, 1)",
    ).bind(fixture.accounts[1].to_string()).execute(&pool).await.unwrap();
    for _ in 0..2 {
        let response = send_resilient_chat(&fixture, None, false).await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        fixture.state.routing_persistence.drain_for_test().await;
    }
    let evidence = sqlx::query(
        "SELECT upstream_model, failure_kind, reset_source, reset_at
         FROM upstream_supplier_availability WHERE upstream_account_id = $1",
    )
    .bind(fixture.accounts[0].to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(evidence.get::<String, _>("upstream_model"), "upstream-0");
    assert_eq!(evidence.get::<String, _>("failure_kind"), "quota_exhausted");
    assert_eq!(evidence.get::<String, _>("reset_source"), "retry_after");
    assert!(evidence.get::<Option<i64>, _>("reset_at").unwrap() > unix_millis());
    let independent = fixture
        .state
        .db
        .upstream_service_probe(fixture.accounts[1])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(independent["checked_at"], 1);
    assert_eq!(independent["status"], "unknown");
    assert_eq!(independent["source"], "legacy_unverified");
    assert_eq!(
        independent["historical_result"]["error_code"],
        "probe_fixture"
    );
    exhausted.verify().await;
    healthy.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn mtc_admission429_is_not_supplier_quota_evidence() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;
    let fixture = resilient_route_fixture("mtc429", &[(upstream.uri(), 0)]).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let policy = KeyPolicy {
        requests_per_minute: Some(1),
        allowed_models: vec![fixture.model.clone()],
        ..KeyPolicy::default()
    };
    sqlx::query("UPDATE key_records SET policy_json = $2 WHERE id = $1")
        .bind(fixture.key_id.to_string())
        .bind(serde_json::to_string(&policy).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let first = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(first.status(), StatusCode::OK);
    let _ = to_bytes(first.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let _ = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    fixture.state.routing_persistence.drain_for_test().await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_supplier_availability")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    upstream.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn default_probe_skips_unusable_cheaper_models_and_explicit_alias_stays_skipped() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/responses"))
        .and(body_partial_json(json!({"model":"upstream-0","max_output_tokens":16})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"probe-fixture", "object":"response", "status":"completed", "model":"upstream-0",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],
            "usage":{"input_tokens":2,"output_tokens":1,"total_tokens":3}
        }))).expect(1).mount(&upstream).await;
    let fixture = resilient_route_fixture("default-probe", &[(upstream.uri(), 0)]).await;
    let tenant = "resilient-default-probe";
    let account = fixture.accounts[0];
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let route_id: String =
        sqlx::query_scalar("SELECT id FROM model_routes WHERE upstream_account_id = $1")
            .bind(account.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let alias = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "cheap-alias".into(),
            upstream_account_id: account,
            upstream_model: "different-model".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    sqlx::query("INSERT INTO routing_grants (tenant_id,key_id,model_route_id,created_at) SELECT tenant_id,$1,id,1 FROM model_routes WHERE id=$2")
        .bind(fixture.key_id.to_string()).bind(alias.id.to_string()).execute(&pool).await.unwrap();
    for name in ["cheap-disabled", "cheap-alias"] {
        fixture
            .state
            .db
            .upsert_model_price(name, "USD", Decimal::new(1, 6), Decimal::new(1, 6))
            .await
            .unwrap();
    }
    let wrong_currency = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: "eur-only".into(),
            upstream_account_id: account,
            upstream_model: "eur-only".into(),
            protocol: "openai".into(),
            priority: 0,
        })
        .await
        .unwrap();
    sqlx::query("INSERT INTO routing_grants (tenant_id,key_id,model_route_id,created_at) SELECT tenant_id,$1,id,1 FROM model_routes WHERE id=$2")
        .bind(fixture.key_id.to_string()).bind(wrong_currency.id.to_string()).execute(&pool).await.unwrap();
    fixture
        .state
        .db
        .upsert_model_price("eur-only", "EUR", Decimal::ZERO, Decimal::ZERO)
        .await
        .unwrap();
    let context = fixture
        .state
        .db
        .probe_account_context(account)
        .await
        .unwrap();
    let lease = Uuid::now_v7();
    assert!(
        fixture
            .state
            .db
            .claim_upstream_model_catalog_sync(
                account,
                tenant,
                context.credential_generation,
                lease
            )
            .await
            .unwrap()
    );
    let models = [
        "cheap-disabled",
        "cheap-alias",
        "different-model",
        "upstream-0",
        "eur-only",
    ]
    .map(|name| crate::db::DiscoveredUpstreamModel {
        model_id: name.into(),
        protocol: "openai".into(),
        context_window: None,
        reservation_token_bound: None,
        reservation_bound_source: None,
    });
    fixture
        .state
        .db
        .replace_upstream_model_catalog(
            account,
            tenant,
            context.credential_generation,
            lease,
            "openai_v1",
            &models,
        )
        .await
        .unwrap();
    fixture
        .state
        .db
        .plugin_kv_put(
            "typed-filter",
            &format!(
                "settings/{}",
                &blake3::hash(tenant.as_bytes()).to_hex()[..32]
            ),
            &serde_json::to_vec(
                &json!({"model_route_id":route_id,"billing_key_id":fixture.key_id,"updated_at":1}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let invoke = || {
        router_for_role(fixture.state.clone(), RuntimeRole::Control).oneshot(
            Request::post(format!("/internal/v1/upstreams/{account}/health"))
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.state.config.service_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
    };
    let response = invoke().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["status"], "healthy", "{result}");
    assert_eq!(result["selected_model"], "upstream-0");
    assert_eq!(result["actual_model"], "upstream-0");
    assert_eq!(result["upstream_status"], 200);
    assert_eq!(result["trigger"], "manual");
    assert!(result["request_id"].as_str().is_some());
    let mut config = context.config;
    config["probe_model"] = json!("cheap-alias");
    sqlx::query("UPDATE upstream_accounts SET config_json=$2 WHERE id=$1")
        .bind(account.to_string())
        .bind(config.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = invoke().await.unwrap();
    let skipped: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(skipped["status"], "skipped");
    assert_eq!(skipped["selected_model"], "cheap-alias");
    assert_eq!(skipped["error_code"], "probe_route_unavailable");
    let saved = fixture
        .state
        .db
        .upstream_service_probe(account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["checked_at"], result["checked_at"]);
    assert_eq!(saved["actual_model"], "upstream-0");
    assert_eq!(saved["last_attempt"]["status"], "skipped");
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::get(format!("/internal/v1/upstreams/{account}/health"))
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", fixture.state.config.service_token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let reloaded: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        reloaded["status"], "skipped",
        "the existing status UI must not reload stale healthy"
    );
    assert_eq!(reloaded["error_code"], "probe_route_unavailable");
    assert_eq!(reloaded["last_result"]["checked_at"], result["checked_at"]);
    assert_eq!(reloaded["last_result"]["actual_model"], "upstream-0");
    // A local preparation timeout is not a dispatched probe and cannot
    // overwrite the real result, even though it is a connection-class error.
    config["probe_model"] = json!("upstream-0");
    config["timeout_seconds"] = json!(1);
    sqlx::query("UPDATE upstream_accounts SET config_json=$2 WHERE id=$1")
        .bind(account.to_string())
        .bind(config.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = routing::with_stalled_probe_preparation(invoke())
        .await
        .unwrap();
    let undispatched: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(undispatched["status"], "skipped", "{undispatched}");
    assert!(undispatched["actual_model"].is_null());
    assert!(undispatched["upstream_status"].is_null());
    let saved = fixture
        .state
        .db
        .upstream_service_probe(account)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["actual_model"], "upstream-0");
    assert_eq!(saved["checked_at"], result["checked_at"]);
    assert_eq!(saved["last_attempt"]["status"], "skipped");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    // Legacy known reset: periodic sends nothing; explicitly requested manual
    // recovery still sends. Neither path changes a disabled account.
    let deadline = unix_millis() + 3_600_000;
    sqlx::query("INSERT INTO upstream_account_health (upstream_account_id, consecutive_failures, cooldown_until, probe_lease_until, probe_lease_token, credential_generation, transport_revision, last_failure_kind, updated_at) VALUES ($1,1,$2,0,'',$3,$4,'quota_exhausted',$5) ON CONFLICT (upstream_account_id) DO UPDATE SET cooldown_until=excluded.cooldown_until, last_failure_kind=excluded.last_failure_kind, credential_generation=excluded.credential_generation, transport_revision=excluded.transport_revision")
        .bind(account.to_string()).bind(deadline).bind(context.credential_generation).bind(context.transport_revision).bind(unix_millis())
        .execute(&pool).await.unwrap();
    let periodic = crate::api::upstreams::run_upstream_service_probe(
        &fixture.state,
        account,
        "periodic",
        None,
    )
    .await;
    assert_eq!(periodic["status"], "skipped");
    assert_eq!(periodic["error_code"], "supplier_quota_reset_pending");
    assert_eq!(periodic["reset_at"], deadline);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    // Ordinary response with no timing and a shorter exhausted response must
    // retain the provider deadline and perform zero background inference.
    let scoped_route = ResolvedUpstream {
        route_id: Uuid::parse_str(&route_id).unwrap(),
        account_id: account,
        credential_generation: context.credential_generation,
        transport_revision: context.transport_revision,
        driver: "http-json".into(),
        base_url: upstream.uri(),
        config: config.clone(),
        upstream_model: "upstream-0".into(),
        credential: UpstreamCredential::None,
    };
    for exhausted in [false, true] {
        for reset_at in [Some(deadline), None, Some(deadline - 60_000)] {
            fixture
                .state
                .db
                .store_supplier_quota(
                    &scoped_route,
                    Uuid::now_v7(),
                    unix_millis(),
                    crate::db::SupplierQuotaEvidence {
                        exhausted,
                        reset_at,
                        reset_source: if reset_at.is_some() {
                            "retry_after"
                        } else {
                            "unknown"
                        },
                    },
                )
                .await
                .unwrap();
        }
        let periodic = crate::api::upstreams::run_upstream_service_probe(
            &fixture.state,
            account,
            "periodic",
            None,
        )
        .await;
        assert_eq!(periodic["reset_at"], deadline);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
    upstream.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = invoke().await.unwrap();
    let sent: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(sent["status"], "unhealthy");
    assert_eq!(sent["actual_model"], "upstream-0");
    assert_eq!(sent["upstream_status"], 503);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
    upstream.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&upstream)
        .await;
    let response = invoke().await.unwrap();
    let timed_out: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(timed_out["status"], "unhealthy", "{timed_out}");
    assert_eq!(timed_out["actual_model"], "upstream-0");
    assert!(timed_out["upstream_status"].is_null());
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    config["probe_model"] = json!("eur-only");
    sqlx::query("UPDATE upstream_accounts SET config_json=$2 WHERE id=$1")
        .bind(account.to_string())
        .bind(config.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = invoke().await.unwrap();
    let unpriced: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(unpriced["status"], "skipped");
    assert_eq!(unpriced["error_code"], "probe_model_unpriced");
    assert_eq!(unpriced["selected_model"], "eur-only");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    sqlx::query("UPDATE upstream_accounts SET status='disabled' WHERE id=$1")
        .bind(account.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let response = invoke().await.unwrap();
    let disabled: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(disabled["error_code"], "probe_account_unavailable");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    upstream.verify().await;
    pool.close().await;
}
