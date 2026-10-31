use super::*;

fn input() -> Value {
    json!({"prompt": "A blue cube slowly rotates on a white table.",
        "duration": 5, "resolution": "720p", "aspect_ratio": "16:9"})
}

fn completed() -> Value {
    json!({"code": "success", "message": "", "data": {
        "task_id": "task_mock", "status": "SUCCESS", "result_url": "https://media.example.test/video.mp4",
        "quota": 381146, "total_tokens": 108899, "model_name": "dreamina-seedance-2-0-idle-260128",
        "duration": 5, "ratio": "16:9", "finish_time": 1788227716_i64, "execution_expires_after": 172800
    }})
}

#[test]
fn exact_models_require_an_explicit_profile_and_keep_text_protocols() {
    let config = json!({"video_api": PROFILE, "video_models": MODELS});
    for model in MODELS {
        let adapter = GenerationProtocolAdapter::detect("http-json", &config, model).unwrap();
        assert_eq!(adapter, GenerationProtocolAdapter::TkmarketSeedanceV1);
        assert_eq!(adapter.catalog_modalities(), &["video"]);
        assert!(adapter.supports_modality("video"));
        assert!(!adapter.supports_modality("image"));
        adapter.normalize_input(&mut input()).unwrap();
        // A configured seconds/job price must not turn provider tokens into
        // seconds/jobs. Even a token price is insufficient without a native
        // reservation bound and durable supplier receipt in the lifecycle.
        for unit in ["second", "job", "token", "million_tokens", "megapixel"] {
            assert!(adapter.estimated_units(unit, &input()).is_err());
        }
        let body = submit_body(model, &input()).unwrap();
        assert_eq!(body["model"], *model);
        assert_eq!(body["duration"], 5);
        assert!(GenerationProtocolAdapter::detect("http-json", &json!({}), model).is_none());
        assert!(
            GenerationProtocolAdapter::detect(
                "http-json",
                &json!({"video_api": PROFILE, "video_models": []}),
                model
            )
            .is_none()
        );
    }
    for model in [
        "ts-seedance-2.0",
        "ts-seedance-2.5",
        "MiniMax-H3",
        "hailuo",
        "deepseek-v4.1-flash",
    ] {
        assert!(GenerationProtocolAdapter::detect("http-json", &config, model).is_none());
        assert!(submit_body(model, &input()).is_err());
    }
    assert!(
        GenerationProtocolAdapter::detect("volcengine-seedance", &config, MODELS[0])
            != Some(GenerationProtocolAdapter::TkmarketSeedanceV1)
    );
}

#[test]
fn profile_rejects_unverified_parameters_and_model_injection() {
    validate_input(&input()).unwrap();
    crate::schema::validate_instance(&input_schema(), &input()).unwrap();
    for (field, value) in [
        ("duration", json!(6)),
        ("duration", json!(5.0)),
        ("duration", json!("5")),
        ("resolution", json!("768P")),
        ("aspect_ratio", json!("9:16")),
        ("prompt", json!(" ")),
        ("model", json!(MODELS[1])),
        ("seconds", json!("8")),
        ("metadata", json!({"duration": 8})),
        ("videos", json!(["https://example.test/ref.mp4"])),
        ("generate_audio", json!(true)),
    ] {
        let mut bad = input();
        bad[field] = value;
        assert!(validate_input(&bad).is_err(), "field {field}");
    }
}

#[test]
fn documented_paths_never_double_v1_or_guess_plural_aliases() {
    assert_eq!(
        url("https://api.example.test/v1", None).unwrap(),
        "https://api.example.test/v1/video/generations"
    );
    assert_eq!(
        url("https://api.example.test/v1", Some("task_mock")).unwrap(),
        "https://api.example.test/v1/video/generations/task_mock"
    );
    for base in [
        "https://api.example.test",
        "https://api.example.test/v1/",
        "https://api.example.test/v1?key=x",
    ] {
        assert!(url(base, None).is_err());
    }
    for id in ["../secret", "id?x=1", "id#x", "id/path", ""] {
        assert!(url("https://api.example.test/v1", Some(id)).is_err());
    }
}

#[test]
fn ack_and_poll_identity_are_fenced_to_the_selected_model_and_task() {
    let body = json!({"id": "task_mock", "model": MODELS[0], "status": "queued"});
    ack(&body, MODELS[0]).unwrap();
    assert!(ack(&body, MODELS[1]).is_err());
    for body in [
        json!({"id": "task_mock"}),
        json!({"id": "../bad", "model": MODELS[0], "status": "queued"}),
        json!({"id": "task_mock", "model": MODELS[0], "status": "unknown"}),
    ] {
        assert!(ack(&body, MODELS[0]).is_err());
    }
    assert!(query_result(&completed(), "another_task").is_err());
    let mut body = completed();
    body["code"] = json!("failure");
    assert!(query_result(&body, "task_mock").is_err());
    assert!(matches!(query_result(&json!({"code": "success", "data": {"task_id": "task_mock", "status": "IN_PROGRESS", "quota": 99999}}), "task_mock").unwrap(), QueryResult::Pending));
    assert!(matches!(query_result(&json!({"code": "success", "data": {"task_id": "task_mock", "status": "FAILURE", "quota": 0}}), "task_mock").unwrap(), QueryResult::Failed { quota: 0 }));
}

