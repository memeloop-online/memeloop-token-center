use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{FinishRequest, NewRequest},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const SERVICE_TOKEN: &str = "test-service-token";
const SQL_PAYLOADS: &[&str] = &[
    "quote-'",
    "comment-'--",
    "boolean-' OR '1'='1",
    "union-' UNION SELECT NULL--",
    "stacked-'; DROP TABLE tenants;--",
];

async fn request_json(
    state: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    request_json_with_token(state, method, path, body, SERVICE_TOKEN).await
}

async fn request_json_with_token(
    state: &AppState,
    method: Method,
    path: &str,
    body: Option<Value>,
    token: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&body).expect("serialize security request"))
        }
        None => Body::empty(),
    };
    let response = api::router(state.clone())
        .oneshot(request.body(body).expect("security request"))
        .await
        .expect("security response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .expect("bounded security response");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

fn query(path: &str, values: &[(&str, &str)]) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values.iter().copied())
        .finish();
    format!("{path}?{encoded}")
}

async fn exercise_injection_matrix(database_url: String, backend: &str) {
    let state = AppState::initialize(Config::for_test(database_url))
        .await
        .unwrap_or_else(|error| panic!("initialize {backend} security fixture: {error}"));
    let run = Uuid::now_v7().to_string();
    let mut first_credential: Option<(String, String, Uuid)> = None;

    for (index, payload) in SQL_PAYLOADS.iter().enumerate() {
        let tenant = format!("security-{backend}-{run}-{index}-{payload}");
        let principal = format!("principal-{index}-{payload}");
        let alias = format!("alias-{index}-{payload}");
        let model = format!("model-{index}-{payload}");
        let error_code = format!("error-{index}-{payload}");
        let source = format!("source-{index}-{payload}");

        let (status, issued) = request_json(
            &state,
            Method::POST,
            "/internal/v1/keys",
            Some(json!({
                "tenant_external_id": tenant,
                "principal_external_id": principal,
                "alias": alias,
                "currency": "USD",
                "initial_balance": "10",
                "policy": {"allowed_models": [model]}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{backend} payload={payload}");
        let key = issued["key"].as_str().expect("issued key");
        let key_id = issued["key_id"].as_str().expect("issued key id");
        let account_id = issued["account_id"].as_str().expect("issued account id");

        let (status, _) = request_json(
            &state,
            Method::POST,
            &format!("/internal/v1/accounts/{account_id}/grants"),
            Some(json!({"amount": "1", "source": source})),
        )
        .await;
        // The endpoint requires an idempotency header. Send the real request
        // below so the externally supplied source crosses the SQL boundary.
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let response = api::router(state.clone())
            .oneshot(
                Request::post(format!("/internal/v1/accounts/{account_id}/grants"))
                    .header(header::AUTHORIZATION, format!("Bearer {SERVICE_TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("idempotency-key", format!("security-{run}-{index}"))
                    .body(Body::from(
                        serde_json::to_vec(&json!({"amount": "1", "source": source})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("grant response");
        assert_eq!(response.status(), StatusCode::CREATED);

        let authenticated = state
            .db
            .authenticate_key(key, state.config.key_pepper.as_bytes())
            .await
            .expect("authenticate injection fixture key");
        let request_id = Uuid::now_v7();
        state
            .db
            .record_request_started(NewRequest {
                request_id,
                key_id: authenticated.key_id,
                tenant_id: authenticated.tenant_id,
                protocol: "openai-chat".to_owned(),
                model: model.clone(),
                request_object: format!("memory://security/{request_id}/request"),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .expect("record injection fixture request");
        state
            .db
            .record_request_finished(FinishRequest {
                request_id,
                status_code: 502,
                duration_ms: 1,
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                service_tier: None,
                cost_micros: 0,
                error_code: Some(error_code.clone()),
                response_object: format!("memory://security/{request_id}/response"),
            })
            .await
            .expect("finish injection fixture request");

        if let Some((first_key, first_key_id, first_request_id)) = &first_credential {
            let attacker_filter = query(
                "/self/v1/requests",
                &[
                    ("key_id", first_key_id.as_str()),
                    ("tenant_external_id", "attacker-controlled"),
                ],
            );
            let (status, self_rows) =
                request_json_with_token(&state, Method::GET, &attacker_filter, None, key).await;
            assert_eq!(status, StatusCode::OK, "{backend} self request filter");
            assert_eq!(self_rows.as_array().map(Vec::len), Some(1));
            assert_eq!(self_rows[0]["request_id"], request_id.to_string());

            for (viewer_key, foreign_request_id) in
                [(key, first_request_id), (first_key.as_str(), &request_id)]
            {
                let (status, _) = request_json_with_token(
                    &state,
                    Method::GET,
                    &format!("/self/v1/requests/{foreign_request_id}"),
                    None,
                    viewer_key,
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::NOT_FOUND,
                    "{backend} cross-credential request detail must be hidden"
                );
            }

            let stats_path = query("/self/v1/stats", &[("key_id", first_key_id.as_str())]);
            let (status, stats) =
                request_json_with_token(&state, Method::GET, &stats_path, None, key).await;
            assert_eq!(status, StatusCode::OK, "{backend} self stats filter");
            assert_eq!(stats["summary"]["total_requests"], 1);
        } else {
            first_credential = Some((key.to_owned(), key_id.to_owned(), request_id));
        }

        for (field, value) in [
            ("model", model.as_str()),
            ("error_code", error_code.as_str()),
            ("key_alias", alias.as_str()),
            ("principal", principal.as_str()),
        ] {
            let path = query(
                "/internal/v1/requests",
                &[("tenant_external_id", tenant.as_str()), (field, value)],
            );
            let (status, rows) = request_json(&state, Method::GET, &path, None).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{backend} field={field} payload={payload}"
            );
            assert_eq!(rows.as_array().map(Vec::len), Some(1));
            assert_eq!(rows[0]["request_id"], request_id.to_string());
        }

        let keys_path = query(
            "/internal/v1/keys",
            &[
                ("tenant_external_id", tenant.as_str()),
                ("principal_external_id", principal.as_str()),
            ],
        );
        let (status, keys) = request_json(&state, Method::GET, &keys_path, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(keys.as_array().map(Vec::len), Some(1));
        assert_eq!(keys[0]["alias"], alias);

        let (status, ledger) = request_json(
            &state,
            Method::GET,
            &format!("/internal/v1/accounts/{account_id}/ledger"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            ledger
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["source"] == source))
        );

        for cursor_field in ["before_id", "before_created_at"] {
            let cursor_path = query(
                "/internal/v1/keys",
                &[
                    ("tenant_external_id", tenant.as_str()),
                    (cursor_field, payload),
                ],
            );
            let (status, _) = request_json(&state, Method::GET, &cursor_path, None).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{backend} cursor={cursor_field} payload={payload}"
            );
        }
    }

    let (status, _) = request_json(&state, Method::GET, "/internal/v1/schemas", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{backend} schema remained queryable after the stacked-query probes"
    );
}

#[tokio::test]
async fn sqlite_external_string_injection_matrix_is_bound_and_fail_closed() {
    let directory = tempfile::tempdir().expect("SQLite security directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("security-injection.db").display()
    );
    exercise_injection_matrix(database_url, "sqlite").await;
}

#[tokio::test]
async fn postgres_external_string_injection_matrix_is_bound_or_explicitly_skipped() {
    let database_url = match std::env::var("MTC_TEST_POSTGRES_URL") {
        Ok(value) => value,
        Err(_)
            if std::env::var_os("MTC_REQUIRE_POSTGRES_SECURITY").is_some()
                || std::env::var_os("CI").is_some() =>
        {
            panic!(
                "PostgreSQL security gate required but MTC_TEST_POSTGRES_URL is unset; this is not a passing PostgreSQL result"
            )
        }
        Err(_) => {
            eprintln!(
                "SECURITY_GATE_SKIPPED backend=postgres reason=MTC_TEST_POSTGRES_URL_unset (set MTC_REQUIRE_POSTGRES_SECURITY=1 in release CI)"
            );
            return;
        }
    };
    exercise_injection_matrix(database_url, "postgres").await;
}
