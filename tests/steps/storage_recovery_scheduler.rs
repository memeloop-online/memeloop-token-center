//! Historical coverage IO must not hold the foreground or fresh body lane.
use super::*;

pub(super) struct SchedulerState {
    historical: Option<Uuid>,
    receipt_prefix: String,
    release: Option<Box<dyn Fn() + Send + Sync>>,
}
impl Drop for SchedulerState {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

#[given("historical coverage receipt publication for this customer is unavailable")]
async fn fail_coverage_receipt(world: &mut TokenCenterWorld) {
    let state = world.state.as_ref().unwrap();
    let pool = AnyPool::connect(&state.config.database_url).await.unwrap();
    let tenant: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id=$1")
        .bind(world.stable_key_id.unwrap().to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    let prefix = format!(
        "/{}/tenants/{tenant}/semantic/v2/requests/",
        state.config.s3_bucket.as_ref().unwrap()
    );
    let recovery = world.recovery.as_mut().unwrap();
    recovery
        .proxy
        .fail_put_scope(&prefix, "-coverage.json", true);
    recovery.scheduler = Some(SchedulerState {
        historical: None,
        receipt_prefix: prefix,
        release: None,
    });
}

#[when("a historical request reaches blocked coverage recovery")]
async fn block_historical_coverage(world: &mut TokenCenterWorld) {
    call_model(world, "gpt-test".into()).await;
    assert_eq!(world.status, Some(StatusCode::OK));
    let state = world.state.as_ref().unwrap();
    let pool = AnyPool::connect(&state.config.database_url).await.unwrap();
    let (request, snapshot): (String, String) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let row = sqlx::query_as("SELECT request_id, semantic_snapshot_json FROM conversation_projection_outbox WHERE key_id=$1 AND projected_at IS NOT NULL AND lease_owner='semantic-coverage-v2' LIMIT 1")
                .bind(world.stable_key_id.unwrap().to_string()).fetch_optional(&pool).await.unwrap();
            if let Some(row) = row { break row; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("actual failed coverage receipt must retain a durable obligation");
    let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
    let publication: memeloop_token_center::archive::SemanticPublication =
        serde_json::from_value(snapshot["semantic_coverage_pending"].clone()).unwrap();
    assert_eq!(publication.request_id.to_string(), request);
    let locator = &publication.manifest.object_locator;
    assert!(
        locator.starts_with("tenants/"),
        "fixture must use its configured single S3 store"
    );
    let path = format!("/{}/{locator}", state.config.s3_bucket.as_ref().unwrap());
    let recovery = world.recovery.as_mut().unwrap();
    let gate = recovery.proxy.gate_get(&path);
    let scheduler = recovery.scheduler.as_mut().unwrap();
    scheduler.historical = Some(publication.request_id);
    recovery
        .proxy
        .fail_put_scope(&scheduler.receipt_prefix, "-coverage.json", false);
    let release = gate.clone();
    scheduler.release = Some(Box::new(move || release.release()));
    tokio::time::timeout(Duration::from_secs(15), gate.entered())
        .await
        .expect("real recovery must enter the exact historical object GET");
    pool.close().await;
}

async fn customer_requests(world: &TokenCenterWorld) -> Vec<Value> {
    let response = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn assert_originals(world: &TokenCenterWorld, id: Uuid) {
    wait_for_bound_proxy_archives(world, &[id]).await;
    let request = json!({"model":"gpt-test","messages":[{"role":"user","content":"hi"}]});
    let response = json!({"id":"chatcmpl-test","choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    for (side, expected) in [("request", request), ("response", response)] {
        let result = world
            .client
            .get(format!(
                "{}/self/v1/requests/{id}/archive/{side}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let actual = result.bytes().await.unwrap();
        assert_eq!(
            actual.as_ref(),
            serde_json::to_vec(&expected).unwrap().as_slice()
        );
    }
}

async fn assert_dialogue(world: &TokenCenterWorld, request: Uuid) {
    let pool = AnyPool::connect(&world.state.as_ref().unwrap().config.database_url)
        .await
        .unwrap();
    let cluster: String = sqlx::query_scalar(
        "SELECT cluster_id FROM conversation_observations WHERE request_id=$1 AND key_id=$2",
    )
    .bind(request.to_string())
    .bind(world.stable_key_id.unwrap().to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    let response = world
        .client
        .get(format!(
            "{}/self/v1/sessions/{cluster}/requests/{request}/dialogue",
            world.service_url
        ))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let dialogue: Value = response.json().await.unwrap();
    assert_eq!(dialogue["representation"], "session_body");
    assert_eq!(dialogue["request_id"], request.to_string());
    let turns: Vec<_> = dialogue["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| {
            let text: String = turn["parts"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect();
            (turn["role"].as_str().unwrap().to_owned(), text)
        })
        .collect();
    assert_eq!(
        turns,
        vec![
            ("user".into(), "hi".into()),
            ("assistant".into(), "hello".into())
        ]
    );
}

#[then("a fresh request succeeds with readable originals and one bill per request")]
async fn fresh_request_progresses(world: &mut TokenCenterWorld) {
    let historical = world
        .recovery
        .as_ref()
        .unwrap()
        .scheduler
        .as_ref()
        .unwrap()
        .historical
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(15),
        call_model(world, "gpt-test".into()),
    )
    .await
    .expect("historical IO must not hold new HTTP forwarding");
    assert_eq!(world.status, Some(StatusCode::OK));
    assert_eq!(world.response["choices"][0]["message"]["content"], "hello");
    let rows = customer_requests(world).await;
    assert_eq!(rows.len(), 2);
    let fresh = rows
        .iter()
        .map(|row| Uuid::parse_str(row["request_id"].as_str().unwrap()).unwrap())
        .find(|id| *id != historical)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = world.state.as_ref().unwrap();
            let pool = AnyPool::connect(&state.config.database_url).await.unwrap();
            let projected: Option<i64> = sqlx::query_scalar(
                "SELECT projected_at FROM conversation_projection_outbox WHERE request_id=$1",
            )
            .bind(fresh.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            pool.close().await;
            if projected.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fresh body projection must finish before historical GET is released");
    assert_originals(world, fresh).await;
    assert_dialogue(world, fresh).await;
    stats_contain_request(world, 2, 20).await;
}

#[when("historical coverage recovery is released")]
async fn release_history(world: &mut TokenCenterWorld) {
    world
        .recovery
        .as_mut()
        .unwrap()
        .scheduler
        .as_mut()
        .unwrap()
        .release
        .take()
        .unwrap()();
}

#[then("historical coverage recovers without another bill")]
async fn historical_recovered(world: &mut TokenCenterWorld) {
    let historical = world
        .recovery
        .as_ref()
        .unwrap()
        .scheduler
        .as_ref()
        .unwrap()
        .historical
        .unwrap();
    let pool = AnyPool::connect(&world.state.as_ref().unwrap().config.database_url)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let owner: Option<String> = sqlx::query_scalar(
                "SELECT lease_owner FROM conversation_projection_outbox WHERE request_id=$1",
            )
            .bind(historical.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if owner.as_deref() != Some("semantic-coverage-v2") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("historical coverage obligation must recover");
    pool.close().await;
    assert_originals(world, historical).await;
    assert_dialogue(world, historical).await;
    stats_contain_request(world, 2, 20).await;
}
