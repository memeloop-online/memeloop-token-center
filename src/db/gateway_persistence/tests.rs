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
async fn saturated_slots_and_bytes_degrade_without_waiting_or_losing_safety_state() {
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
            .expect("saturation must never await capacity")
            .unwrap();
        assert_eq!(
            admitted.archive_admission,
            RequestArchiveAdmission::GapCapacity
        );
        fixture.assert_reserved(request_id).await;
        drop(permits);
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
    assert!(metrics.contains("request_persistence_total{outcome=\"capacity\"} 2\n"));
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
        Err(AppError::Overloaded)
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
