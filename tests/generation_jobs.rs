use memeloop_token_center::{
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, ArchiveStagingWriteLease,
        BeginArchiveStagingInput, BeginArchiveStagingResult,
    },
    db::{
        AttachGenerationJobResult, CreateGenerationJobInput, CreateGenerationJobResult,
        CreateKeyInput, CreateUpstreamAccountInput, Database, FinishGenerationJobInput,
        GenerationJobIdempotency, StartGenerationJobInput, StatsFilter, unix_millis,
    },
    error::AppError,
    generation::generation_request_hash,
    model::{
        ArchivedGenerationAsset, AuthenticatedKey, GenerationPrice, GenerationStagedAssets,
        KeyPolicy, UsageReservation,
    },
    provider::UpstreamCredential,
};
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::{AnyPool, Row};
use uuid::Uuid;

const PEPPER: &[u8] = b"generation test pepper longer than thirty-two bytes";

async fn fixture() -> (
    tempfile::TempDir,
    Database,
    AuthenticatedKey,
    Uuid,
    GenerationPrice,
) {
    fixture_with_currency("USD", Decimal::new(25, 2)).await
}

async fn fixture_with_currency(
    currency: &str,
    unit_price: Decimal,
) -> (
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
                currency: currency.to_owned(),
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
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let price = database
        .upsert_generation_price("image-test", currency, "job", unit_price)
        .await
        .unwrap();
    (directory, database, key, upstream.id, price)
}

#[tokio::test]
async fn generation_terminal_settlement_preserves_the_keys_non_usd_currency() {
    let (_directory, database, key, upstream_id, price) =
        fixture_with_currency("CNY", Decimal::new(123, 2)).await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("generation-cny-worker")
        .await
        .unwrap()
        .expect("queued CNY generation");
    let asset = archived_asset(job.job_id, 0);
    assert_eq!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "generation-cny-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: std::slice::from_ref(&asset),
                staged_assets: None,
            })
            .await
            .unwrap(),
        1_230_000
    );
    let finished = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(finished.cost, "1.23");
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "8.77"
    );
    let usage = database
        .list_account_ledger(key.account_id, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
        .expect("CNY usage ledger entry");
    assert_eq!(usage.currency, "CNY");
    assert_eq!(usage.amount, "-1.23");
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

fn start_input(
    key: &AuthenticatedKey,
    upstream_account_id: Uuid,
    price: &GenerationPrice,
    job_id: Uuid,
    model: &str,
    request_hash: &str,
) -> StartGenerationJobInput {
    StartGenerationJobInput {
        job_id,
        key: key.clone(),
        upstream_account_id,
        reservation_price: price.reservation_price().unwrap(),
        public_model: model.to_owned(),
        upstream_model: "workflow-v1".to_owned(),
        driver: "comfyui".to_owned(),
        request_hash: request_hash.to_owned(),
        estimated_units: 1,
        billing_unit: price.billing_unit.clone(),
        micros_per_unit: price.micros_per_unit,
    }
}

fn archived_asset(job_id: Uuid, index: i64) -> ArchivedGenerationAsset {
    ArchivedGenerationAsset {
        asset_id: Uuid::now_v7(),
        index,
        object_locator: format!("objects/blake3/{job_id}-{index}"),
        mime_type: "image/png".to_owned(),
        size_bytes: 17,
        filename: format!("result-{index}.png"),
    }
}

async fn generation_staging_lease(
    database: &Database,
    job_id: Uuid,
    purpose: ArchiveStagingPurpose,
    attempt_id: Uuid,
) -> ArchiveStagingWriteLease {
    let result = database
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key: ArchiveStagingKey::new(
                ArchiveStagingOwner::GenerationJob(job_id),
                purpose,
                attempt_id,
            )
            .unwrap(),
            // Tests use a fixed non-secret typed intent. Production uses only
            // random fencing material and typed identities here as well.
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::now_v7(),
            lease_owner: ArchiveStagingLeaseOwner::new("generation-staging-test").unwrap(),
        })
        .await
        .unwrap();
    match result {
        BeginArchiveStagingResult::Created(lease) => lease,
        other => panic!("expected a new staging lease, got {other:?}"),
    }
}

