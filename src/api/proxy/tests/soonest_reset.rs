//! Real CI-built guest, persisted read-only quota evidence and mock-only upstreams.
use super::*;
use crate::{
    db::{
        CreateGroupInput, GroupKind, GroupRoutingStrategy, ReplaceGroupMembersInput,
        UpdateGroupRoutingStrategyInput,
    },
    plugin::PluginRuntime,
};
use std::fs;

fn quota_plugin(fixture: &ResilientRouteFixture) -> PluginRuntime {
    let source = std::path::PathBuf::from(
        std::env::var("MTC_SOONEST_RESET_PACKAGE").expect("real guest package required"),
    );
    let root = fixture._directory.path().join("preferred-plugins");
    let package = root.join("mtc-soonest-reset");
    fs::create_dir_all(&package).unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(source.join("plugin.json")).unwrap()).unwrap();
    let mut files = vec!["plugin.json", "plugin.wasm"];
    for ui in manifest["contributions"]["operator_ui"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(entry) = ui["module_entry"].as_str() {
            files.push(entry);
        }
    }
    for name in files {
        fs::create_dir_all(package.join(name).parent().unwrap()).unwrap();
        fs::copy(source.join(name), package.join(name)).unwrap();
    }
    PluginRuntime::load(root.to_str(), fixture.state.db.clone()).unwrap()
}

