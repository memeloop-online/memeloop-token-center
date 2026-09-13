use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::{Config, RuntimeRole},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn request(state: &AppState, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = api::router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.config.service_token),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(serde_json::to_vec(&body).unwrap())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn account_secret_config_is_write_only_preserved_and_compare_and_swap_fenced() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::initialize(Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("secrets.db").display()
    )))
    .await
    .unwrap();
    let mock = wiremock::MockServer::start().await;
    let mut provider = state.providers.get("http-json").unwrap().clone();
    provider.id = "secret-config-fixture".into();
    provider.config_schema = json!({
        "type":"object", "additionalProperties":false, "required":["base_url","nested"],
        "$defs":{"secret":{"type":"string","minLength":1,"writeOnly":true}},
        "properties":{
            "base_url":{"type":"string"},
            "nested":{"type":"object","additionalProperties":false,"required":["token"],"properties":{
                "token":{"allOf":[{"$ref":"#/$defs/secret"}]},"label":{"type":"string"}
            }}
        }
    });
    state.providers.extend([provider]).unwrap();
    let config = json!({"base_url":mock.uri(),"nested":{"token":"synthetic-old","label":"before"}});
    let (status, created) = request(&state,"POST","/internal/v1/upstreams",json!({
        "tenant_external_id":"secret-config-test","name":"fixture","driver":"secret-config-fixture",
        "config":config,"credential":{"type":"api_key","value":"synthetic-credential"}
    })).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!created.to_string().contains("synthetic-"));
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let path = format!("/internal/v1/upstreams/{id}");
    let update = |revision: Value, config: Value| json!({"tenant_external_id":"secret-config-test","name":"edited","expected_updated_at":revision,"config":config});
    let (status, preserved) = request(
        &state,
        "PUT",
        &path,
        update(
            created["updated_at"].clone(),
            json!({"base_url":mock.uri(),"nested":{"label":"after"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preserved["config"]["nested"].get("token").is_none());
    let stored = state
        .db
        .upstream_account_with_current_credential(id, state.config.key_pepper.as_bytes())
        .await
        .unwrap()
        .0;
    assert!(stored.config["nested"]["token"] == "synthetic-old");
    for invalid in [Value::Null, json!(""), json!({})] {
        let (status, _) = request(
            &state,
            "PUT",
            &path,
            update(
                preserved["updated_at"].clone(),
                json!({"base_url":mock.uri(),"nested":{"token":invalid}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = request(
        &state,
        "PUT",
        &path,
        update(
            preserved["updated_at"].clone(),
            json!({"base_url":mock.uri(),"nested":{"unknown":"not-merged"}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let first = update(
        preserved["updated_at"].clone(),
        json!({"base_url":mock.uri(),"nested":{"token":"synthetic-new"}}),
    );
    let second = update(
        preserved["updated_at"].clone(),
        json!({"base_url":mock.uri(),"nested":{"token":"synthetic-other"}}),
    );
    let (a, b) = tokio::join!(
        request(&state, "PUT", &path, first),
        request(&state, "PUT", &path, second)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert!(!a.1.to_string().contains("synthetic-"));
    assert!(!b.1.to_string().contains("synthetic-"));
    let stored = state
        .db
        .upstream_account_with_current_credential(id, state.config.key_pepper.as_bytes())
        .await
        .unwrap()
        .0;
    assert!(matches!(
        stored.config["nested"]["token"].as_str(),
        Some("synthetic-new" | "synthetic-other")
    ));
    let (status, listed) = request(
        &state,
        "GET",
        "/internal/v1/upstreams?tenant_external_id=secret-config-test",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!listed.to_string().contains("synthetic-"));
}