#[tokio::test]
async fn generation_idempotency_replays_before_any_duplicate_reservation() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let request = json!({"prompt": "a small orange cat"});
    let idempotency = GenerationJobIdempotency {
        key: "generation-replay-1".to_owned(),
        request_hash: generation_request_hash("image-test", &request),
    };
    let first = database
        .start_generation_job(
            start_input(
                &key,
                upstream_id,
                &price,
                Uuid::now_v7(),
                "image-test",
                &idempotency.request_hash,
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
        .start_generation_job(
            start_input(
                &key,
                upstream_id,
                &price,
                Uuid::now_v7(),
                "image-test",
                &idempotency.request_hash,
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
            .start_generation_job(
                start_input(
                    &key,
                    upstream_id,
                    &price,
                    Uuid::now_v7(),
                    "image-test",
                    &mismatched.request_hash,
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
async fn staged_request_attach_binds_atomically_and_exactly_replays() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let request_hash = generation_request_hash("image-test", &json!({"prompt": "tracked"}));
    let job_id = Uuid::now_v7();
    let started = database
        .start_generation_job(
            start_input(
                &key,
                upstream_id,
                &price,
                job_id,
                "image-test",
                &request_hash,
            ),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(started, CreateGenerationJobResult::Created(_)));
    let lease = generation_staging_lease(
        &database,
        job_id,
        ArchiveStagingPurpose::Request,
        Uuid::now_v7(),
    )
    .await;
    let locator = format!("{}/request.json", lease.key.canonical_prefix());
    assert!(matches!(
        database
            .attach_generation_job_request_staged(
                key.key_id,
                job_id,
                &request_hash,
                &locator,
                &lease,
            )
            .await
            .unwrap(),
        AttachGenerationJobResult::Attached(_)
    ));
    assert_eq!(
        database
            .archive_staging_attempt(lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert!(matches!(
        database
            .attach_generation_job_request_staged(
                key.key_id,
                job_id,
                &request_hash,
                &locator,
                &lease,
            )
            .await
            .unwrap(),
        AttachGenerationJobResult::Attached(_)
    ));
}

#[tokio::test]
async fn staged_request_bind_failure_rolls_back_the_job_locator() {
    let (directory, database, key, upstream_id, price) = fixture().await;
    let request_hash = generation_request_hash("image-test", &json!({"prompt": "rollback"}));
    let job_id = Uuid::now_v7();
    database
        .start_generation_job(
            start_input(
                &key,
                upstream_id,
                &price,
                job_id,
                "image-test",
                &request_hash,
            ),
            None,
        )
        .await
        .unwrap();
    let lease = generation_staging_lease(
        &database,
        job_id,
        ArchiveStagingPurpose::Request,
        Uuid::now_v7(),
    )
    .await;
    let locator = format!("{}/request.json", lease.key.canonical_prefix());
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation.db").display()
    );
    let inspection = AnyPool::connect(&database_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER generation_bind_failure BEFORE UPDATE OF state ON archive_staging_attempts WHEN NEW.state = 'bound' BEGIN SELECT RAISE(ABORT, 'forced bind failure'); END",
    )
    .execute(&inspection)
    .await
    .unwrap();
    assert!(
        database
            .attach_generation_job_request_staged(
                key.key_id,
                job_id,
                &request_hash,
                &locator,
                &lease,
            )
            .await
            .is_err()
    );
    assert_eq!(
        database
            .generation_job(key.key_id, job_id)
            .await
            .unwrap()
            .status,
        "preparing"
    );
    assert_eq!(
        database
            .archive_staging_attempt(lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing
    );
}

#[tokio::test]
async fn staged_assets_bind_with_manifest_and_terminal_failure_releases_them() {
    let (directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation.db").display()
    );
    let inspection = AnyPool::connect(&database_url).await.unwrap();
    sqlx::query(
        "UPDATE generation_jobs SET status = 'running', upstream_job_id = 'upstream-job', lease_owner = 'winner', lease_expires_at = $1 WHERE id = $2",
    )
    .bind(unix_millis().saturating_add(60_000))
    .bind(job.job_id.to_string())
    .execute(&inspection)
    .await
    .unwrap();

    let loser_attempt = Uuid::now_v7();
    let loser_lease = generation_staging_lease(
        &database,
        job.job_id,
        ArchiveStagingPurpose::Assets,
        loser_attempt,
    )
    .await;
    let loser_manifest = GenerationStagedAssets {
        attempt_nonce: loser_attempt,
        billed_units: 1,
        assets: vec![ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator: format!("{}/asset-0", loser_lease.key.canonical_prefix()),
            mime_type: "image/png".to_owned(),
            size_bytes: 7,
            filename: "loser.png".to_owned(),
        }],
    };
    assert!(
        !database
            .save_generation_staged_assets_staged(
                job.job_id,
                "loser",
                &loser_manifest,
                &loser_lease,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .archive_staging_attempt(loser_attempt)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    );

    let winner_attempt = Uuid::now_v7();
    let winner_lease = generation_staging_lease(
        &database,
        job.job_id,
        ArchiveStagingPurpose::Assets,
        winner_attempt,
    )
    .await;
    let winner_manifest = GenerationStagedAssets {
        attempt_nonce: winner_attempt,
        billed_units: 1,
        assets: vec![ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator: format!("{}/asset-0", winner_lease.key.canonical_prefix()),
            mime_type: "image/png".to_owned(),
            size_bytes: 11,
            filename: "winner.png".to_owned(),
        }],
    };
    assert!(
        database
            .save_generation_staged_assets_staged(
                job.job_id,
                "winner",
                &winner_manifest,
                &winner_lease,
            )
            .await
            .unwrap()
    );
    assert!(
        database
            .save_generation_staged_assets_staged(
                job.job_id,
                "winner",
                &winner_manifest,
                &winner_lease,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .archive_staging_attempt(winner_attempt)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    database
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id: "winner",
            status: "failed",
            billed_units: 0,
            error_code: Some("generation_staging_lost"),
            assets: &[],
            staged_assets: Some(&winner_manifest),
        })
        .await
        .unwrap();
    assert_eq!(
        database
            .archive_staging_attempt(winner_attempt)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    );
    let manifest: Option<String> =
        sqlx::query_scalar("SELECT staged_assets_json FROM generation_jobs WHERE id = $1")
            .bind(job.job_id.to_string())
            .fetch_one(&inspection)
            .await
            .unwrap();
    assert!(manifest.is_none());
    let assets: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM generation_assets WHERE job_id = $1")
            .bind(job.job_id.to_string())
            .fetch_one(&inspection)
            .await
            .unwrap();
    assert_eq!(assets, 0);
}

struct AtomicGenerationFixture {
    database: Database,
    key: AuthenticatedKey,
    upstream_id: Uuid,
    price: GenerationPrice,
    model: String,
}

#[derive(Debug, PartialEq, Eq)]
struct AtomicGenerationState {
    reservations: i64,
    jobs: i64,
    started_events: i64,
    event_locators: i64,
    rate_requests: i64,
    rate_tokens: i64,
    active_requests: i64,
    available_micros: i64,
    reserved_micros: i64,
    budget_reserved_micros: i64,
}

async fn atomic_generation_fixture(database_url: &str) -> AtomicGenerationFixture {
    let database = Database::connect_with_max(database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let tenant = format!("generation-atomic-{unique}");
    let model = format!("generation-atomic-model-{unique}");
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.clone(),
                principal_external_id: "member".to_owned(),
                alias: "generation-atomic".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    allowed_models: vec![model.clone()],
                    requests_per_minute: 60,
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
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
                name: format!("generation-atomic-upstream-{unique}"),
                driver: "comfyui".to_owned(),
                config: json!({"base_url": "http://127.0.0.1:8188"}),
                credential: UpstreamCredential::None,
                oauth_session_id: None,
                oauth_driver: None,
                oauth_refresh_url: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let price = database
        .upsert_generation_price(&model, "USD", "job", Decimal::new(25, 2))
        .await
        .unwrap();
    AtomicGenerationFixture {
        database,
        key,
        upstream_id: upstream.id,
        price,
        model,
    }
}

async fn atomic_generation_state(pool: &AnyPool, key: &AuthenticatedKey) -> AtomicGenerationState {
    let reservations =
        sqlx::query("SELECT COUNT(*) AS count FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
            .get("count");
    let jobs = sqlx::query("SELECT COUNT(*) AS count FROM generation_jobs WHERE key_id = $1")
        .bind(key.key_id.to_string())
        .fetch_one(pool)
        .await
        .unwrap()
        .get("count");
    let started_events = sqlx::query(
        "SELECT COUNT(*) AS count FROM request_events WHERE key_id = $1 AND event_kind = 'started' AND protocol = 'generation'",
    )
    .bind(key.key_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
    .get("count");
    let event_locators =
        sqlx::query("SELECT COUNT(*) AS count FROM request_event_locators WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
            .get("count");
    let rate = sqlx::query(
        "SELECT CAST(COALESCE(SUM(requests), 0) AS BIGINT) AS requests, CAST(COALESCE(SUM(tokens), 0) AS BIGINT) AS tokens FROM rate_limit_windows WHERE key_id = $1",
    )
    .bind(key.key_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap();
    let active_requests = sqlx::query(
        "SELECT COUNT(*) AS active_requests FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'",
    )
    .bind(key.key_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
    .get("active_requests");
    let credit =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(key.account_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    let budget_reserved_micros =
        sqlx::query("SELECT reserved_micros FROM key_budget_state WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
            .get("reserved_micros");
    AtomicGenerationState {
        reservations,
        jobs,
        started_events,
        event_locators,
        rate_requests: rate.get("requests"),
        rate_tokens: rate.get("tokens"),
        active_requests,
        available_micros: credit.get("available_micros"),
        reserved_micros: credit.get("reserved_micros"),
        budget_reserved_micros,
    }
}

async fn install_generation_event_failure_trigger(
    pool: &AnyPool,
    postgres: bool,
    request_id: Uuid,
) -> (String, Option<String>) {
    let suffix = request_id.simple().to_string();
    let trigger = format!("generation_atomic_fail_{suffix}");
    if postgres {
        let function = format!("generation_atomic_fail_fn_{suffix}");
        sqlx::query(&format!(
            "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN IF NEW.request_id = '{}' THEN RAISE EXCEPTION 'forced generation event failure'; END IF; RETURN NEW; END $body$",
            request_id
        ))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(&format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON request_events FOR EACH ROW EXECUTE FUNCTION {function}()"
        ))
        .execute(pool)
        .await
        .unwrap();
        (trigger, Some(function))
    } else {
        sqlx::query(&format!(
            "CREATE TRIGGER {trigger} BEFORE INSERT ON request_events WHEN NEW.request_id = '{}' BEGIN SELECT RAISE(ABORT, 'forced generation event failure'); END",
            request_id
        ))
        .execute(pool)
        .await
        .unwrap();
        (trigger, None)
    }
}

async fn remove_generation_event_failure_trigger(
    pool: &AnyPool,
    trigger: &str,
    function: Option<&str>,
) {
    sqlx::query(&format!("DROP TRIGGER {trigger} ON request_events"))
        .execute(pool)
        .await
        .unwrap_or_else(|_| panic!("failed to drop PostgreSQL trigger {trigger}"));
    if let Some(function) = function {
        sqlx::query(&format!("DROP FUNCTION {function}()"))
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn exercise_atomic_generation_start(database_url: &str, postgres: bool) {
    let fixture = atomic_generation_fixture(database_url).await;
    let pool = AnyPool::connect(database_url).await.unwrap();
    let idempotency = GenerationJobIdempotency {
        key: format!("generation-atomic-idempotency-{}", Uuid::now_v7()),
        request_hash: generation_request_hash(&fixture.model, &json!({"prompt": "same"})),
    };
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let database = fixture.database.clone();
        let key = fixture.key.clone();
        let price = fixture.price.clone();
        let model = fixture.model.clone();
        let idempotency = idempotency.clone();
        let barrier = barrier.clone();
        let upstream_id = fixture.upstream_id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .start_generation_job(
                    start_input(
                        &key,
                        upstream_id,
                        &price,
                        Uuid::now_v7(),
                        &model,
                        &idempotency.request_hash,
                    ),
                    Some(&idempotency),
                )
                .await
        }));
    }
    let mut created = 0;
    let mut responses = Vec::new();
    for task in tasks {
        match task.await.unwrap().unwrap() {
            CreateGenerationJobResult::Created(job) => {
                created += 1;
                responses.push(job);
            }
            CreateGenerationJobResult::Replayed(job) => responses.push(job),
        }
    }
    assert_eq!(created, 1);
    assert_eq!(responses.len(), 8);
    let preparing = serde_json::to_value(&responses[0]).unwrap();
    assert!(
        responses
            .iter()
            .all(|response| serde_json::to_value(response).unwrap() == preparing)
    );
    let job_id = responses[0].job_id;
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        AtomicGenerationState {
            reservations: 1,
            jobs: 1,
            started_events: 1,
            event_locators: 1,
            rate_requests: 1,
            rate_tokens: 1,
            active_requests: 1,
            available_micros: 9_750_000,
            reserved_micros: 250_000,
            budget_reserved_micros: 250_000,
        }
    );
    let queued = fixture
        .database
        .attach_generation_job_request(
            fixture.key.key_id,
            job_id,
            &idempotency.request_hash,
            "objects/blake3/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await
        .unwrap();
    let AttachGenerationJobResult::Attached(queued) = queued else {
        panic!("a healthy database must acknowledge the archive attach");
    };
    assert_eq!(queued.status, "queued");
    let expected = serde_json::to_value(&queued).unwrap();

    let mismatched = GenerationJobIdempotency {
        key: idempotency.key.clone(),
        request_hash: generation_request_hash(&fixture.model, &json!({"prompt": "different"})),
    };
    assert!(matches!(
        fixture
            .database
            .start_generation_job(
                start_input(
                    &fixture.key,
                    fixture.upstream_id,
                    &fixture.price,
                    Uuid::now_v7(),
                    &fixture.model,
                    &mismatched.request_hash,
                ),
                Some(&mismatched),
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
    let replay = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Replayed(replay) = replay else {
        panic!("same request must replay the existing generation job");
    };
    assert_eq!(serde_json::to_value(replay).unwrap(), expected);
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        AtomicGenerationState {
            reservations: 1,
            jobs: 1,
            started_events: 1,
            event_locators: 1,
            rate_requests: 1,
            rate_tokens: 1,
            active_requests: 1,
            available_micros: 9_750_000,
            reserved_micros: 250_000,
            budget_reserved_micros: 250_000,
        }
    );

    fixture
        .database
        .cancel_generation_job(fixture.key.key_id, job_id)
        .await
        .unwrap();
    let before_failure = atomic_generation_state(&pool, &fixture.key).await;
    let terminal_replay = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Replayed(terminal_replay) = terminal_replay else {
        panic!("a terminal generation request must replay its existing job");
    };
    assert_eq!(terminal_replay.status, "cancelled");
    assert_eq!(terminal_replay.job_id, job_id);
    assert_eq!(
        serde_json::to_value(&terminal_replay).unwrap(),
        serde_json::to_value(
            fixture
                .database
                .generation_job(fixture.key.key_id, job_id)
                .await
                .unwrap()
        )
        .unwrap(),
        "the terminal replay must return the exact durable job view"
    );
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        before_failure,
        "a terminal replay must not consume quota or rate limits"
    );
    let failed_job_id = Uuid::now_v7();
    let (trigger, function) =
        install_generation_event_failure_trigger(&pool, postgres, failed_job_id).await;
    let failure = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                failed_job_id,
                &fixture.model,
                &generation_request_hash(&fixture.model, &json!({"prompt": "trigger-failure"})),
            ),
            None,
        )
        .await;
    if postgres {
        remove_generation_event_failure_trigger(&pool, &trigger, function.as_deref()).await;
    } else {
        sqlx::query(&format!("DROP TRIGGER {trigger}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(failure.is_err());
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        before_failure,
        "a late started-event trigger failure must roll back the reservation and queued job"
    );
    pool.close().await;
}

#[tokio::test]
async fn sqlite_generation_start_is_concurrent_idempotent_and_atomic() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation-atomic.db").display()
    );
    exercise_atomic_generation_start(&database_url, false).await;
}

#[tokio::test]
async fn postgres_generation_start_is_concurrent_idempotent_and_atomic() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    exercise_atomic_generation_start(&database_url, true).await;
}

#[tokio::test]
async fn expired_generation_preparation_is_taken_over_without_a_second_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation-takeover.db").display()
    );
    let fixture = atomic_generation_fixture(&database_url).await;
    let pool = AnyPool::connect(&database_url).await.unwrap();
    let idempotency = GenerationJobIdempotency {
        key: "generation-expired-takeover".to_owned(),
        request_hash: generation_request_hash(&fixture.model, &json!({"prompt": "same"})),
    };
    let first = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Created(first) = first else {
        panic!("first preparation must be owned by its creator");
    };
    let admitted = atomic_generation_state(&pool, &fixture.key).await;
    sqlx::query("UPDATE generation_jobs SET lease_expires_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(1))
        .bind(first.job_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        fixture
            .database
            .generation_job_by_idempotency(fixture.key.key_id, &idempotency)
            .await
            .unwrap()
            .is_none(),
        "the API fast replay path must hand an expired preparation to the atomic owner CAS"
    );

    let takeover = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Created(takeover) = takeover else {
        panic!("one retry must take over the expired preparation");
    };
    assert_eq!(takeover.job_id, first.job_id);
    assert_eq!(atomic_generation_state(&pool, &fixture.key).await, admitted);
    let queued = fixture
        .database
        .attach_generation_job_request(
            fixture.key.key_id,
            takeover.job_id,
            &idempotency.request_hash,
            "objects/blake3/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await
        .unwrap();
    let AttachGenerationJobResult::Attached(queued) = queued else {
        panic!("a healthy database must acknowledge the archive attach");
    };
    assert_eq!(queued.status, "queued");
    pool.close().await;
}

#[tokio::test]
async fn archive_attach_recovers_an_applied_commit_after_its_ack_is_lost() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation-attach-ack.db").display()
    );
    let fixture = atomic_generation_fixture(&database_url).await;
    let pool = AnyPool::connect(&database_url).await.unwrap();
    let idempotency = GenerationJobIdempotency {
        key: "generation-attach-ack".to_owned(),
        request_hash: generation_request_hash(&fixture.model, &json!({"prompt": "same"})),
    };
    let started = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Created(started) = started else {
        panic!("first preparation must be created");
    };
    let request_object =
        "objects/blake3/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    // Model a transport failure after the server applied the autocommit but
    // before the original owner received its ACK. Its bounded retry sees zero
    // updated rows and must compare/recover the exact committed attachment.
    sqlx::query(
        "UPDATE generation_jobs SET request_object = $1, status = 'queued', lease_expires_at = NULL WHERE id = $2 AND status = 'preparing'",
    )
    .bind(request_object)
    .bind(started.job_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let recovered = fixture
        .database
        .attach_generation_job_request(
            fixture.key.key_id,
            started.job_id,
            &idempotency.request_hash,
            request_object,
        )
        .await
        .unwrap();
    let AttachGenerationJobResult::Attached(recovered) = recovered else {
        panic!("an already-applied exact attach must be recovered, not reported indeterminate");
    };
    assert_eq!(recovered.job_id, started.job_id);
    assert_eq!(recovered.status, "queued");
    assert!(matches!(
        fixture
            .database
            .attach_generation_job_request(
                fixture.key.key_id,
                started.job_id,
                &idempotency.request_hash,
                "objects/blake3/bb/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        AtomicGenerationState {
            reservations: 1,
            jobs: 1,
            started_events: 1,
            event_locators: 1,
            rate_requests: 1,
            rate_tokens: 1,
            active_requests: 1,
            available_micros: 9_750_000,
            reserved_micros: 250_000,
            budget_reserved_micros: 250_000,
        }
    );
    assert_eq!(
        sqlx::query("SELECT COUNT(*) AS count FROM upstream_accounts")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get::<i64, _>("count"),
        1
    );
    assert!(
        fixture
            .database
            .claim_generation_job("attach-ack-worker")
            .await
            .unwrap()
            .is_some(),
        "exactly one durable job becomes upstream-worker visible"
    );
    assert!(
        fixture
            .database
            .claim_generation_job("attach-ack-worker-2")
            .await
            .unwrap()
            .is_none(),
        "the recovered attach must not create a second upstream execution"
    );
    pool.close().await;
}

#[tokio::test]
async fn preparation_reaper_is_idempotent_and_refunds_every_reserved_dimension() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("generation-reaper.db").display()
    );
    let fixture = atomic_generation_fixture(&database_url).await;
    let pool = AnyPool::connect(&database_url).await.unwrap();
    let idempotency = GenerationJobIdempotency {
        key: "generation-preparation-reaper".to_owned(),
        request_hash: generation_request_hash(&fixture.model, &json!({"prompt": "same"})),
    };
    let started = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Created(started) = started else {
        panic!("first preparation must be created");
    };
    sqlx::query("UPDATE generation_jobs SET lease_expires_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(1))
        .bind(started.job_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .database
            .expire_preparing_generation_jobs(100)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        fixture
            .database
            .expire_preparing_generation_jobs(100)
            .await
            .unwrap(),
        0
    );
    let failed = fixture
        .database
        .generation_job(fixture.key.key_id, started.job_id)
        .await
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("generation_archive_expired")
    );
    assert_eq!(
        atomic_generation_state(&pool, &fixture.key).await,
        AtomicGenerationState {
            reservations: 1,
            jobs: 1,
            started_events: 1,
            event_locators: 2,
            rate_requests: 1,
            rate_tokens: 0,
            active_requests: 0,
            available_micros: 10_000_000,
            reserved_micros: 0,
            budget_reserved_micros: 0,
        },
        "the admitted RPM remains charged while token, concurrency, budget, and balance reservations are refunded"
    );
    assert!(
        fixture
            .database
            .claim_generation_job("preparation-reaper-worker")
            .await
            .unwrap()
            .is_none()
    );
    let replay = fixture
        .database
        .start_generation_job(
            start_input(
                &fixture.key,
                fixture.upstream_id,
                &fixture.price,
                Uuid::now_v7(),
                &fixture.model,
                &idempotency.request_hash,
            ),
            Some(&idempotency),
        )
        .await
        .unwrap();
    let CreateGenerationJobResult::Replayed(replay) = replay else {
        panic!("an expired terminal preparation must replay without a new reservation");
    };
    assert_eq!(replay.job_id, started.job_id);
    assert_eq!(replay.status, "failed");
    pool.close().await;
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
        .cancel_generation_job(key.key_id, job.job_id)
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
        .cancel_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(replayed.status, "cancelled");
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "10"
    );
    assert_eq!(
        database
            .key_limit_snapshot(key.key_id)
            .await
            .unwrap()
            .concurrency
            .active,
        0,
        "replayed cancellation must release active concurrency exactly once"
    );
    let stats = database
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(job.created_at),
                to_created_at: Some(unix_millis().saturating_add(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(stats.summary.total_requests, 1);
    assert_eq!(stats.summary.failed_requests, 1);
}

#[tokio::test]
async fn running_cancellation_fences_the_polling_lease_and_refunds_only_after_confirmation() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation, &price))
        .await
        .unwrap();
    let submit_worker = "generation-cancel-submit-worker";
    database
        .claim_generation_job(submit_worker)
        .await
        .unwrap()
        .expect("queued job");
    let submission_nonce = Uuid::now_v7();
    database
        .mark_generation_submitting(job.job_id, submit_worker, submission_nonce)
        .await
        .unwrap();
    database
        .mark_generation_submitted(
            job.job_id,
            submit_worker,
            submission_nonce,
            "upstream-running-job",
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2_050)).await;
    let stale_worker = "generation-cancel-stale-poller";
    let stale_claim = database
        .claim_generation_job(stale_worker)
        .await
        .unwrap()
        .expect("running job claim");
    assert_eq!(stale_claim.status, "running");

    let cancelling = database
        .cancel_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(cancelling.status, "cancelling");
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75",
        "an unconfirmed upstream cancellation must remain reserved"
    );
    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: stale_worker,
                status: "failed",
                billed_units: 0,
                error_code: Some("comfyui_failed"),
                assets: &[],
                staged_assets: None,
            })
            .await,
        Err(AppError::NotFound)
    ));

    let replay = database
        .cancel_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(replay.status, "cancelling");
    let cancel_worker = "generation-cancel-confirm-worker";
    let cancel_claim = database
        .claim_generation_job(cancel_worker)
        .await
        .unwrap()
        .expect("cancellation claim");
    assert_eq!(cancel_claim.status, "cancelling");
    database
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id: cancel_worker,
            status: "cancelled",
            billed_units: 0,
            error_code: Some("cancelled_by_user"),
            assets: &[],
            staged_assets: None,
        })
        .await
        .unwrap();
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "10"
    );
    let terminal_replay = database
        .cancel_generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(terminal_replay.status, "cancelled");
    assert_eq!(terminal_replay.cost, "0");
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
    let asset = archived_asset(job.job_id, 0);
    let cost_micros = database
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id: "generation-stats-worker",
            status: "succeeded",
            billed_units: 1,
            error_code: None,
            assets: std::slice::from_ref(&asset),
            staged_assets: None,
        })
        .await
        .unwrap();

    let replayed_cost = database
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id: "generation-stats-worker",
            status: "succeeded",
            billed_units: 1,
            error_code: None,
            assets: std::slice::from_ref(&asset),
            staged_assets: None,
        })
        .await
        .unwrap();
    assert_eq!(replayed_cost, cost_micros);
    assert_eq!(
        database
            .key_limit_snapshot(key.key_id)
            .await
            .unwrap()
            .concurrency
            .active,
        0,
        "replayed terminal completion must release active concurrency exactly once"
    );
    let usage_entries = database
        .list_account_ledger(key.account_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
        .count();
    assert_eq!(usage_entries, 1);
    let finished = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    let completed_at = finished.completed_at.unwrap();
    let duration_ms = completed_at.saturating_sub(job.created_at);

    let stats = database
        .stats_filtered(
            key.key_id,
            StatsFilter {
                from_created_at: Some(job.created_at),
                to_created_at: Some(unix_millis().saturating_add(1)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
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
    assert_eq!(exact.summary.total_cost.as_deref(), Some("0.25"));

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
async fn lost_generation_owner_cannot_settle_and_the_new_owner_charges_once() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("expired-generation-worker")
        .await
        .unwrap()
        .expect("queued job");
    database
        .reschedule_generation_job(job.job_id, "expired-generation-worker", 0, None)
        .await
        .unwrap();

    let stale_asset = archived_asset(job.job_id, 0);
    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "expired-generation-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: std::slice::from_ref(&stale_asset),
                staged_assets: None,
            })
            .await,
        Err(AppError::NotFound)
    ));
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "9.75"
    );
    assert_eq!(
        database
            .list_account_ledger(key.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
            .count(),
        0
    );

    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    let reclaimed = database
        .claim_generation_job("replacement-generation-worker")
        .await
        .unwrap()
        .expect("rescheduled job");
    assert_eq!(reclaimed.job_id, job.job_id);
    let asset = archived_asset(job.job_id, 0);
    assert_eq!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "replacement-generation-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: std::slice::from_ref(&asset),
                staged_assets: None,
            })
            .await
            .unwrap(),
        250_000
    );
    assert_eq!(
        database
            .list_account_ledger(key.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
            .count(),
        1
    );
}

