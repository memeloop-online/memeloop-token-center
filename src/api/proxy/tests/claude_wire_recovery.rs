use super::*;
use crate::plugin::{
    PluginRuntime,
    application::PreinstalledInventory,
    lifecycle::{PluginGrant, manifest_digest},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, serde::Serialize)]
struct Capture {
    body: String,
    header_names: Vec<String>,
}

async fn capture(
    axum::extract::State(captures): axum::extract::State<Arc<tokio::sync::Mutex<Vec<Capture>>>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> axum::Json<Value> {
    let body = String::from_utf8(body.to_vec()).unwrap();
    let request: Value = serde_json::from_str(&body).unwrap();
    let mut header_names = headers
        .keys()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    header_names.sort();
    captures.lock().await.push(Capture { body, header_names });
    axum::Json(json!({
        "id":"msg_synthetic_wire_001","type":"message","role":"assistant",
        "model":request["model"],"content":[{"type":"text","text":"synthetic-ok"}],
        "stop_reason":"end_turn","stop_sequence":null,
        "usage":{"input_tokens":1,"output_tokens":1}
    }))
}

async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), task)
}

async fn control(state: &AppState, action: &str, key: &str, body: Value) -> Value {
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::post(format!("/internal/v1/plugin-runtime/{action}"))
                .header(
                    "authorization",
                    format!("Bearer {}", state.config.service_token),
                )
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "control action {action}: {}",
        response.status()
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

fn cases(model: &str) -> Vec<(&'static str, &'static str, Value)> {
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/claude-wire-synthetic.json"
    ))
    .unwrap();
    let mut native = fixtures["native_messages"]["body"].clone();
    native["model"] = json!(model);
    let mut no_user_id = native.clone();
    no_user_id["metadata"]
        .as_object_mut()
        .unwrap()
        .remove("user_id");
    let mut no_metadata = native.clone();
    no_metadata.as_object_mut().unwrap().remove("metadata");
    let mut responses = fixtures["responses_bridge"]["body"].clone();
    responses["model"] = json!(model);
    vec![
        ("native-remove-user-id", "/v1/messages", native),
        ("native-no-user-id", "/v1/messages", no_user_id),
        ("native-no-metadata", "/v1/messages", no_metadata),
        ("responses-bridge", "/v1/responses", responses),
    ]
}

fn finalize_observations(state: &AppState) -> Vec<String> {
    state.metrics.render(&crate::metrics::RuntimeMetrics::default()).lines()
        .filter(|line| line.starts_with("memeloop_token_center_plugin_execution_observations_total{phase=\"wire_shim_finalize\""))
        .map(str::to_owned).collect()
}

fn assert_finalize_count(state: &AppState, returned: usize) {
    let observations = finalize_observations(state);
    assert_eq!(observations.len(), 7);
    for observation in observations {
        let expected = if observation.contains("outcome=\"returned\"") {
            returned
        } else {
            0
        };
        assert_eq!(
            observation.rsplit_once(' ').unwrap().1,
            expected.to_string()
        );
    }
}

