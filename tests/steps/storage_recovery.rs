//! Version acceptance through the customer HTTP surface and real object storage.
use super::*;
#[path = "../support/recovery_object_proxy.rs"]
mod recovery_object_proxy;
use recovery_object_proxy::RecoveryObjectProxy;
#[path = "../support/postgres_isolation.rs"]
mod postgres_isolation;

pub(super) struct RecoveryState {
    pub(super) proxy: RecoveryObjectProxy,
    failed_prefix: Option<String>,
    isolated: postgres_isolation::IsolatedPostgres,
}

#[given("a recovery service backed by PostgreSQL and real object storage")]
pub(super) async fn start_recovery_service(world: &mut TokenCenterWorld) {
    let database = std::env::var("MTC_TEST_POSTGRES_URL").expect("PostgreSQL acceptance URL");
    let endpoint = std::env::var("MTC_RUSTFS_TEST_ENDPOINT").expect("real S3 acceptance endpoint");
    let proxy = RecoveryObjectProxy::start(&endpoint).await;
    let mock = MockServer::start().await;
    let isolated = postgres_isolation::IsolatedPostgres::create(&database).await;
    let mut config = Config::for_test(isolated.url().to_owned());
    config.archive_backend = memeloop_token_center::config::ArchiveBackend::S3;
    config.upstream_openai_url = Some(mock.uri());
    config.s3_endpoint = Some(proxy.endpoint().to_owned());
    config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
    config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    config.archive_spool_compression_enabled = true;
    config.archive_object_compression_enabled = true;
    let state = AppState::initialize(config)
        .await
        .expect("initialize recovery application");
    start_test_http_service(world, state, mock).await;
    world.recovery = Some(RecoveryState {
        proxy,
        failed_prefix: None,
        isolated,
    });
}

#[given("object publication for this customer temporarily returns service unavailable")]
async fn fail_customer_publication(world: &mut TokenCenterWorld) {
    let state = world.state.as_ref().unwrap();
    let pool = AnyPool::connect(&state.config.database_url).await.unwrap();
    let tenant: String = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1")
        .bind(world.stable_key_id.unwrap().to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    let prefix = format!(
        "/{}/tenants/{tenant}/raw/v1/",
        state.config.s3_bucket.as_ref().unwrap()
    );
    let recovery = world.recovery.as_mut().unwrap();
    recovery.proxy.fail_put_prefix(&prefix, true);
    recovery.failed_prefix = Some(prefix);
}

#[then("the original HTTP response succeeds while publication has failed")]
async fn response_survives_publication_failure(world: &mut TokenCenterWorld) {
    assert_eq!(world.status, Some(StatusCode::OK));
    assert_eq!(world.response["choices"][0]["message"]["content"], "hello");
    tokio::time::timeout(Duration::from_secs(10), async {
        while world.recovery.as_ref().unwrap().proxy.failed_puts() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real object PUT must fail at least once");
}

#[when("object publication recovers")]
async fn recover_publication(world: &mut TokenCenterWorld) {
    let recovery = world.recovery.as_mut().unwrap();
    let prefix = recovery
        .failed_prefix
        .take()
        .expect("publication fault installed");
    recovery.proxy.fail_put_prefix(&prefix, false);
}

#[then("both customer archive bodies recover exactly and billing occurs once")]
async fn exact_originals_and_once(world: &mut TokenCenterWorld) {
    let rows: Value = world
        .client
        .get(format!("{}/self/v1/requests", world.service_url))
        .bearer_auth(&world.current_key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let id = Uuid::parse_str(rows[0]["request_id"].as_str().unwrap()).unwrap();
    wait_for_bound_proxy_archives(world, &[id]).await;
    let expected_request = json!({"model":"gpt-test","messages":[{"role":"user","content":"hi"}]});
    let expected_response = json!({"id":"chatcmpl-test","choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    for (side, expected) in [
        ("request", expected_request),
        ("response", expected_response),
    ] {
        let response = world
            .client
            .get(format!(
                "{}/self/v1/requests/{id}/archive/{side}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let actual = response.bytes().await.unwrap();
        let expected = serde_json::to_vec(&expected).unwrap();
        assert_eq!(actual.len(), expected.len());
        assert_eq!(blake3::hash(&actual), blake3::hash(&expected));
        assert_eq!(actual.as_ref(), expected.as_slice());
    }
    stats_contain_request(world, 1, 10).await;
    assert_eq!(world.response["summary"]["total_cost"], "0.00001");
    let pool = AnyPool::connect(&world.state.as_ref().unwrap().config.database_url)
        .await
        .unwrap();
    let (count, amount): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(amount_micros), 0) FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'"
    ).bind(world.stable_key_id.unwrap().to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!((count, amount), (1, -10));
    pool.close().await;
}

pub(super) async fn cleanup(world: &mut TokenCenterWorld) {
    if let Some(recovery) = world.recovery.take() {
        if let Some(task) = world.server_task.take() {
            task.abort();
            let _ = task.await;
        }
        recovery.isolated.cleanup().await;
    }
}
