use super::*;
use crate::{
    db::CreateKeyInput,
    model::{AuthenticatedKey, ModelPrice},
};
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    database: Database,
    key: AuthenticatedKey,
    price: ModelPrice,
}

const PEPPER: &[u8] = b"gateway-persistence-acceptance-pepper-over-32-bytes";

/// Delay only the TCP startup. Real PostgreSQL still performs authentication,
/// session initialization and queries, including after a forced reconnect.
struct DelayedPostgres {
    url: url::Url,
    task: tokio::task::JoinHandle<()>,
}

impl DelayedPostgres {
    async fn new() -> Self {
        let database_url = std::env::var("MTC_TEST_POSTGRES_URL")
            .expect("archive pool regression requires the CI PostgreSQL service");
        let mut url = url::Url::parse(&database_url).unwrap();
        let host = url.host_str().unwrap().to_owned();
        let port = url.port().unwrap_or(5432);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        url.set_host(Some("127.0.0.1")).unwrap();
        url.set_port(Some(listener.local_addr().unwrap().port()))
            .unwrap();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let host = host.clone();
                connections.spawn(async move {
                    let mut server = tokio::net::TcpStream::connect((host.as_str(), port))
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(350)).await;
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                });
            }
        });
        Self { url, task }
    }
}

impl Drop for DelayedPostgres {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn postgres_archive_pool_cold_reconnect_and_wait_preserve_session_policy() {
    let proxy = DelayedPostgres::new().await;
    sqlx::any::install_default_drivers();
    let persistence =
        GatewayPersistence::new(proxy.url.as_str(), DatabaseBackend::PostgreSql).unwrap();
    assert_eq!(persistence.pool.options().get_max_connections(), 1);
    assert_eq!(persistence.pool.size(), 0, "the pool remains lazy");
    for _ in 0..2 {
        let started = tokio::time::Instant::now();
        let mut connection = persistence.pool.acquire().await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(350));
        for (setting, expected) in [
            ("statement_timeout", "30s"),
            ("lock_timeout", "10s"),
            ("idle_in_transaction_session_timeout", "30s"),
        ] {
            let value: String = sqlx::query_scalar("SELECT current_setting($1)")
                .bind(setting)
                .fetch_one(&mut *connection)
                .await
                .unwrap();
            assert_eq!(value, expected);
        }
        // A healthy statement above the old 250ms limit must finish.
        sqlx::raw_sql("SELECT pg_sleep(0.35)")
            .execute(&mut *connection)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let (waiting, ()) = tokio::join!(persistence.pool.acquire(), async {
            tokio::time::sleep(Duration::from_millis(350)).await;
            drop(connection);
        });
        let mut reused = waiting.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
                .fetch_one(&mut *reused)
                .await
                .unwrap(),
            pid,
            "waiting for metadata must reuse the connection, not grow the pool"
        );
        reused.close().await.unwrap();
    }
    persistence.pool.close().await;
}

