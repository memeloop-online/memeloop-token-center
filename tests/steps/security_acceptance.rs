use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn unauthenticated_declared_oversized_status(service_url: &str, path: &str) -> StatusCode {
    let url = url::Url::parse(service_url).expect("parse test service URL");
    let host = url.host_str().expect("test service host");
    let port = url.port_or_known_default().expect("test service port");
    let authority = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&authority)
        .await
        .expect("connect oversized authentication probe");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        5 * 1024 * 1024
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write oversized authentication probe headers");
    stream
        .flush()
        .await
        .expect("flush oversized authentication probe headers");

    // Do not send the declared body. A prompt response proves authentication
    // runs before either body parsing or the body-size guard. It also avoids a
    // client-side BrokenPipe race when the server correctly rejects the request
    // while a high-level HTTP client is still uploading several MiB.
    const MAX_STATUS_LINE_BYTES: usize = 1024;
    let response_line = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut response_line = Vec::with_capacity(MAX_STATUS_LINE_BYTES);
        let mut chunk = [0_u8; 128];
        loop {
            assert!(
                response_line.len() < MAX_STATUS_LINE_BYTES,
                "authentication response status line exceeded {MAX_STATUS_LINE_BYTES} bytes"
            );
            let read_limit = chunk.len().min(MAX_STATUS_LINE_BYTES - response_line.len());
            let received = stream
                .read(&mut chunk[..read_limit])
                .await
                .expect("read oversized authentication response");
            assert!(received > 0, "server closed before an HTTP status line");
            response_line.extend_from_slice(&chunk[..received]);
            if let Some(line_end) = response_line.windows(2).position(|bytes| bytes == b"\r\n") {
                response_line.truncate(line_end);
                return response_line;
            }
        }
    })
    .await
    .expect("unauthenticated response arrived before the declared body");
    std::str::from_utf8(&response_line)
        .expect("authentication response head is ASCII")
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .and_then(|value| StatusCode::from_u16(value).ok())
        .expect("parse authentication response status")
}

async fn assert_normalized_control_rejection(response: reqwest::Response, label: &str) {
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
    assert!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")),
        "{label} did not return JSON"
    );
    let body = response
        .json::<Value>()
        .await
        .unwrap_or_else(|error| panic!("{label} rejection JSON: {error}"));
    assert_eq!(body["error"]["code"], "invalid_request", "{label}");
    assert_eq!(
        body["error"]["message"],
        "invalid request: request parameters or body do not match the API schema",
        "{label}"
    );
}

async fn issue_service_token(world: &TokenCenterWorld, name: &str, scopes: &[&str]) -> String {
    let response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({"name": name, "scopes": scopes}))
        .send()
        .await
        .expect("issue exact-scope service credential");
    assert_eq!(response.status(), StatusCode::CREATED, "scope={scopes:?}");
    response
        .json::<Value>()
        .await
        .expect("exact-scope service credential JSON")["token"]
        .as_str()
        .expect("issued exact-scope service credential")
        .to_owned()
}

async fn create_key(
    world: &TokenCenterWorld,
    tenant: &str,
    principal: &str,
    model: &str,
    policy_overrides: Value,
) -> Value {
    let mut policy = json!({"allowed_models": [model]});
    for (name, value) in policy_overrides
        .as_object()
        .expect("policy overrides object")
    {
        policy
            .as_object_mut()
            .expect("policy object")
            .insert(name.clone(), value.clone());
    }
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": tenant,
            "principal_external_id": principal,
            "alias": principal,
            "currency": "USD",
            "initial_balance": "100",
            "policy": policy
        }))
        .send()
        .await
        .expect("create security acceptance credential");
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "principal={principal}"
    );
    response
        .json()
        .await
        .expect("security acceptance credential JSON")
}

async fn call_model(world: &TokenCenterWorld, key: &str, model: &str) -> reqwest::Response {
    world
        .client
        .post(format!("{}/v1/chat/completions", world.service_url))
        .bearer_auth(key)
        .json(&json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "security probe"}]
        }))
        .send()
        .await
        .expect("security policy model request")
}

async fn upsert_price(world: &TokenCenterWorld, model: &str) {
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/{model}",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("upsert security acceptance price");
    assert_eq!(response.status(), StatusCode::OK, "model={model}");
}

