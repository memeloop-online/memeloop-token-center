use std::{fs, time::Duration};

use memeloop_token_center::{
    db::Database,
    plugin::{
        PluginRuntime,
        routing::{
            GroupRoutingCandidate, GroupRoutingHealth, GroupRoutingInput, GroupRoutingObserveInput,
            GroupRoutingOutcome,
        },
    },
};
use serde_json::json;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

fn component(output: &str, loop_forever: bool) -> Vec<u8> {
    component_with_observe(output, output, loop_forever)
}

fn component_with_observe(output: &str, observe: &str, loop_forever: bool) -> Vec<u8> {
    let escaped: String = output.bytes().map(|byte| format!("\\{byte:02x}")).collect();
    let observe_escaped: String = observe
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect();
    let observe_body = format!(
        "i32.const 32 i32.const 0 i32.store i32.const 36 i32.const 4096 i32.store i32.const 40 i32.const {} i32.store i32.const 32",
        observe.len()
    );
    let body = if loop_forever {
        "(loop br 0) unreachable".to_owned()
    } else {
        format!(
            "i32.const 32 i32.const 0 i32.store i32.const 36 i32.const 1024 i32.store i32.const 40 i32.const {} i32.store i32.const 32",
            output.len()
        )
    };
    let source = format!(
        r#"(module
        (memory (export "memory") 2)
        (global $heap (mut i32) (i32.const 8192))
        (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
            (local $result i32)
            global.get $heap local.set $result
            global.get $heap local.get 3 i32.add global.set $heap
            local.get $result)
        (data (i32.const 1024) "{escaped}")
        (data (i32.const 4096) "{observe_escaped}")
        (func (export "memeloop:token-center/group-routing-v1@0.2.0#plan")
            (param i32 i32) (result i32) {body})
        (func (export "memeloop:token-center/group-routing-v1@0.2.0#observe")
            (param i32 i32) (result i32) {observe_body})
    )"#
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

#[tokio::test]
async fn pinned_runtime_keeps_old_plan_and_observe_after_package_upgrade() {
    let (directory, initial) = runtime(r#"{"candidates":[]}"#, false).await;
    let package = directory.path().join("plugins/router");
    let directive = |cooldown| {
        json!({
            "tenant_id":"tenant", "route_id":"route", "account_id":"account", "generation":1,
            "allow_transient_probe":true, "cooldown_ms":cooldown,
            "recovery_wait_ms":0, "recheck_ms":100, "stickiness":false
        })
    };
    let write_version = |cooldown, version| {
        let observe = directive(cooldown).to_string();
        let plan = json!({"candidates":[directive(cooldown)]}).to_string();
        fs::write(
            package.join("plugin.wasm"),
            component_with_observe(&plan, &observe, false),
        )
        .unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(package.join("plugin.json")).unwrap()).unwrap();
        manifest["version"] = json!(version);
        fs::write(
            package.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    };
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("db.sqlite").display()
    ))
    .await
    .unwrap();
    drop(initial);
    write_version(10, "1.0.0");
    let old =
        PluginRuntime::load(directory.path().join("plugins").to_str(), database.clone()).unwrap();
    let pinned = old.clone();
    write_version(20, "1.1.0");
    let current = PluginRuntime::load(directory.path().join("plugins").to_str(), database).unwrap();
    drop(old);
    let candidate = GroupRoutingCandidate {
        tenant_id: "tenant".into(),
        route_id: "route".into(),
        account_id: "account".into(),
        generation: 1,
        health: GroupRoutingHealth::Transient,
    };
    let mut request = input();
    request.candidates.push(candidate.clone());
    let observation = GroupRoutingObserveInput {
        tenant_id: "tenant".into(),
        seed: 42,
        remaining_deadline_ms: 0,
        config: json!({}),
        candidate,
        outcome: GroupRoutingOutcome::TransientFailure,
    };
    assert_eq!(
        pinned
            .execute_group_routing_plan("router", &request)
            .unwrap()
            .candidates[0]
            .cooldown_ms,
        10
    );
    assert_eq!(
        current
            .execute_group_routing_plan("router", &request)
            .unwrap()
            .candidates[0]
            .cooldown_ms,
        20
    );
    assert_eq!(
        pinned
            .execute_group_routing_observe("router", &observation)
            .unwrap()
            .cooldown_ms,
        10
    );
    assert_eq!(
        current
            .execute_group_routing_observe("router", &observation)
            .unwrap()
            .cooldown_ms,
        20
    );
}

async fn runtime(output: &str, loop_forever: bool) -> (tempfile::TempDir, PluginRuntime) {
    let directory = tempfile::tempdir().unwrap();
    let package = directory.path().join("plugins/router");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("plugin.json"),
        serde_json::to_vec(&json!({
            "id":"router", "version":"1.0.0", "wit_version":"0.2.0",
            "wasm":"plugin.wasm", "capabilities":[],
            "contributions":{"group_routing":{
                "version":"group-routing-v1",
                "schema":{"type":"object","additionalProperties":false},
                "default":{}
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(package.join("plugin.wasm"), component(output, loop_forever)).unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("db.sqlite").display()
    ))
    .await
    .unwrap();
    let runtime = PluginRuntime::load(directory.path().join("plugins").to_str(), database).unwrap();
    (directory, runtime)
}

fn input() -> GroupRoutingInput {
    GroupRoutingInput {
        tenant_id: "tenant".into(),
        seed: 42,
        remaining_deadline_ms: 1000,
        config: json!({}),
        candidates: Vec::new(),
    }
}

#[tokio::test]
async fn independent_routing_world_runs_without_old_exports_and_is_deterministic() {
    let (_directory, runtime) = runtime(r#"{"candidates":[]}"#, false).await;
    let pinned = runtime.clone();
    assert_eq!(
        pinned.group_routing_strategies()[0].version,
        "group-routing-v1"
    );
    assert!(
        pinned
            .validate_group_routing_configuration("router", &json!({"unexpected":true}))
            .is_err()
    );
    let first = pinned
        .execute_group_routing_plan("router", &input())
        .unwrap();
    let second = pinned
        .execute_group_routing_plan("router", &input())
        .unwrap();
    assert_eq!(first.candidates, second.candidates);
}

#[tokio::test]
async fn routing_guest_unknown_fields_and_infinite_loop_fail_closed() {
    let (_directory, runtime) = runtime(r#"{"candidates":[],"replay":true}"#, false).await;
    assert!(
        runtime
            .execute_group_routing_plan("router", &input())
            .is_err()
    );
    let (_directory, mut runtime) = self::runtime("", true).await;
    runtime.set_execution_limits_for_tests(Duration::from_millis(50), 1000);
    assert!(
        runtime
            .execute_group_routing_plan("router", &input())
            .is_err()
    );
}