#[tokio::test]
async fn source_metadata_waits_past_two_seconds_without_cancellation_or_replay() {
    let fixture = Fixture::new().await;
    let request_id = Uuid::new_v4();
    let locator =
        crate::gateway_body::request_spool::request_body_object_key(&request_id.to_string())
            .unwrap();
    let body = b"{\"input\":\"metadata queue must not cancel a transaction\"}";
    let digest = *blake3::hash(body).as_bytes();
    let reservation = fixture
        .database
        .start_proxy_forwarding_request_with_output_limit(
            StartProxyRequest {
                request_id,
                key: &fixture.key,
                price: &fixture.price,
                input_token_ceiling: 7,
                output_token_ceiling: 11,
                protocol: "openai",
                model: "isolation",
                request_object: &locator,
                upstream_account_id: None,
                model_route_id: None,
            },
            None,
            Some(11),
        )
        .await
        .unwrap();
    let identity = ArchiveSpoolIdentity {
        request_id,
        tenant_id: fixture.key.tenant_id,
        reservation_id: reservation.id,
    };
    let held = fixture
        .database
        .hold_gateway_archive_connection_for_test()
        .await;
    let pending =
        fixture
            .database
            .persist_request_source(identity, &locator, body.len(), digest, Some(true));
    let (result, ()) = tokio::join!(pending, async {
        tokio::time::sleep(Duration::from_millis(2200)).await;
        drop(held);
    });
    assert_eq!(result, Some(true), "the first attempt must reach COMMIT");
    assert_eq!(fixture.database.request_source_write_failures_for_test(), 0);
    assert_eq!(
        fixture
            .database
            .persist_request_source(identity, &locator, body.len(), digest, Some(true),)
            .await,
        Some(false),
        "a completed staging attempt must not be replayed"
    );
    let row = sqlx::query("SELECT state, bound_locator, body_byte_count, body_blake3 FROM request_archive_spools WHERE request_id = $1")
        .bind(request_id.to_string()).fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("state"), "bound");
    assert_eq!(row.get::<String, _>("bound_locator"), locator);
    assert_eq!(row.get::<i64, _>("body_byte_count"), body.len() as i64);
    assert_eq!(
        row.get::<String, _>("body_blake3"),
        blake3::hash(body).to_hex().to_string()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_bound'")
            .bind(request_id.to_string()).fetch_one(&fixture.database.pool).await.unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        "reserved",
        "optional metadata must not change financial responsibility"
    );
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("db.sqlite").display()
        );
        let database = Database::connect_with_max(&url, 1).await.unwrap();
        database.migrate().await.unwrap();
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "isolation".into(),
                    principal_external_id: "member".into(),
                    alias: "isolation".into(),
                    currency: "USD".into(),
                    policy: crate::model::KeyPolicy::default(),
                    initial_balance: rust_decimal::Decimal::ONE,
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
        let price = database
            .upsert_model_price(
                "isolation",
                "USD",
                rust_decimal::Decimal::ONE,
                rust_decimal::Decimal::ONE,
            )
            .await
            .unwrap();
        Self {
            _directory: directory,
            database,
            key,
            price,
        }
    }

    async fn admit(&self, request_id: Uuid) -> Result<StartedProxyRequest, AppError> {
        let locator = format!("gap://{request_id}/request");
        self.database
            .start_proxy_request_with_deferred_archive(
                StartProxyRequest {
                    request_id,
                    key: &self.key,
                    price: &self.price,
                    input_token_ceiling: 7,
                    output_token_ceiling: 11,
                    protocol: "openai",
                    model: "isolation",
                    request_object: &locator,
                    upstream_account_id: None,
                    model_route_id: None,
                },
                &Bytes::from_static(b"{\"input\":\"acceptance\"}"),
                PEPPER,
                false,
                None,
                Some(11),
            )
            .await
    }

    async fn assert_reserved(&self, request_id: Uuid) {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1 AND r.status = 'reserved' AND q.request_object = $2")
            .bind(request_id.to_string())
            .bind(format!("gap://{request_id}/request"))
            .fetch_one(&self.database.pool).await.unwrap();
        assert_eq!(count, 1);
    }

    fn archive_database(&self) -> Database {
        let mut database = self.database.clone();
        database.pool = self.database.gateway_persistence.pool.clone();
        database.pool_role = "gateway_archive";
        database
    }

    async fn wait_for_busy_retry(&self) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while self
                .database
                .gateway_persistence
                .busy_retries
                .load(Ordering::Relaxed)
                == 0
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the real competing SQLite writer must cause a BEGIN retry");
    }
}

