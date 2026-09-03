use std::{collections::HashSet, sync::Arc};

use memeloop_token_center::{
    archive_staging::{
        ArchiveStagingCleanupErrorCode, ArchiveStagingEmptyResult, ArchiveStagingIntentDigest,
        ArchiveStagingKey, ArchiveStagingLeaseOwner, ArchiveStagingOwner, ArchiveStagingPurpose,
        ArchiveStagingReferenceProof, ArchiveStagingState, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    db::Database,
    error::AppError,
};
use sqlx::{AnyPool, Row, any::AnyPoolOptions};
use tokio::sync::Barrier;
use url::Url;
use uuid::Uuid;

async fn sqlite_fixture() -> (tempfile::TempDir, String, Database, AnyPool) {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("archive-staging.db").display()
    );
    let database = Database::connect_with_max(&database_url, 16).await.unwrap();
    database.migrate().await.unwrap();
    sqlx::any::install_default_drivers();
    let inspection = AnyPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .unwrap();
    (directory, database_url, database, inspection)
}

fn owner(value: &str) -> ArchiveStagingLeaseOwner {
    ArchiveStagingLeaseOwner::new(value).unwrap()
}

fn proxy_input(attempt_id: Uuid, token: Uuid, digest: char) -> BeginArchiveStagingInput {
    BeginArchiveStagingInput {
        key: ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(Uuid::now_v7()),
            ArchiveStagingPurpose::Request,
            attempt_id,
        )
        .unwrap(),
        intent_digest: ArchiveStagingIntentDigest::new(digest.to_string().repeat(64)).unwrap(),
        lease_token: token,
        lease_owner: owner("writer-a"),
    }
}

fn created(
    result: BeginArchiveStagingResult,
) -> memeloop_token_center::archive_staging::ArchiveStagingWriteLease {
    match result {
        BeginArchiveStagingResult::Created(lease) => lease,
        other => panic!("expected created attempt, got {other:?}"),
    }
}

#[tokio::test]
async fn fresh_sqlite_migrates_latest_schema_and_rejects_untyped_rows() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(latest, 61);
    database.readiness_check().await.unwrap();
    sqlx::query(
        "SELECT r.enforcement_mode, o.projected_at FROM usage_reservations r CROSS JOIN metered_usage_projection_outbox o WHERE 1 = 0",
    )
    .fetch_all(&pool)
    .await
    .unwrap();

    let invalid = sqlx::query(
        "INSERT INTO archive_staging_attempts (attempt_id, owner_kind, owner_id, purpose, intent_digest, state, writer_owner, writer_token, lease_owner, lease_token, lease_expires_at, created_at, updated_at) VALUES ($1, 'proxy_request', $2, 'assets', $3, 'writing', 'writer', $4, 'writer', $4, 100, 0, 0)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(Uuid::now_v7().to_string())
    .bind("a".repeat(64))
    .bind(Uuid::now_v7().to_string())
    .execute(&pool)
    .await;
    assert!(invalid.is_err());
}

