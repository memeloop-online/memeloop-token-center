use super::*;
use crate::{
    config::RuntimeRole,
    db::{CreateKeyInput, CreateServiceTokenInput},
    model::KeyPolicy,
};
use tower::ServiceExt;

// A complete 1 × 1 transparent RGBA PNG, including its zlib payload and CRCs.
const IMAGE_FIXTURE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=";

fn inference(token: &str, key_id: Uuid) -> Request {
    Request::post("/internal/v1/inference/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"key_id":key_id,"request":{"model":"fixture","messages":[{"role":"user","content":"hello"}],"stream":false}}).to_string()))
        .unwrap()
}

#[tokio::test]
async fn inference_requires_explicit_key_copy_permission_and_tenant() {
    for path in [
        "/internal/v1/inference/chat/completions",
        "/internal/v1/inference/messages",
        "/internal/v1/inference/responses",
    ] {
        assert_inference_permissions(path).await;
    }
}

async fn assert_inference_permissions(path: &str) {
    let inference = |token: &str, key_id: Uuid| {
        let mut request = inference(token, key_id);
        *request.uri_mut() = path.parse().unwrap();
        request
    };
    let (state, _directory) = crate::api::tests::test_state().await;
    for role in [RuntimeRole::Gateway, RuntimeRole::Worker] {
        let response = router_for_role(state.clone(), role)
            .oneshot(inference(&state.config.service_token, Uuid::now_v7()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let key = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "inference-owner".into(),
                principal_external_id: "owner".into(),
                alias: "billing".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    state
        .db
        .create_tenant("inference-other", None)
        .await
        .unwrap();
    for (name, scopes, tenant) in [
        ("reader", vec!["keys:read".to_owned()], "inference-owner"),
        (
            "other-writer",
            vec!["keys:write".to_owned()],
            "inference-other",
        ),
    ] {
        let service = state
            .db
            .create_service_token(
                CreateServiceTokenInput {
                    name: name.into(),
                    scopes,
                    tenant_external_id: Some(tenant.into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let response = router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(inference(&service.token, key.key_id))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{name}");
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(&key.key));
    }
    let writer = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "owner-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some("inference-owner".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    // Copy authority does not add any downstream model grants.
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // An active tenant query cannot override a scoped service identity.
    let mut request = inference(&writer.token, key.key_id);
    *request.uri_mut() = format!("{path}?tenant_external_id=inference-other")
        .parse()
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Global authority is narrowed by the UI's explicit active tenant too.
    let mut request = inference(&state.config.service_token, key.key_id);
    *request.uri_mut() = format!("{path}?tenant_external_id=inference-other")
        .parse()
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // Invalid requests are rejected before a nonexistent key is copied.
    let mut request = inference(&writer.token, Uuid::now_v7());
    *request.body_mut() =
        Body::from(json!({"key_id":Uuid::now_v7(),"request":{"stream":true}}).to_string());
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut request = inference(&writer.token, Uuid::now_v7());
    *request.body_mut() = Body::from(
        json!({"key_id":Uuid::now_v7(),"request":{"stream":false},"conversation":{"id":"invalid session"}})
            .to_string(),
    );
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    state
        .db
        .set_key_status(key.key_id, "suspended")
        .await
        .unwrap();
    let response = router_for_role(state.clone(), RuntimeRole::Control)
        .oneshot(inference(&writer.token, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A downstream key is never accepted as operator authorization.
    let response = router_for_role(state, RuntimeRole::Control)
        .oneshot(inference(&key.key, key.key_id))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn operator_bridge_requires_nonstreaming_json() {
    assert!(validate_request(&json!({"stream":true})).is_err());
    assert!(validate_request(&json!({"stream":"false"})).is_err());
    assert!(validate_request(&json!([])).is_err());
    assert!(validate_request(&json!({"stream":false})).is_ok());
    assert!(validate_request(&json!({})).is_ok());
}

#[test]
fn conversation_wrapper_accepts_only_bounded_identifiers_and_known_fields() {
    let key_id = Uuid::now_v7();
    for value in [
        "",
        "has space",
        "line\nbreak",
        "unicode-会话",
        "slash/id",
        &"a".repeat(129),
    ] {
        for field in ["id", "turn_id", "parent_turn_id"] {
            let mut conversation = json!({"id":"session-1"});
            conversation[field] = json!(value);
            let body: InferenceRequest = serde_json::from_value(json!({
                "key_id":key_id,"request":{},"conversation":conversation
            }))
            .unwrap();
            assert!(
                inference_headers(body.conversation.as_ref()).is_err(),
                "{field}"
            );
        }
    }
    let body: InferenceRequest = serde_json::from_value(json!({
        "key_id":key_id,"request":{},
        "conversation":{"id":"session.uuid_1:branch-2","turn_id":"turn-1","parent_turn_id":"turn-0"}
    }))
    .unwrap();
    let headers = inference_headers(body.conversation.as_ref()).unwrap();
    assert_eq!(headers["x-mtc-conversation-id"], "session.uuid_1:branch-2");
    assert_eq!(headers["x-mtc-turn-id"], "turn-1");
    assert_eq!(headers["x-mtc-parent-turn-id"], "turn-0");
    let body: InferenceRequest = serde_json::from_value(json!({
        "key_id":key_id,"request":{},"conversation":{"id":"a".repeat(128)}
    }))
    .unwrap();
    assert!(inference_headers(body.conversation.as_ref()).is_ok());
    for extra in [
        json!({"conversation":{"turn_id":"missing-session"}}),
        json!({"conversation":{"id":"session-1","headers":{"authorization":"Bearer spoofed"}}}),
        json!({"headers":{"x-mtc-conversation-id":"spoofed"}}),
        json!({"tenant_external_id":"other"}),
    ] {
        let mut body = json!({"key_id":key_id,"request":{}});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(serde_json::from_value::<InferenceRequest>(body).is_err());
    }
}

#[tokio::test]
async fn control_inference_returns_tool_calls_and_settles_selected_billing_key() {
    successful_inference(Protocol::OpenAiChat).await;
}

#[tokio::test]
async fn control_messages_returns_tool_use_and_settles_selected_billing_key() {
    successful_inference(Protocol::AnthropicMessages).await;
}

#[tokio::test]
async fn control_responses_preserves_image_generation_and_archives_the_conversation() {
    successful_inference(Protocol::OpenAiResponses).await;
}

async fn successful_inference(protocol: Protocol) {
    use crate::{
        db::{CreateModelRouteInput, CreateUpstreamAccountInput},
        provider::UpstreamCredential,
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header as header_matcher, method, path},
    };
    let upstream = MockServer::start().await;
    let anthropic = matches!(protocol, Protocol::AnthropicMessages);
    let responses = matches!(protocol, Protocol::OpenAiResponses);
    let completion = if anthropic {
        json!({
            "id":"msg_operator_fixture","type":"message","role":"assistant","model":"fixture",
            "content":[{"type":"tool_use","id":"toolu_fixture","name":"inspect","input":{}}],
            "stop_reason":"tool_use","stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":2}
        })
    } else if responses {
        json!({
            "id":"resp_operator_fixture", "object":"response", "status":"completed", "model":"fixture",
            "output":[{"type":"image_generation_call","id":"ig_fixture","status":"completed","result":IMAGE_FIXTURE_PNG}],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        })
    } else {
        json!({
            "id":"chatcmpl-operator-fixture", "object":"chat.completion", "created":1, "model":"fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"call-fixture", "type":"function", "function":{"name":"inspect","arguments":"{}"}
            }]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        })
    };
    Mock::given(method("POST"))
        .and(path(protocol.path()))
        .and(header_matcher("authorization", "Bearer provider-fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&completion))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, _directory) = crate::api::tests::test_state().await;
    let account = state
        .db
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: "inference-success".into(),
                name: "inference-upstream".into(),
                driver: "http-json".into(),
                config: json!({"base_url":upstream.uri(),"network_scope":"public"}),
                credential: UpstreamCredential::ApiKey {
                    value: "provider-fixture".into(),
                    header: "authorization".into(),
                    prefix: "Bearer ".into(),
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
            tenant_external_id: "inference-success".into(),
            public_model: "fixture".into(),
            upstream_account_id: account.id,
            upstream_model: "fixture".into(),
            protocol: protocol.name().into(),
            priority: 0,
        })
        .await
        .unwrap();
    let key = state
        .db
        .create_key_with_routing(
            CreateKeyInput {
                tenant_external_id: "inference-success".into(),
                principal_external_id: "billing-owner".into(),
                alias: "selected-billing-key".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
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
        .upsert_model_price("fixture", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "inference-writer".into(),
                scopes: vec!["keys:write".into()],
                tenant_external_id: Some("inference-success".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut request = inference(&service.token, key.key_id);
    *request.uri_mut() = format!(
        "/internal/v1/inference{}",
        protocol.path().strip_prefix("/v1").unwrap()
    )
    .parse()
    .unwrap();
    let provider_request = if anthropic {
        json!({
            "model":"fixture","messages":[{"role":"user","content":"hello"}],"max_tokens":64,
            "tools":[{"name":"inspect","description":"Inspect fixture","input_schema":{"type":"object"}}],"stream":false
        })
    } else if responses {
        json!({
            "model":"fixture","input":"Draw a tiny fixture",
            "tools":[{"type":"image_generation","model":"gpt-image-2"}],
            "tool_choice":{"type":"image_generation"},"store":false,"stream":false
        })
    } else {
        json!({"model":"fixture","messages":[{"role":"user","content":"hello"}],"stream":false})
    };
    *request.body_mut() = Body::from(
        json!({
            "key_id":key.key_id,"request":provider_request,
            "conversation":{"id":"playground-fixture","turn_id":"turn-1","parent_turn_id":"turn-0"}
        })
        .to_string(),
    );
    // Incoming operator headers cannot override the bounded wrapper or enter a
    // provider request. The gateway derives the conversation from fixed headers.
    request.headers_mut().insert(
        "x-mtc-conversation-id",
        HeaderValue::from_static("spoofed-session"),
    );
    request
        .headers_mut()
        .insert("x-mtc-subagent", HeaderValue::from_static("true"));
    request.headers_mut().insert(
        "x-operator-secret",
        HeaderValue::from_static("operator-header-fixture"),
    );
    let response = state
        .db
        .with_request_archive_capture_for_test(
            router_for_role(state.clone(), RuntimeRole::Control).oneshot(request),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let bytes = axum::body::to_bytes(response.into_body(), 65_536)
        .await
        .unwrap();
    let actual: Value = serde_json::from_slice(&bytes).unwrap();
    if anthropic {
        assert_eq!(actual["content"], completion["content"]);
        assert_eq!(actual["stop_reason"], "tool_use");
    } else if responses {
        assert_eq!(
            actual, completion,
            "the caller receives the complete original image result"
        );
    } else {
        assert_eq!(actual["choices"], completion["choices"]);
    }
    assert_eq!(actual["usage"], completion["usage"]);
    let output = String::from_utf8_lossy(&bytes);
    for secret in [&key.key, &service.token] {
        assert!(!output.contains(secret));
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let rows = state.db.list_requests(key.key_id, 10).await.unwrap();
            if rows.len() == 1 && rows[0].completed_at.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rows = state.db.list_requests(key.key_id, 10).await.unwrap();
    assert_eq!(rows[0].status_code, Some(200));
    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
    let settlements = state
        .db
        .list_account_settlements(key.account_id, 10, None, None)
        .await
        .unwrap();
    assert_eq!(settlements.items.len(), 1);
    assert_eq!(settlements.items[0].key_id, key.key_id);
    assert_eq!(
        Decimal::from_str(&settlements.items[0].cost).unwrap(),
        Decimal::new(5, 6)
    );
    let forwarded = upstream.received_requests().await.unwrap();
    assert_eq!(forwarded.len(), 1);
    for value in forwarded[0].headers.values() {
        assert!(!value.to_str().unwrap_or_default().contains(&key.key));
        assert!(!value.to_str().unwrap_or_default().contains(&service.token));
    }
    assert!(!String::from_utf8_lossy(&forwarded[0].body).contains(&key.key));
    assert!(!String::from_utf8_lossy(&forwarded[0].body).contains(&service.token));
    let forwarded_body: Value = serde_json::from_slice(&forwarded[0].body).unwrap();
    assert_eq!(forwarded_body, provider_request);
    for name in [
        "x-mtc-conversation-id",
        "x-mtc-turn-id",
        "x-mtc-parent-turn-id",
        "x-mtc-subagent",
        "x-operator-secret",
    ] {
        assert!(!forwarded[0].headers.contains_key(name), "forwarded {name}");
    }
    assert_inference_archive(
        &state,
        key.key_id,
        rows[0].request_id,
        &provider_request,
        &completion,
    )
    .await;
}

async fn assert_inference_archive(
    state: &AppState,
    key_id: Uuid,
    request_id: Uuid,
    request: &Value,
    response: &Value,
) {
    state.db.drain_gateway_persistence_for_test().await;
    state.persistence.drain_for_test().await;
    let writers = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state
            .proxy_archive_stream_permits
            .acquire_many(crate::PROXY_ARCHIVE_STREAM_CONCURRENCY as u32),
    )
    .await
    .unwrap()
    .unwrap();
    drop(writers);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let refs = state
                .db
                .request_archive_refs(key_id, request_id)
                .await
                .unwrap();
            let detail = crate::api::request_detail::request_detail(state, refs).await;
            if detail.archive_complete {
                assert_eq!(&detail.request_body, request);
                assert_eq!(&detail.response_body, response);
                break;
            }
            crate::response_archive_spool::process_one_for_test(state).await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("operator inference request and response archives");

    let owner = Uuid::now_v7();
    let pool = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            for pending_id in state.db.claim_terminal_projection_tasks(owner, 32).await.unwrap() {
                state.db.project_claimed_terminal_projection_task(owner, pending_id).await.unwrap();
            }
            for task in state.db.claim_conversation_projection_tasks(owner, 32).await.unwrap() {
                crate::worker::archive_session_body(state, owner, task.request_id).await.unwrap();
            }
            use sqlx::Row;
            let observation = sqlx::query("SELECT explicit_session_id, turn_id, parent_turn_id, leaf_node_hash FROM conversation_observations WHERE request_id = $1")
                .bind(request_id.to_string()).fetch_optional(&pool).await.unwrap();
            if let Some(row) = observation {
                assert_eq!(row.get::<String, _>("explicit_session_id"), "playground-fixture");
                assert_eq!(row.get::<String, _>("turn_id"), "turn-1");
                assert_eq!(row.get::<String, _>("parent_turn_id"), "turn-0");
                let (_, expected_prefix) = crate::conversation::session_body_from_documents(request, response).into_prefix();
                assert_eq!(row.get::<Option<String>, _>("leaf_node_hash").as_deref(), expected_prefix.last().map(|node| node.node_hash.as_str()));
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.expect("operator inference conversation projection");
    pool.close().await;
}

#[tokio::test]
async fn messages_requires_the_same_key_write_permission() {
    let (state, _directory) = crate::api::tests::test_state().await;
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: "messages-readonly".into(),
                scopes: vec!["keys:read".into()],
                tenant_external_id: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let mut request = inference(&service.token, Uuid::now_v7());
    *request.uri_mut() = "/internal/v1/inference/messages".parse().unwrap();
    let response = router_for_role(state, RuntimeRole::Control)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