#[tokio::test]
async fn sqlite_busy_begin_recovers_without_replaying_capture_or_capacity_reservation() {
    let fixture = Fixture::new().await;
    sqlx::raw_sql(
        "CREATE TABLE reservation_audit (request_id TEXT NOT NULL);
         CREATE TRIGGER audit_reservation AFTER INSERT ON archive_budget_reservations
         BEGIN INSERT INTO reservation_audit VALUES (NEW.request_id); END;",
    )
    .execute(&fixture.database.pool)
    .await
    .unwrap();
    let request_id = Uuid::new_v4();
    let (entered, release, _registration) =
        crate::response_archive_spool::scoped_request_preseal_pause_for_test(request_id);
    let admitted = fixture.admit(request_id).await.unwrap();
    assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    let held = fixture.database.begin_write_transaction().await.unwrap();
    release.send(()).unwrap();
    fixture.wait_for_busy_retry().await;
    held.rollback().await.unwrap();
    fixture.database.drain_gateway_persistence_for_test().await;

    let metrics = fixture.database.gateway_persistence_metrics();
    assert!(metrics.contains("request_persistence_total{outcome=\"accepted\"} 1\n"));
    assert!(metrics.contains("request_persistence_total{outcome=\"failed\"} 0\n"));
    assert!(metrics.contains("request_persistence_total{outcome=\"capacity\"} 0\n"));
    assert!(metrics.contains("request_persistence_jobs 0\n"));
    assert!(metrics.contains("request_persistence_bytes 0\n"));
    for statement in [
        "SELECT COUNT(*) FROM reservation_audit",
        "SELECT COUNT(*) FROM request_archive_spools WHERE state = 'pending'",
        "SELECT COUNT(*) FROM request_archive_spool_chunks",
        "SELECT COUNT(*) FROM request_events WHERE event_kind = 'started'",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(statement)
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap(),
            1,
            "{statement}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM archive_budget_reservations")
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap(),
        0
    );
    let stored: i64 = sqlx::query_scalar("SELECT cipher_bytes FROM request_archive_spools")
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
    let budget: (i64, i64) = sqlx::query_as(
        "SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
    )
    .fetch_one(&fixture.database.pool)
    .await
    .unwrap();
    assert!(stored > 0);
    assert_eq!(budget, (stored, stored));
}

#[tokio::test]
async fn sqlite_busy_retry_keeps_the_original_queued_job_deadline_and_reports_failure() {
    let fixture = Fixture::new().await;
    let writer = fixture
        .database
        .gateway_persistence
        .writer
        .acquire()
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let request_id = Uuid::new_v4();
    fixture.admit(request_id).await.unwrap();
    tokio::time::sleep(JOB_TIMEOUT * 3 / 4).await;
    let held = fixture.database.begin_write_transaction().await.unwrap();
    drop(writer);
    fixture.wait_for_busy_retry().await;
    tokio::time::timeout_at(
        started + JOB_TIMEOUT + Duration::from_secs(1),
        fixture.database.drain_gateway_persistence_for_test(),
    )
    .await
    .expect("BEGIN retry must not restart the queued job's two-second deadline");
    held.rollback().await.unwrap();
    fixture.assert_reserved(request_id).await;
    let metrics = fixture.database.gateway_persistence_metrics();
    assert!(metrics.contains("request_persistence_total{outcome=\"failed\"} 1\n"));
    assert!(metrics.contains("request_persistence_jobs 0\n"));
    assert!(metrics.contains("request_persistence_bytes 0\n"));
    for statement in [
        "SELECT COUNT(*) FROM archive_budget_reservations",
        "SELECT COUNT(*) FROM request_archive_spools",
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
        "SELECT request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
    ] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(statement)
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn cancelled_busy_begin_releases_the_optional_connection_without_late_writes() {
    let fixture = Fixture::new().await;
    let archive = fixture.archive_database();
    let held = fixture.database.begin_write_transaction().await.unwrap();
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let owner = archive.gateway_persistence.clone();
    let pending = tokio::spawn(ARCHIVE_DEADLINE.scope((owner, deadline), async move {
        archive.begin_write_transaction().await
    }));
    fixture.wait_for_busy_retry().await;
    pending.abort();
    match pending.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("the blocked BEGIN must be cancelled"),
    }
    held.rollback().await.unwrap();
    let archive = fixture.archive_database();
    let transaction =
        tokio::time::timeout(Duration::from_secs(1), archive.begin_write_transaction())
            .await
            .expect("cancelled BEGIN must return its only pool connection")
            .unwrap();
    transaction.rollback().await.unwrap();
}

