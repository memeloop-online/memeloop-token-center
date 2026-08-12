use memeloop_token_center::{
    db::{
        CreateGenerationJobInput, CreateKeyInput, CreateModelRouteInput,
        CreateUpstreamAccountInput, Database, FinishGenerationJobInput,
    },
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn postgres_migrations_queue_aggregates_and_events_work_together() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    database.maintain_partitions().await.unwrap();

    let unique = Uuid::now_v7();
    let tenant = format!("postgres-test-{unique}");
    let pepper = b"postgres integration pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".to_owned(),
                alias: "postgres-integration".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["video-test".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let account = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: "postgres-comfy".to_owned(),
                driver: "comfyui".to_owned(),
                config: json!({"base_url": "http://comfy.example.test", "api_prefix": ""}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
            },
            pepper,
        )
        .await
        .unwrap();
    database
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: tenant.clone(),
            public_model: "video-test".to_owned(),
            upstream_account_id: account.id,
            upstream_model: "workflow-test".to_owned(),
            protocol: "generation".to_owned(),
            priority: 0,
        })
        .await
        .unwrap();
    let price = database
        .upsert_generation_price("video-test", "USD", "job", Decimal::new(25, 2))
        .await
        .unwrap();
    let reservation = database
        .reserve_usage(&key, &price.reservation_price().unwrap(), 0, 1)
        .await
        .unwrap();
    let job_id = Uuid::now_v7();
    database
        .create_generation_job(CreateGenerationJobInput {
            job_id,
            key: key.clone(),
            upstream_account_id: account.id,
            reservation: reservation.clone(),
            public_model: "video-test".to_owned(),
            upstream_model: "workflow-test".to_owned(),
            driver: "comfyui".to_owned(),
            request_object: "objects/blake3/test".to_owned(),
            estimated_units: 1,
        })
        .await
        .unwrap();
    let claimed = database
        .claim_generation_job("postgres-integration-worker")
        .await
        .unwrap()
        .expect("queued generation job");
    assert_eq!(claimed.job_id, job_id);
    let cost = database.settle_usage(&reservation, 0, 1).await.unwrap();
    let result = json!({"archive_objects": ["objects/blake3/test-result"]});
    database
        .finish_generation_job(FinishGenerationJobInput {
            job_id,
            worker_id: "postgres-integration-worker",
            status: "succeeded",
            billed_units: 1,
            cost_micros: cost,
            result: Some(&result),
            error_code: None,
        })
        .await
        .unwrap();

    let stats = database.stats(key.key_id).await.unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);
    assert_eq!(stats.summary.total_cost, "0.25");
    assert_eq!(stats.by_model[0].name, "video-test");
    let requests = database.list_all_requests(&tenant, 10).await.unwrap();
    assert_eq!(requests[0].protocol, "generation");
    assert_eq!(requests[0].status_code, Some(200));
    let events = database
        .request_events_after(&tenant, 0, None, 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_kind, "started");
    assert_eq!(events[1].event_kind, "finished");
}
