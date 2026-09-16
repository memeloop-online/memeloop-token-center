//! Real router requests through a credential-free Wasm group strategy and the
//! shared health database. All upstream traffic stays on wiremock loopback.
use super::*;
use crate::{
    db::{
        CreateGroupInput, GroupKind, GroupRoutingStrategy, ReplaceGroupMembersInput,
        UpdateGroupRoutingStrategyInput,
    },
    plugin::PluginRuntime,
};
use std::fs;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

fn routing_component(plan: &Value, observe: &Value) -> Vec<u8> {
    let plan = plan.to_string();
    let observe = observe.to_string();
    let escape = |text: &str| {
        text.bytes()
            .map(|byte| format!("\\{byte:02x}"))
            .collect::<String>()
    };
    let body = |offset, length| {
        format!(
            "i32.const 32 i32.const 0 i32.store i32.const 36 i32.const {offset} i32.store i32.const 40 i32.const {length} i32.store i32.const 32"
        )
    };
    let source = format!(
        r#"(module
      (memory (export "memory") 2)
      (global $heap (mut i32) (i32.const 8192))
      (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
        (local $result i32) global.get $heap local.set $result
        global.get $heap local.get 3 i32.add global.set $heap local.get $result)
      (data (i32.const 1024) "{}")
      (data (i32.const 4096) "{}")
      (func (export "memeloop:token-center/group-routing-v1@0.2.0#plan")
        (param i32 i32) (result i32) {})
      (func (export "memeloop:token-center/group-routing-v1@0.2.0#observe")
        (param i32 i32) (result i32) {})
    )"#,
        escape(&plan),
        escape(&observe),
        body(1024, plan.len()),
        body(4096, observe.len())
    );
    let mut module = wat::parse_str(source).unwrap();
    let mut resolve = Resolve::default();
    let (package, _) = resolve.push_path("wit/token-center.wit").unwrap();
    let world = resolve
        .select_world(&[package], Some("group-routing-plugin"))
        .unwrap();
    embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
    ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

async fn install_strategy(
    fixture: &mut ResilientRouteFixture,
    label: &str,
    observe_cooldown: u64,
) -> (Uuid, i64) {
    let tenant_external_id = format!("resilient-{label}");
    let group = fixture
        .state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant_external_id.clone(),
                name: "Wasm recovery".into(),
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
                member_ids: vec![fixture.accounts[0]],
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
                    plugin_id: "recovery".into(),
                    config: json!({}),
                }),
                routing_priority: 0,
            },
        )
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let row = sqlx::query("SELECT route.id AS route_id, account.credential_generation FROM model_routes route JOIN upstream_accounts account ON account.id = route.upstream_account_id WHERE account.id = $1")
        .bind(fixture.accounts[0].to_string()).fetch_one(&pool).await.unwrap();
    let route_id: String = row.get("route_id");
    let generation: i64 = row.get("credential_generation");
    sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
        .bind(group.tenant_id.to_string()).bind(&route_id).bind(group.id.to_string()).execute(&pool).await.unwrap();
    pool.close().await;
    let directive = |cooldown, probe| {
        json!({
            "tenant_id":group.tenant_id.to_string(), "route_id":route_id,
            "account_id":fixture.accounts[0].to_string(), "generation":generation,
            "allow_transient_probe":probe, "cooldown_ms":cooldown,
            "recovery_wait_ms":0, "recheck_ms":100, "stickiness":false
        })
    };
    let root = fixture._directory.path().join("group-plugins");
    let package = root.join("recovery");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("plugin.json"), serde_json::to_vec(&json!({
        "id":"recovery", "version":"1.0.0", "wit_version":"0.2.0", "wasm":"plugin.wasm",
        "capabilities":[], "contributions":{"group_routing":{
            "version":"group-routing-v1", "schema":{"type":"object","additionalProperties":false}, "default":{}
        }}
    })).unwrap()).unwrap();
    // The same plan tries an immediate transient probe in all tests. Hard
    // quota must reject this output and preserve native closed admission.
    fs::write(
        package.join("plugin.wasm"),
        routing_component(
            &json!({"candidates":[directive(0, true)]}),
            &directive(observe_cooldown, false),
        ),
    )
    .unwrap();
    fixture.state.plugins = PluginRuntime::load(root.to_str(), fixture.state.db.clone()).unwrap();
    (group.tenant_id, generation)
}

