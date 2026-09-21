//! HTTP-level end-to-end: an arbitrary (non-Claude-Code) client calling the
//! Anthropic-compatible /v1/messages endpoint is rewritten by the
//! claude-code-wire plugin into the exact Claude Code 2.1.258 wire format
//! before the gateway forwards it to the anthropic-claude OAuth upstream.

use std::{fs, path::Path};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
    db::{CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput},
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const MODEL: &str = "claude-sonnet-4-5";
const PROMPT: &str = "hello world, this is a prompt";

#[tokio::test]
async fn anthropic_messages_request_is_rewritten_to_claude_code_wire_format() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_wire_shim",
            "type": "message",
            "role": "assistant",
            "model": MODEL,
            "content": [{"type": "text", "text": "shimmed reply"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 3}
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    // Plugin discovery requires every first-level subdirectory to be a
    // package, so stage the checked-in fixture alone in a temp dir.
    let directory = tempfile::tempdir().unwrap();
    let package = directory.path().join("claude-code-wire");
    fs::create_dir(&package).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code-wire");
    fs::copy(fixture.join("plugin.json"), package.join("plugin.json")).unwrap();
    fs::copy(fixture.join("plugin.wasm"), package.join("plugin.wasm")).unwrap();

    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("wire-shim-gateway.db").display()
    );
    let mut config = Config::for_test(database_url);
    config.plugin_dir = Some(directory.path().display().to_string());
    let state = AppState::initialize(config).await.unwrap();

    let tenant = "wire-shim-gateway";
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.into(),
                name: "claude-oauth".into(),
                driver: "anthropic-claude".into(),
                config: json!({
                    "base_url": upstream.uri(),
                    "network_scope": "public"
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "fixture-access-token".into(),
                    refresh_token: None,
                    expires_at: None,
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
                    adapter_state: None,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let route = state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.into(),
            public_model: MODEL.into(),
            upstream_account_id: account.id,
            upstream_model: MODEL.into(),
            protocol: "anthropic".into(),
            priority: 0,
        })
        .await
        .unwrap();
    let issued = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "wire-shim-user".into(),
                alias: "wire-shim-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec![MODEL.into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            &[route.id],
            &[],
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .upsert_model_price(MODEL, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();

    // A generic Python client: its fingerprint headers must never reach the
    // upstream when the wire shim applies.
    let response = api::router_for_role(state.clone(), RuntimeRole::Gateway)
        .oneshot(
            Request::post("/v1/messages")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .header(header::CONTENT_TYPE, "application/json")
                .header("anthropic-version", "2023-06-01")
                .header(header::USER_AGENT, "python-httpx/0.28.1")
                .header("x-stainless-lang", "python")
                .header("x-claude-code-session-id", "client-supplied-session")
                .header("x-claude-code-trace", "client-supplied-trace")
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "model": MODEL,
                        "max_tokens": 64,
                        "stream": false,
                        "messages": [{"role": "user", "content": PROMPT}]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let downstream = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "gateway response: {}",
        String::from_utf8_lossy(&downstream)
    );
    let downstream: Value = serde_json::from_slice(&downstream).unwrap();
    assert_eq!(downstream["content"][0]["text"], json!("shimmed reply"));

    // Exactly one upstream request, carrying the Claude Code wire format.
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "expected one upstream request");
    let received = &received[0];
    let body: Value = serde_json::from_slice(&received.body).unwrap();
    let system = body["system"].as_array().expect("system array");
    assert_eq!(system.len(), 2, "billing + Agent SDK blocks only: {body}");
    let billing = system[0]["text"].as_str().unwrap();
    // The fingerprint 2d2 belongs to PROMPT under version 2.1.258
    // (cross-validated against the reference implementation).
    assert!(
        billing.starts_with(
            "x-anthropic-billing-header: cc_version=2.1.258.2d2; cc_entrypoint=sdk-cli; cch="
        ),
        "unexpected billing block: {billing}"
    );
    let cch_start = billing.rfind("cch=").map(|index| index + 4).unwrap();
    let cch = &billing[cch_start..cch_start + 5];
    assert!(
        cch.len() == 5
            && cch != "00000"
            && cch
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "cch must be a patched 5-digit lowercase hex value: {cch}"
    );
    assert_eq!(
        system[1]["text"].as_str().unwrap(),
        "You are a Claude agent, built on Anthropic's Claude Agent SDK."
    );
    assert_eq!(body["model"], json!(MODEL));
    assert_eq!(body["max_tokens"], json!(64));

    // Headers: the plugin's canonical Claude Code set wins; the client's
    // fingerprint headers are stripped or overwritten.
    let headers = &received.headers;
    assert_eq!(
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
        Some("claude-cli/2.1.258 (external, sdk-cli)")
    );
    let stainless_lang: Vec<_> = headers.get_all("x-stainless-lang").iter().collect();
    assert_eq!(stainless_lang.len(), 1);
    assert_eq!(stainless_lang[0].to_str().unwrap(), "js");
    uuid::Uuid::parse_str(
        headers
            .get("x-client-request-id")
            .and_then(|v| v.to_str().ok())
            .expect("x-client-request-id"),
    )
    .expect("x-client-request-id must be a UUID");
    let session = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("x-claude-code-session-id");
    uuid::Uuid::parse_str(session).expect("session id must be a derived UUID");
    assert_ne!(session, "client-supplied-session");
    assert!(headers.get("x-claude-code-trace").is_none());
    // The core still owns the credential header.
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer fixture-access-token")
    );
}
