use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use memeloop_token_center::{
    archive::{ArchiveStagingObjectStore, ArchiveStore},
    archive_reaper::ArchiveReaper,
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, BeginArchiveStagingInput,
        BeginArchiveStagingResult,
    },
    config::{ArchiveBackend, Config},
    db::Database,
    error::AppError,
};
use sqlx::{AnyPool, Row, any::AnyPoolOptions};
use tokio::sync::Notify;
use uuid::Uuid;

struct Fixture {
    _database_directory: tempfile::TempDir,
    _archive_directory: Option<tempfile::TempDir>,
    database: Database,
    inspection: AnyPool,
    archive: ArchiveStore,
}

impl Fixture {
    async fn new(backend: ArchiveBackend) -> Self {
        let database_directory = tempfile::tempdir().expect("database directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            database_directory
                .path()
                .join("archive-reaper.db")
                .display()
        );
        let database = Database::connect_with_max(&database_url, 8)
            .await
            .expect("database");
        database.migrate().await.expect("migrations");
        sqlx::any::install_default_drivers();
        let inspection = AnyPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("inspection pool");

        let archive_directory = matches!(backend, ArchiveBackend::Filesystem)
            .then(|| tempfile::tempdir().expect("archive directory"));
        let mut config = Config::for_test(database_url);
        config.archive_backend = backend;
        config.archive_path = archive_directory
            .as_ref()
            .map(|directory| directory.path().to_string_lossy().into_owned());
        let archive = ArchiveStore::from_config(&config).await.expect("archive");

        Self {
            _database_directory: database_directory,
            _archive_directory: archive_directory,
            database,
            inspection,
            archive,
        }
    }

    fn reaper(&self, archive: Arc<dyn ArchiveStagingObjectStore>) -> ArchiveReaper {
        ArchiveReaper::with_store(
            self.database.clone(),
            archive,
            ArchiveStagingLeaseOwner::new("archive-reaper-test").expect("reaper owner"),
        )
    }

    async fn make_attempt(
        &self,
        owner_id: Uuid,
        attempt_id: Uuid,
    ) -> memeloop_token_center::archive_staging::ArchiveStagingWriteLease {
        let input = BeginArchiveStagingInput {
            key: ArchiveStagingKey::new(
                ArchiveStagingOwner::ProxyRequest(owner_id),
                ArchiveStagingPurpose::Request,
                attempt_id,
            )
            .expect("typed key"),
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).expect("digest"),
            lease_token: Uuid::now_v7(),
            lease_owner: ArchiveStagingLeaseOwner::new("archive-writer-test")
                .expect("writer owner"),
        };
        match self
            .database
            .begin_archive_staging_attempt(input)
            .await
            .expect("begin attempt")
        {
            BeginArchiveStagingResult::Created(lease) => lease,
            other => panic!("expected a new attempt, got {other:?}"),
        }
    }

    async fn make_due(&self, attempt_id: Uuid) {
        sqlx::query(
            "UPDATE archive_staging_attempts SET next_cleanup_at = created_at WHERE attempt_id = $1",
        )
        .bind(attempt_id.to_string())
        .execute(&self.inspection)
        .await
        .expect("make cleanup due");
    }

    async fn pass_stability_window(&self, attempt_id: Uuid) {
        sqlx::query(
            "UPDATE archive_staging_attempts SET created_at = created_at - 61000, empty_observed_at = empty_observed_at - 61000, next_cleanup_at = next_cleanup_at - 61000 WHERE attempt_id = $1",
        )
        .bind(attempt_id.to_string())
        .execute(&self.inspection)
        .await
        .expect("advance durable observation timestamps");
    }
}

#[derive(Clone)]
struct CountingStore {
    inner: ArchiveStore,
    deletes: Arc<AtomicUsize>,
}

