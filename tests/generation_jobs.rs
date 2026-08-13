use memeloop_token_center::{
    db::{
        CreateGenerationJobInput, CreateGenerationJobResult, CreateKeyInput,
        CreateUpstreamAccountInput, Database, FinishGenerationJobInput, GenerationJobIdempotency,
        StatsFilter, unix_millis,
    },
    error::AppError,
    generation::generation_request_hash,
    model::{AuthenticatedKey, GenerationPrice, KeyPolicy, UsageReservation},
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

const PEPPER: &[u8] = b"generation test pepper longer than thirty-two bytes";

async fn fixture() -> (
    tempfile::TempDir,
    Database,
    AuthenticatedKey,
    Uuid,
    GenerationPrice,
) {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let tenant = format!("generation-test-{}", Uuid::now_v7());
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".to_owned(),
                alias: "generation-test".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec!["image-test".to_owned()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap();
    let upstream = database
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant,
                name: "generation-test-upstream".to_owned(),
                driver: "comfyui".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:8188"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let price = database
        .upsert_generation_price("image-test", "USD", "job", Decimal::new(25, 2))
        .await
        .unwrap();
    (directory, database, key, upstream.id, price)
}

async fn reserve(
    database: &Database,
    key: &AuthenticatedKey,
    price: &GenerationPrice,
) -> UsageReservation {
    database
        .reserve_usage(key, &price.reservation_price().unwrap(), 0, 1)
        .await
        .unwrap()
}

fn input(
    key: &AuthenticatedKey,
    upstream_account_id: Uuid,
    reservation: UsageReservation,
    price: &GenerationPrice,
) -> CreateGenerationJobInput {
    CreateGenerationJobInput {
        job_id: Uuid::now_v7(),
        key: key.clone(),
        upstream_account_id,
        reservation,
        public_model: "image-test".to_owned(),
        upstream_model: "workflow-v1".to_owned(),
        driver: "comfyui".to_owned(),
        request_object: "objects/blake3/generation-test".to_owned(),
        estimated_units: 1,
        billing_unit: price.billing_unit.clone(),
        micros_per_unit: price.micros_per_unit,
    }
}

#[tokio::test]
async fn generation_idempotency_replays_one_job_and_refunds_duplicate_reservations() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let request = json!({"prompt": "a small orange cat"});
    let idempotency = GenerationJobIdempotency {
        key: "generation-replay-1".to_owned(),
        request_hash: generation_request_hash("image-test", &request),
    };
    let first = database
        .create_generation_job_idempotent(
            input(
                &key,
                upstream_id,
                reserve(&database, &key, &price).await,
                &price,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let first_job_id = match first {
        CreateGenerationJobResult::Created(job) => job.job_id,
        CreateGenerationJobResult::Replayed(_) => panic!("first request was unexpectedly replayed"),
    };
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
    assert_eq!(
        database
            .generation_job_by_idempotency(key.key_id, &idempotency)
            .await
            .unwrap()
            .unwrap()
            .job_id,
        first_job_id
    );

    let replay = database
        .create_generation_job_idempotent(
            input(
                &key,
                upstream_id,
                reserve(&database, &key, &price).await,
                &price,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    match replay {
        CreateGenerationJobResult::Replayed(job) => assert_eq!(job.job_id, first_job_id),
        CreateGenerationJobResult::Created(_) => panic!("replay created a second job"),
    }
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );

    let mismatched = GenerationJobIdempotency {
        key: idempotency.key.clone(),
        request_hash: generation_request_hash("image-test", &json!({"prompt": "a dog"})),
    };
    assert!(matches!(
        database
            .create_generation_job_idempotent(
                input(
                    &key,
                    upstream_id,
                    reserve(&database, &key, &price).await,
                    &price,
                ),
                Some(&mismatched),
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
    assert_eq!(
        database
            .list_generation_jobs(key.key_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn queued_cancellation_is_idempotent_and_refunds_in_one_transaction() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );

    let cancelled = database
        .cancel_queued_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(cancelled.error_code.as_deref(), Some("cancelled_by_user"));
    assert_eq!(cancelled.cost, "0");
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "10"
    );

    let replayed = database
        .cancel_queued_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(replayed.status, "cancelled");
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "10"
    );
    let stats = database.stats(key.key_id).await.unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.failed_requests, 1);
}

#[tokio::test]
async fn terminal_generation_stats_are_idempotent_and_keep_exact_filters() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("generation-stats-worker")
        .await
        .unwrap()
        .expect("queued job");
    let cost_micros = database.settle_usage(&reservation, 0, 1).await.unwrap();
    database
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id: "generation-stats-worker",
            status: "succeeded",
            billed_units: 1,
            cost_micros,
            result: None,
            error_code: None,
        })
        .await
        .unwrap();

    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "generation-stats-worker",
                status: "succeeded",
                billed_units: 1,
                cost_micros,
                result: None,
                error_code: None,
            })
            .await,
        Err(AppError::NotFound)
    ));
    let finished = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    let completed_at = finished.completed_at.unwrap();
    let duration_ms = completed_at.saturating_sub(job.created_at);

    let stats = database.stats(key.key_id).await.unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.successful_requests, 1);

    let exact = database
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(job.created_at),
                to_created_at: Some(unix_millis()),
                model: Some("image-test".into()),
                protocol: Some("generation".into()),
                status: Some("success".into()),
                upstream_account_id: Some(upstream_id),
                min_duration_ms: Some(duration_ms),
                max_duration_ms: Some(duration_ms),
                min_cost_micros: Some(cost_micros),
                max_cost_micros: Some(cost_micros),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(exact.summary.total_requests, 1);
    assert_eq!(exact.summary.total_cost, "0.25");

    let excludes_first_millisecond = database
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(job.created_at.saturating_add(1)),
                to_created_at: Some(unix_millis()),
                protocol: Some("generation".into()),
                ..StatsFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(excludes_first_millisecond.summary.total_requests, 0);
}

#[tokio::test]
async fn an_active_generation_lease_blocks_client_cancellation() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    let claimed = database
        .claim_generation_job("generation-test-worker")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job_id, job.job_id);
    database
        .renew_generation_lease(job.job_id, "generation-test-worker")
        .await
        .unwrap();
    assert!(matches!(
        database
            .cancel_queued_generation_job(key.key_id, job.job_id)
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
}

#[tokio::test]
async fn a_queued_job_keeps_the_price_snapshot_from_submission_time() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    database
        .upsert_generation_price("image-test", "USD", "job", Decimal::new(75, 2))
        .await
        .unwrap();

    let claimed = database
        .claim_generation_job("generation-price-snapshot-worker")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.job_id, job.job_id);
    assert_eq!(
        claimed.reservation.output_micros_per_million,
        price.micros_per_unit * 1_000_000
    );
}