#[tokio::test]
async fn begin_is_exactly_replayable_and_changed_identity_conflicts() {
    let (_directory, _url, database, _pool) = sqlite_fixture().await;
    let attempt_id = Uuid::now_v7();
    let token = Uuid::now_v7();
    let input = proxy_input(attempt_id, token, 'a');
    let first = database
        .begin_archive_staging_attempt(input.clone())
        .await
        .unwrap();
    let first_lease = created(first);
    let replay = database
        .begin_archive_staging_attempt(input.clone())
        .await
        .unwrap();
    assert!(matches!(replay, BeginArchiveStagingResult::Replayed(_)));

    let mut changed_digest = input.clone();
    changed_digest.intent_digest = ArchiveStagingIntentDigest::new("b".repeat(64)).unwrap();
    assert!(matches!(
        database.begin_archive_staging_attempt(changed_digest).await,
        Err(AppError::Conflict(_))
    ));
    let mut changed_token = input.clone();
    changed_token.lease_token = Uuid::now_v7();
    assert!(matches!(
        database.begin_archive_staging_attempt(changed_token).await,
        Err(AppError::Conflict(_))
    ));

    let bound_locator = format!("{}/body", first_lease.key.canonical_prefix());
    assert!(
        database
            .bind_archive_staging_attempt(&first_lease, &bound_locator)
            .await
            .unwrap()
    );
    assert!(
        database
            .bind_archive_staging_attempt(&first_lease, &bound_locator)
            .await
            .unwrap()
    );
    let existing = database.begin_archive_staging_attempt(input).await.unwrap();
    assert!(matches!(
        existing,
        BeginArchiveStagingResult::Existing(attempt)
            if attempt.state == ArchiveStagingState::Bound
    ));
    assert!(
        database
            .release_bound_archive_staging_attempt(first_lease.key, &bound_locator)
            .await
            .unwrap()
    );
    assert!(
        database
            .release_bound_archive_staging_attempt(first_lease.key, &bound_locator)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn expired_writer_token_cannot_heartbeat_bind_or_abandon() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let input = proxy_input(Uuid::now_v7(), Uuid::now_v7(), 'c');
    let mut lease = created(database.begin_archive_staging_attempt(input).await.unwrap());
    sqlx::query("UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1")
        .bind(lease.key.attempt_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !database
            .heartbeat_archive_staging_write(&mut lease)
            .await
            .unwrap()
    );
    assert!(
        !database
            .bind_archive_staging_attempt(&lease, &format!("{}/body", lease.key.canonical_prefix()))
            .await
            .unwrap()
    );
    assert!(
        !database
            .abandon_archive_staging_attempt(&lease)
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .promote_stale_archive_staging_attempts()
            .await
            .unwrap(),
        1
    );
    let grace: i64 = sqlx::query_scalar(
        "SELECT next_cleanup_at - updated_at FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(lease.key.attempt_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grace, 30 * 60 * 1_000);
}

#[tokio::test]
async fn cleanup_claims_are_fenced_and_expired_tokens_never_revive() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let lease = created(
        database
            .begin_archive_staging_attempt(proxy_input(Uuid::now_v7(), Uuid::now_v7(), 'd'))
            .await
            .unwrap(),
    );
    database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let mut first = database
        .claim_archive_staging_cleanup(owner("cleanup-a"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        database
            .claim_archive_staging_cleanup(owner("cleanup-b"))
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1")
        .bind(first.attempt.key.attempt_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !database
            .heartbeat_archive_staging_cleanup(&mut first)
            .await
            .unwrap()
    );
    assert!(matches!(
        database
            .record_archive_staging_cleanup_failure(
                &first,
                ArchiveStagingCleanupErrorCode::DeleteFailed
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let second = database
        .claim_archive_staging_cleanup(owner("cleanup-b"))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(second.token, first.token);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sqlite_cleanup_workers_claim_each_attempt_once() {
    let (_directory, _url, database, _pool) = sqlite_fixture().await;
    for index in 0..24_u128 {
        let lease = created(
            database
                .begin_archive_staging_attempt(proxy_input(
                    Uuid::from_u128(index + 1),
                    Uuid::from_u128(index + 101),
                    'e',
                ))
                .await
                .unwrap(),
        );
        database
            .abandon_archive_staging_attempt(&lease)
            .await
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(24));
    let mut tasks = Vec::new();
    for index in 0..24 {
        let database = database.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .claim_archive_staging_cleanup(owner(&format!("worker-{index}")))
                .await
                .unwrap()
                .unwrap()
                .attempt
                .key
                .attempt_id
        }));
    }
    let mut claimed = HashSet::new();
    for task in tasks {
        assert!(claimed.insert(task.await.unwrap()));
    }
    assert_eq!(claimed.len(), 24);
}

#[tokio::test]
async fn an_exact_or_descendant_reference_protects_but_neighbor_uuid_does_not() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let protected_lease = created(
        database
            .begin_archive_staging_attempt(proxy_input(
                Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff0").unwrap(),
                Uuid::now_v7(),
                'f',
            ))
            .await
            .unwrap(),
    );
    database
        .abandon_archive_staging_attempt(&protected_lease)
        .await
        .unwrap();
    let cleanup = database
        .claim_archive_staging_cleanup(owner("reference-worker"))
        .await
        .unwrap()
        .unwrap();
    let referenced_locator = format!("{}/body", cleanup.canonical_prefix());
    insert_archive_reference(
        &pool,
        "protected",
        cleanup.attempt.key.owner.id(),
        &referenced_locator,
    )
    .await;
    assert!(matches!(
        database
            .prove_archive_staging_unreferenced(cleanup)
            .await
            .unwrap(),
        ArchiveStagingReferenceProof::Protected { locator }
            if locator == referenced_locator
    ));
    assert_eq!(
        database
            .archive_staging_attempt(protected_lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    );
    let last_error: String = sqlx::query_scalar(
        "SELECT last_error_code FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(protected_lease.key.attempt_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(last_error, "reference_present");

    let neighbor_lease = created(
        database
            .begin_archive_staging_attempt(proxy_input(
                Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff1").unwrap(),
                Uuid::now_v7(),
                '1',
            ))
            .await
            .unwrap(),
    );
    database
        .abandon_archive_staging_attempt(&neighbor_lease)
        .await
        .unwrap();
    let cleanup = database
        .claim_archive_staging_cleanup(owner("neighbor-worker"))
        .await
        .unwrap()
        .unwrap();
    insert_archive_reference(
        &pool,
        "neighbor",
        cleanup.attempt.key.owner.id(),
        &format!("{}0/body", cleanup.canonical_prefix()),
    )
    .await;
    let proof = database
        .prove_archive_staging_unreferenced(cleanup)
        .await
        .unwrap();
    assert!(matches!(
        proof,
        ArchiveStagingReferenceProof::Unreferenced(_)
    ));
}

#[tokio::test]
async fn cleanup_failure_uses_fixed_code_bounded_backoff_and_resets_empty_proof() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let lease = created(
        database
            .begin_archive_staging_attempt(proxy_input(Uuid::now_v7(), Uuid::now_v7(), '2'))
            .await
            .unwrap(),
    );
    database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let cleanup = database
        .claim_archive_staging_cleanup(owner("failure-worker"))
        .await
        .unwrap()
        .unwrap();
    let next = database
        .record_archive_staging_cleanup_failure(
            &cleanup,
            ArchiveStagingCleanupErrorCode::ObjectStoreUnavailable,
        )
        .await
        .unwrap();
    let row = sqlx::query(
        "SELECT cleanup_failures, next_cleanup_at, last_error_code, empty_observed_at FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(lease.key.attempt_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("cleanup_failures"), 1);
    assert_eq!(row.get::<i64, _>("next_cleanup_at"), next);
    assert_eq!(
        row.get::<String, _>("last_error_code"),
        "object_store_unavailable"
    );
    assert!(row.get::<Option<i64>, _>("empty_observed_at").is_none());
    assert!(
        database
            .claim_archive_staging_cleanup(owner("too-early"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cleaned_requires_two_separate_empty_observations_across_stability_window() {
    let (_directory, _url, database, pool) = sqlite_fixture().await;
    let lease = created(
        database
            .begin_archive_staging_attempt(proxy_input(Uuid::now_v7(), Uuid::now_v7(), '3'))
            .await
            .unwrap(),
    );
    database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let first = database
        .claim_archive_staging_cleanup(owner("empty-first"))
        .await
        .unwrap()
        .unwrap();
    let first_proof = match database
        .prove_archive_staging_unreferenced(first)
        .await
        .unwrap()
    {
        ArchiveStagingReferenceProof::Unreferenced(proof) => proof,
        ArchiveStagingReferenceProof::Protected { .. } => panic!("unexpected reference"),
    };
    assert!(matches!(
        database
            .record_archive_staging_empty(first_proof)
            .await
            .unwrap(),
        ArchiveStagingEmptyResult::FirstObservation { .. }
    ));
    assert!(
        database
            .claim_archive_staging_cleanup(owner("empty-too-early"))
            .await
            .unwrap()
            .is_none()
    );

    // Advance only this isolated fixture's durable timestamps beyond the
    // stability window; production code always uses authoritative DB time.
    sqlx::query(
        "UPDATE archive_staging_attempts SET created_at = created_at - 61000, empty_observed_at = empty_observed_at - 61000, next_cleanup_at = next_cleanup_at - 61000 WHERE attempt_id = $1",
    )
    .bind(lease.key.attempt_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    let second = database
        .claim_archive_staging_cleanup(owner("empty-second"))
        .await
        .unwrap()
        .unwrap();
    let second_proof = match database
        .prove_archive_staging_unreferenced(second)
        .await
        .unwrap()
    {
        ArchiveStagingReferenceProof::Unreferenced(proof) => proof,
        ArchiveStagingReferenceProof::Protected { .. } => panic!("unexpected reference"),
    };
    assert_eq!(
        database
            .record_archive_staging_empty(second_proof)
            .await
            .unwrap(),
        ArchiveStagingEmptyResult::Cleaned
    );
    assert_eq!(
        database
            .archive_staging_attempt(lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Cleaned
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn postgres_skip_locked_claims_are_disjoint_across_pools() {
    let Ok(base_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let admin = AnyPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await
        .unwrap();
    let schema = format!("archive_staging_{}", Uuid::now_v7().simple());
    // Test-only SQL safety boundary: the schema identifier is a literal prefix followed by a
    // library-generated UUID rendered as lowercase hexadecimal; no external input is present.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let scoped_url = postgres_schema_url(&base_url, &schema);
    let databases = [
        Database::connect_with_max(&scoped_url, 12).await.unwrap(),
        Database::connect_with_max(&scoped_url, 12).await.unwrap(),
        Database::connect_with_max(&scoped_url, 12).await.unwrap(),
        Database::connect_with_max(&scoped_url, 12).await.unwrap(),
    ];
    databases[0].migrate().await.unwrap();
    for index in 0..32_u128 {
        let lease = created(
            databases[0]
                .begin_archive_staging_attempt(proxy_input(
                    Uuid::from_u128(index + 1),
                    Uuid::from_u128(index + 101),
                    '4',
                ))
                .await
                .unwrap(),
        );
        databases[0]
            .abandon_archive_staging_attempt(&lease)
            .await
            .unwrap();
    }
    let barrier = Arc::new(Barrier::new(32));
    let mut tasks = Vec::new();
    for index in 0..32 {
        let database = databases[index % databases.len()].clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .claim_archive_staging_cleanup(owner(&format!("pg-worker-{index}")))
                .await
                .unwrap()
                .unwrap()
                .attempt
                .key
                .attempt_id
        }));
    }
    let mut claimed = HashSet::new();
    for task in tasks {
        assert!(claimed.insert(task.await.unwrap()));
    }
    assert_eq!(claimed.len(), 32);
    drop(databases);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

async fn insert_archive_reference(pool: &AnyPool, suffix: &str, request_id: Uuid, locator: &str) {
    sqlx::query(
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'openai', 'test', 0, 0, 0, $4, $5)",
    )
    .bind(request_id.to_string())
    .bind(format!("tenant-{suffix}"))
    .bind(Uuid::now_v7().to_string())
    .bind(locator)
    .bind(Uuid::now_v7().to_string())
    .execute(pool)
    .await
    .unwrap();
}

fn postgres_schema_url(base_url: &str, schema: &str) -> String {
    let mut url = Url::parse(base_url).expect("MTC_TEST_POSTGRES_URL must be a URL");
    url.query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    url.to_string()
}