#[tokio::test]
async fn installed_hook_without_group_configuration_skips_candidate_health_snapshot() {
    let upstream = MockServer::start().await;
    let label = "group-native-fast-path";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 0).await;
    assert!(fixture.state.plugins.has_group_routing_hooks());
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE provider_groups SET routing_strategy = NULL WHERE tenant_id = $1")
        .bind(tenant.to_string())
        .execute(&pool)
        .await
        .unwrap();
    // This isolated test database intentionally makes the larger batch query
    // impossible. The native-only path needs only the group existence query.
    sqlx::query("DROP TABLE upstream_account_health")
        .execute(&pool)
        .await
        .unwrap();
    let mut candidates = vec![crate::provider::AuthorizedUpstreamCandidate {
        route_id: Uuid::now_v7(),
        account_id: fixture.accounts[0],
        driver: "openai".into(),
        transport_revision: 1,
        credential_generation: generation,
    }];
    let before = candidates.clone();
    crate::group_routing::prepare(
        &mut fixture.state,
        tenant,
        Uuid::nil(),
        Uuid::now_v7(),
        tokio::time::Instant::now() + Duration::from_secs(1),
        &mut candidates,
    )
    .await
    .unwrap();
    assert_eq!(before, candidates);
    assert!(fixture.state.group_routing.is_none());
    let metrics = fixture.state.metrics.render(&Default::default());
    assert!(metrics.contains("phase=\"group_routing_plan\",outcome=\"returned\"} 0"));
    pool.close().await;
}

struct HealthSnapshot {
    consecutive_failures: i64,
    cooldown_until: i64,
    probe_lease_until: i64,
    last_failure_kind: String,
    updated_at: i64,
}

#[tokio::test]
#[ignore = "requires MTC_PREFERRED_ACCOUNT_PACKAGE built in CI"]
async fn preferred_account_real_gateway_uses_order_without_overriding_native_health() {
    let native = MockServer::start().await;
    let preferred = MockServer::start().await;
    for upstream in [&native, &preferred] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(successful_chat_response())
            .expect(1)
            .mount(upstream)
            .await;
    }
    let label = "preferred-account-native-health";
    let mut fixture =
        resilient_route_fixture(label, &[(native.uri(), 100), (preferred.uri(), 0)]).await;
    let tenant_external_id = format!("resilient-{label}");
    let group = fixture
        .state
        .db
        .create_group(
            GroupKind::Provider,
            CreateGroupInput {
                tenant_external_id: tenant_external_id.clone(),
                name: "Preferred account".into(),
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
                    plugin_id: "mtc-preferred-account".into(),
                    config: json!({"preferred_account_ids":[fixture.accounts[1].to_string()]}),
                }),
                routing_priority: 0,
            },
        )
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let rows = sqlx::query("SELECT route.id, route.upstream_account_id, account.credential_generation FROM model_routes route JOIN upstream_accounts account ON account.id = route.upstream_account_id WHERE route.tenant_id = $1 ORDER BY route.id")
        .bind(group.tenant_id.to_string()).fetch_all(&pool).await.unwrap();
    let mut candidates = Vec::new();
    let mut preferred_generation = 0;
    for row in rows {
        let route: String = row.get("id");
        let account: String = row.get("upstream_account_id");
        let generation: i64 = row.get("credential_generation");
        sqlx::query("INSERT INTO model_route_included_provider_groups (tenant_id,model_route_id,provider_group_id,created_at) VALUES ($1,$2,$3,1)")
            .bind(group.tenant_id.to_string()).bind(&route).bind(group.id.to_string()).execute(&pool).await.unwrap();
        let account_id = Uuid::parse_str(&account).unwrap();
        if account_id == fixture.accounts[1] {
            preferred_generation = generation;
        }
        candidates.push(crate::provider::AuthorizedUpstreamCandidate {
            route_id: Uuid::parse_str(&route).unwrap(),
            account_id,
            driver: "openai".into(),
            transport_revision: 1,
            credential_generation: generation,
        });
    }
    pool.close().await;
    let source = std::path::PathBuf::from(
        std::env::var("MTC_PREFERRED_ACCOUNT_PACKAGE").expect("real guest package required"),
    );
    let root = fixture._directory.path().join("preferred-plugins");
    let package = root.join("mtc-preferred-account");
    fs::create_dir_all(&package).unwrap();
    for name in ["plugin.json", "plugin.wasm"] {
        fs::copy(source.join(name), package.join(name)).unwrap();
    }
    fixture.state.plugins = PluginRuntime::load(root.to_str(), fixture.state.db.clone()).unwrap();
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
    assert_eq!(native.received_requests().await.unwrap().len(), 1);
}