impl CountingStore {
    fn new(inner: ArchiveStore) -> Self {
        Self {
            inner,
            deletes: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for CountingStore {
    async fn delete_archive_staging_segment(&self, key: ArchiveStagingKey) -> Result<(), AppError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        self.inner.delete_archive_staging_segment(key).await
    }

    async fn archive_staging_segment_is_empty(
        &self,
        key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        self.inner.archive_staging_segment_is_empty(key).await
    }
}

struct FlakyStore {
    delete_failures: AtomicUsize,
    verification_failures: AtomicUsize,
}

struct BlockingStore {
    started: Arc<Notify>,
}

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for BlockingStore {
    async fn delete_archive_staging_segment(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<(), AppError> {
        self.started.notify_one();
        pending::<()>().await;
        unreachable!("pending delete operation returned")
    }

    async fn archive_staging_segment_is_empty(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        unreachable!("a blocked delete cannot reach verification")
    }
}

#[async_trait::async_trait]
impl ArchiveStagingObjectStore for FlakyStore {
    async fn delete_archive_staging_segment(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<(), AppError> {
        if self
            .delete_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(AppError::Storage(
                "injected endpoint and credential detail".into(),
            ));
        }
        Ok(())
    }

    async fn archive_staging_segment_is_empty(
        &self,
        _key: ArchiveStagingKey,
    ) -> Result<bool, AppError> {
        if self
            .verification_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(AppError::Storage(
                "injected object path and owner detail".into(),
            ));
        }
        Ok(true)
    }
}

#[tokio::test]
async fn typed_delete_and_empty_verification_preserve_uuid_neighbours() {
    for backend in [ArchiveBackend::Memory, ArchiveBackend::Filesystem] {
        let fixture = Fixture::new(backend).await;
        let owner_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff0").unwrap();
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(owner_id),
            ArchiveStagingPurpose::Response,
            Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff1").unwrap(),
        )
        .unwrap();
        let neighbour = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(owner_id),
            ArchiveStagingPurpose::Response,
            Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff2").unwrap(),
        )
        .unwrap();
        let target = format!("{}/body", key.canonical_prefix());
        let typed_neighbour = format!("{}/body", neighbour.canonical_prefix());
        let lexical_neighbour = format!("{}0/body", key.canonical_prefix());
        fixture
            .archive
            .put(&target, Bytes::from_static(b"target"))
            .await
            .unwrap();
        fixture
            .archive
            .put(&typed_neighbour, Bytes::from_static(b"typed-neighbour"))
            .await
            .unwrap();
        fixture
            .archive
            .put(&lexical_neighbour, Bytes::from_static(b"lexical-neighbour"))
            .await
            .unwrap();

        assert!(
            !fixture
                .archive
                .archive_staging_segment_is_empty(key)
                .await
                .unwrap()
        );
        fixture
            .archive
            .delete_archive_staging_segment(key)
            .await
            .unwrap();
        assert!(
            fixture
                .archive
                .archive_staging_segment_is_empty(key)
                .await
                .unwrap()
        );
        assert_eq!(
            fixture.archive.get(&typed_neighbour).await.unwrap(),
            Bytes::from_static(b"typed-neighbour")
        );
        assert_eq!(
            fixture.archive.get(&lexical_neighbour).await.unwrap(),
            Bytes::from_static(b"lexical-neighbour")
        );
    }
}