#[test]
fn supplier_tokens_and_quota_are_distinct_exact_evidence() {
    let QueryResult::Succeeded(video) = query_result(&completed(), "task_mock").unwrap() else {
        panic!("expected completed video")
    };
    assert_eq!(video.usage.total_tokens, 108899);
    assert_eq!(video.usage.quota, 381146);
    assert_eq!(video.usage.supplier_usd_micros, 762292);
    assert_eq!(video.expires_at, Some(1788400516000));
    assert_eq!(
        video.provider_model_name,
        "dreamina-seedance-2-0-idle-260128"
    );
    for (field, value) in [
        ("total_tokens", json!(null)),
        ("total_tokens", json!(0)),
        ("total_tokens", json!("108899")),
        ("total_tokens", json!(108899.5)),
        ("quota", json!(-1)),
        ("quota", json!(1.5)),
        ("quota", json!(i64::MAX)),
        ("duration", json!(6)),
        ("duration", json!(5.0)),
        ("ratio", json!("9:16")),
        ("status", json!("UNKNOWN")),
        ("result_url", json!("http://media.example.test/video.mp4")),
        (
            "result_url",
            json!("https://user:secret@media.example.test/video.mp4"),
        ),
        ("model_name", json!("")),
        ("finish_time", json!(i64::MAX)),
    ] {
        let mut body = completed();
        body["data"][field] = value;
        assert!(query_result(&body, "task_mock").is_err(), "field {field}");
    }
    for field in [
        "total_tokens",
        "quota",
        "duration",
        "ratio",
        "result_url",
        "model_name",
    ] {
        let mut body = completed();
        body["data"].as_object_mut().unwrap().remove(field);
        assert!(query_result(&body, "task_mock").is_err(), "missing {field}");
    }
    let mut zero_cost = completed();
    zero_cost["data"]["quota"] = json!(0);
    let QueryResult::Succeeded(video) = query_result(&zero_cost, "task_mock").unwrap() else {
        panic!("expected completed video")
    };
    assert_eq!(video.usage.supplier_usd_micros, 0);
    assert_eq!(video.usage.total_tokens, 108899);
    let failure = json!({"code": "success", "data": {"task_id": "task_mock", "status": "FAILURE", "quota": 23}});
    assert!(matches!(
        query_result(&failure, "task_mock").unwrap(),
        QueryResult::Failed { quota: 23 }
    ));
}

// Real HTTP contract: both exact API IDs use the same documented dialect.
// Synthetic credentials and results only. This does not claim MTC settlement
// succeeds: admission above explicitly requires the missing billing interface.
#[tokio::test]
async fn mock_submit_poll_output_and_native_usage_contract_for_both_exact_models() {
    use axum::{
        Json, Router,
        extract::State,
        http::HeaderMap,
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Clone)]
    struct Mock {
        submits: Arc<AtomicUsize>,
        polls: Arc<AtomicUsize>,
        model: &'static str,
    }
    async fn submit(
        State(mock): State<Mock>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer synthetic-private-key");
        assert_eq!(body["model"], mock.model);
        assert_eq!(body["duration"], 5);
        assert_eq!(body["resolution"], "720p");
        mock.submits.fetch_add(1, Ordering::SeqCst);
        Json(json!({"id": "task_mock", "model": mock.model, "status": "queued"}))
    }
    async fn query(State(mock): State<Mock>, headers: HeaderMap) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer synthetic-private-key");
        if mock.polls.fetch_add(1, Ordering::SeqCst) == 0 {
            Json(
                json!({"code": "success", "data": {"task_id": "task_mock", "status": "IN_PROGRESS", "quota": 123}}),
            )
        } else {
            Json(completed())
        }
    }
    async fn content(headers: HeaderMap) -> impl axum::response::IntoResponse {
        assert!(!headers.contains_key("authorization"));
        (
            [("content-type", "video/mp4")],
            b"synthetic-mp4-output".to_vec(),
        )
    }
    for model in MODELS {
        let mock = Mock {
            submits: Arc::default(),
            polls: Arc::default(),
            model,
        };
        let app = Router::new()
            .route("/v1/video/generations", post(submit))
            .route("/v1/video/generations/task_mock", get(query))
            .route("/mock-output", get(content))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = crate::build_no_retry_http_client(None, &[]).unwrap();
        let base = format!("{origin}/v1");
        let response = client
            .post(url(&base, None).unwrap())
            .bearer_auth("synthetic-private-key")
            .json(&submit_body(model, &input()).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        let task_id = ack(&body, model).unwrap()["id"].as_str().unwrap();
        for terminal in [false, true] {
            let body: Value = client
                .get(url(&base, Some(task_id)).unwrap())
                .bearer_auth("synthetic-private-key")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            match query_result(&body, task_id).unwrap() {
                QueryResult::Pending => assert!(!terminal),
                QueryResult::Succeeded(video) => {
                    assert!(terminal);
                    assert_eq!(video.result_url, "https://media.example.test/video.mp4");
                    assert_eq!(video.usage.total_tokens, 108899);
                    assert_eq!(video.usage.quota, 381146);
                    assert_eq!(video.usage.supplier_usd_micros, 762292);
                    // Loopback synthetic bytes stand in for the HTTPS asset
                    // fixture. The production worker owns origin/DNS/auth and
                    // repeatable range validation through existing helpers.
                    let output = client
                        .get(format!("{origin}/mock-output"))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(output.headers()["content-type"], "video/mp4");
                    assert_eq!(
                        output.bytes().await.unwrap().as_ref(),
                        b"synthetic-mp4-output"
                    );
                }
                QueryResult::Failed { .. } => panic!("unexpected failure"),
            }
        }
        assert_eq!(mock.submits.load(Ordering::SeqCst), 1);
        assert_eq!(mock.polls.load(Ordering::SeqCst), 2);
        server.abort();
        let _ = server.await;
    }
}