async fn health(fixture: &ResilientRouteFixture, tenant: Uuid, generation: i64) -> HealthSnapshot {
    let snapshot = fixture
        .state
        .db
        .group_routing_health(tenant, fixture.accounts[0], generation)
        .await
        .unwrap()
        .unwrap();
    HealthSnapshot {
        consecutive_failures: snapshot.consecutive_failures,
        cooldown_until: snapshot.cooldown_until,
        probe_lease_until: snapshot.probe_lease_until,
        last_failure_kind: snapshot.last_failure_kind,
        updated_at: snapshot.updated_at,
    }
}

#[tokio::test]
async fn gateway_wasm_zero_transient_cooldown_recovers_only_after_durable_success() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&upstream)
        .await;
    let label = "group-transient-recovery";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 12345).await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::Unavailable,
        )
        .await
        .unwrap();
    assert!(health(&fixture, tenant, generation).await.cooldown_until > unix_millis());
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let recovered = health(&fixture, tenant, generation).await;
    assert_eq!(recovered.consecutive_failures, 0);
    assert_eq!(recovered.probe_lease_until, 0);
    let metrics = fixture.state.metrics.render(&Default::default());
    assert!(metrics.contains("phase=\"group_routing_plan\",outcome=\"returned\"} 1"));
    assert!(metrics.contains("phase=\"group_routing_observe\",outcome=\"returned\"} 1"));
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let status: i64 = sqlx::query_scalar("SELECT status_code FROM request_records WHERE key_id = $1 ORDER BY created_at DESC LIMIT 1")
        .bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(status, 200);
    pool.close().await;
}

#[tokio::test]
async fn gateway_same_wasm_probe_cannot_resurrect_hard_quota() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&upstream)
        .await;
    let label = "group-hard-quota";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 0).await;
    let until = unix_millis() + 60_000;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::RateLimitedUntil {
                until,
                exhausted: true,
            },
        )
        .await
        .unwrap();
    let before = health(&fixture, tenant, generation).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let after = health(&fixture, tenant, generation).await;
    assert_eq!(after.cooldown_until, before.cooldown_until);
    assert_eq!(after.last_failure_kind, "quota_exhausted");
    assert_eq!(after.probe_lease_until, 0);
}

#[tokio::test]
async fn gateway_wasm_observe_sets_transient_cooldown_without_replaying_503() {
    let upstream = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(503)
                .set_body_json(json!({"error":{"message":"mock unavailable"}})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let label = "group-observe-failure";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices":[{"message":{"role":"assistant","content":"must not replay"}}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })))
        .expect(0)
        .mount(&standby)
        .await;
    let mut fixture =
        resilient_route_fixture(label, &[(upstream.uri(), 100), (standby.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 12345).await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::Unavailable,
        )
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert!(!response.status().is_success());
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "503 must never replay"
    );
    assert!(
        standby.received_requests().await.unwrap().is_empty(),
        "a confirmed POST response must not replay on the remaining authorized candidate"
    );
    let observed = health(&fixture, tenant, generation).await;
    assert_eq!(observed.last_failure_kind, "unavailable");
    assert_eq!(
        observed.cooldown_until - observed.updated_at,
        12345 * 2,
        "must consume the actual Wasm observe output"
    );
    assert_eq!(observed.probe_lease_until, 0);
}

async fn install_component_provider(fixture: &mut ResilientRouteFixture) {
    // Reuse the audited real buffered provider component, disabling only its
    // unrelated traffic rewrite and allowing this credential-free mock account.
    let root = fixture._directory.path().join("group-plugins");
    let package = root.join("example-provider");
    fs::create_dir_all(&package).unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../../examples/plugins/policy-rewrite/plugin.json"
    ))
    .unwrap();
    manifest["contributions"]["traffic_policy"] = json!(false);
    manifest["contributions"]["request_rewrite"] = json!(false);
    manifest["contributions"]["providers"][0]["credential_schema"] = json!({
        "type":"object", "additionalProperties":false,
        "required":["type"], "properties":{"type":{"const":"none"}}
    });
    manifest["contributions"]["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("oauth_adapter");
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        package.join("plugin.wasm"),
        include_bytes!("../../../../examples/plugins/policy-rewrite/plugin.wasm"),
    )
    .unwrap();
    fixture.state.plugins = PluginRuntime::load(root.to_str(), fixture.state.db.clone()).unwrap();
    fixture
        .state
        .providers
        .extend(fixture.state.plugins.provider_types())
        .unwrap();
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET driver = 'example-oauth-http' WHERE id = $1")
        .bind(fixture.accounts[0].to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn gateway_group_component_invalid_wire_response_observes_failure_without_replay() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/vendor/infer"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-encoding", "unsupported-test-encoding")
                .set_body_json(json!({"vendor_answer":"not-decodable"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let label = "group-component-invalid-wire";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 12345).await;
    install_component_provider(&mut fixture).await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::Unavailable,
        )
        .await
        .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let observed = health(&fixture, tenant, generation).await;
    assert_eq!(observed.last_failure_kind, "invalid_response");
    assert_eq!(observed.cooldown_until - observed.updated_at, 12345 * 2);
    assert_eq!(observed.probe_lease_until, 0);
}