#[tokio::test]
async fn crashed_unbound_attempt_is_deleted_and_needs_two_independent_claims() {
    for backend in [ArchiveBackend::Memory, ArchiveBackend::Filesystem] {
        let fixture = Fixture::new(backend).await;
        let lease = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
        let target = format!("{}/body", lease.key.canonical_prefix());
        let neighbour = format!("{}0/body", lease.key.canonical_prefix());
        fixture
            .archive
            .put(&target, Bytes::from_static(b"unbound"))
            .await
            .unwrap();
        fixture
            .archive
            .put(&neighbour, Bytes::from_static(b"neighbour"))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1",
        )
        .bind(lease.key.attempt_id.to_string())
        .execute(&fixture.inspection)
        .await
        .unwrap();

        let counted = CountingStore::new(fixture.archive.clone());
        let deletes = counted.deletes.clone();
        let reaper = fixture.reaper(Arc::new(counted));
        let promoted = reaper.reap_once().await.unwrap();
        assert_eq!(promoted.promoted, 1);
        assert_eq!(promoted.claimed, 0, "stale grace must be durable");
        assert_eq!(fixture.archive.get(&target).await.unwrap(), b"unbound"[..]);

        fixture.make_due(lease.key.attempt_id).await;
        let first = reaper.reap_once().await.unwrap();
        assert_eq!(first.claimed, 1);
        assert_eq!(first.cleaned, 0);
        assert_eq!(deletes.load(Ordering::SeqCst), 1);
        assert!(fixture.archive.get(&target).await.is_err());
        assert_eq!(
            fixture.archive.get(&neighbour).await.unwrap(),
            b"neighbour"[..]
        );
        assert_eq!(
            fixture
                .database
                .archive_staging_attempt(lease.key.attempt_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            ArchiveStagingState::CleanupPending
        );

        fixture.pass_stability_window(lease.key.attempt_id).await;
        let second = reaper.reap_once().await.unwrap();
        assert_eq!(second.claimed, 1);
        assert_eq!(second.cleaned, 1);
        assert_eq!(deletes.load(Ordering::SeqCst), 2);
        assert_eq!(
            fixture
                .database
                .archive_staging_attempt(lease.key.attempt_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            ArchiveStagingState::Cleaned
        );
    }
}

#[tokio::test]
async fn bound_and_exactly_referenced_attempts_are_never_deleted() {
    for backend in [ArchiveBackend::Memory, ArchiveBackend::Filesystem] {
        let fixture = Fixture::new(backend).await;
        let bound = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
        let bound_locator = format!("{}/body", bound.key.canonical_prefix());
        fixture
            .archive
            .put(&bound_locator, Bytes::from_static(b"bound"))
            .await
            .unwrap();
        assert!(
            fixture
                .database
                .bind_archive_staging_attempt(&bound, &bound_locator)
                .await
                .unwrap()
        );

        let protected = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
        let protected_locator = format!("{}/body", protected.key.canonical_prefix());
        fixture
            .archive
            .put(&protected_locator, Bytes::from_static(b"protected"))
            .await
            .unwrap();
        fixture
            .database
            .abandon_archive_staging_attempt(&protected)
            .await
            .unwrap();
        insert_request_reference(
            &fixture.inspection,
            protected.key.owner.id(),
            &protected_locator,
        )
        .await;

        let counted = CountingStore::new(fixture.archive.clone());
        let deletes = counted.deletes.clone();
        let pass = fixture.reaper(Arc::new(counted)).reap_once().await.unwrap();
        assert_eq!(pass.claimed, 1);
        assert_eq!(deletes.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture.archive.get(&bound_locator).await.unwrap(),
            b"bound"[..]
        );
        assert_eq!(
            fixture.archive.get(&protected_locator).await.unwrap(),
            b"protected"[..]
        );
        let error_code: String = sqlx::query_scalar(
            "SELECT last_error_code FROM archive_staging_attempts WHERE attempt_id = $1",
        )
        .bind(protected.key.attempt_id.to_string())
        .fetch_one(&fixture.inspection)
        .await
        .unwrap();
        assert_eq!(error_code, "reference_present");
    }
}

#[tokio::test]
async fn object_failures_release_the_lease_and_persist_fixed_retry_codes() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    let lease = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
    fixture
        .database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let reaper = fixture.reaper(Arc::new(FlakyStore {
        delete_failures: AtomicUsize::new(1),
        verification_failures: AtomicUsize::new(1),
    }));

    reaper.reap_once().await.unwrap();
    assert_retry_row(
        &fixture.inspection,
        lease.key.attempt_id,
        1,
        "delete_failed",
    )
    .await;

    fixture.make_due(lease.key.attempt_id).await;
    reaper.reap_once().await.unwrap();
    assert_retry_row(
        &fixture.inspection,
        lease.key.attempt_id,
        2,
        "verification_failed",
    )
    .await;

    fixture.make_due(lease.key.attempt_id).await;
    let recovered = reaper.reap_once().await.unwrap();
    assert_eq!(recovered.claimed, 1);
    let row = sqlx::query(
        "SELECT empty_observed_at, last_error_code FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(lease.key.attempt_id.to_string())
    .fetch_one(&fixture.inspection)
    .await
    .unwrap();
    assert!(row.get::<Option<i64>, _>("empty_observed_at").is_some());
    assert!(row.get::<Option<String>, _>("last_error_code").is_none());
}

#[tokio::test]
async fn a_long_generation_task_does_not_block_the_independent_reaper() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    let lease = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
    fixture
        .database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let target = format!("{}/body", lease.key.canonical_prefix());
    fixture
        .archive
        .put(&target, Bytes::from_static(b"cleanup while generating"))
        .await
        .unwrap();

    let generation = tokio::spawn(async { pending::<()>().await });
    let reaper = fixture.reaper(Arc::new(fixture.archive.clone()));
    tokio::time::timeout(Duration::from_secs(2), reaper.reap_once())
        .await
        .expect("reaper is scheduled independently")
        .expect("reaper pass");
    assert!(!generation.is_finished());
    assert!(fixture.archive.get(&target).await.is_err());
    generation.abort();
}