#[tokio::test]
async fn generation_staging_takeover_rejects_a_late_writer_and_replays_the_exact_manifest() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("old-generation-writer")
        .await
        .unwrap()
        .expect("queued job");
    let submission_nonce = Uuid::now_v7();
    database
        .mark_generation_submitting(job.job_id, "old-generation-writer", submission_nonce)
        .await
        .unwrap();
    database
        .mark_generation_submitted(
            job.job_id,
            "old-generation-writer",
            submission_nonce,
            "upstream-generation-job",
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2_050)).await;
    database
        .claim_generation_job("old-generation-writer")
        .await
        .unwrap()
        .expect("running job");

    let old_nonce = Uuid::now_v7();
    let old_manifest = GenerationStagedAssets {
        attempt_nonce: old_nonce,
        billed_units: 1,
        assets: vec![ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator: format!("staging/generation/{}/{old_nonce}/asset-0", job.job_id),
            mime_type: "image/png".to_owned(),
            size_bytes: 17,
            filename: "old.png".to_owned(),
        }],
    };
    database
        .reschedule_generation_job(job.job_id, "old-generation-writer", 0, None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    database
        .claim_generation_job("replacement-generation-writer")
        .await
        .unwrap()
        .expect("replacement claim");

    assert!(matches!(
        database
            .save_generation_staged_assets(job.job_id, "old-generation-writer", &old_manifest)
            .await,
        Err(AppError::NotFound)
    ));

    let replacement_nonce = Uuid::now_v7();
    assert_ne!(old_nonce, replacement_nonce);
    let replacement_manifest = GenerationStagedAssets {
        attempt_nonce: replacement_nonce,
        billed_units: 1,
        assets: vec![ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator: format!(
                "staging/generation/{}/{replacement_nonce}/asset-0",
                job.job_id
            ),
            mime_type: "image/png".to_owned(),
            size_bytes: 19,
            filename: "replacement.png".to_owned(),
        }],
    };
    database
        .save_generation_staged_assets(
            job.job_id,
            "replacement-generation-writer",
            &replacement_manifest,
        )
        .await
        .unwrap();

    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "old-generation-writer",
                status: "succeeded",
                billed_units: old_manifest.billed_units,
                error_code: None,
                assets: &old_manifest.assets,
                staged_assets: Some(&old_manifest),
            })
            .await,
        Err(AppError::NotFound)
    ));
    let finish = || FinishGenerationJobInput {
        job_id: job.job_id,
        worker_id: "replacement-generation-writer",
        status: "succeeded",
        billed_units: replacement_manifest.billed_units,
        error_code: None,
        assets: &replacement_manifest.assets,
        staged_assets: Some(&replacement_manifest),
    };
    assert_eq!(
        database.finish_generation_job(finish()).await.unwrap(),
        250_000
    );
    // This exact replay models a successful terminal commit whose response was lost. The
    // manifest remains referenced and must be reusable without fetching or deleting the asset.
    assert_eq!(
        database.finish_generation_job(finish()).await.unwrap(),
        250_000
    );
    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "old-generation-writer",
                status: "succeeded",
                billed_units: old_manifest.billed_units,
                error_code: None,
                assets: &old_manifest.assets,
                staged_assets: Some(&old_manifest),
            })
            .await,
        Err(AppError::Conflict(_))
    ));
    assert_eq!(
        database
            .generation_asset_for_key(
                key.key_id,
                job.job_id,
                replacement_manifest.assets[0].asset_id
            )
            .await
            .unwrap()
            .object_locator,
        replacement_manifest.assets[0].object_locator
    );
    assert_eq!(
        database
            .list_account_ledger(key.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
            .count(),
        1
    );
}

