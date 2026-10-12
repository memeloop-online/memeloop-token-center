use super::*;

fn wan_input() -> Value {
    json!({
        "input": {"prompt": "a cat running on grass", "media": [
            {"type": "reference_image", "url": "https://example.test/reference.png"}
        ]},
        "parameters": {
            "resolution": "720P", "ratio": "16:9", "duration": 5,
            "prompt_extend": false, "watermark": false
        }
    })
}

#[given("the mock Wasu Wan upstream completes a reference image video")]
async fn mock_wan(world: &mut TokenCenterWorld) {
    let assets = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wan.mp4"))
        .and(header("range", "bytes=0-0"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-type", "video/mp4")
                .insert_header("content-range", "bytes 0-0/9")
                .set_body_bytes(b"w"),
        )
        .with_priority(1)
        .expect(1)
        .mount(&assets)
        .await;
    Mock::given(method("GET"))
        .and(path("/wan.mp4"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "video/mp4")
                .set_body_bytes(b"wan-video"),
        )
        .with_priority(10)
        .expect(1)
        .mount(&assets)
        .await;
    let server = world.mock.as_ref().unwrap();
    let mut expected = wan_input();
    expected["model"] = json!("wan3.0-video");
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/services/aigc/video-generation/video-synthesis",
        ))
        .and(header("authorization", "Bearer wasu-wan-secret"))
        .and(header("x-dashscope-async", "enable"))
        .and(body_partial_json(expected))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": {"task_id": "wan-task-1", "task_status": "PENDING"}
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/tasks/wan-task-1"))
        .and(header("authorization", "Bearer wasu-wan-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": {"task_id": "wan-task-1", "task_status": "SUCCEEDED",
                "video_url": format!("{}/wan.mp4?signature=private-wan-signature", assets.uri())},
            "usage": {"video_count": 1, "duration": 5, "output_video_duration": 5}
        })))
        .mount(server)
        .await;
    world.asset_mock = Some(assets);
}

#[given("the mock Wasu Wan upstream returns an unknown submit acknowledgement")]
async fn mock_wan_unknown_ack(world: &mut TokenCenterWorld) {
    let assets = MockServer::start().await;
    let server = world.mock.as_ref().unwrap();
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/services/aigc/video-generation/video-synthesis",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "output": {"task_status": "PENDING", "private": "must-not-persist"}
        })))
        .expect(1)
        .mount(server)
        .await;
    world.asset_mock = Some(assets);
}

#[when("the service creates a second-priced Wasu Wan video route and key")]
async fn create_wasu_wan_video_route_and_key(world: &mut TokenCenterWorld) {
    let mock_url = world.mock.as_ref().expect("mock server").uri();
    let mut provider_config = json!({
        "base_url": format!("{mock_url}/v1"),
        "network_scope": "private",
        "video_api": "wasu-wan-v1",
        "video_models": ["wan3.0-video"],
        "result_origins": [world.asset_mock.as_ref().expect("asset mock").uri()]
    });
    if world.provider_asset_repeatability_declared {
        provider_config["provider_asset_reads_repeatable"] =
            json!(world.provider_asset_reads_repeatable);
    }
    let response = world
        .client
        .post(format!("{}/internal/v1/upstreams", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "name": "wasu-wan-shared-account",
            "driver": "http-json",
            "config": provider_config,
            "credential": {"type": "api_key", "value": "wasu-wan-secret"}
        }))
        .send()
        .await
        .expect("create Wasu Wan upstream");
    let status = response.status();
    let response_body = response.text().await.expect("Wasu Wan account response");
    assert_eq!(status, StatusCode::CREATED, "{response_body}");
    let account: Value = serde_json::from_str(&response_body).expect("Wasu Wan account JSON");
    let response = world
        .client
        .post(format!("{}/internal/v1/model-routes", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "public_model": "wasu-wan-video-public",
            "upstream_account_id": account["id"],
            "upstream_model": "wan3.0-video",
            "protocol": "generation",
            "custom_model_confirmed": true
        }))
        .send()
        .await
        .expect("create Wasu Wan video route");
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = world
        .client
        .post(format!(
            "{}/internal/v1/generation-prices/USD/wasu-wan-video-public",
            world.service_url
        ))
        .bearer_auth("test-service-token")
        .json(&json!({"billing_unit": "second", "price_per_unit": "0.1"}))
        .send()
        .await
        .expect("create Wasu Wan video price");
    assert_eq!(response.status(), StatusCode::OK);
    let response = world
        .client
        .post(format!("{}/internal/v1/keys", world.service_url))
        .bearer_auth("test-service-token")
        .json(&json!({
            "principal_external_id": "wasu-wan-video-user",
            "alias": "wasu-wan-video",
            "currency": "USD",
            "initial_balance": "10",
            "policy": {}
        }))
        .send()
        .await
        .expect("create Wasu Wan video key");
    assert_eq!(response.status(), StatusCode::CREATED);
    let key: Value = response.json().await.expect("Wasu Wan video key JSON");
    world.stable_key_id = key["key_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    world.current_key = key["key"].as_str().expect("Wasu Wan key").to_owned();
    grant_fixture_model(
        world,
        "default",
        &key,
        "wasu-wan-video-public",
        "generation",
    )
    .await;
}