#[tokio::test]
async fn gateway_component_hard_quota_survives_invalid_strategy_native_fallback() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"vendor_answer":"must-not-send"})),
        )
        .expect(0)
        .mount(&upstream)
        .await;
    let label = "group-component-hard-fallback";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 0).await;
    install_component_provider(&mut fixture).await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::RateLimitedUntil {
                until: unix_millis() + 60_000,
                exhausted: true,
            },
        )
        .await
        .unwrap();
    let before = health(&fixture, tenant, generation).await;
    // The real plan always requests allow_transient_probe=true. Hard quota
    // makes that result invalid and leaves no per-candidate policy. The core
    // component admission gate must still run under this native fallback.
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let after = health(&fixture, tenant, generation).await;
    assert_eq!(after.last_failure_kind, "quota_exhausted");
    assert_eq!(after.cooldown_until, before.cooldown_until);
    assert_eq!(after.consecutive_failures, before.consecutive_failures);
    assert_eq!(after.probe_lease_until, 0);
}

#[tokio::test]
async fn gateway_component_without_configured_strategy_dispatches_healthy_account() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/vendor/infer"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"vendor_answer":"legacy-result"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let label = "component-no-strategy-legacy";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 12345).await;
    install_component_provider(&mut fixture).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE provider_groups SET routing_strategy = NULL WHERE tenant_id = $1")
        .bind(tenant.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let before = health(&fixture, tenant, generation).await;
    // An installed plugin alone is not an opted-in group strategy. The native
    // core health gate admits a healthy component without scheduling hooks.
    let response = send_resilient_chat(&fixture, None, false).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "normalized by component"
    );
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let after = health(&fixture, tenant, generation).await;
    assert_eq!(after.last_failure_kind, before.last_failure_kind);
    assert_eq!(after.cooldown_until, before.cooldown_until);
    assert_eq!(after.consecutive_failures, before.consecutive_failures);
    assert_eq!(after.probe_lease_until, before.probe_lease_until);
    assert!(
        after.updated_at > before.updated_at,
        "native success may rotate the healthy cohort fence"
    );
}

#[tokio::test]
async fn gateway_component_without_configured_strategy_still_blocks_hard_quota() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"vendor_answer":"must-not-send"})),
        )
        .expect(0)
        .mount(&upstream)
        .await;
    let label = "component-no-strategy-hard-quota";
    let mut fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
    let (tenant, generation) = install_strategy(&mut fixture, label, 12345).await;
    install_component_provider(&mut fixture).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE provider_groups SET routing_strategy = NULL WHERE tenant_id = $1")
        .bind(tenant.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    fixture
        .state
        .db
        .record_upstream_account_failure(
            fixture.accounts[0],
            generation,
            UpstreamFailureKind::RateLimitedUntil {
                until: unix_millis() + 60_000,
                exhausted: true,
            },
        )
        .await
        .unwrap();
    let before = health(&fixture, tenant, generation).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let after = health(&fixture, tenant, generation).await;
    assert_eq!(after.last_failure_kind, "quota_exhausted");
    assert_eq!(after.cooldown_until, before.cooldown_until);
    assert_eq!(after.consecutive_failures, before.consecutive_failures);
    assert_eq!(after.probe_lease_until, 0);
    assert_eq!(after.updated_at, before.updated_at);
}