#[tokio::test]
#[ignore = "GHA installs and verifies the exact official signed OCI artifact"]
async fn signed_claude_gateway_empty_recovery_finalize_matrix() {
    let signed_package = std::path::PathBuf::from(
        std::env::var("MTC_CLAUDE_WIRE_FIXTURE").expect("signed OCI fixture required"),
    );
    for (name, expected) in [
        (
            "plugin.wasm",
            "51043af426632581a218c227a0f90b2de2253d27b5438519098c44d965c01f3c",
        ),
        (
            "plugin.json",
            "95aa1c2ec715a766e865d861b6e9b00cb157c1f77056fab300fc55fa3925c68e",
        ),
    ] {
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(signed_package.join(name)).unwrap())
            ),
            expected
        );
    }
    let provenance: Value = serde_json::from_slice(
        &std::fs::read(signed_package.join(".mtc-oci-install.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        provenance["source"],
        "ghcr.io/memeloop-online/claude-code-wire"
    );
    assert_eq!(
        provenance["digest"],
        "sha256:3e4160580005b15c9db1e3bd419df79ee65de1d86a8fe1d4ec5aac4c88fe5906"
    );
    assert_eq!(provenance["signature_policy"], "cosign-keyless");
    let captures = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (upstream, upstream_task) = serve(
        axum::Router::new()
            .route("/v1/messages", axum::routing::post(capture))
            .with_state(captures.clone()),
    )
    .await;
    let label = "signed-claude-wire";
    let fixture = response_usage_fixture_with_uri_contract_driver_model_and_credential(
        label,
        upstream,
        0,
        None,
        crate::oauth::claude::PROVIDER_DRIVER,
        "mtc-claude-wire-synthetic-v1",
        UpstreamCredential::OAuth {
            access_token: "synthetic-not-an-oauth-authorization".into(),
            refresh_token: None,
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({"schema":"anthropic-claude-oauth-v1"})),
            proxy_url: None,
            proxy_network_scope: None,
        },
    )
    .await;
    let tenant = format!("compatibility-route-{label}");
    let native_route = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: fixture.model.clone(),
            upstream_account_id: fixture.upstream_account_id,
            upstream_model: fixture.model.clone(),
            protocol: "anthropic".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let routing = fixture
        .state
        .db
        .credential_routing(fixture.key_id, &tenant)
        .await
        .unwrap();
    fixture
        .state
        .db
        .replace_credential_routing(
            fixture.key_id,
            crate::db::ReplaceCredentialRoutingInput {
                tenant_external_id: tenant,
                route_ids: vec![fixture.route_id, native_route.id],
                route_group_ids: vec![],
                expected_grant_revision: routing.grant_revision,
            },
        )
        .await
        .unwrap();
    let inventory_file = fixture._directory.path().join("inventory.json");
    let root = fixture._directory.path().join("signed-inventory");
    let package = root.join("claude-code-wire");
    std::fs::create_dir_all(&package).unwrap();
    for name in ["plugin.json", "plugin.wasm", ".mtc-oci-install.json"] {
        std::fs::copy(signed_package.join(name), package.join(name)).unwrap();
    }
    let runtime = PluginRuntime::load(root.to_str(), fixture.state.db.clone()).unwrap();
    let manifests = runtime.manifests();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].id, "claude-code-wire");
    assert!(manifests[0].capabilities.is_empty());
    let grant = PluginGrant {
        version: manifests[0].version.clone(),
        capabilities: vec![],
        manifest_digest: manifest_digest(&manifests[0]).unwrap(),
        identity: runtime.package_identities()["claude-code-wire"].clone(),
    };
    let trusted = BTreeMap::from([(
        "claude-reviewed".to_owned(),
        PreinstalledInventory {
            root,
            grants: BTreeMap::from([("claude-code-wire".into(), vec![grant])]),
        },
    )]);
    std::fs::write(&inventory_file, serde_json::to_vec(&trusted).unwrap()).unwrap();
    let mut config = (*fixture.state.config).clone();
    config.plugin_inventory_file = Some(inventory_file.to_str().unwrap().into());
    let control_state = AppState::initialize(config.clone()).await.unwrap();
    let mut gateway_state = AppState::initialize(config.clone()).await.unwrap();
    let authority = control_state.application_plugins.as_ref().unwrap();
    assert!(authority.status().await.unwrap().current.is_none());
    control(
        &control_state,
        "empty-inventories",
        "register-empty",
        json!({"inventory_id":"empty-recovery"}),
    )
    .await;
    assert!(authority.status().await.unwrap().current.is_none());
    assert!(
        authority
            .status()
            .await
            .unwrap()
            .candidates
            .iter()
            .any(|candidate| candidate.inventory_id == "empty-recovery"
                && candidate.staged
                && candidate.plugins.is_empty())
    );
    let (mut gateway, mut gateway_task) =
        serve(router_for_role(gateway_state.clone(), RuntimeRole::Gateway)).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();
    let cases = cases(&fixture.model);
    let mut baseline: Vec<Capture> = Vec::new();
    let mut evidence = Vec::new();
    for phase in [
        "current-null",
        "empty-revision-1",
        "claude-revision-2",
        "rollback-empty-revision-3",
        "restart-empty-revision-3",
    ] {
        match phase {
            "empty-revision-1" => {
                assert_eq!(
                    control(
                        &control_state,
                        "publish",
                        "publish-empty",
                        json!({"inventory_id":"empty-recovery","expected_revision":0})
                    )
                    .await["revision"],
                    1
                );
            }
            "claude-revision-2" => {
                control(
                    &control_state,
                    "candidates",
                    "stage-claude",
                    json!({"inventory_id":"claude-reviewed"}),
                )
                .await;
                assert_eq!(
                    control(
                        &control_state,
                        "publish",
                        "publish-claude",
                        json!({"inventory_id":"claude-reviewed","expected_revision":1})
                    )
                    .await["revision"],
                    2
                );
            }
            "rollback-empty-revision-3" => {
                assert_eq!(
                    control(
                        &control_state,
                        "rollback",
                        "rollback-empty",
                        json!({"target_revision":1,"expected_revision":2})
                    )
                    .await["revision"],
                    3
                );
            }
            "restart-empty-revision-3" => {
                gateway_task.abort();
                let _ = gateway_task.await;
                gateway_state = AppState::initialize(config.clone()).await.unwrap();
                (gateway, gateway_task) =
                    serve(router_for_role(gateway_state.clone(), RuntimeRole::Gateway)).await;
            }
            _ => {}
        }
        for (index, (name, path, request)) in cases.iter().enumerate() {
            let response = client
                .post(format!("{gateway}{path}"))
                .bearer_auth(&fixture.key)
                .json(request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{phase}/{name}");
            let response: Value = response.json().await.unwrap();
            if *path == "/v1/responses" {
                assert_eq!(response["status"], "completed");
            } else {
                assert_eq!(response["type"], "message");
            }
            let capture = captures
                .lock()
                .await
                .pop()
                .expect("real upstream wire capture");
            assert!(
                captures.lock().await.is_empty(),
                "no implicit retry or duplicate upstream call"
            );
            if phase == "current-null" {
                baseline.push(capture.clone());
            } else if phase == "claude-revision-2" && index == 0 {
                let mut expected: Value = serde_json::from_str(&baseline[index].body).unwrap();
                assert_eq!(expected["metadata"]["user_id"], "synthetic-tracking-001");
                expected["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("user_id");
                assert_eq!(
                    serde_json::from_str::<Value>(&capture.body).unwrap(),
                    expected
                );
            } else {
                assert_eq!(
                    capture.body.as_bytes(),
                    baseline[index].body.as_bytes(),
                    "unchanged wire bytes: {phase}/{name}"
                );
            }
            assert_eq!(capture.header_names, baseline[index].header_names);
            if *path == "/v1/responses" {
                let wire: Value = serde_json::from_str(&capture.body).unwrap();
                assert!(wire.get("metadata").is_none());
                assert_eq!(wire["system"][0]["text"], request["instructions"]);
                assert_eq!(
                    wire["tools"][0]["input_schema"],
                    request["tools"][0]["parameters"]
                );
                assert_eq!(wire["messages"][1]["content"][0]["id"], "call_wire_001");
                assert_eq!(
                    wire["messages"][1]["content"][0]["input"]["user_id"],
                    "synthetic-user-001"
                );
                assert_eq!(
                    wire["messages"][2]["content"][0]["tool_use_id"],
                    "call_wire_001"
                );
                assert_eq!(
                    wire["messages"][2]["content"][0]["content"],
                    "synthetic-result-001"
                );
            }
            let count = match phase {
                "claude-revision-2" => index + 1,
                "rollback-empty-revision-3" => 4,
                _ => 0,
            };
            assert_finalize_count(&gateway_state, count);
            evidence.push(json!({"phase":phase,"case":name,"capture":capture,"finalize_observations":finalize_observations(&gateway_state)}));
        }
    }
    assert_eq!(
        authority.status().await.unwrap().current.unwrap().revision,
        3
    );
    let pinned = gateway_state.pin_application_plugins().await.unwrap();
    assert!(pinned.plugins.manifests().is_empty());
    gateway_task.abort();
    upstream_task.abort();
    let _ = gateway_task.await;
    let _ = upstream_task.await;
    if let Ok(output) = std::env::var("MTC_CLAUDE_WIRE_EVIDENCE") {
        std::fs::write(output, serde_json::to_vec_pretty(&json!({
            "environment":"GHA-only real Wasmtime and TCP gateway; not cluster acceptance",
            "artifact":"ghcr.io/memeloop-online/claude-code-wire@sha256:3e4160580005b15c9db1e3bd419df79ee65de1d86a8fe1d4ec5aac4c88fe5906",
            "synthetic_only":true,"cases":evidence,"final_revision":3,"final_plugins":[]
        })).unwrap()).unwrap();
    }
}
