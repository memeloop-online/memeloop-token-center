//! Real PostgreSQL cancellation tests. The deferred trigger blocks inside the
//! server's COMMIT, not before commit and not after an already-acknowledged API.
use std::time::Duration;

use sqlx::{AnyPool, any::AnyPoolOptions};
use tokio::task::JoinHandle;

use super::*;

#[tokio::test]
async fn postgres_request_preseal_and_capture_do_not_hold_the_event_cursor() {
    use crate::{
        db::{CreateKeyInput, StartProxyRequest},
        response_archive_spool::pause_next_request_preseal_for_test,
    };

    let Some(fixture) = PgFixture::new_with_schema(true).await else {
        return;
    };
    let pepper = b"durable-admission-test-pepper-over-32-bytes";
    let issued = fixture
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "request-preseal".into(),
                principal_external_id: "member".into(),
                alias: "request-preseal".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = fixture
        .db
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = fixture
        .db
        .upsert_model_price(
            "request-preseal",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let locator = format!("gap://{request_id}/request");
    let body = bytes::Bytes::from(vec![b'x'; 16 * crate::response_archive_spool::CHUNK_BYTES]);
    let (presealed, release) = pause_next_request_preseal_for_test(request_id);
    let task_db = fixture.db.clone();
    let producer = tokio::spawn(async move {
        task_db
            .start_proxy_request_with_archive(
                StartProxyRequest {
                    request_id,
                    key: &key,
                    price: &price,
                    input_token_ceiling: 7,
                    output_token_ceiling: 11,
                    protocol: "openai",
                    model: "request-preseal",
                    request_object: &locator,
                    upstream_account_id: None,
                    model_route_id: None,
                },
                &body,
                pepper,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), presealed)
        .await
        .expect("the first ciphertext batch must finish before database admission")
        .unwrap();

    // If pre-sealing regresses below spool_transaction(), this independent
    // budget acquisition blocks and the bounded assertion fails.
    let mut budget_holder = fixture.db.pool.begin().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query(
            "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
        )
        .execute(&mut *budget_holder),
    )
    .await
    .expect("pre-sealing must not hold the global archive budget")
    .unwrap();
    let admission_facts: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM usage_reservations) + (SELECT COUNT(*) FROM request_records) + (SELECT COUNT(*) FROM request_archive_spools)",
    )
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    assert_eq!(
        admission_facts, 0,
        "pre-sealing before the transaction cannot expose admission facts"
    );

    // Pause a real insert, not a scheduler delay. An unrelated tenant must
    // publish an event while this admission owns only private capacity.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION pause_request_chunk() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN PERFORM pg_advisory_xact_lock({}); RETURN NEW; END $body$;
         CREATE TRIGGER pause_request_chunk BEFORE INSERT ON request_archive_spool_chunks FOR EACH ROW EXECUTE FUNCTION pause_request_chunk();",
        fixture.gate
    )))
    .execute(&fixture.db.pool).await.unwrap();
    let mut chunk_gate = fixture.db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *chunk_gate)
        .await
        .unwrap();

    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes +%'",
            )
            .bind(&fixture.schema)
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admission must reserve capacity before starting request work");
    budget_holder.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock' AND wait_event = 'advisory' AND query LIKE 'INSERT INTO request_archive_spool_chunks%'",
            )
            .bind(&fixture.schema).fetch_one(&fixture.admin).await.unwrap();
            if waiting == 1 { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("admission must reach the real chunk insert barrier");
    let mut available_budget = fixture.db.pool.begin().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query(
            "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
        )
        .execute(&mut *available_budget),
    )
    .await
    .expect("real buffered admission must release the shared budget before chunk insertion")
    .unwrap();
    available_budget.rollback().await.unwrap();
    let mut other_tenant = fixture.db.pool.begin().await.unwrap();
    let other_request = Uuid::new_v4().to_string();
    let other_tenant_id = Uuid::new_v4().to_string();
    let other_key_id = Uuid::new_v4().to_string();
    let cursor = tokio::time::timeout(
        Duration::from_secs(2),
        allocate_request_event_cursor(
            &mut other_tenant,
            4_000_000_000_000,
            &other_tenant_id,
            &other_key_id,
            &other_request,
        ),
    )
    .await
    .expect("archive insertion must not hold the cross-tenant event cursor")
    .unwrap();
    sqlx::query("INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'openai', 'cursor-contract', 0, 0, 0)")
        .bind(&cursor.event_id).bind(other_tenant_id).bind(other_key_id).bind(other_request).bind(cursor.event_at)
        .execute(&mut *other_tenant).await.unwrap();
    other_tenant.commit().await.unwrap();
    chunk_gate.commit().await.unwrap();
    let reservation = tokio::time::timeout(Duration::from_secs(5), producer)
        .await
        .expect("admission must finish after the budget is released")
        .unwrap()
        .unwrap();
    let admission_cursor =
        sqlx::query("SELECT event_at, event_id FROM request_events WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&fixture.db.pool)
            .await
            .unwrap();
    assert!(
        (
            admission_cursor.get::<i64, _>("event_at"),
            admission_cursor.get::<String, _>("event_id")
        ) > (cursor.event_at, cursor.event_id),
        "cursor order must follow the unrelated tenant's earlier commit"
    );
    let row = sqlx::query(
        "SELECT s.chunk_count, s.reservation_id, b.cipher_bytes, b.request_cipher_bytes FROM request_archive_spools s CROSS JOIN response_archive_spool_budget b WHERE s.request_id = $1 AND b.singleton = 1",
    )
    .bind(request_id.to_string())
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("chunk_count"), 16);
    assert_eq!(
        row.get::<String, _>("reservation_id"),
        reservation.id.to_string()
    );
    assert_eq!(
        row.get::<i64, _>("cipher_bytes"),
        row.get::<i64, _>("request_cipher_bytes")
    );
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_response_writer_does_not_block_streaming_on_the_budget_lock() {
    use crate::{AppState, config::Config, response_archive_spool::ResponseArchiveProducer};

    let Some(fixture) = PgFixture::new_with_schema(true).await else {
        return;
    };
    let application_name = format!("response-writer-{}", Uuid::new_v4());
    let mut scoped_url = url::Url::parse(&fixture.url).unwrap();
    scoped_url.query_pairs_mut().append_pair(
        "options",
        &format!(
            "-csearch_path={} -capplication_name={application_name}",
            fixture.schema
        ),
    );
    let mut config = Config::for_test(scoped_url.to_string());
    config.run_migrations_on_start = false;
    let state = AppState::initialize(config).await.unwrap();
    let now: i64 = sqlx::query_scalar(
        "SELECT CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT)",
    )
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, $4, 'responses', 'owned-writer', 0, 0, 0, $5, $6)")
        .bind(fixture.id.request_id.to_string())
        .bind(fixture.id.tenant_id.to_string())
        .bind(Uuid::new_v4().to_string())
        .bind(now)
        .bind(format!("gap://{}/request", fixture.id.request_id))
        .bind(fixture.id.reservation_id.to_string())
        .execute(&fixture.db.pool)
        .await
        .unwrap();

    let mut budget_holder = fixture.db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
    )
    .execute(&mut *budget_holder)
    .await
    .unwrap();
    let memory = state.proxy_memory_budget.reservation();
    let mut producer = ResponseArchiveProducer::begin(&state, fixture.id, memory).unwrap();
    assert!(producer.append(vec![
        bytes::Bytes::from(vec![b'x'; crate::response_archive_spool::CHUNK_BYTES]),
        bytes::Bytes::from_static(b"tail"),
    ]));
    let settlement = producer
        .seal()
        .expect("terminal handoff must not wait for the writer's budget lock");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock' AND query LIKE 'UPDATE response_archive_spool_budget SET cipher_bytes = cipher_bytes +%'",
            )
            .bind(&application_name)
            .fetch_one(&fixture.admin)
            .await
            .unwrap();
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the owned writer must independently wait at the real budget barrier");

    budget_holder.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), settlement.wait())
        .await
        .expect("the supervised writer must finish after the budget barrier is released");
    let row = sqlx::query(
        "SELECT state, chunk_count, byte_count FROM response_archive_spools WHERE request_id = $1",
    )
    .bind(fixture.id.request_id.to_string())
    .fetch_one(&fixture.db.pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(row.get::<i64, _>("chunk_count"), 2);
    assert_eq!(
        row.get::<i64, _>("byte_count"),
        crate::response_archive_spool::CHUNK_BYTES as i64 + 4
    );
    state.db.close().await;
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_concurrent_response_begins_share_one_spool_and_budget_charge() {
    let Some(fixture) = PgFixture::new().await else {
        return;
    };
    let contenders = 8;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(contenders + 1));
    let mut tasks = Vec::new();
    for _ in 0..contenders {
        let db = fixture.db.clone();
        let barrier = barrier.clone();
        let identity = fixture.id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            db.begin_response_archive_spool(identity).await
        }));
    }
    barrier.wait().await;
    for task in tasks {
        assert!(task.await.unwrap().unwrap());
    }
    let spools: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1")
            .bind(fixture.id.request_id.to_string())
            .fetch_one(&fixture.db.pool)
            .await
            .unwrap();
    assert_eq!(spools, 1);
    assert_eq!(budget(&fixture.db).await, SPOOL_OVERHEAD);
    fixture.finish().await;
}