#[then("a credential with an empty model allowlist cannot list or call a priced model")]
async fn empty_model_allowlist_is_deny_all(world: &mut TokenCenterWorld) {
    upsert_price(world, "deny-all-priced-model").await;
    let issued = create_key(
        world,
        "deny-all-tenant",
        "deny-all-user",
        "deny-all-priced-model",
        json!({"allowed_models": []}),
    )
    .await;
    let key = issued["key"].as_str().expect("deny-all credential");
    let models = world
        .client
        .get(format!("{}/v1/models", world.service_url))
        .bearer_auth(key)
        .send()
        .await
        .expect("list models for deny-all credential");
    assert_eq!(models.status(), StatusCode::OK);
    assert_eq!(
        models.json::<Value>().await.expect("deny-all models JSON")["data"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    let response = call_model(world, key, "deny-all-priced-model").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[then("a managed credential can be suspended and restored but never restored after revocation")]
async fn downstream_status_lifecycle_is_terminal(world: &mut TokenCenterWorld) {
    let issued = create_key(
        world,
        "status-tenant",
        "status-user",
        "unused-status-model",
        json!({}),
    )
    .await;
    let key = issued["key"].as_str().expect("managed credential");
    let key_id = issued["key_id"].as_str().expect("managed key id");
    let status_url = format!("{}/internal/v1/keys/{key_id}/status", world.service_url);
    for (status, expected_auth) in [
        ("suspended", StatusCode::UNAUTHORIZED),
        ("active", StatusCode::OK),
        ("revoked", StatusCode::UNAUTHORIZED),
    ] {
        let changed = world
            .client
            .patch(&status_url)
            .bearer_auth("test-service-token")
            .json(&json!({"status": status}))
            .send()
            .await
            .expect("change managed credential status");
        assert_eq!(changed.status(), StatusCode::OK, "status={status}");
        let probe = world
            .client
            .get(format!("{}/self/v1/key", world.service_url))
            .bearer_auth(key)
            .send()
            .await
            .expect("probe managed credential status");
        assert_eq!(probe.status(), expected_auth, "status={status}");
    }
    let reactivate = world
        .client
        .patch(&status_url)
        .bearer_auth("test-service-token")
        .json(&json!({"status": "active"}))
        .send()
        .await
        .expect("attempt reactivation after revocation");
    assert_eq!(reactivate.status(), StatusCode::BAD_REQUEST);
    let rotate = world
        .client
        .post(format!(
            "{}/internal/v1/keys/{key_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "security:revoked-key:rotate")
        .send()
        .await
        .expect("attempt rotation after revocation");
    assert_eq!(rotate.status(), StatusCode::FORBIDDEN);
}

#[then(
    "service credential status management requires a global operator and enforces terminal revocation"
)]
async fn service_status_lifecycle_and_global_only(world: &mut TokenCenterWorld) {
    for scopes in [
        json!(["*"]),
        json!(["keys:*"]),
        json!(["future:admin"]),
        json!(["keys:read", "keys:read"]),
    ] {
        let rejected = world
            .client
            .post(format!("{}/internal/v1/service-tokens", world.service_url))
            .bearer_auth("test-service-token")
            .json(&json!({"name": "invalid-scope", "scopes": scopes}))
            .send()
            .await
            .expect("reject non-exact managed service credential scope");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }

    let target_response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "status-target",
            "scopes": ["keys:read", "service_tokens:read"]
        }))
        .send()
        .await
        .expect("create managed service credential");
    assert_eq!(target_response.status(), StatusCode::CREATED);
    let target = target_response
        .json::<Value>()
        .await
        .expect("managed service credential JSON");
    let target_token = target["token"].as_str().expect("managed service token");
    let service_id = target["service_id"].as_str().expect("managed service id");
    let scoped_response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "tenant-status-manager",
            "scopes": ["service_tokens:read", "service_tokens:write"],
            "tenant_external_id": "status-tenant"
        }))
        .send()
        .await
        .expect("create tenant-scoped status manager");
    assert_eq!(scoped_response.status(), StatusCode::CREATED);
    let scoped = scoped_response
        .json::<Value>()
        .await
        .expect("tenant-scoped status manager JSON");
    let scoped_token = scoped["token"]
        .as_str()
        .expect("tenant-scoped status token");
    let list = world
        .client
        .get(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth(scoped_token)
        .send()
        .await
        .expect("tenant-scoped service token list attempt");
    assert_eq!(list.status(), StatusCode::FORBIDDEN);
    let status_url = format!(
        "{}/internal/v1/service-tokens/{service_id}/status",
        world.service_url
    );
    let scoped_status = world
        .client
        .patch(&status_url)
        .bearer_auth(scoped_token)
        .json(&json!({"status": "suspended"}))
        .send()
        .await
        .expect("tenant-scoped service status attempt");
    assert_eq!(scoped_status.status(), StatusCode::FORBIDDEN);
    for (status, expected_auth) in [
        ("suspended", StatusCode::UNAUTHORIZED),
        ("active", StatusCode::OK),
        ("revoked", StatusCode::UNAUTHORIZED),
    ] {
        let changed = world
            .client
            .patch(&status_url)
            .bearer_auth("test-service-token")
            .json(&json!({"status": status}))
            .send()
            .await
            .expect("change managed service credential status");
        assert_eq!(changed.status(), StatusCode::OK, "service status={status}");
        let probe = world
            .client
            .get(format!("{}/internal/v1/keys", world.service_url))
            .bearer_auth(target_token)
            .send()
            .await
            .expect("probe managed service credential status");
        assert_eq!(probe.status(), expected_auth, "service status={status}");
    }
    let reactivate = world
        .client
        .patch(&status_url)
        .bearer_auth("test-service-token")
        .json(&json!({"status": "active"}))
        .send()
        .await
        .expect("attempt service reactivation after revocation");
    assert_eq!(reactivate.status(), StatusCode::BAD_REQUEST);
    let rotate = world
        .client
        .post(format!(
            "{}/internal/v1/service-tokens/{service_id}/rotate",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .header("idempotency-key", "security:revoked-service:rotate")
        .send()
        .await
        .expect("attempt service rotation after revocation");
    assert!(
        matches!(
            rotate.status(),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ),
        "revoked service rotation status={}",
        rotate.status()
    );
}

async fn assert_scope_is_not_wildcard(world: &TokenCenterWorld, scope: &str, token: &str) {
    let path = if scope == "schemas:read" {
        "/internal/v1/plugins"
    } else {
        "/internal/v1/schemas"
    };
    let response = world
        .client
        .get(format!("{}{path}", world.service_url))
        .bearer_auth(token)
        .send()
        .await
        .expect("probe unrelated service scope");
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "scope={scope}");
}

#[then("each service scope independently authorizes its matching control-plane operation")]
async fn every_service_scope_is_exact(world: &mut TokenCenterWorld) {
    let seed = create_key(
        world,
        "scope-tenant",
        "scope-seed-user",
        "scope-seed-model",
        json!({}),
    )
    .await;
    let account_id = seed["account_id"].as_str().expect("scope account id");

    let token = issue_service_token(world, "scope-keys-read", &["keys:read"]).await;
    let response = world
        .client
        .get(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&token)
        .send()
        .await
        .expect("keys:read operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "keys:read", &token).await;

    let token = issue_service_token(world, "scope-keys-write", &["keys:write"]).await;
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth(&token)
        .json(&json!({
            "tenant_external_id": "scope-write-tenant",
            "principal_external_id": "scope-write-user",
            "alias": "scope-write",
            "currency": "USD"
        }))
        .send()
        .await
        .expect("keys:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_scope_is_not_wildcard(world, "keys:write", &token).await;

    let token = issue_service_token(world, "scope-credits-read", &["credits:read"]).await;
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/accounts/{account_id}/ledger",
            world.service_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("credits:read operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "credits:read", &token).await;

    let token = issue_service_token(world, "scope-credits-write", &["credits:write"]).await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/accounts/{account_id}/grants",
            world.service_url
        ))
        .bearer_auth(&token)
        .header("idempotency-key", "security:scope-credit-grant")
        .json(&json!({"amount": "1", "source": "scope-test"}))
        .send()
        .await
        .expect("credits:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_scope_is_not_wildcard(world, "credits:write", &token).await;

    let token = issue_service_token(world, "scope-entitlements-read", &["entitlements:read"]).await;
    let response = world
        .client
        .get(format!("{}/internal/v1/entitlements", world.service_url))
        .bearer_auth(&token)
        .send()
        .await
        .expect("entitlements:read operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "entitlements:read", &token).await;

    let token =
        issue_service_token(world, "scope-entitlements-write", &["entitlements:write"]).await;
    let response = world
        .client
        .put(format!("{}/internal/v1/entitlements", world.service_url))
        .bearer_auth(&token)
        .header("idempotency-key", "security:scope-entitlement")
        .json(&json!({
            "tenant_external_id": "scope-tenant",
            "account_id": account_id,
            "provider": "scope-provider",
            "external_subscription_id": "scope-subscription",
            "external_cycle_id": "scope-cycle",
            "period_start": 1_700_000_000_000_i64,
            "period_end": 4_100_000_000_000_i64,
            "currency": "USD",
            "desired": "1",
            "version": 1,
            "source": "scope-matrix"
        }))
        .send()
        .await
        .expect("entitlements:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_scope_is_not_wildcard(world, "entitlements:write", &token).await;

    let token = issue_service_token(world, "scope-imports-cpa-write", &["imports:cpa:write"]).await;
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/imports/cpa/managed-oauth/capabilities",
            world.service_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("imports:cpa:write operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "imports:cpa:write", &token).await;

    let token = issue_service_token(
        world,
        "scope-quarantine-read",
        &["imports:session_archive:quarantine:read"],
    )
    .await;
    let response = world
        .client
        .get(format!(
            "{}/internal/v1/imports/session-archive/quarantine?tenant_external_id=scope-tenant",
            world.service_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("imports:session_archive:quarantine:read operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "imports:session_archive:quarantine:read", &token).await;

    let token = issue_service_token(
        world,
        "scope-quarantine-resolve",
        &["imports:session_archive:quarantine:resolve"],
    )
    .await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/imports/session-archive/quarantine/{}/resolutions",
            world.service_url,
            Uuid::now_v7()
        ))
        .bearer_auth(&token)
        .header("idempotency-key", "security:scope-quarantine-resolution")
        .json(&json!({
            "tenant_external_id": "scope-tenant",
            "action": "dismiss",
            "key_id": null,
            "expected_record_digest": "a".repeat(64),
            "evidence_digest": "b".repeat(64),
            "note": "scope authorization matrix"
        }))
        .send()
        .await
        .expect("imports:session_archive:quarantine:resolve operation");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "the exact scope passed authorization and reached the deliberately missing fixture"
    );
    assert_scope_is_not_wildcard(world, "imports:session_archive:quarantine:resolve", &token).await;

    for (scope, path) in [
        ("plugins:read", "/internal/v1/plugins"),
        ("prices:read", "/internal/v1/generation-prices"),
        ("providers:read", "/internal/v1/provider-types"),
        ("requests:read", "/internal/v1/stats"),
        ("routes:read", "/internal/v1/model-routes"),
        ("schemas:read", "/internal/v1/schemas"),
        ("service_tokens:read", "/internal/v1/service-tokens"),
    ] {
        let token = issue_service_token(world, &format!("scope-{scope}"), &[scope]).await;
        let response = world
            .client
            .get(format!("{}{path}", world.service_url))
            .bearer_auth(&token)
            .send()
            .await
            .expect("read-scope operation");
        assert_eq!(response.status(), StatusCode::OK, "scope={scope}");
        assert_scope_is_not_wildcard(world, scope, &token).await;
    }

    let token = issue_service_token(world, "scope-metrics-read", &["metrics:read"]).await;
    let response = world
        .client
        .get(format!("{}/version", world.service_url))
        .bearer_auth(&token)
        .send()
        .await
        .expect("metrics:read operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "metrics:read", &token).await;

    let token = issue_service_token(world, "scope-oauth-write", &["oauth:write"]).await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/oauth/cursor/start",
            world.service_url
        ))
        .bearer_auth(&token)
        .json(&json!({
            "tenant_external_id": "scope-oauth-tenant",
            "account_name": "scope-oauth-account",
            "provider_driver": "http-json",
            "provider_config": {
                "base_url": world.mock.as_ref().expect("mock upstream").uri(),
                "network_scope": "private"
            }
        }))
        .send()
        .await
        .expect("oauth:write operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "oauth:write", &token).await;

    let token = issue_service_token(world, "scope-plugins-write", &["plugins:write"]).await;
    let response = world
        .client
        .put(format!(
            "{}/internal/v1/plugins/not-installed/configuration",
            world.service_url
        ))
        .bearer_auth(&token)
        .header("idempotency-key", "security:scope-plugin-configuration")
        .json(&json!({"expected_version": 0, "value": {}}))
        .send()
        .await
        .expect("plugins:write operation");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "the exact scope passed authorization and reached the deliberately absent plugin"
    );
    assert_scope_is_not_wildcard(world, "plugins:write", &token).await;

    let token = issue_service_token(world, "scope-prices-write", &["prices:write"]).await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/prices/USD/scope-price-model",
            world.service_url
        ))
        .bearer_auth(&token)
        .json(&json!({"input_per_million": "1", "output_per_million": "1"}))
        .send()
        .await
        .expect("prices:write operation");
    assert_eq!(response.status(), StatusCode::OK);
    assert_scope_is_not_wildcard(world, "prices:write", &token).await;

    let token = issue_service_token(world, "scope-providers-write", &["providers:write"]).await;
    let mock_url = world.mock.as_ref().expect("mock upstream").uri();
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth(&token)
        .json(&json!({
            "tenant_external_id": "scope-route-tenant",
            "name": "scope-provider",
            "driver": "http-json",
            "config": {"base_url": mock_url, "network_scope": "private"},
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("providers:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    let provider: Value = response.json().await.expect("scope provider JSON");
    assert_scope_is_not_wildcard(world, "providers:write", &token).await;

    let token = issue_service_token(world, "scope-routes-write", &["routes:write"]).await;
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth(&token)
        .json(&json!({
            "tenant_external_id": "scope-route-tenant",
            "public_model": "scope-route-public",
            "upstream_account_id": provider["id"],
            "upstream_model": "scope-route-upstream",
            "protocol": "openai",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("routes:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_scope_is_not_wildcard(world, "routes:write", &token).await;

    let token = issue_service_token(
        world,
        "scope-service-tokens-write",
        &["service_tokens:write"],
    )
    .await;
    let response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth(&token)
        .json(&json!({"name": "scope-child", "scopes": ["keys:read"]}))
        .send()
        .await
        .expect("service_tokens:write operation");
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_scope_is_not_wildcard(world, "service_tokens:write", &token).await;
}

#[then(
    "tenant scoped OAuth cannot target private or metadata endpoints while a global private connection is allowed"
)]
async fn oauth_start_enforces_network_boundary(world: &mut TokenCenterWorld) {
    let scoped_response = world
        .client
        .post(format!("{}/internal/v1/service-tokens", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "tenant-oauth",
            "scopes": ["oauth:write"],
            "tenant_external_id": "oauth-boundary-tenant"
        }))
        .send()
        .await
        .expect("issue tenant OAuth service credential");
    assert_eq!(scoped_response.status(), StatusCode::CREATED);
    let scoped = scoped_response
        .json::<Value>()
        .await
        .expect("tenant OAuth credential JSON")["token"]
        .as_str()
        .expect("tenant OAuth token")
        .to_owned();
    for (base_url, network_scope) in [
        ("http://127.0.0.1:8080", Some("private")),
        ("http://169.254.169.254/latest/meta-data", None),
    ] {
        let mut config = json!({"base_url": base_url});
        if let Some(scope) = network_scope {
            config["network_scope"] = Value::String(scope.to_owned());
        }
        let response = world
            .client
            .post(format!(
                "{}/internal/v1/oauth/cursor/start",
                world.service_url
            ))
            .bearer_auth(&scoped)
            .json(&json!({
                "tenant_external_id": "oauth-boundary-tenant",
                "account_name": "blocked-private",
                "provider_driver": "http-json",
                "provider_config": config
            }))
            .send()
            .await
            .expect("tenant OAuth private destination attempt");
        assert!(
            matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST
            ),
            "base_url={base_url}, status={}",
            response.status()
        );
    }

    let global = issue_service_token(world, "global-oauth", &["oauth:write"]).await;
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/oauth/cursor/start",
            world.service_url
        ))
        .bearer_auth(&global)
        .json(&json!({
            "tenant_external_id": "oauth-boundary-tenant",
            "account_name": "allowed-private",
            "provider_driver": "http-json",
            "provider_config": {
                "base_url": "http://127.0.0.1:8080",
                "network_scope": "private"
            }
        }))
        .send()
        .await
        .expect("global OAuth private destination start");
    assert_eq!(response.status(), StatusCode::OK);
    let started: Value = response.json().await.expect("global OAuth start JSON");
    assert!(started["login_url"].as_str().is_some());
    assert!(started["session_token"].as_str().is_some());
}

#[then("provider configuration and credential schemas are authoritative on every write")]
async fn provider_schemas_are_authoritative(world: &mut TokenCenterWorld) {
    let token = issue_service_token(world, "schema-provider-writer", &["providers:write"]).await;
    let base_url = world.mock.as_ref().expect("mock upstream").uri();

    for config in [
        json!({"base_url": base_url, "undeclared": true}),
        json!({"base_url": "not a URI"}),
    ] {
        let response = world
            .client
            .post(format!("{}/internal/v1/upstreams", world.service_url))
            .bearer_auth(&token)
            .json(&json!({
                "tenant_external_id": "schema-authority-tenant",
                "name": "rejected-provider",
                "driver": "http-json",
                "config": config,
                "credential": {"type": "none"}
            }))
            .send()
            .await
            .expect("invalid provider configuration response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    let created = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth(&token)
        .json(&json!({
            "tenant_external_id": "schema-authority-tenant",
            "name": "schema-provider",
            "driver": "http-json",
            "config": {"base_url": base_url, "network_scope": "private"},
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("valid provider configuration response");
    assert_eq!(created.status(), StatusCode::CREATED);
    let account_id = created
        .json::<Value>()
        .await
        .expect("created provider JSON")["id"]
        .as_str()
        .expect("created provider id")
        .to_owned();

    let secret = "schema-contract-secret-must-not-echo";
    let rejected = world
        .client
        .put(format!(
            "{}/internal/v1/upstreams/{account_id}/credential",
            world.service_url
        ))
        .bearer_auth(&token)
        .header("idempotency-key", "schema-authority-invalid-rotation")
        .json(&json!({"credential": {
            "type": "api_key", "value": secret, "undeclared": true
        }}))
        .send()
        .await
        .expect("invalid provider credential response");
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(
        !rejected
            .text()
            .await
            .expect("schema error body")
            .contains(secret)
    );
}

#[then("malformed and oversized unauthenticated bodies are rejected as unauthorized")]
async fn authentication_precedes_body_parsing(world: &mut TokenCenterWorld) {
    for path in ["/internal/v1/keys", "/v1/chat/completions"] {
        let malformed = world
            .client
            .post(format!("{}{path}", world.service_url))
            .header("content-type", "application/json")
            .body("{ definitely-not-json")
            .send()
            .await
            .expect("malformed unauthenticated request");
        assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED, "path={path}");
        let oversized_status =
            unauthenticated_declared_oversized_status(&world.service_url, path).await;
        assert_eq!(oversized_status, StatusCode::UNAUTHORIZED, "path={path}");
    }

    let valid_key_body = json!({
        "tenant_external_id": "normalized-rejection-tenant",
        "principal_external_id": "normalized-rejection-principal",
        "alias": "normalized-rejection",
        "currency": "USD"
    });
    for (label, body) in [
        ("malformed body", "{ definitely-not-json".to_owned()),
        ("missing fields", "{}".to_owned()),
        (
            "unknown field",
            serde_json::to_string(&{
                let mut body = valid_key_body.clone();
                body["unknown_field"] = json!(true);
                body
            })
            .expect("serialize unknown-field rejection"),
        ),
    ] {
        let response = world
            .client
            .post(format!("{}/internal/v1/keys", world.service_url))
            .bearer_auth("test-service-token")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap_or_else(|error| panic!("{label} control rejection: {error}"));
        assert_normalized_control_rejection(response, label).await;
    }

    for (label, path) in [
        (
            "invalid query extractor",
            "/internal/v1/keys?limit=not-a-number",
        ),
        (
            "invalid path extractor",
            "/internal/v1/imports/session-archive/quarantine/not-a-uuid",
        ),
    ] {
        let response = world
            .client
            .get(format!("{}{path}", world.service_url))
            .bearer_auth("test-service-token")
            .send()
            .await
            .unwrap_or_else(|error| panic!("{label} control rejection: {error}"));
        assert_normalized_control_rejection(response, label).await;
    }
}

async fn wait_for_own_conversation_cluster(world: &TokenCenterWorld, key: &str) -> String {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let response = world
            .client
            .get(format!("{}/self/v1/conversations", world.service_url))
            .bearer_auth(key)
            .send()
            .await
            .expect("credential conversations");
        assert_eq!(response.status(), StatusCode::OK);
        let conversations = response
            .json::<Value>()
            .await
            .expect("credential conversations JSON");
        if let Some(cluster_id) = conversations
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|row| row["cluster_id"].as_str())
        {
            return cluster_id.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "credential conversation finalization exceeded three seconds: {conversations}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[then("self-service object and filter access remains bound to the authenticated credential")]
async fn self_service_idor_is_closed(world: &mut TokenCenterWorld) {
    let managed = world
        .client
        .get(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .send()
        .await
        .expect("list IDOR matrix credentials")
        .json::<Value>()
        .await
        .expect("IDOR matrix credentials JSON");
    let rows = managed.as_array().expect("managed key rows");
    let first = rows
        .iter()
        .find(|row| row["tenant_external_id"] == "matrix-first")
        .expect("first matrix key");
    let second = rows
        .iter()
        .find(|row| row["tenant_external_id"] == "matrix-second")
        .expect("second matrix key");
    let second_key_id = second["key_id"].as_str().expect("second matrix key id");

    let filtered = world
        .client
        .get(format!(
            "{}/self/v1/requests?key_id={second_key_id}&tenant_external_id=matrix-second",
            world.service_url
        ))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("cross-key self request filter");
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered: Value = filtered.json().await.expect("cross-key filter JSON");
    assert_eq!(filtered.as_array().map(Vec::len), Some(1));
    assert_eq!(
        filtered[0]["request_id"],
        world
            .matrix_first_request_id
            .expect("first matrix request id")
            .to_string(),
        "self-service must ignore an attacker-controlled key_id and remain bound to the authenticated key"
    );

    let cluster_id = wait_for_own_conversation_cluster(world, &world.matrix_second_key).await;
    let conversation_idor = world
        .client
        .get(format!(
            "{}/self/v1/conversations/{cluster_id}",
            world.service_url
        ))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("cross-key conversation detail");
    assert_eq!(conversation_idor.status(), StatusCode::NOT_FOUND);

    let policy = world
        .client
        .put(format!(
            "{}/internal/v1/keys/{}/policy",
            world.service_url,
            second["key_id"].as_str().expect("second key id")
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"allowed_models": ["matrix-model", "idor-comfy"]}))
        .send()
        .await
        .expect("allow IDOR generation model");
    assert_eq!(policy.status(), StatusCode::OK);
    let mock_url = world.mock.as_ref().expect("mock upstream").uri();
    let upstream = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": "matrix-second",
            "name": "idor-comfy",
            "driver": "comfyui",
            "config": {
                "base_url": mock_url,
                "network_scope": "private",
                "workflow_id": "idor-workflow",
                "workflow_template": {"1": {"class_type": "SaveImage", "inputs": {}}}
            },
            "credential": {"type": "none"}
        }))
        .send()
        .await
        .expect("create IDOR generation upstream");
    assert_eq!(upstream.status(), StatusCode::CREATED);
    let upstream: Value = upstream.json().await.expect("IDOR upstream JSON");
    let route = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "tenant_external_id": "matrix-second",
            "public_model": "idor-comfy",
            "upstream_account_id": upstream["id"],
            "upstream_model": "idor-workflow",
            "protocol": "generation"
        }))
        .send()
        .await
        .expect("create IDOR generation route");
    assert_eq!(route.status(), StatusCode::CREATED);
    let price = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/idor-comfy",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "job", "price_per_unit": "0.1"}))
        .send()
        .await
        .expect("create IDOR generation price");
    assert_eq!(price.status(), StatusCode::OK);
    let generation = world
        .client
        .post(format!("{}/v1/generations", world.service_url))
        .bearer_auth(&world.matrix_second_key)
        .json(&json!({"model": "idor-comfy", "input": {"seed": 1}}))
        .send()
        .await
        .expect("create second credential generation");
    assert_eq!(generation.status(), StatusCode::ACCEPTED);
    let generation: Value = generation.json().await.expect("IDOR generation JSON");
    let job_id = generation["job_id"]
        .as_str()
        .expect("IDOR generation job id");
    for request_method in [Method::GET, Method::DELETE] {
        let response = world
            .client
            .request(
                request_method.clone(),
                format!("{}/self/v1/generations/{job_id}", world.service_url),
            )
            .bearer_auth(&world.matrix_first_key)
            .send()
            .await
            .expect("cross-key generation operation");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "method={request_method}"
        );
    }
    let first_generations = world
        .client
        .get(format!("{}/self/v1/generations", world.service_url))
        .bearer_auth(&world.matrix_first_key)
        .send()
        .await
        .expect("first credential generation list")
        .json::<Value>()
        .await
        .expect("first credential generation list JSON");
    assert_eq!(first_generations.as_array().map(Vec::len), Some(0));
    assert_eq!(first["tenant_external_id"], "matrix-first");
}

#[then("a grant idempotency key replays the same payload and conflicts on a changed payload")]
async fn grant_payload_replay_is_exact(world: &mut TokenCenterWorld) {
    let issued = create_key(
        world,
        "grant-replay-tenant",
        "grant-replay-user",
        "unused-grant-model",
        json!({}),
    )
    .await;
    let account_id = issued["account_id"].as_str().expect("grant account id");
    let grant_url = format!(
        "{}/internal/v1/accounts/{account_id}/grants",
        world.service_url
    );
    for _ in 0..2 {
        let response = world
            .client
            .post(&grant_url)
            .bearer_auth("test-service-token")
            .header("idempotency-key", "security:grant:exact-replay")
            .json(&json!({"amount": "2", "source": "subscription:pro"}))
            .send()
            .await
            .expect("replay identical grant");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.json::<Value>().await.expect("grant replay JSON")["granted"],
            "2"
        );
    }
    for changed_payload in [
        json!({"amount": "3", "source": "subscription:pro"}),
        json!({"amount": "2", "source": "subscription:enterprise"}),
    ] {
        let changed = world
            .client
            .post(&grant_url)
            .bearer_auth("test-service-token")
            .header("idempotency-key", "security:grant:exact-replay")
            .json(&changed_payload)
            .send()
            .await
            .expect("changed grant replay");
        assert_eq!(changed.status(), StatusCode::BAD_REQUEST);
    }
    let self_view = world
        .client
        .get(format!("{}/self/v1/key", world.service_url))
        .bearer_auth(issued["key"].as_str().expect("grant credential"))
        .send()
        .await
        .expect("grant account self view")
        .json::<Value>()
        .await
        .expect("grant account self JSON");
    assert_eq!(self_view["available_balance"], "102");
}

#[then("TPM concurrency daily weekly and lifetime policies are independently enforced")]
async fn all_runtime_limits_are_enforced(world: &mut TokenCenterWorld) {
    for model in [
        "tpm-policy-model",
        "daily-policy-model",
        "weekly-policy-model",
        "lifetime-policy-model",
        "concurrency-policy-model",
    ] {
        upsert_price(world, model).await;
    }
    let tpm = create_key(
        world,
        "limit-tenant",
        "tpm-policy-user",
        "tpm-policy-model",
        json!({"tokens_per_minute": 1}),
    )
    .await;
    let response = call_model(
        world,
        tpm["key"].as_str().expect("TPM credential"),
        "tpm-policy-model",
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response.json::<Value>().await.expect("TPM error JSON")["error"]["code"],
        "rate_limit_exceeded"
    );

    for (field, principal, model) in [
        ("daily_budget", "daily-policy-user", "daily-policy-model"),
        ("weekly_budget", "weekly-policy-user", "weekly-policy-model"),
        (
            "lifetime_budget",
            "lifetime-policy-user",
            "lifetime-policy-model",
        ),
    ] {
        let mut overrides = serde_json::Map::new();
        overrides.insert(field.to_owned(), Value::String("0".to_owned()));
        let issued = create_key(
            world,
            "limit-tenant",
            principal,
            model,
            Value::Object(overrides),
        )
        .await;
        let response = call_model(
            world,
            issued["key"].as_str().expect("budget credential"),
            model,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{field}");
        assert_eq!(
            response.json::<Value>().await.expect("budget error JSON")["error"]["code"],
            "insufficient_quota",
            "{field}"
        );
    }

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            json!({"model": "concurrency-policy-model"}),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(500))
                .set_body_json(json!({
                    "id": "chatcmpl-concurrency",
                    "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                })),
        )
        .with_priority(1)
        .expect(1)
        .mount(world.mock.as_ref().expect("mock server"))
        .await;
    let concurrency = create_key(
        world,
        "limit-tenant",
        "concurrency-policy-user",
        "concurrency-policy-model",
        json!({"max_concurrency": 1}),
    )
    .await;
    let key = concurrency["key"]
        .as_str()
        .expect("concurrency credential")
        .to_owned();
    let first_client = world.client.clone();
    let first_url = world.service_url.clone();
    let first_key = key.clone();
    let first = tokio::spawn(async move {
        first_client
            .post(format!("{first_url}/v1/chat/completions"))
            .bearer_auth(first_key)
            .json(&json!({
                "model": "concurrency-policy-model",
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "first"}]
            }))
            .send()
            .await
            .expect("first concurrent request")
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let second = call_model(world, &key, "concurrency-policy-model").await;
    let first = first.await.expect("first concurrent request task");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        second
            .json::<Value>()
            .await
            .expect("concurrency error JSON")["error"]["code"],
        "rate_limit_exceeded"
    );
}