#[tokio::test]
async fn cancellation_after_begin_rolls_back_without_replaying_the_transaction() {
    let fixture = Fixture::new().await;
    sqlx::query("CREATE TABLE cancellation_probe (value INTEGER NOT NULL)")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    let archive = fixture.archive_database();
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let owner = archive.gateway_persistence.clone();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(ARCHIVE_DEADLINE.scope((owner, deadline), async move {
        let mut transaction = archive.begin_write_transaction().await.unwrap();
        sqlx::query("INSERT INTO cancellation_probe VALUES (1)")
            .execute(&mut *transaction)
            .await
            .unwrap();
        entered.send(()).unwrap();
        std::future::pending::<()>().await;
        transaction.commit().await
    }));
    tokio::time::timeout(Duration::from_secs(1), ready)
        .await
        .unwrap()
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    let archive = fixture.archive_database();
    let mut transaction =
        tokio::time::timeout(Duration::from_secs(1), archive.begin_write_transaction())
            .await
            .expect("the cancelled transaction must release the writer")
            .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM cancellation_probe")
            .fetch_one(&mut *transaction)
            .await
            .unwrap(),
        0
    );
    transaction.rollback().await.unwrap();
    assert_eq!(
        archive
            .gateway_persistence
            .busy_retries
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn archive_begin_retry_scope_excludes_other_tasks_databases_roles_and_backends() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let archive = fixture.archive_database();
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    assert!(archive.gateway_archive_retry_deadline().is_none());
    ARCHIVE_DEADLINE
        .scope((archive.gateway_persistence.clone(), deadline), async {
            assert_eq!(archive.gateway_archive_retry_deadline(), Some(deadline));
            assert!(fixture.database.gateway_archive_retry_deadline().is_none());
            assert!(
                other
                    .archive_database()
                    .gateway_archive_retry_deadline()
                    .is_none()
            );
            let mut postgres = archive.clone();
            postgres.backend = DatabaseBackend::PostgreSql;
            assert!(postgres.gateway_archive_retry_deadline().is_none());
            let spawned = archive.clone();
            assert!(
                tokio::spawn(async move { spawned.gateway_archive_retry_deadline() })
                    .await
                    .unwrap()
                    .is_none()
            );
            let mut connection = archive.pool.acquire().await.unwrap();
            sqlx::query("PRAGMA query_only = ON")
                .execute(&mut *connection)
                .await
                .unwrap();
            drop(connection);
            assert!(matches!(
                archive.begin_write_transaction().await,
                Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("8")
            ));
            archive.pool.close().await;
            assert!(matches!(
                archive.begin_write_transaction().await,
                Err(sqlx::Error::PoolClosed)
            ));
            assert!(matches!(
                ARCHIVE_DEADLINE
                    .scope(
                        (
                            archive.gateway_persistence.clone(),
                            tokio::time::Instant::now()
                        ),
                        archive.begin_write_transaction(),
                    )
                    .await,
                Err(sqlx::Error::PoolTimedOut)
            ));
            assert_eq!(
                archive
                    .gateway_persistence
                    .busy_retries
                    .load(Ordering::Relaxed),
                0
            );
        })
        .await;
}