#[when("the client creates and replays a Wasu Wan reference image generation")]
async fn create_wan(world: &mut TokenCenterWorld) {
    stop_test_worker(world).await;
    let body = json!({"model": "wasu-wan-video-public", "input": wan_input()});
    let first = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "wan-single-task")
        .json(&body)
        .send()
        .await
        .unwrap();
    world.status = Some(first.status());
    world.response = first.json().await.unwrap();
    world.generation_job_id = world.response["job_id"]
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok());
    let replay = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "wan-single-task")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(replay.json::<Value>().await.unwrap(), world.response);
    let mut changed = body;
    changed["input"]["input"]["prompt"] = json!("another prompt");
    let mismatch = world
        .client
        .post(format!("{}/v1/videos/generations", world.service_url))
        .bearer_auth(&world.current_key)
        .header("idempotency-key", "wan-single-task")
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    restart_test_worker(world);
}

#[then("the Wasu Wan video is delivered through MTC once with seconds billing")]
async fn wan_succeeds(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.unwrap();
    for _ in 0..80 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        if value["status"] == "succeeded" {
            assert_eq!(value["billing_unit"], "second");
            assert_eq!(value["estimated_units"], 5);
            assert_eq!(value["billed_units"], 5);
            assert_eq!(value["cost"], "0.5");
            let public = value.to_string();
            for secret in [
                "private-wan-signature",
                "wasu-wan-secret",
                "wan.mp4",
                "video_url",
            ] {
                assert!(!public.contains(secret));
            }
            let asset_id = value["assets"][0]["asset_id"].as_str().unwrap();
            let asset_path = format!(
                "{}/self/v1/generations/{job_id}/assets/{asset_id}",
                world.service_url
            );
            let other = world
                .client
                .post(format!("{}/internal/v1/keys", world.service_url))
                .bearer_auth("test-service-token")
                .json(&json!({
                    "principal_external_id": "wan-other", "alias": "wan-other",
                    "currency": "USD", "initial_balance": "1", "policy": {}
                }))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(
                world
                    .client
                    .get(&asset_path)
                    .bearer_auth(other["key"].as_str().unwrap())
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
            let asset = world
                .client
                .get(&asset_path)
                .bearer_auth(&world.current_key)
                .send()
                .await
                .unwrap();
            assert_eq!(asset.status(), StatusCode::OK);
            assert_eq!(asset.bytes().await.unwrap(), b"wan-video".as_slice());
            let requests = world
                .mock
                .as_ref()
                .unwrap()
                .received_requests()
                .await
                .unwrap();
            let submits: Vec<_> = requests
                .iter()
                .filter(|request| {
                    request.url.path() == "/api/v1/services/aigc/video-generation/video-synthesis"
                })
                .collect();
            assert_eq!(submits.len(), 1);
            let mut expected = wan_input();
            expected["model"] = json!("wan3.0-video");
            assert_eq!(
                serde_json::from_slice::<Value>(&submits[0].body).unwrap(),
                expected
            );
            assert!(
                requests
                    .iter()
                    .any(|request| request.method.as_str() == "GET"
                        && request.url.path() == "/api/v1/tasks/wan-task-1")
            );
            let asset_requests = world
                .asset_mock
                .as_ref()
                .unwrap()
                .received_requests()
                .await
                .unwrap();
            assert_eq!(asset_requests.len(), 2);
            assert!(
                asset_requests
                    .iter()
                    .all(|request| request.headers.get("authorization").is_none())
            );
            assert_generation_stats(world, "wasu-wan-video-public", "0.5").await;
            world.response = value;
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("Wan mock task did not complete");
}

#[then("the Wasu Wan unknown acknowledgement is quarantined without resubmission")]
async fn wan_unknown_ack(world: &mut TokenCenterWorld) {
    let job_id = world.generation_job_id.unwrap();
    for _ in 0..80 {
        let value = world
            .client
            .get(format!(
                "{}/self/v1/generations/{job_id}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        if value["error_code"] == "shutdown_delivery_unknown" {
            assert!(value["upstream_job_id"].is_null());
            tokio::time::sleep(Duration::from_millis(500)).await;
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
                    .filter(|request| request.method.as_str() == "POST")
                    .count(),
                1
            );
            assert!(
                world
                    .asset_mock
                    .as_ref()
                    .unwrap()
                    .received_requests()
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(!value.to_string().contains("must-not-persist"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("Wan unknown ACK was not quarantined");
}