#[tokio::test]
async fn a_previously_settled_generation_is_finished_without_a_second_charge() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("generation-settled-intermediate-worker")
        .await
        .unwrap()
        .expect("queued job");
    assert_eq!(
        database.settle_usage(&reservation, 0, 1).await.unwrap(),
        250_000
    );

    let asset = archived_asset(job.job_id, 0);
    assert_eq!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "generation-settled-intermediate-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: std::slice::from_ref(&asset),
                staged_assets: None,
            })
            .await
            .unwrap(),
        250_000
    );
    assert_eq!(
        database
            .list_account_ledger(key.account_id, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
            .count(),
        1
    );
    assert_eq!(
        database
            .generation_job(key.key_id, job.job_id)
            .await
            .unwrap()
            .cost,
        "0.25"
    );
}

#[tokio::test]
async fn missing_generation_assets_become_a_fixed_failure_and_refund_in_full() {
    let (_directory, database, key, upstream_id, price) = fixture().await;
    let reservation = reserve(&database, &key, &price).await;
    let job = database
        .create_generation_job(input(&key, upstream_id, reservation.clone(), &price))
        .await
        .unwrap();
    database
        .claim_generation_job("generation-missing-assets-worker")
        .await
        .unwrap()
        .expect("queued job");
    assert!(matches!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "generation-missing-assets-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: &[],
                staged_assets: None,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        database
            .finish_generation_job(FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id: "generation-missing-assets-worker",
                status: "failed",
                billed_units: 0,
                error_code: Some("comfyui_missing_assets"),
                assets: &[],
                staged_assets: None,
            })
            .await
            .unwrap(),
        0
    );
    let failed = database
        .generation_job(key.key_id, job.job_id)
        .await
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.cost, "0");
    assert_eq!(failed.error_code.as_deref(), Some("comfyui_missing_assets"));
    assert_eq!(failed.result, None);
    assert_eq!(
        database.key_view(&key).await.unwrap().available_balance,
        "10"
    );
    let usage = database
        .list_account_ledger(key.account_id, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "usage" && entry.source == reservation.id.to_string())
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].amount, "0");
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
        database.cancel_generation_job(key.key_id, job.job_id).await,
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