#[tokio::test]
async fn reaper_loop_stops_cleanly_on_shutdown() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    let reaper = fixture.reaper(Arc::new(fixture.archive.clone()));
    let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { reaper.run(shutdown).await });
    shutdown_sender.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("clean reaper shutdown")
        .expect("reaper task");
}

#[tokio::test]
async fn shutdown_cancels_a_blocked_store_pass_without_finalizing_its_lease() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    let lease = fixture.make_attempt(Uuid::now_v7(), Uuid::now_v7()).await;
    fixture
        .database
        .abandon_archive_staging_attempt(&lease)
        .await
        .unwrap();
    let started = Arc::new(Notify::new());
    let reaper = fixture.reaper(Arc::new(BlockingStore {
        started: started.clone(),
    }));
    let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { reaper.run(shutdown).await });
    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("blocking delete started");

    shutdown_sender.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("blocked pass is cancellation-safe")
        .expect("reaper task");
    let row = sqlx::query(
        "SELECT state, lease_owner, cleaned_at FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(lease.key.attempt_id.to_string())
    .fetch_one(&fixture.inspection)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "cleanup_pending");
    assert!(row.get::<Option<String>, _>("lease_owner").is_some());
    assert!(row.get::<Option<i64>, _>("cleaned_at").is_none());
}

#[tokio::test]
async fn idle_pass_has_no_claims_or_state_mutations() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    let reaper = fixture.reaper(Arc::new(fixture.archive.clone()));
    let pass = reaper.reap_once().await.unwrap();
    assert_eq!(pass.promoted, 0);
    assert_eq!(pass.claimed, 0);
    assert_eq!(pass.cleaned, 0);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts")
        .fetch_one(&fixture.inspection)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn one_pass_claims_only_the_configured_small_batch() {
    let fixture = Fixture::new(ArchiveBackend::Memory).await;
    for index in 1..=6_u128 {
        let lease = fixture
            .make_attempt(Uuid::from_u128(index), Uuid::from_u128(100 + index))
            .await;
        fixture
            .database
            .abandon_archive_staging_attempt(&lease)
            .await
            .unwrap();
    }
    let reaper = fixture.reaper(Arc::new(fixture.archive.clone()));
    let pass = reaper.reap_once().await.unwrap();
    assert_eq!(pass.claimed, 4);
    let observed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM archive_staging_attempts WHERE empty_observed_at IS NOT NULL",
    )
    .fetch_one(&fixture.inspection)
    .await
    .unwrap();
    assert_eq!(observed, 4);
}

async fn insert_request_reference(pool: &AnyPool, request_id: Uuid, locator: &str) {
    sqlx::query(
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'openai', 'test', 0, 0, 0, $4, $5)",
    )
    .bind(request_id.to_string())
    .bind(format!("tenant-{}", Uuid::now_v7()))
    .bind(Uuid::now_v7().to_string())
    .bind(locator)
    .bind(Uuid::now_v7().to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn assert_retry_row(pool: &AnyPool, attempt_id: Uuid, failures: i64, expected_code: &str) {
    let row = sqlx::query(
        "SELECT cleanup_failures, next_cleanup_at, updated_at, last_error_code, lease_owner, lease_token, lease_expires_at FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(attempt_id.to_string())
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("cleanup_failures"), failures);
    assert!(row.get::<i64, _>("next_cleanup_at") > row.get::<i64, _>("updated_at"));
    assert_eq!(row.get::<String, _>("last_error_code"), expected_code);
    assert!(row.get::<Option<String>, _>("lease_owner").is_none());
    assert!(row.get::<Option<String>, _>("lease_token").is_none());
    assert!(row.get::<Option<i64>, _>("lease_expires_at").is_none());
}
