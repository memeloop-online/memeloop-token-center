use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{CreateKeyInput, CreateServiceTokenInput, NewRequest, egress},
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &[u8] = b"request egress list fixture pepper";

async fn response_json(
    state: &AppState,
    path: &str,
    credential: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {credential}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            body.map(|value| value.to_string()).unwrap_or_default(),
        ))
        .unwrap();
    let response = api::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn attempt(
    request_id: Uuid,
    observed_at: i64,
    final_response: bool,
) -> egress::RequestEgressAttempt {
    let mut selected = egress::SelectedEgress::capture(Some(
        "socks5h://fixture-user:fixture-password@[2001:db8::1]:1081/private?token=fixture-token#fragment",
    ))
    .unwrap();
    selected.group_id = Some(Uuid::now_v7());
    selected.group_name = Some("Historical group".into());
    selected.group_version = Some(3);
    selected.member_name = Some("Historical member".into());
    egress::RequestEgressAttempt {
        request_id,
        attempt_id: Uuid::now_v7(),
        outbound_attempt: 1,
        connect_attempt: 1,
        route_id: Uuid::now_v7(),
        upstream_account_id: Uuid::now_v7(),
        credential_generation: 1,
        transport_revision: 1,
        observed_at,
        selected,
        transport: json!({"private_transport_diagnostic": true}),
        connection_binding: None,
        request_payload_bytes: None,
        response_payload_bytes: None,
        completed: false,
        final_response,
        response_body_complete: false,
        measurement_basis: "application_payload_estimate",
    }
}