#[tokio::test]
async fn postgres_request_admission_lost_commit_ack_never_dispatches_and_orphan_settles_once() {
    use crate::db::{CreateKeyInput, StartProxyRequest};
    let Some(mut fixture) = PgFixture::new_with_schema(true).await else {
        return;
    };
    let db = &fixture.db;
    let pepper = b"durable-admission-test-pepper-over-32-bytes";
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "durable-admission".into(),
                principal_external_id: "member".into(),
                alias: "durable-admission".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = db.authenticate_key(&issued.key, pepper).await.unwrap();
    let price = db
        .upsert_model_price(
            "durable-admission",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    fixture.install_request_admission_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let task_db = fixture.db.clone();
    let task_key = key.clone();
    let task_price = price.clone();
    let dispatches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let task_dispatches = dispatches.clone();
    let producer = tokio::spawn(async move {
        let locator = format!("gap://{request_id}/request");
        let body = bytes::Bytes::from_static(b"{\"private\":\"request\"}");
        let admitted = task_db
            .start_proxy_request_with_archive(
                StartProxyRequest {
                    request_id,
                    key: &task_key,
                    price: &task_price,
                    input_token_ceiling: 7,
                    output_token_ceiling: 11,
                    protocol: "openai",
                    model: "durable-admission",
                    request_object: &locator,
                    upstream_account_id: None,
                    model_route_id: None,
                },
                &body,
                pepper,
            )
            .await;
        if admitted.is_ok() {
            task_dispatches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        admitted
    });
    fixture.wait_for_commit().await;
    assert_eq!(dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Lose the API future while PostgreSQL is executing the actual COMMIT.
    // This is an unknown ACK, not a fabricated post-success application error.
    producer.abort();
    assert!(producer.await.unwrap_err().is_cancelled());
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let committed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records r JOIN request_archive_spools s ON s.request_id = r.id AND s.reservation_id = r.reservation_id JOIN usage_reservations u ON u.id = r.reservation_id WHERE r.id = $1 AND s.state = 'pending' AND u.status = 'reserved'").bind(request_id.to_string()).fetch_one(&fixture.db.pool).await.unwrap();
            if committed == 1 { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("server finishes atomic admission after client loses COMMIT ACK");
    fixture.db.close().await;
    fixture.db = Database {
        pool: schema_pool(&fixture.url, &fixture.schema).await,
        backend: DatabaseBackend::PostgreSql,
        oauth_refresh_write_phase_seam: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
    };
    assert!(
        fixture
            .db
            .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE usage_reservations SET created_at = 0 WHERE id = (SELECT reservation_id FROM request_records WHERE id = $1)").bind(request_id.to_string()).execute(&fixture.db.pool).await.unwrap();
    assert_eq!(
        fixture.db.release_orphaned_reservations(32).await.unwrap(),
        1
    );
    assert_eq!(
        fixture.db.release_orphaned_reservations(32).await.unwrap(),
        0
    );
    let row = sqlx::query("SELECT status_code, cost_micros, input_tokens, output_tokens FROM request_records WHERE id = $1").bind(request_id.to_string()).fetch_one(&fixture.db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("status_code"), 504);
    assert_eq!(row.get::<i64, _>("cost_micros"), 0);
    assert_eq!(row.get::<i64, _>("input_tokens"), 0);
    assert_eq!(row.get::<i64, _>("output_tokens"), 0);
    assert_eq!(
        dispatches.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "unknown admission ACK never reaches the upstream-dispatch continuation"
    );
    assert!(
        fixture
            .db
            .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_some()
    );
    // Also exercise a real server-side connection loss, not only caller
    // cancellation. Terminate only this isolated fixture's COMMIT backend.
    let disconnected_id = Uuid::new_v4();
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let task_db = fixture.db.clone();
    let task_dispatches = dispatches.clone();
    let disconnected = tokio::spawn(async move {
        let locator = format!("gap://{disconnected_id}/request");
        let result = task_db
            .start_proxy_request_with_archive(
                StartProxyRequest {
                    request_id: disconnected_id,
                    key: &key,
                    price: &price,
                    input_token_ceiling: 7,
                    output_token_ceiling: 11,
                    protocol: "openai",
                    model: "durable-admission",
                    request_object: &locator,
                    upstream_account_id: None,
                    model_route_id: None,
                },
                &bytes::Bytes::from_static(b"{\"private\":\"request\"}"),
                pepper,
            )
            .await;
        if result.is_ok() {
            task_dispatches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        result
    });
    fixture.wait_for_commit().await;
    let killed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT pg_terminate_backend(pid) AS terminated FROM pg_stat_activity WHERE application_name = $1 AND UPPER(query) LIKE 'COMMIT%' AND wait_event_type = 'Lock' AND wait_event = 'advisory') stopped WHERE terminated")
        .bind(&fixture.schema).fetch_one(&fixture.admin).await.unwrap();
    assert_eq!(killed, 1);
    assert!(matches!(
        disconnected.await.unwrap(),
        Err(AppError::Overloaded)
    ));
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_records WHERE id = $1")
        .bind(disconnected_id.to_string())
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(
        reservations, 1,
        "disconnected admission rolled back its reservation"
    );
    assert_eq!(dispatches.load(std::sync::atomic::Ordering::SeqCst), 0);
    fixture.finish().await;
}
#[tokio::test]
async fn postgres_durable_admission_rollback_ha_and_archive_bind() {
    use crate::db::{CreateKeyInput, StartProxyRequest};
    let Some(fixture) = PgFixture::new_with_schema(true).await else {
        return;
    };
    let db = &fixture.db;
    let pepper = b"durable-admission-test-pepper-over-32-bytes";
    let issued = db
        .create_key(
            CreateKeyInput {
                tenant_external_id: "durable-admission".into(),
                principal_external_id: "member".into(),
                alias: "durable-admission".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = db.authenticate_key(&issued.key, pepper).await.unwrap();
    let price = db
        .upsert_model_price(
            "durable-admission",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    let request_id = Uuid::new_v4();
    let locator = format!("gap://{request_id}/request");
    let input = || StartProxyRequest {
        request_id,
        key: &key,
        price: &price,
        input_token_ceiling: 7,
        output_token_ceiling: 11,
        protocol: "openai",
        model: "durable-admission",
        request_object: &locator,
        upstream_account_id: None,
        model_route_id: None,
    };
    let body = bytes::Bytes::from(format!(
        "{{\"private\":\"{}\"}}",
        "x".repeat(128 * 64 * 1024)
    ));
    // seq 128 is the first row of the second real multirow INSERT.
    sqlx::raw_sql("CREATE FUNCTION reject_admission() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.seq = 128 THEN RAISE EXCEPTION 'test admission rollback'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_admission_chunk BEFORE INSERT ON request_archive_spool_chunks FOR EACH ROW EXECUTE FUNCTION reject_admission()").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.start_proxy_request_with_archive(input(), &body, pepper)
            .await,
        Err(AppError::Overloaded)
    ));
    let reservations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(reservations, 0);
    for table in [
        "request_records",
        "request_record_locators",
        "request_events",
    ] {
        let column = if table == "request_events" {
            "request_id"
        } else {
            "id"
        };
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} = $1"
        )))
        .bind(request_id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while budget(db).await != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failed admission must return its independently reserved capacity");
    sqlx::query("DROP TRIGGER reject_admission_chunk ON request_archive_spool_chunks")
        .execute(&db.pool)
        .await
        .unwrap();
    let reservation = db
        .start_proxy_request_with_archive(input(), &body, pepper)
        .await
        .unwrap();
    let row = sqlx::query("SELECT s.state, s.reservation_id, s.chunk_count, r.completed_at FROM request_archive_spools s JOIN request_records r ON r.id = s.request_id WHERE s.request_id = $1").bind(request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.get::<i64, _>("chunk_count"), 129);
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(
        row.get::<String, _>("reservation_id"),
        reservation.id.to_string()
    );
    assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
    assert!(
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
            .await
            .unwrap()
            .is_none()
    );

    sqlx::query("UPDATE request_records SET completed_at = created_at + 1, status_code = 200, cost_micros = 123 WHERE id = $1").bind(request_id.to_string()).execute(&db.pool).await.unwrap();
    let (left, right) = tokio::join!(
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true),
        db.claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true),
    );
    let tasks: Vec<_> = [left.unwrap(), right.unwrap()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(tasks.len(), 1, "exactly one HA worker acquires the request");
    let stale = &tasks[0];
    sqlx::query("UPDATE request_archive_spools SET lease_expires_at = 0 WHERE request_id = $1")
        .bind(request_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let recovered = db
        .claim_archive_spool_if(Uuid::new_v4(), BufferedArchivePurpose::Request, || true)
        .await
        .unwrap()
        .unwrap();
    assert!(!db.heartbeat_response_archive_spool(stale).await.unwrap());
    assert!(
        db.load_response_archive_spool_batch(stale, 0)
            .await
            .unwrap()
            .is_empty()
    );
    let attempt = crate::proxy_lifecycle::begin_proxy_archive_attempt(
        db,
        request_id,
        ArchiveStagingPurpose::Request,
    )
    .await
    .unwrap();
    assert!(
        !db.complete_response_archive_spool(stale, &attempt.lease, &attempt.object_locator)
            .await
            .unwrap()
    );
    assert!(
        db.complete_response_archive_spool(&recovered, &attempt.lease, &attempt.object_locator)
            .await
            .unwrap()
    );
    db.retry_response_archive_spool(&recovered, "upload_failed")
        .await
        .unwrap();
    assert!(
        !db.complete_response_archive_spool(&recovered, &attempt.lease, &attempt.object_locator)
            .await
            .unwrap()
    );
    let terminal = sqlx::query(
        "SELECT request_object, status_code, cost_micros FROM request_records WHERE id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        terminal.get::<String, _>("request_object"),
        attempt.object_locator
    );
    assert_eq!(terminal.get::<i64, _>("status_code"), 200);
    assert_eq!(terminal.get::<i64, _>("cost_micros"), 123);
    assert_eq!(db.cleanup_response_archive_spools(32).await.unwrap(), 1);
    let outstanding: i64 = sqlx::query_scalar(
        "SELECT request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(outstanding, 0);
    fixture.finish().await;
}
use crate::archive_staging::{
    ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
    BeginArchiveStagingInput, BeginArchiveStagingResult,
};

// Keep bulk-seed disk pressure from distorting this suite's ordering-sensitive
// PostgreSQL contracts. Each test still uses multiple independent connections.
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
        Self::new_with_schema(false).await
    }

    async fn new_with_schema(full_schema: bool) -> Option<Self> {
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
        let id = ArchiveSpoolIdentity {
            request_id: nonce,
            tenant_id: Uuid::new_v4(),
            reservation_id: Uuid::new_v4(),
        };
        if full_schema {
            db.migrate().await.unwrap();
        } else {
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
            sqlx::raw_sql(include_str!(
                "../../../migrations/common/0080_request_archive_spool.sql"
            ))
            .execute(&db.pool)
            .await
            .unwrap();
            sqlx::raw_sql(include_str!(
                "../../../migrations/common/0106_archive_budget_reservations.sql"
            ))
            .execute(&db.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO request_records (id, tenant_id, reservation_id) VALUES ($1, $2, $3)",
            )
            .bind(id.request_id.to_string())
            .bind(id.tenant_id.to_string())
            .bind(id.reservation_id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
        }
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

    async fn install_request_admission_commit_barrier(&self) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE FUNCTION pause_request_admission_commit() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN PERFORM pg_advisory_xact_lock({}); RETURN NEW; END $body$;
             CREATE CONSTRAINT TRIGGER pause_request_admission_commit AFTER INSERT ON request_archive_spools DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION pause_request_admission_commit();",
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
async fn postgres_claims_ignore_busy_budget_and_still_claim_once() {
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
    let mut blocker = fixture.db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1 FOR UPDATE",
    )
    .fetch_one(&mut *blocker)
    .await
    .unwrap();
    let first_db = fixture.db.clone();
    let first = tokio::spawn(async move {
        crate::response_archive_spool::observed_claim_for_test(&first_db, Uuid::new_v4()).await
    });
    let second_db = fixture.db.clone();
    let second = tokio::spawn(async move {
        crate::response_archive_spool::observed_claim_for_test(&second_db, Uuid::new_v4()).await
    });
    // The budget row remains locked throughout both claims. Lease-only state
    // transitions must serialize on the candidate spool row instead, so a
    // slow producer/GC accounting transaction cannot stall the upload worker.
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("claims must not wait for the unrelated global budget lock");
    let tasks = [first.unwrap().unwrap(), second.unwrap().unwrap()];
    assert_eq!(tasks.iter().filter(|task| task.is_some()).count(), 1);
    let row = sqlx::query("SELECT state, attempts, lease_token FROM response_archive_spools")
        .fetch_one(&fixture.db.pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("state"), "uploading");
    assert_eq!(row.get::<i64, _>("attempts"), 1);
    let winner = tasks.into_iter().flatten().next().unwrap();
    assert_eq!(
        row.get::<String, _>("lease_token"),
        winner.lease_token.to_string()
    );
    blocker.commit().await.unwrap();
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
        purpose: BufferedArchivePurpose::Response,
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

    // Reproduce a lease that expires while heartbeat is waiting for the spool
    // row. Validation must use a database clock read after the row lock, not a
    // timestamp captured before the wait.
    let mut expiry_blocker = fixture.db.pool.begin().await.unwrap();
    sqlx::query("SELECT request_id FROM response_archive_spools WHERE request_id = $1 FOR UPDATE")
        .bind(fixture.id.request_id.to_string())
        .fetch_one(&mut *expiry_blocker)
        .await
        .unwrap();
    let heartbeat_db = fixture.db.clone();
    let heartbeat_task = recovered.clone();
    let heartbeat = tokio::spawn(async move {
        heartbeat_db
            .heartbeat_response_archive_spool(&heartbeat_task)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_stat_activity WHERE application_name = $1 AND query LIKE 'SELECT * FROM response_archive_spools%' AND wait_event_type = 'Lock'")
                .bind(&fixture.schema)
                .fetch_one(&fixture.admin)
                .await
                .unwrap();
            if waiting == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("heartbeat must wait on the real spool row lock");
    // Set expiry only after the waiter has captured any pre-lock timestamp.
    // The broken ordering accepts this lease; the lock-then-clock ordering
    // observes a clock greater than or equal to this committed expiry.
    sqlx::query("UPDATE response_archive_spools SET lease_expires_at = CAST(FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000) AS BIGINT) WHERE request_id = $1")
        .bind(fixture.id.request_id.to_string())
        .execute(&mut *expiry_blocker)
        .await
        .unwrap();
    expiry_blocker.commit().await.unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), heartbeat)
            .await
            .expect("heartbeat must finish after the row lock is released")
            .unwrap()
            .unwrap()
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
#[path = "postgres_reservation_tests.rs"]
mod postgres_reservation_tests;
