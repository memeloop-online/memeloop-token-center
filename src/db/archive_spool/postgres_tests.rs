//! Real PostgreSQL cancellation tests. The deferred trigger blocks inside the
//! server's COMMIT, not before commit and not after an already-acknowledged API.
use std::time::Duration;

use sqlx::{AnyPool, any::AnyPoolOptions};
use tokio::task::JoinHandle;

use super::*;
use crate::archive_staging::{
    ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
    BeginArchiveStagingInput, BeginArchiveStagingResult,
};

// Keep bulk-seed disk pressure from distorting this suite's real 250ms ACK
// contract. Each individual test still uses multiple independent connections.
static PG_SPOOL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct PgFixture {
    db: Database,
    admin: AnyPool,
    url: String,
    schema: String,
    gate: i64,
    id: ArchiveSpoolIdentity,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

async fn schema_pool(url: &str, schema: &str) -> AnyPool {
    let schema = schema.to_owned();
    AnyPoolOptions::new()
        .max_connections(5)
        .after_connect(move |connection, _| {
            let schema = schema.clone();
            Box::pin(async move {
                // Identifier is solely a literal prefix plus a generated UUID.
                sqlx::query(sqlx::AssertSqlSafe(format!("SET search_path = {schema}")))
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SELECT set_config('application_name', $1, false)")
                    .bind(&schema)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("SET statement_timeout = 10000")
                    .execute(&mut *connection)
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
        .unwrap()
}

impl PgFixture {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            eprintln!("MTC_TEST_POSTGRES_URL unset; skipping real PostgreSQL spool cancellation");
            return None;
        };
        let serial = PG_SPOOL_TEST_LOCK.lock().await;
        sqlx::any::install_default_drivers();
        let admin = AnyPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let nonce = Uuid::new_v4();
        let schema = format!("spool_cancel_{}", nonce.simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let db = Database {
            pool: schema_pool(&url, &schema).await,
            backend: DatabaseBackend::PostgreSql,
            oauth_refresh_write_phase_seam: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        };
        // Same request columns exercised by the production spool API; there is
        // deliberately no FK to billing tables, matching request_records.
        sqlx::raw_sql("CREATE TABLE request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, reservation_id TEXT NOT NULL, completed_at BIGINT, response_object TEXT, status_code BIGINT NOT NULL DEFAULT 200, cost_micros BIGINT NOT NULL DEFAULT 123)")
            .execute(&db.pool).await.unwrap();
        // This focused fixture intentionally omits the production request
        // projection tables. Keeping the locator table empty exercises the
        // legacy/audit-row path where no request-stream signal is emitted.
        sqlx::raw_sql("CREATE TABLE request_record_locators (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL)")
            .execute(&db.pool).await.unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/common/0035_archive_staging_attempts.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../migrations/common/0071_response_archive_spool.sql"
        ))
        .execute(&db.pool)
        .await
        .unwrap();
        let id = ArchiveSpoolIdentity {
            request_id: nonce,
            tenant_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
        };
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, reservation_id) VALUES ($1, $2, $3)",
        )
        .bind(id.request_id.to_string())
        .bind(id.tenant_id.to_string())
        .bind(id.reservation_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
        let gate = i64::from(u32::from_be_bytes(
            nonce.as_bytes()[0..4].try_into().unwrap(),
        ));
        Some(Self {
            db,
            admin,
            url,
            schema,
            gate,
            id,
            _serial: serial,
        })
    }

    async fn capture(&self) {
        assert!(self.db.begin_response_archive_spool(self.id).await.unwrap());
        assert!(
            self.db
                .append_response_archive_spool(self.id, 0, 1, "opaque")
                .await
                .unwrap()
        );
    }

    async fn terminal(&self) {
        sqlx::query(
            "UPDATE request_records SET completed_at = 2, response_object = $1 WHERE id = $2",
        )
        .bind(format!("gap://{}/response", self.id.request_id))
        .bind(self.id.request_id.to_string())
        .execute(&self.db.pool)
        .await
        .unwrap();
    }

    async fn install_commit_barrier(&self) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE FUNCTION pause_spool_commit() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN PERFORM pg_advisory_xact_lock({}); RETURN NEW; END $body$;
             CREATE CONSTRAINT TRIGGER pause_spool_commit AFTER UPDATE ON response_archive_spools DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION pause_spool_commit();",
            self.gate
        ))).execute(&self.db.pool).await.unwrap();
    }

    async fn wait_for_commit(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND UPPER(query) LIKE 'COMMIT%' AND wait_event_type = 'Lock' AND wait_event = 'advisory'")
                    .bind(&self.schema).fetch_one(&self.admin).await.unwrap();
                if waiting == 1 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("API must reach a real PostgreSQL COMMIT blocked by deferred trigger");
    }

    async fn cancel_at_commit<T: Send + 'static>(&self, task: JoinHandle<T>) {
        self.wait_for_commit().await;
        task.abort();
        match task.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("COMMIT-blocked task unexpectedly completed before cancellation"),
        }
    }

    async fn wait_state(&self, state: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let actual: String = sqlx::query_scalar(
                    "SELECT state FROM response_archive_spools WHERE request_id = $1",
                )
                .bind(self.id.request_id.to_string())
                .fetch_one(&self.db.pool)
                .await
                .unwrap();
                if actual == state {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("released server COMMIT must finish independently of cancelled client");
    }

    async fn finish(self) {
        self.db.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

#[tokio::test]
async fn postgres_seal_precedes_terminal_delivery_and_survives_producer_loss() {
    let Some(mut fixture) = PgFixture::new().await else {
        return;
    };
    fixture.capture().await;
    fixture.install_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let db = fixture.db.clone();
    let id = fixture.id;
    let (terminal_tx, mut terminal_rx) = tokio::sync::mpsc::channel(1);
    let producer = tokio::spawn(async move {
        assert!(db.seal_response_archive_spool(id, 1, 1).await.unwrap());
        terminal_tx.send("success-terminal").await.unwrap();
    });
    fixture.wait_for_commit().await;
    assert!(matches!(
        terminal_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    // Fault window: the producer is lost inside COMMIT. No successful
    // downstream terminal was observable, even if the server later commits.
    producer.abort();
    assert!(producer.await.unwrap_err().is_cancelled());
    assert!(terminal_rx.recv().await.is_none());
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    fixture.wait_state("pending").await;
    fixture.db.close().await;
    fixture.db = Database {
        pool: schema_pool(&fixture.url, &fixture.schema).await,
        backend: DatabaseBackend::PostgreSql,
        oauth_refresh_write_phase_seam: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    };
    // Simulate the existing orphan finalizer. It remains the only owner of
    // settlement; spool recovery never edits the stored charged cost.
    fixture.terminal().await;
    let task = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.chunk_count, 1);
    assert!(
        fixture
            .db
            .load_response_archive_spool_chunk(&task, 0)
            .await
            .unwrap()
            .is_some()
    );
    let cost: i64 = sqlx::query_scalar("SELECT cost_micros FROM request_records WHERE id = $1")
        .bind(id.request_id.to_string())
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(cost, 123);
    assert!(
        fixture
            .db
            .claim_response_archive_spool(Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_seal_cancelled_inside_commit_remains_recoverable_after_reconnect() {
    let Some(mut fixture) = PgFixture::new().await else {
        return;
    };
    fixture.capture().await;
    fixture.install_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let db = fixture.db.clone();
    let id = fixture.id;
    fixture
        .cancel_at_commit(tokio::spawn(async move {
            db.seal_response_archive_spool(id, 1, 1).await
        }))
        .await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    fixture.wait_state("pending").await;
    fixture.db.close().await;
    fixture.db = Database {
        pool: schema_pool(&fixture.url, &fixture.schema).await,
        backend: DatabaseBackend::PostgreSql,
        oauth_refresh_write_phase_seam: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    };
    fixture
        .db
        .fail_response_archive_spool(id, "capture_timeout")
        .await
        .unwrap();
    fixture.terminal().await;
    let task = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.chunk_count, 1);
    assert!(
        fixture
            .db
            .load_response_archive_spool_chunk(&task, 0)
            .await
            .unwrap()
            .is_some()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_claim_cancelled_inside_commit_is_reclaimed_with_new_fence() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    fixture.capture().await;
    assert!(
        fixture
            .db
            .seal_response_archive_spool(fixture.id, 1, 1)
            .await
            .unwrap()
    );
    fixture.terminal().await;
    fixture.install_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let db = fixture.db.clone();
    let owner = Uuid::new_v4();
    fixture
        .cancel_at_commit(tokio::spawn(async move {
            db.claim_response_archive_spool(owner).await
        }))
        .await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    fixture.wait_state("uploading").await;
    let token: String = sqlx::query_scalar("SELECT lease_token FROM response_archive_spools")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert!(
        fixture
            .db
            .claim_response_archive_spool(Uuid::new_v4())
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE response_archive_spools SET lease_expires_at = 0")
        .execute(&fixture.db.pool)
        .await
        .unwrap();
    let recovered = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(recovered.lease_token.to_string(), token);
    let stale = ArchiveSpoolTask {
        identity: fixture.id,
        lease_owner: owner,
        lease_token: Uuid::parse_str(&token).unwrap(),
        chunk_count: 1,
        byte_count: 1,
    };
    assert!(
        !fixture
            .db
            .heartbeat_response_archive_spool(&stale)
            .await
            .unwrap()
    );
    assert!(
        fixture
            .db
            .load_response_archive_spool_chunk(&stale, 0)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .db
            .load_response_archive_spool_chunk(&recovered, 0)
            .await
            .unwrap()
            .is_some()
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_complete_cancelled_inside_commit_keeps_atomic_binding() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    fixture.capture().await;
    assert!(
        fixture
            .db
            .seal_response_archive_spool(fixture.id, 1, 1)
            .await
            .unwrap()
    );
    fixture.terminal().await;
    let task = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(fixture.id.request_id),
        ArchiveStagingPurpose::Response,
        Uuid::new_v4(),
    )
    .unwrap();
    let lease = match fixture
        .db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::new_v4(),
            lease_owner: ArchiveStagingLeaseOwner::new("pg-spool-test").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        _ => panic!("expected fresh staging lease"),
    };
    let locator = format!("{}/response.json", key.canonical_prefix());
    fixture.install_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let db = fixture.db.clone();
    let worker_task = task.clone();
    let worker_locator = locator.clone();
    fixture
        .cancel_at_commit(tokio::spawn(async move {
            db.complete_response_archive_spool(&worker_task, &lease, &worker_locator)
                .await
        }))
        .await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    fixture.wait_state("bound").await;
    fixture
        .db
        .retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    fixture
        .db
        .fail_response_archive_spool(fixture.id, "capture_failed")
        .await
        .unwrap();
    let row = sqlx::query("SELECT response_object, status_code, cost_micros FROM request_records")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("response_object"), locator);
    assert_eq!(row.get::<i64, _>("status_code"), 200);
    assert_eq!(row.get::<i64, _>("cost_micros"), 123);
    let bound: String = sqlx::query_scalar(
        "SELECT bound_locator FROM archive_staging_attempts WHERE attempt_id = $1",
    )
    .bind(key.attempt_id.to_string())
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    assert_eq!(bound, locator);
    assert_eq!(
        fixture
            .db
            .cleanup_response_archive_spools(32)
            .await
            .unwrap(),
        1
    );
    assert_eq!(budget(&fixture.db).await, 0);
    fixture.finish().await;
}

#[path = "postgres_load_tests.rs"]
mod postgres_load_tests;
