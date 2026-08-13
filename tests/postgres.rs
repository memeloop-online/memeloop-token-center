use memeloop_token_center::{
    db::{
        CreateGenerationJobInput, CreateKeyInput, CreateModelRouteInput, CreateServiceTokenInput,
        CreateUpstreamAccountInput, Database, FinishGenerationJobInput,
    },
    error::AppError,
    model::KeyPolicy,
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn postgres_budget_reservations_and_settlement_replays_are_serialized() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"postgres budget pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("postgres-budget-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "budget-key".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 10_000,
                    daily_budget: Some("0.001".to_owned()),
                    weekly_budget: Some("0.001".to_owned()),
                    lifetime_budget: Some("0.001".to_owned()),
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price("postgres-budget", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.reserve_usage(&key, &price, 0, 600).await
        }));
    }
    let mut reservation = None;
    let mut rejected = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(value) => reservation = Some(value),
            Err(AppError::QuotaExceeded) => rejected += 1,
            result => panic!("unexpected PostgreSQL budget result: {result:?}"),
        }
    }
    assert_eq!(rejected, 7);
    let reservation = reservation.unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.settle_usage(&reservation, 0, 700).await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), 700);
    }
    let usage_entries = database
        .list_account_ledger(issued.account_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "usage")
        .count();
    assert_eq!(usage_entries, 1);
}

#[tokio::test]
async fn postgres_credential_rotations_are_locked_and_idempotent() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper: &'static [u8] = b"postgres rotation pepper longer than thirty-two bytes";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("postgres-rotation-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "stable-key".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let idempotency_key = format!("postgres:{unique}:same-key-rotation");
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let idempotency_key = idempotency_key.clone();
        let key_id = issued.key_id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.rotate_key(key_id, &idempotency_key, pepper).await
        }));
    }
    let mut replays = Vec::new();
    for task in tasks {
        replays.push(task.await.unwrap().unwrap());
    }
    assert!(replays.iter().all(|value| {
        value.key_id == issued.key_id
            && value.account_id == issued.account_id
            && value.credential_generation == 2
            && value.key == replays[0].key
    }));

    let distinct_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
    let mut distinct_tasks = Vec::new();
    for index in 0..4 {
        let database = database.clone();
        let barrier = distinct_barrier.clone();
        let key_id = issued.key_id;
        let idempotency_key = format!("postgres:{unique}:distinct-key-rotation:{index}");
        distinct_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database.rotate_key(key_id, &idempotency_key, pepper).await
        }));
    }
    let mut generations = Vec::new();
    for task in distinct_tasks {
        generations.push(task.await.unwrap().unwrap().credential_generation);
    }
    generations.sort_unstable();
    assert_eq!(generations, vec![3, 4, 5, 6]);
    let replay_after_later_rotations = database
        .rotate_key(issued.key_id, &idempotency_key, pepper)
        .await
        .unwrap();
    assert_eq!(replay_after_later_rotations.credential_generation, 2);
    assert_eq!(replay_after_later_rotations.key, replays[0].key);

    let service = database
        .create_service_token(
            CreateServiceTokenInput {
                name: format!("postgres-rotation-service-{unique}"),
                scopes: vec!["keys:write".to_owned()],
                tenant_external_id: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let service_idempotency_key = format!("postgres:{unique}:same-service-rotation");
    let service_barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut service_tasks = Vec::new();
    for _ in 0..8 {
        let database = database.clone();
        let barrier = service_barrier.clone();
        let idempotency_key = service_idempotency_key.clone();
        let service_id = service.service_id;
        service_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .rotate_service_token(service_id, &idempotency_key, pepper)
                .await
        }));
    }
    let mut service_replays = Vec::new();
    for task in service_tasks {
        service_replays.push(task.await.unwrap().unwrap());
    }
    assert!(service_replays.iter().all(|value| {
        value.service_id == service.service_id
            && value.credential_generation == 2
            && value.token == service_replays[0].token
    }));
    assert!(
        database
            .rotate_key(issued.key_id, &service_idempotency_key, pepper)
            .await
            .is_err()
    );
}

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
                idempotency_key: None,
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
            billing_unit: price.billing_unit.clone(),
            micros_per_unit: price.micros_per_unit,
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
    let operator_stats = database.operator_stats(&tenant).await.unwrap();
    assert_eq!(operator_stats.summary.total_requests, 1);
    assert_eq!(operator_stats.summary.successful_requests, 1);
    assert_eq!(operator_stats.summary.total_cost, "0.25");
    assert_eq!(operator_stats.by_model[0].name, "video-test");
    let requests = database.list_all_requests(&tenant, 10).await.unwrap();
    assert_eq!(requests[0].protocol, "generation");
    assert_eq!(requests[0].status_code, Some(200));
    let key_detail = database
        .request_archive_refs(key.key_id, job_id)
        .await
        .unwrap();
    assert_eq!(key_detail.view.protocol, "generation");
    assert_eq!(key_detail.response_json, Some(result.clone()));
    let operator_detail = database
        .request_archive_refs_for_tenant(&tenant, job_id)
        .await
        .unwrap();
    assert_eq!(operator_detail.view.cost, "0.25");
    assert_eq!(operator_detail.response_json, Some(result));
    let events = database
        .request_events_after(&tenant, 0, None, 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_kind, "started");
    assert_eq!(events[1].event_kind, "finished");

    assert_eq!(
        database
            .grant(
                issued.account_id,
                Decimal::ONE,
                "postgres-subscription",
                &format!("postgres:{unique}:grant"),
            )
            .await
            .unwrap(),
        "1"
    );
    let reversal_idempotency = format!("postgres:{unique}:reversal");
    for _ in 0..2 {
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    &format!("postgres:{unique}:grant"),
                    "postgres-subscription-cancelled",
                    &reversal_idempotency,
                )
                .await
                .unwrap(),
            "1"
        );
    }
    database
        .plugin_kv_put("postgres-plugin", &format!("state/{unique}"), b"durable")
        .await
        .unwrap();
    assert_eq!(
        database
            .plugin_kv_get("postgres-plugin", &format!("state/{unique}"))
            .await
            .unwrap(),
        Some(b"durable".to_vec())
    );
}