#[tokio::test]
async fn sqlite_busy_commit_is_not_replayed_after_begin_has_succeeded() {
    let fixture = Fixture::new().await;
    sqlx::query("PRAGMA journal_mode = DELETE")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE commit_probe (value INTEGER NOT NULL)")
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    let mut reader = fixture.database.pool.begin().await.unwrap();
    sqlx::query("SELECT * FROM commit_probe")
        .fetch_all(&mut *reader)
        .await
        .unwrap();
    let archive = fixture.archive_database();
    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let error = ARCHIVE_DEADLINE
        .scope((archive.gateway_persistence.clone(), deadline), async {
            let mut transaction = archive.begin_write_transaction().await.unwrap();
            sqlx::query("INSERT INTO commit_probe VALUES (1)")
                .execute(&mut *transaction)
                .await
                .unwrap();
            transaction.commit().await.unwrap_err()
        })
        .await;
    assert!(matches!(error, sqlx::Error::Database(error) if error.code().as_deref() == Some("5")));
    assert_eq!(
        archive
            .gateway_persistence
            .busy_retries
            .load(Ordering::Relaxed),
        0
    );
    reader.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM commit_probe")
            .fetch_one(&archive.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn exact_archive_barrier_waits_for_capture_and_is_isolated_to_its_database_and_task() {
    let fixture = Fixture::new().await;
    let other = Fixture::new().await;
    let request_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();
    let (entered, release, _registration) =
        crate::response_archive_spool::scoped_request_preseal_pause_for_test(request_id);
    let (other_entered, other_release, _other_registration) =
        crate::response_archive_spool::scoped_request_preseal_pause_for_test(other_id);

    fixture
        .database
        .with_request_archive_capture_for_test(async {
            // Even in the scoped task, another database must retain deferred
            // admission. Its capture is held at a deterministic preseal gate.
            let admitted = other.admit(other_id).await.unwrap();
            assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
            tokio::time::timeout(Duration::from_secs(5), other_entered)
                .await
                .expect("bounded other database preseal entry")
                .unwrap();
            other.assert_reserved(other_id).await;

            let admission = fixture.admit(request_id);
            tokio::pin!(admission);
            tokio::select! {
                result = &mut admission => panic!("archive barrier returned before capture: {}", result.is_ok()),
                result = tokio::time::timeout(Duration::from_secs(5), entered) => {
                    result.expect("bounded scoped preseal entry").unwrap();
                },
            }
            fixture.assert_reserved(request_id).await;
            release.send(()).unwrap();
            let admitted = admission.await.unwrap();
            assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND tenant_id = $2 AND reservation_id = $3 AND state = 'pending'")
                .bind(request_id.to_string())
                .bind(fixture.key.tenant_id.to_string())
                .bind(admitted.reservation.id.to_string())
                .fetch_one(&fixture.database.pool).await.unwrap();
            assert_eq!(count, 1, "scoped admission must finish the owned request capture");
            other_release.send(()).unwrap();
            other.database.drain_gateway_persistence_for_test().await;
        })
        .await;

    // Leaving the scope restores deferred admission on the very same database.
    let unscoped_id = Uuid::new_v4();
    let (entered, release, _unscoped_registration) =
        crate::response_archive_spool::scoped_request_preseal_pause_for_test(unscoped_id);
    let admitted = fixture.admit(unscoped_id).await.unwrap();
    assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .expect("bounded scope exit preseal entry")
        .unwrap();
    fixture.assert_reserved(unscoped_id).await;
    release.send(()).unwrap();
    fixture.database.drain_gateway_persistence_for_test().await;
}

#[tokio::test]
async fn expired_capture_before_preseal_can_clean_its_unreached_registration() {
    let fixture = Fixture::new().await;
    let _writer = fixture
        .database
        .gateway_persistence
        .writer
        .acquire()
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let (mut entered, _release, registration) =
        crate::response_archive_spool::scoped_request_preseal_pause_for_test(request_id);
    let admitted = fixture.admit(request_id).await.unwrap();
    assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
    fixture.database.drain_gateway_persistence_for_test().await;
    fixture.assert_reserved(request_id).await;
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .failed
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        entered.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    );
    drop(registration);
    assert_eq!(
        entered.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    );
}

#[tokio::test]
async fn slow_optional_pool_does_not_wait_or_steal_the_only_safety_connection() {
    let fixture = Fixture::new().await;
    let held = fixture
        .database
        .gateway_persistence
        .pool
        .acquire()
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let admitted = tokio::time::timeout(Duration::from_secs(1), fixture.admit(request_id))
        .await
        .expect("admission must not await the occupied optional pool")
        .unwrap();
    assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
    fixture.assert_reserved(request_id).await;
    fixture.database.drain_gateway_persistence_for_test().await;
    fixture.assert_reserved(request_id).await;
    drop(held);
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .slots
            .available_permits(),
        TASK_LIMIT
    );
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .bytes
            .available_permits(),
        BYTE_LIMIT
    );
}

#[tokio::test]
async fn unavailable_optional_pool_keeps_reservation_and_durable_gap() {
    let fixture = Fixture::new().await;
    fixture.database.gateway_persistence.pool.close().await;
    let request_id = Uuid::new_v4();
    assert!(fixture.admit(request_id).await.is_ok());
    fixture.database.drain_gateway_persistence_for_test().await;
    fixture.assert_reserved(request_id).await;
    assert!(fixture.admit(request_id).await.is_err());
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(fixture.key.key_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "duplicate admission must roll back its new reservation"
    );
}