#[tokio::test]
#[ignore = "requires MTC_SOONEST_RESET_PACKAGE built in CI"]
async fn soonest_reset_real_gateway_uses_shared_evidence_and_preserves_native_health() {
    let native = MockServer::start().await;
    let preferred = MockServer::start().await;
    for upstream in [&native, &preferred] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(successful_chat_response())
            .expect(if std::ptr::eq(upstream, &native) {
                2
            } else {
                1
            })
            .mount(upstream)
            .await;
    }
    let label = "soonest-reset-native-health";
    // Native route priority is ascending: the plugin must override this
    // deliberate baseline only while complete fresh evidence is available.
    let mut fixture =
        resilient_route_fixture(label, &[(native.uri(), 0), (preferred.uri(), 100)]).await;
    let tenant_external_id = format!("resilient-{label}");
    let group = fixture
        .state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant_external_id.clone(),
                name: "Soonest reset".into(),
            },
        )
        .await
        .unwrap();
    let group = fixture
        .state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant_external_id.clone(),
                member_ids: fixture.accounts.clone(),
                expected_updated_at: group.updated_at,
            },
        )
        .await
        .unwrap();
    fixture
        .state
        .db
        .update_group_routing_strategy(
            GroupKind::Provider,
            group.id,
            UpdateGroupRoutingStrategyInput {
                tenant_external_id,
                expected_updated_at: group.updated_at,
                expected_strategy_version: group.strategy_version,
                routing_strategy: Some(GroupRoutingStrategy {
                    plugin_id: "mtc-soonest-reset".into(),
                    config: json!({"target_provider":"http-json","target_window_id":"summary"}),
                }),
                routing_priority: 0,
            },
        )
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let rows = sqlx::query("SELECT route.id, route.upstream_account_id, account.credential_generation, account.updated_at, account.driver FROM model_routes route JOIN upstream_accounts account ON account.id = route.upstream_account_id WHERE route.tenant_id = $1 ORDER BY route.id")
        .bind(group.tenant_id.to_string()).fetch_all(&pool).await.unwrap();
    let mut preferred_generation = 0;
    for row in rows {
        let route: String = row.get("id");
        let account: String = row.get("upstream_account_id");
        let generation: i64 = row.get("credential_generation");
        let revision: i64 = row.get("updated_at");
        let driver: String = row.get("driver");
        sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
            .bind(group.tenant_id.to_string()).bind(&route).bind(group.id.to_string()).execute(&pool).await.unwrap();
        let account_id = Uuid::parse_str(&account).unwrap();
        if account_id == fixture.accounts[1] {
            preferred_generation = generation;
        }
        // Fixture-only persisted observation: no supplier calls or producer tricks.
        let now = unix_millis();
        let observation = json!({
            "account_id": account_id, "generation": generation, "config_revision": revision,
            "provider": driver, "observed_at": now, "valid_until": now + 300_000,
            "windows": [{"id":"summary","period_seconds":604800,
                "reset_at":now + if account_id == fixture.accounts[1] {600_000} else {1_200_000},
                "reset_is_estimated":false,"remaining_fraction":0.5,"exhausted":false}]
        });
        sqlx::query("INSERT INTO upstream_quota_observations (upstream_account_id,tenant_id,credential_generation,config_revision,observation_json,valid_until) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(&account).bind(group.tenant_id.to_string()).bind(generation).bind(revision)
            .bind(observation.to_string()).bind(now + 300_000).execute(&pool).await.unwrap();
    }
    pool.close().await;
    let mut candidates = fixture
        .state
        .db
        .list_authorized_upstream_candidates_with_hint(
            fixture.key_id,
            group.tenant_id,
            &fixture.model,
            "openai",
            crate::db::RouteSelectionOptions {
                upstream_account_hint: None,
                avoid_route_account: None,
                selection_seed: Uuid::from_u128(7),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        candidates[0].account_id, fixture.accounts[0],
        "the real native resolver must prefer the lower-priority-number route before quota ordering"
    );
    fixture.state.plugins = quota_plugin(&fixture);
    crate::group_routing::prepare(
        &mut fixture.state,
        group.tenant_id,
        Uuid::from_u128(7),
        Uuid::now_v7(),
        tokio::time::Instant::now() + Duration::from_secs(5),
        &mut candidates,
    )
    .await
    .unwrap();
    assert_eq!(candidates[0].account_id, fixture.accounts[1]);
    let snapshot = fixture.state.group_routing.as_ref().unwrap();
    for candidate in &candidates {
        assert!(
            snapshot
                .policy(
                    candidate.route_id,
                    candidate.account_id,
                    candidate.credential_generation
                )
                .is_none(),
            "order-only must never install recovery overrides"
        );
    }
    let durable = crate::group_routing::durable::capture(&fixture.state)
        .unwrap()
        .unwrap();
    assert!(durable["policies"].as_array().unwrap().is_empty());
    let restored = crate::group_routing::durable::restore_selected(
        &fixture.state,
        Some(&durable),
        group.tenant_id,
        Some(candidates[0].route_id),
        candidates[0].account_id,
        Uuid::now_v7(),
    )
    .await
    .unwrap();
    assert!(
        restored
            .group_routing
            .as_ref()
            .unwrap()
            .policy(
                candidates[0].route_id,
                candidates[0].account_id,
                preferred_generation
            )
            .is_none()
    );
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        preferred.received_requests().await.unwrap().len(),
        1,
        "native weighted/rendezvous selection must not overwrite the guest order"
    );
    assert!(native.received_requests().await.unwrap().is_empty());
    // Expired evidence must not keep a previous preference alive or trigger
    // on-request quota refresh: the otherwise healthy native-priority route wins.
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query(
        "UPDATE upstream_quota_observations SET valid_until=0 WHERE upstream_account_id=$1",
    )
    .bind(fixture.accounts[1].to_string())
    .execute(&pool)
    .await
    .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(native.received_requests().await.unwrap().len(), 1);
    assert_eq!(preferred.received_requests().await.unwrap().len(), 1);
    sqlx::query(
        "UPDATE upstream_quota_observations SET valid_until=$2 WHERE upstream_account_id=$1",
    )
    .bind(fixture.accounts[1].to_string())
    .bind(unix_millis() + 300_000)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[1],
            preferred_generation,
            UpstreamFailureKind::RateLimitedUntil {
                until: unix_millis() + 60_000,
                exhausted: true,
            },
        )
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        preferred.received_requests().await.unwrap().len(),
        1,
        "known exhausted preferred account must not be contacted"
    );
    assert_eq!(native.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
#[ignore = "requires MTC_SOONEST_RESET_PACKAGE with remaining_balance built in CI"]
async fn remaining_quota_real_gateway_keeps_one_session_on_one_account() {
    let high = MockServer::start().await;
    let low = MockServer::start().await;
    for upstream in [&high, &low] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(successful_chat_response())
            .mount(upstream)
            .await;
    }
    let label = "remaining-quota-same-session";
    let mut fixture = resilient_route_fixture(label, &[(high.uri(), 0), (low.uri(), 0)]).await;
    let tenant_external_id = format!("resilient-{label}");
    let group = fixture
        .state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant_external_id.clone(),
                name: "Quota allocation".into(),
            },
        )
        .await
        .unwrap();
    let group = fixture
        .state
        .db
        .replace_group_members(
            GroupKind::Provider,
            group.id,
            ReplaceGroupMembersInput {
                tenant_external_id: tenant_external_id.clone(),
                member_ids: fixture.accounts.clone(),
                expected_updated_at: group.updated_at,
            },
        )
        .await
        .unwrap();
    fixture.state.db.update_group_routing_strategy(GroupKind::Provider, group.id, UpdateGroupRoutingStrategyInput {
        tenant_external_id, expected_updated_at: group.updated_at, expected_strategy_version: group.strategy_version,
        routing_strategy: Some(GroupRoutingStrategy { plugin_id: "mtc-soonest-reset".into(), config: json!({"target_provider":"http-json","target_window_id":"summary","mode":"remaining_balance"}) }), routing_priority: 0,
    }).await.unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let rows = sqlx::query("SELECT route.id, route.upstream_account_id, account.credential_generation, account.updated_at FROM model_routes route JOIN upstream_accounts account ON account.id=route.upstream_account_id WHERE route.tenant_id=$1")
        .bind(group.tenant_id.to_string()).fetch_all(&pool).await.unwrap();
    for row in rows {
        let route: String = row.get("id");
        let account: String = row.get("upstream_account_id");
        let generation: i64 = row.get("credential_generation");
        let revision: i64 = row.get("updated_at");
        sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
            .bind(group.tenant_id.to_string()).bind(route).bind(group.id.to_string()).execute(&pool).await.unwrap();
        let now = unix_millis();
        let remaining = if account == fixture.accounts[0].to_string() {
            0.8
        } else {
            0.4
        };
        let observation = json!({"account_id":account,"generation":generation,"config_revision":revision,"provider":"http-json","observed_at":now,"valid_until":now+300_000,
            "windows":[{"id":"summary","period_seconds":604800,"reset_at":null,"reset_is_estimated":false,"remaining_fraction":remaining,"exhausted":false}]});
        sqlx::query("INSERT INTO upstream_quota_observations (upstream_account_id,tenant_id,credential_generation,config_revision,observation_json,valid_until) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(account).bind(group.tenant_id.to_string()).bind(generation).bind(revision).bind(observation.to_string()).bind(now+300_000).execute(&pool).await.unwrap();
    }
    fixture.state.plugins = quota_plugin(&fixture);
    for _ in 0..4 {
        let response = send_resilient_chat(&fixture, Some("one-shared-session"), false).await;
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    let high_count = high.received_requests().await.unwrap().len();
    let low_count = low.received_requests().await.unwrap().len();
    assert_eq!(high_count + low_count, 4);
    assert!(
        high_count == 4 || low_count == 4,
        "high={high_count}, low={low_count}"
    );
    assert_eq!(
        high_count, 4,
        "the initially larger remaining quota receives the first session"
    );
    let hour = unix_millis() / 3_600_000;
    for (account, requests) in [(fixture.accounts[0], 90_i64), (fixture.accounts[1], 10_i64)] {
        sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,account_id,hour_bucket) DO UPDATE SET requests=excluded.requests,tokens=excluded.tokens")
            .bind(group.tenant_id.to_string()).bind(account.to_string()).bind(hour).bind(requests).bind(requests*1000).execute(&pool).await.unwrap();
    }
    // New sessions correct historical load while the established conversation keeps its account.
    for session in ["new-session-after-long-conversation", "one-shared-session"] {
        let response = send_resilient_chat(&fixture, Some(session), false).await;
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    assert_eq!(high.received_requests().await.unwrap().len(), 5);
    assert_eq!(
        low.received_requests().await.unwrap().len(),
        1,
        "new session follows the underallocated account"
    );
    let bindings: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_allocation_sessions WHERE tenant_id=$1")
            .bind(group.tenant_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        bindings, 2,
        "both choices committed through the allocation path"
    );
    sqlx::query("UPDATE upstream_quota_observations SET valid_until=0")
        .execute(&pool)
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, Some("one-shared-session"), false).await;
    let status = response.status();
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        high.received_requests().await.unwrap().len()
            + low.received_requests().await.unwrap().len(),
        7
    );
    sqlx::query("DROP TABLE group_allocation_usage")
        .execute(&pool)
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, Some("one-shared-session"), false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        high.received_requests().await.unwrap().len()
            + low.received_requests().await.unwrap().len(),
        8,
        "allocation storage failure preserves normal routing"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        responses_transport::wait_for_financial_events(&fixture.state, fixture.key_id, 8),
    )
    .await
    .unwrap();
    let financial=sqlx::query("SELECT q.status_code,(SELECT COUNT(*) FROM ledger_entries l WHERE l.source=q.reservation_id) AS ledgers,(SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id=q.id) AS facts,(SELECT COUNT(*) FROM session_routing_terminals s WHERE s.request_id=q.id) AS terminals FROM request_records q WHERE key_id=$1")
        .bind(fixture.key_id.to_string()).fetch_all(&pool).await.unwrap();
    assert_eq!(financial.len(), 8);
    for row in financial {
        assert_eq!(row.get::<i64, _>("status_code"), 200);
        assert_eq!(row.get::<i64, _>("ledgers"), 1);
        assert_eq!(row.get::<i64, _>("facts"), 1);
        assert_eq!(row.get::<i64, _>("terminals"), 1);
    }
    pool.close().await;
}