async fn list_contract(database_url: String) {
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let tenant = format!("egress-list-{}", Uuid::now_v7());
    let foreign_tenant = format!("egress-foreign-{}", Uuid::now_v7());
    let mut credentials = Vec::new();
    let mut keys = Vec::new();
    for (index, owner) in [&tenant, &tenant, &foreign_tenant].into_iter().enumerate() {
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: owner.clone(),
                    principal_external_id: format!("fixture-{index}"),
                    alias: format!("fixture-{index}"),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                PEPPER,
            )
            .await
            .unwrap();
        keys.push(
            state
                .db
                .authenticate_key(&issued.key, PEPPER)
                .await
                .unwrap(),
        );
        credentials.push(issued.key);
    }
    let mut requests = Vec::new();
    for owner in [0, 0, 0, 0, 1, 2] {
        let request_id = Uuid::now_v7();
        state
            .db
            .record_request_started(NewRequest {
                request_id,
                key_id: keys[owner].key_id,
                tenant_id: keys[owner].tenant_id,
                protocol: "openai".into(),
                model: "egress-list-fixture".into(),
                request_object: "inline-json:{}".into(),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        requests.push(request_id);
    }
    let mut winner = attempt(requests[0], 200, true);
    winner.selected.member_name = Some("Final historical member".into());
    let mut tied = winner.clone();
    tied.attempt_id = Uuid::now_v7();
    tied.selected.member_name = Some("Tied final historical member".into());
    for recorded in [
        attempt(requests[0], 100, true),
        winner,
        tied.clone(),
        attempt(requests[0], 900, false),
        attempt(requests[1], 100, false),
        attempt(requests[1], 800, false),
        attempt(requests[4], 400, true),
        attempt(requests[5], 500, true),
    ] {
        state
            .db
            .record_request_egress_attempt(&recorded)
            .await
            .unwrap();
    }
    let mut direct = attempt(requests[3], 300, false);
    direct.selected = egress::SelectedEgress::capture(None).unwrap();
    direct.selected.kind = "direct".into();
    state
        .db
        .record_request_egress_attempt(&direct)
        .await
        .unwrap();

    let (status, own) = response_json(&state, "/self/v1/requests", &credentials[0], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(own.as_array().unwrap().len(), 4);
    for (index, expected_time) in [(0, Some(200)), (1, Some(800)), (2, None), (3, Some(300))] {
        let row = own
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["request_id"] == requests[index].to_string())
            .unwrap();
        assert!(row["credential_identity"].is_null());
        if let Some(expected_time) = expected_time {
            assert_eq!(row["egress"]["observed_at"], expected_time);
            assert_eq!(row["egress"].as_object().unwrap().len(), 2);
        } else {
            assert!(row["egress"].is_null());
        }
        if index == 0 {
            assert_eq!(
                row["egress"]["selected"],
                serde_json::to_value(&tied.selected).unwrap()
            );
            assert_eq!(row["egress"]["selected"]["host"], "[2001:db8::1]");
            assert_eq!(row["egress"]["selected"]["port"], 1081);
        }
        if index == 3 {
            assert_eq!(row["egress"]["selected"]["kind"], "direct");
            assert!(row["egress"]["selected"]["host"].is_null());
        }
    }
    let serialized = own.to_string();
    for excluded in [
        "fixture-user",
        "fixture-password",
        "private",
        "fixture-token",
        "fragment",
        "transport_revision",
    ] {
        assert!(
            !serialized.contains(excluded),
            "unexpected field: {excluded}"
        );
    }
    let service = state
        .db
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("egress-reader-{tenant}"),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some(tenant.clone()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    for (path, body) in [
        ("/internal/v1/requests", None),
        (
            "/internal/v1/requests/query",
            Some(json!({"ast":{"logical_operator":"and", "conditions":[]}})),
        ),
    ] {
        let (status, rows) = response_json(&state, path, &service.token, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rows.as_array().unwrap().len(), 5);
        assert!(!rows.to_string().contains(&requests[5].to_string()));
        for own_row in own.as_array().unwrap() {
            let operator_row = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["request_id"] == own_row["request_id"])
                .unwrap();
            assert_eq!(operator_row["egress"], own_row["egress"]);
        }
    }
    let (status, _) = response_json(
        &state,
        "/internal/v1/requests/query",
        &service.token,
        Some(json!({
            "tenant_external_id": foreign_tenant,
            "ast": {"logical_operator":"and", "conditions":[]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = response_json(
        &state,
        "/internal/v1/requests/query",
        &credentials[0],
        Some(json!({
            "ast": {"logical_operator":"and", "conditions":[]}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, foreign) = response_json(&state, "/self/v1/requests", &credentials[2], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(foreign.as_array().unwrap().len(), 1);
    assert_eq!(foreign[0]["request_id"], requests[5].to_string());
    assert_eq!(foreign[0]["egress"]["observed_at"], 500);

    let (status, page) = response_json(
        &state,
        "/internal/v1/requests/query",
        &service.token,
        Some(json!({"limit": 2, "paged": true, "ast": {"logical_operator": "and", "conditions": []}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["requests"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_object());

    let mut malformed = attempt(requests[1], 1000, true);
    malformed.selected = egress::SelectedEgress::capture(None).unwrap();
    state
        .db
        .record_request_egress_attempt(&malformed)
        .await
        .unwrap();
    let inspection = sqlx::AnyPool::connect(&database_url).await.unwrap();
    sqlx::query("UPDATE request_egress_attempts SET snapshot_json = '{bad' WHERE request_id = $1 AND attempt_id = $2")
        .bind(requests[1].to_string())
        .bind(malformed.attempt_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    let (status, rows) = response_json(&state, "/self/v1/requests", &credentials[0], None).await;
    assert_eq!(status, StatusCode::OK);
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["request_id"] == requests[1].to_string())
        .unwrap();
    assert!(row["egress"].is_null());
    inspection.close().await;
}

#[tokio::test]
async fn sqlite_request_egress_lists_preserve_history_and_authorized_scope() {
    let directory = tempfile::tempdir().unwrap();
    list_contract(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("egress-list.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_request_egress_lists_preserve_history_and_authorized_scope() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    list_contract(database_url).await;
}