#[tokio::test]
async fn saturated_memory_permits_queue_the_raw_request_without_blocking_admission() {
    let fixture = Fixture::new().await;
    for by_bytes in [false, true] {
        let permits = if by_bytes {
            fixture
                .database
                .gateway_persistence
                .bytes
                .clone()
                .acquire_many_owned(BYTE_LIMIT as u32)
                .await
                .unwrap()
        } else {
            fixture
                .database
                .gateway_persistence
                .slots
                .clone()
                .acquire_many_owned(TASK_LIMIT as u32)
                .await
                .unwrap()
        };
        let request_id = Uuid::new_v4();
        let admitted = tokio::time::timeout(Duration::from_secs(1), fixture.admit(request_id))
            .await
            .expect("memory permits must not block admission")
            .unwrap();
        assert_eq!(admitted.archive_admission, RequestArchiveAdmission::Queued);
        fixture.assert_reserved(request_id).await;
        drop(permits);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND state = 'pending'",
                )
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("memory permits must still archive the raw request JSON");
        fixture.database.drain_gateway_persistence_for_test().await;
    }
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .slots
            .available_permits(),
        TASK_LIMIT
    );
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .bytes
            .available_permits(),
        BYTE_LIMIT
    );
    let metrics = fixture.database.gateway_persistence_metrics();
    assert!(metrics.contains("request_persistence_total{outcome=\"accepted\"} 2\n"));
    assert!(metrics.contains("request_persistence_total{outcome=\"capacity\"} 0\n"));
    assert!(metrics.contains("request_persistence_total{outcome=\"failed\"} 0\n"));
    assert!(metrics.contains("request_persistence_jobs 0\n"));
    assert!(metrics.contains("request_persistence_bytes 0\n"));
}

#[tokio::test]
async fn expired_queued_jobs_release_all_capacity_without_database_work() {
    let fixture = Fixture::new().await;
    let held = fixture
        .database
        .gateway_persistence
        .writer
        .acquire()
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    assert_eq!(
        fixture.admit(request_id).await.unwrap().archive_admission,
        RequestArchiveAdmission::Queued
    );
    fixture.database.drain_gateway_persistence_for_test().await;
    let metrics = fixture.database.gateway_persistence_metrics();
    assert!(metrics.contains("request_persistence_total{outcome=\"accepted\"} 1\n"));
    assert!(metrics.contains("request_persistence_total{outcome=\"failed\"} 1\n"));
    fixture.assert_reserved(request_id).await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools")
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .bytes
            .available_permits(),
        BYTE_LIMIT
    );
    drop(held);
}

#[tokio::test]
async fn archive_can_capture_after_terminal_only_with_matching_gap_and_owner() {
    let fixture = Fixture::new().await;
    let held = fixture
        .database
        .gateway_persistence
        .writer
        .acquire()
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let admitted = fixture.admit(request_id).await.unwrap();
    sqlx::query("UPDATE request_records SET completed_at = 1 WHERE id = $1")
        .bind(request_id.to_string())
        .execute(&fixture.database.pool)
        .await
        .unwrap();
    drop(held);
    fixture.database.drain_gateway_persistence_for_test().await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND reservation_id = $2 AND state = 'pending'")
        .bind(request_id.to_string()).bind(admitted.reservation.id.to_string())
        .fetch_one(&fixture.database.pool).await.unwrap();
    assert_eq!(count, 1);

    let held = fixture
        .database
        .gateway_persistence
        .writer
        .acquire()
        .await
        .unwrap();
    let fenced_id = Uuid::new_v4();
    fixture.admit(fenced_id).await.unwrap();
    sqlx::query("UPDATE request_records SET completed_at = 1, request_object = 'already-bound' WHERE id = $1")
        .bind(fenced_id.to_string()).execute(&fixture.database.pool).await.unwrap();
    drop(held);
    fixture.database.drain_gateway_persistence_for_test().await;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1")
            .bind(fenced_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn safety_database_failure_never_enqueues_archive_work() {
    let fixture = Fixture::new().await;
    sqlx::query("CREATE TRIGGER reject_safety BEFORE INSERT ON request_records BEGIN SELECT RAISE(ABORT, 'safety unavailable'); END")
        .execute(&fixture.database.pool).await.unwrap();
    assert!(matches!(
        fixture.admit(Uuid::new_v4()).await,
        Err(AppError::Capacity {
            code: "request_record_persist_failed",
            ..
        })
    ));
    assert_eq!(
        fixture
            .database
            .gateway_persistence
            .slots
            .available_permits(),
        TASK_LIMIT
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
