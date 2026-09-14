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

struct HealthSnapshot {
    consecutive_failures: i64,
    cooldown_until: i64,
    probe_lease_until: i64,
    last_failure_kind: String,
    updated_at: i64,
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
    let response = send_resilient_chat(&fixture, None, false).await;
    assert!(!response.status().is_success());
    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "503 must never replay"
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
