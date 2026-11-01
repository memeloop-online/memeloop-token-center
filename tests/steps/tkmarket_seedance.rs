use super::*;

fn input() -> Value {
    json!({"prompt": "A blue cube slowly rotates on a white table.",
        "duration": 5, "resolution": "720p", "aspect_ratio": "16:9"})
}

async fn post_control(
    world: &TokenCenterWorld,
    path: &str,
    body: Value,
    status: StatusCode,
) -> Value {
    let response = world
        .client
        .post(format!("{}{path}", world.service_url))
        .bearer_auth("test-service-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    let actual_status = response.status();
    let value: Value = response.json().await.unwrap();
    assert_eq!(actual_status, status, "{value}");
    value
}

#[when(expr = "the service configures tkmarket model {string} with terminal {string}")]
async fn configure(world: &mut TokenCenterWorld, model: String, terminal: String) {
    stop_test_worker(world).await;
    let assets = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/video.mp4"))
        .and(header("range", "bytes=0-0"))
        .respond_with(if terminal == "missing-asset" {
            ResponseTemplate::new(404)
        } else {
            ResponseTemplate::new(206)
                .insert_header("content-type", "video/mp4")
                .insert_header("content-range", "bytes 0-0/9")
                .set_body_bytes(b"m")
        })
        .with_priority(1)
        .mount(&assets)
        .await;
    Mock::given(method("GET"))
        .and(path("/video.mp4"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"mtc-video"),
        )
        .with_priority(10)
        .mount(&assets)
        .await;
    let server = world.mock.as_ref().unwrap();
    let mut expected = input();
    expected["model"] = json!(model);
    Mock::given(method("POST"))
        .and(path("/v1/video/generations"))
        .and(header("authorization", "Bearer synthetic-tkmarket-key"))
        .and(body_partial_json(expected))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(if terminal == "unknown-ack" {
                json!({"status": "queued", "model": model})
            } else {
                json!({"id": "tk-task-1", "status": "queued", "model": model})
            }),
        )
        .expect(1)
        .mount(server)
        .await;
    let mut data = json!({"task_id": "tk-task-1", "status": "SUCCESS", "total_tokens": 108899,
        "quota": 381146, "duration": 5, "ratio": "16:9",
        "model_name": "provider-implementation-provenance",
        "result_url": format!("{}/video.mp4?signature=private-tk-signature", assets.uri())});
    match terminal.as_str() {
        "failure" => {
            data["status"] = json!("FAILURE");
            data["quota"] = json!(23);
            data.as_object_mut().unwrap().remove("total_tokens");
        }
        "unknown-cost" => {
            data["status"] = json!("FAILURE");
            data.as_object_mut().unwrap().remove("quota");
            data.as_object_mut().unwrap().remove("total_tokens");
        }
        "missing-tokens" => {
            data.as_object_mut().unwrap().remove("total_tokens");
        }
        "wrong-task" => {
            data["task_id"] = json!("another-task");
        }
        _ => {}
    }
    // One pending query tests ACK persistence across worker polling. No second POST is possible.
    let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("GET")).and(path("/v1/video/generations/tk-task-1"))
        .and(header("authorization", "Bearer synthetic-tkmarket-key"))
        .respond_with(move |_: &wiremock::Request| {
            let pending = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            ResponseTemplate::new(200).set_body_json(if pending {
                json!({"code": "success", "data": {"task_id": "tk-task-1", "status": "IN_PROGRESS", "quota": 999999}})
            } else { json!({"code": "success", "data": data}) })
        }).mount(server).await;
    let config = json!({"base_url": format!("{}/v1", server.uri()), "network_scope": "private",
        "video_api": "tkmarket-seedance-v1", "video_models": [model],
        "result_origins": [assets.uri()], "provider_asset_reads_repeatable": true});
    for (field, value) in [
        ("video_models", json!(["ts-seedance-2.0"])),
        ("video_models", json!(["hailuo"])),
        ("video_models", json!([])),
        ("result_origins", json!([])),
        ("provider_asset_reads_repeatable", json!(false)),
        ("base_url", json!(format!("{}/v2", server.uri()))),
    ] {
        let mut bad = config.clone();
        bad[field] = value;
        post_control(
            world,
            "/internal/v1/upstreams",
            json!({"name": "invalid-tk-profile",
            "driver": "http-json", "config": bad, "credential": {"type": "none"}}),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let account = post_control(
        world,
        "/internal/v1/upstreams",
        json!({"name": "tkmarket-video",
        "driver": "http-json", "config": config,
        "credential": {"type": "api_key", "value": "synthetic-tkmarket-key"}}),
        StatusCode::CREATED,
    )
    .await;
    post_control(
        world,
        "/internal/v1/model-routes",
        json!({"public_model": "tk-video-public",
        "upstream_account_id": account["id"], "upstream_model": model,
        "protocol": "generation", "custom_model_confirmed": true}),
        StatusCode::CREATED,
    )
    .await;
    // Synthetic MTC customer price intentionally differs from both supplier list prices.
    post_control(
        world,
        "/internal/v1/generation-prices/USD/tk-video-public",
        json!({"billing_unit": "million_tokens", "price_per_unit": "12"}),
        StatusCode::OK,
    )
    .await;
    let key = post_control(
        world,
        "/internal/v1/keys",
        json!({"principal_external_id": "tk-video-user",
        "alias": "tk-video", "currency": "USD", "initial_balance": "0",
        "policy": {"enforcement_mode": "metered_unlimited"}}),
        StatusCode::CREATED,
    )
    .await;
    world.current_key = key["key"].as_str().unwrap().to_owned();
    world.stable_account_id = Some(Uuid::parse_str(key["account_id"].as_str().unwrap()).unwrap());
    world.stable_key_id = Some(Uuid::parse_str(key["key_id"].as_str().unwrap()).unwrap());
    grant_fixture_model(world, "default", &key, "tk-video-public", "generation").await;
    // A prepaid credential has no documented ceiling and must fail before submitting.
    let prepaid = post_control(
        world,
        "/internal/v1/keys",
        json!({"principal_external_id": "tk-prepaid",
        "alias": "tk-prepaid", "currency": "USD", "initial_balance": "10", "policy": {}}),
        StatusCode::CREATED,
    )
    .await;
    grant_fixture_model(world, "default", &prepaid, "tk-video-public", "generation").await;
    let rejected = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(prepaid["key"].as_str().unwrap())
        .json(&json!({"model": "tk-video-public", "input": input()}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    world.asset_mock = Some(assets);
}

async fn create(world: &TokenCenterWorld) -> reqwest::Response {
    world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "tk-single-task")
        .json(&json!({"model": "tk-video-public", "input": input()}))
        .send()
        .await
        .unwrap()
}

#[when("the client creates and replays a tkmarket video before the customer price changes")]
async fn submit(world: &mut TokenCenterWorld) {
    let response = create(world).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let first: Value = response.json().await.unwrap();
    assert_eq!(first["estimated_units"], 0);
    world.generation_job_id = Some(Uuid::parse_str(first["job_id"].as_str().unwrap()).unwrap());
    let replay = create(world).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(replay.json::<Value>().await.unwrap(), first);
    post_control(
        world,
        "/internal/v1/generation-prices/USD/tk-video-public",
        json!({"billing_unit": "million_tokens", "price_per_unit": "99"}),
        StatusCode::OK,
    )
    .await;
    restart_test_worker(world);
}

#[then(expr = "the tkmarket job retains {string} usage and one independent customer settlement")]
async fn settled(world: &mut TokenCenterWorld, terminal: String) {
    let job_id = world.generation_job_id.unwrap();
    let mut value = Value::Null;
    for _ in 0..160 {
        value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if matches!(value["status"].as_str(), Some("succeeded" | "failed"))
            || (matches!(terminal.as_str(), "unknown-ack" | "wrong-task")
                && value["error_code"].is_string())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let success = terminal == "success";
    if matches!(terminal.as_str(), "unknown-ack" | "wrong-task") {
        assert_ne!(value["status"], "succeeded");
        assert_eq!(value["cost"], "0");
        assert!(value["result"].is_null());
    } else {
        assert_eq!(
            value["status"],
            if success { "succeeded" } else { "failed" },
            "{value}"
        );
        assert_eq!(value["billing_unit"], "million_tokens");
        assert_eq!(value["billed_units"], if success { 108899 } else { 0 });
        assert_eq!(value["cost"], if success { "1.306788" } else { "0" });
        let receipt = &value["result"]["provider_usage"];
        assert_eq!(receipt["task_id"], "tk-task-1");
        if terminal == "unknown-cost" {
            assert!(receipt["quota"].is_null());
            assert!(receipt["supplier_usd_micros"].is_null());
        } else {
            let quota = if terminal == "failure" { 23 } else { 381146 };
            assert_eq!(receipt["quota"], quota);
            assert_eq!(receipt["supplier_usd_micros"], quota * 2);
        }
        assert_eq!(value["result"]["customer_price"]["usage_unit"], "token");
        assert_eq!(
            value["result"]["customer_price"]["micros_per_unit"],
            12000000
        );
        let key_id = world.stable_key_id.unwrap();
        let pool = AnyPool::connect(&world.state.as_ref().unwrap().config.database_url)
            .await
            .unwrap();
        let account_id = world.stable_account_id.unwrap();
        let mut feed = Value::Null;
        for _ in 0..80 {
            feed = world
                .client
                .get(format!(
                    "{}/internal/v1/accounts/{account_id}/settlements",
                    world.service_url
                ))
                .bearer_auth("test-service-token")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if feed["items"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let items = feed["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{feed}");
        assert_eq!(items[0]["request_id"], job_id.to_string());
        assert_eq!(items[0]["cost"], value["cost"]);
        let row = sqlx::query("SELECT COUNT(*) AS count, COALESCE(SUM(actual_micros),0) AS cost FROM usage_reservations WHERE key_id = $1 AND status = 'settled'")
            .bind(key_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(row.try_get::<i64, _>("count").unwrap(), 1);
        assert_eq!(
            row.try_get::<i64, _>("cost").unwrap(),
            if success { 1306788 } else { 0 }
        );
        pool.close().await;
    }
    if success {
        assert_eq!(value["result"]["provider_usage"]["total_tokens"], 108899);
        let asset_id = value["assets"][0]["asset_id"].as_str().unwrap();
        for _ in 0..2 {
            let response = world
                .client
                .get(format!(
                    "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                    world.service_url
                ))
                .bearer_auth(&world.current_key)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap(), b"mtc-video".as_slice());
        }
    }
    stop_test_worker(world).await;
    // Exercise the database's terminal replay path with exactly the delivered
    // metadata, then reject a different usage count. Neither can debit twice.
    if success {
        let state = world.state.as_ref().unwrap();
        let asset = memeloop_token_center::model::ProviderGenerationAsset {
            asset_id: Uuid::parse_str(value["assets"][0]["asset_id"].as_str().unwrap()).unwrap(),
            index: 0,
            url: format!(
                "{}/video.mp4?signature=private-tk-signature",
                world.asset_mock.as_ref().unwrap().uri()
            ),
            expires_at: None,
            mime_type: "video/mp4".to_owned(),
            filename: "video.mp4".to_owned(),
        };
        for units in [108899, 108899, 108900] {
            let replay = state
                .db
                .finish_generation_job_with_provider_assets(
                    memeloop_token_center::db::FinishGenerationJobInput {
                        job_id,
                        worker_id: "terminal-replay",
                        status: "succeeded",
                        billed_units: units,
                        error_code: None,
                        assets: &[],
                        staged_assets: None,
                    },
                    std::slice::from_ref(&asset),
                    state.config.key_pepper.as_bytes(),
                )
                .await;
            if units == 108899 {
                assert_eq!(replay.unwrap(), 1306788);
            } else {
                assert!(replay.is_err());
            }
        }
        let pool = AnyPool::connect(&state.config.database_url).await.unwrap();
        let ledger = sqlx::query("SELECT COUNT(*) AS count, SUM(amount_micros) AS cost FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'")
            .bind(world.stable_key_id.unwrap().to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(ledger.try_get::<i64, _>("count").unwrap(), 1);
        assert_eq!(ledger.try_get::<i64, _>("cost").unwrap(), -1306788);
        pool.close().await;
    }
    let replay = create(world).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(replay.json::<Value>().await.unwrap(), value);
    let requests = world
        .mock
        .as_ref()
        .unwrap()
        .received_requests()
        .await
        .unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == "/v1/video/generations")
            .count(),
        1
    );
    let assets = world
        .asset_mock
        .as_ref()
        .unwrap()
        .received_requests()
        .await
        .unwrap();
    assert_eq!(
        assets.len(),
        if success {
            3
        } else if terminal == "missing-asset" {
            1
        } else {
            0
        }
    );
    assert!(
        assets
            .iter()
            .all(|r| r.headers.get("authorization").is_none())
    );
    for secret in [
        "private-tk-signature",
        "synthetic-tkmarket-key",
        "result_url",
        "provider_reference",
    ] {
        assert!(!value.to_string().contains(secret));
    }
}
