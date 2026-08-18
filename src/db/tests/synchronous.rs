use super::super::*;

#[tokio::test]
async fn expired_synchronous_image_early_claim_is_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("image-idempotency-lease.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"image idempotency lease test pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "image-idempotency-lease".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "image-idempotency-lease".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let idempotency = GenerationJobIdempotency {
        key: "crash-recovery".to_owned(),
        request_hash: "a".repeat(64),
    };
    let first_request = Uuid::now_v7();
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(issued.key_id, &idempotency, first_request,)
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Claimed
    ));
    database
        .renew_synchronous_image_idempotency_claim(issued.key_id, &idempotency.key, first_request)
        .await
        .unwrap();
    let renewed_until: i64 = sqlx::query_scalar(
            "SELECT lease_expires_at FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2",
        )
        .bind(issued.key_id.to_string())
        .bind(&idempotency.key)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert!(renewed_until > unix_millis());
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(
                issued.key_id,
                &idempotency,
                Uuid::now_v7(),
            )
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Pending { request_id }
            if request_id == first_request
    ));
    sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind(issued.key_id.to_string())
        .bind(&idempotency.key)
        .execute(&database.pool)
        .await
        .unwrap();
    let takeover_request = Uuid::now_v7();
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(issued.key_id, &idempotency, takeover_request,)
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Claimed
    ));
    let stored_owner: String = sqlx::query_scalar(
            "SELECT request_id FROM synchronous_image_idempotency WHERE key_id = $1 AND idempotency_key = $2",
        )
        .bind(issued.key_id.to_string())
        .bind(&idempotency.key)
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(stored_owner, first_request.to_string());
}

async fn synchronous_image_atomic_fixture(
    database_name: &str,
) -> (tempfile::TempDir, Database, AuthenticatedKey, ModelPrice) {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(database_name).display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"atomic synchronous image test pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: database_name.to_owned(),
                principal_external_id: "member".to_owned(),
                alias: database_name.to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::new(1, 3),
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
        .upsert_generation_price("atomic-image", "USD", "image", Decimal::new(1, 3))
        .await
        .unwrap()
        .reservation_price()
        .unwrap();
    (directory, database, key, price)
}

#[tokio::test]
async fn synchronous_image_early_claim_start_and_expired_takeover_refund_are_atomic() {
    let (_directory, database, key, price) =
        synchronous_image_atomic_fixture("image-atomic-start.db").await;
    let idempotency = GenerationJobIdempotency {
        key: "early-claim-crash".to_owned(),
        request_hash: "b".repeat(64),
    };
    let first_request_id = Uuid::now_v7();
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(key.key_id, &idempotency, first_request_id)
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Claimed
    ));
    let first_reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id: first_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/first-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("early claim must atomically start, got {replay:?}"),
    };
    assert_eq!(first_reservation.reserved_micros, 1_000);

    sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind(key.key_id.to_string())
        .bind(&idempotency.key)
        .execute(&database.pool)
        .await
        .unwrap();
    let second_request_id = Uuid::now_v7();
    let second_reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id: second_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/second-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("expired reserved owner must be refunded and taken over: {replay:?}"),
    };
    let old = sqlx::query(
            "SELECT q.status_code, q.error_code, r.status AS reservation_status, r.actual_micros FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
        )
        .bind(first_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(old.get::<i64, _>("status_code"), 502);
    assert_eq!(
        old.get::<String, _>("error_code"),
        "idempotency_claim_expired"
    );
    assert_eq!(old.get::<String, _>("reservation_status"), "settled");
    assert_eq!(old.get::<i64, _>("actual_micros"), 0);
    let account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(key.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(account.get::<i64, _>("available_micros"), 0);
    assert_eq!(account.get::<i64, _>("reserved_micros"), 1_000);

    assert!(matches!(
        database
            .finish_synchronous_image_request(FinishSynchronousImageRequest {
                key_id: key.key_id,
                idempotency_key: Some(&idempotency.key),
                request_id: second_request_id,
                reservation: &second_reservation,
                status_code: 200,
                duration_ms: 25,
                input_tokens: 0,
                output_tokens: 1,
                error_code: None,
                response_object: "objects/blake3/final-image-response",
                assets: &[],
            })
            .await
            .unwrap(),
        FinishSynchronousImageResult::Finished { cost_micros: 1_000 }
    ));
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(
                key.key_id,
                &idempotency,
                Uuid::now_v7()
            )
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Completed {
            request_id,
            response_status: 200,
            response_object,
        } if request_id == second_request_id
            && response_object == "objects/blake3/final-image-response"
    ));
    let charged: i64 = sqlx::query_scalar(
            "SELECT COALESCE(-SUM(amount_micros), 0) FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(charged, 1_000);
}

#[tokio::test]
async fn synchronous_image_terminal_crash_is_recovered_without_second_charge() {
    let (_directory, database, key, price) =
        synchronous_image_atomic_fixture("image-terminal-recovery.db").await;
    let idempotency = GenerationJobIdempotency {
        key: "terminal-crash".to_owned(),
        request_hash: "c".repeat(64),
    };
    let request_id = Uuid::now_v7();
    let reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/terminal-crash-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("first request must start: {replay:?}"),
    };
    let cost_micros = database.settle_usage(&reservation, 0, 1).await.unwrap();
    assert_eq!(cost_micros, 1_000);
    database
        .record_request_finished(FinishRequest {
            request_id,
            status_code: 200,
            duration_ms: 50,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 1,
            service_tier: None,
            cost_micros,
            error_code: None,
            response_object: "objects/blake3/recoverable-response".to_owned(),
        })
        .await
        .unwrap();
    sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind(key.key_id.to_string())
        .bind(&idempotency.key)
        .execute(&database.pool)
        .await
        .unwrap();

    let recovery_request_id = Uuid::now_v7();
    assert!(matches!(
        database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                request_id: recovery_request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: Some(&idempotency),
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/must-not-start",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap(),
        StartSynchronousImageResult::Replay(
            SynchronousImageIdempotencyClaim::Completed {
                request_id: replay_request_id,
                response_status: 200,
                response_object,
            }
        ) if replay_request_id == request_id
            && response_object == "objects/blake3/recoverable-response"
    ));
    let reservation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(reservation_count, 1);
    let charged: i64 = sqlx::query_scalar(
            "SELECT COALESCE(-SUM(amount_micros), 0) FROM ledger_entries WHERE key_id = $1 AND kind = 'usage'",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(charged, 1_000);
    let stats_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(stats_count, 1);
}

#[tokio::test]
async fn synchronous_image_without_idempotency_has_atomic_start_and_terminal_writes() {
    let (_directory, database, key, price) =
        synchronous_image_atomic_fixture("image-without-idempotency.db").await;
    let conflicting_request_id = Uuid::now_v7();
    sqlx::query(
            "INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(conflicting_request_id.to_string())
        .bind(unix_millis())
        .bind(key.tenant_id.to_string())
        .bind(key.key_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                request_id: conflicting_request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: None,
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/conflicting-request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .is_err()
    );
    let reservations_after_failure: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(reservations_after_failure, 0);

    let failed_request_id = Uuid::now_v7();
    let failed_reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id: failed_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: None,
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/failed-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("non-idempotent request cannot replay: {replay:?}"),
    };
    assert!(matches!(
        database
            .finish_synchronous_image_request(FinishSynchronousImageRequest {
                key_id: key.key_id,
                idempotency_key: None,
                request_id: failed_request_id,
                reservation: &failed_reservation,
                status_code: 502,
                duration_ms: 10,
                input_tokens: 0,
                output_tokens: 0,
                error_code: Some("upstream_connection"),
                response_object: "gap://failed/response",
                assets: &[],
            })
            .await
            .unwrap(),
        FinishSynchronousImageResult::Finished { cost_micros: 0 }
    ));
    let refunded =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(key.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(refunded.get::<i64, _>("available_micros"), 1_000);
    assert_eq!(refunded.get::<i64, _>("reserved_micros"), 0);

    let success_request_id = Uuid::now_v7();
    let success_reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id: success_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: None,
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/success-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("non-idempotent request cannot replay: {replay:?}"),
    };
    let asset = ArchivedGenerationAsset {
        asset_id: Uuid::now_v7(),
        index: 0,
        object_locator: "objects/blake3/success-asset".to_owned(),
        mime_type: "image/png".to_owned(),
        size_bytes: 128,
        filename: "success.png".to_owned(),
    };
    sqlx::query(
            "CREATE TRIGGER fail_synchronous_image_terminal BEFORE UPDATE OF completed_at ON request_records WHEN NEW.completed_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'injected terminal failure'); END",
        )
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(
        database
            .finish_synchronous_image_request(FinishSynchronousImageRequest {
                key_id: key.key_id,
                idempotency_key: None,
                request_id: success_request_id,
                reservation: &success_reservation,
                status_code: 200,
                duration_ms: 20,
                input_tokens: 0,
                output_tokens: 1,
                error_code: None,
                response_object: "objects/blake3/success-response",
                assets: std::slice::from_ref(&asset),
            })
            .await
            .is_err()
    );
    let rolled_back_reservation =
        sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
            .bind(success_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        rolled_back_reservation.get::<String, _>("status"),
        "reserved"
    );
    assert!(
        rolled_back_reservation
            .get::<Option<i64>, _>("actual_micros")
            .is_none()
    );
    let rolled_back_account =
        sqlx::query("SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1")
            .bind(key.account_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(rolled_back_account.get::<i64, _>("available_micros"), 0);
    assert_eq!(rolled_back_account.get::<i64, _>("reserved_micros"), 1_000);
    let rolled_back_side_effects: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM ledger_entries WHERE source = $1) + (SELECT COUNT(*) FROM generation_assets WHERE request_id = $2) + (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $3)",
        )
        .bind(success_reservation.id.to_string())
        .bind(success_request_id.to_string())
        .bind(success_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(rolled_back_side_effects, 0);
    let still_pending: Option<i64> =
        sqlx::query_scalar("SELECT completed_at FROM request_records WHERE id = $1")
            .bind(success_request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert!(still_pending.is_none());
    sqlx::query("DROP TRIGGER fail_synchronous_image_terminal")
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(matches!(
        database
            .finish_synchronous_image_request(FinishSynchronousImageRequest {
                key_id: key.key_id,
                idempotency_key: None,
                request_id: success_request_id,
                reservation: &success_reservation,
                status_code: 200,
                duration_ms: 20,
                input_tokens: 0,
                output_tokens: 1,
                error_code: None,
                response_object: "objects/blake3/success-response",
                assets: std::slice::from_ref(&asset),
            })
            .await
            .unwrap(),
        FinishSynchronousImageResult::Finished { cost_micros: 1_000 }
    ));
    let pending_requests: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_records WHERE key_id = $1 AND completed_at IS NULL",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(pending_requests, 0);
    let claims: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM synchronous_image_idempotency WHERE key_id = $1")
            .bind(key.key_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(claims, 0);
}

#[tokio::test]
async fn synchronous_image_reaper_failure_allows_same_hash_takeover() {
    let (_directory, database, key, price) =
        synchronous_image_atomic_fixture("image-reaper-takeover.db").await;
    let idempotency = GenerationJobIdempotency {
        key: "reaper-retry".to_owned(),
        request_hash: "d".repeat(64),
    };
    let old_request_id = Uuid::now_v7();
    let old_reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id: old_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/reaped-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("first request must start: {replay:?}"),
    };
    database.settle_usage(&old_reservation, 0, 0).await.unwrap();
    database
        .record_request_finished(FinishRequest {
            request_id: old_request_id,
            status_code: 504,
            duration_ms: 1_800_001,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            service_tier: None,
            cost_micros: 0,
            error_code: Some("request_expired".to_owned()),
            response_object: format!("gap://{old_request_id}/response"),
        })
        .await
        .unwrap();
    sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind(key.key_id.to_string())
        .bind(&idempotency.key)
        .execute(&database.pool)
        .await
        .unwrap();
    let new_request_id = Uuid::now_v7();
    assert!(matches!(
        database
            .claim_synchronous_image_idempotency(key.key_id, &idempotency, new_request_id)
            .await
            .unwrap(),
        SynchronousImageIdempotencyClaim::Claimed
    ));
    assert!(matches!(
        database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                request_id: new_request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: Some(&idempotency),
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/retry-after-reaper",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap(),
        StartSynchronousImageResult::Started(_)
    ));
    let mismatched = GenerationJobIdempotency {
        key: idempotency.key.clone(),
        request_hash: "e".repeat(64),
    };
    assert!(matches!(
        database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                request_id: Uuid::now_v7(),
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: Some(&mismatched),
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/mismatched",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));
}

#[tokio::test]
async fn generic_reaper_skips_active_synchronous_image_lease() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("proxy-reaper-image-lease.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy reaper image lease pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-reaper-image".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-reaper-image".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
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
        .upsert_model_price("sync-image-reaper", "USD", Decimal::ZERO, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let idempotency = GenerationJobIdempotency {
        key: "active-image".to_owned(),
        request_hash: "a".repeat(64),
    };
    let reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "sync-image-reaper",
            request_object: "objects/blake3/sync-image-reaper",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        replay => panic!("unexpected synchronous image start: {replay:?}"),
    };
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.release_orphaned_reservations(10).await.unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        "reserved"
    );

    sqlx::query(
            "UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE key_id = $2 AND idempotency_key = $3",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind(key.key_id.to_string())
        .bind(&idempotency.key)
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.release_orphaned_reservations(10).await.unwrap(), 1);
    let terminal = sqlx::query(
            "SELECT q.status_code, q.error_code, r.status AS reservation_status FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(terminal.get::<i64, _>("status_code"), 504);
    assert_eq!(terminal.get::<String, _>("error_code"), "request_expired");
    assert_eq!(terminal.get::<String, _>("reservation_status"), "settled");

    let crashed_request_id = Uuid::now_v7();
    let staging_object = format!("staging://blake3/{}", "b".repeat(64));
    let archived_object = format!("staging/proxy/{crashed_request_id}/request.bin");
    let crashed_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: crashed_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            protocol: "openai",
            model: "sync-image-reaper",
            request_object: &staging_object,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(crashed_reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.release_orphaned_reservations(10).await.unwrap(), 1);
    assert!(matches!(
        database
            .attach_proxy_request_archive(
                crashed_request_id,
                key.tenant_id,
                crashed_reservation.id,
                &staging_object,
                &archived_object,
            )
            .await,
        Err(AppError::Conflict(_))
    ));

    let prepared_request_id = Uuid::now_v7();
    let prepared_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: prepared_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            protocol: "openai",
            model: "sync-image-reaper",
            request_object: "objects/blake3/prepared-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database
        .prepare_proxy_delivery(
            prepared_request_id,
            key.tenant_id,
            &prepared_reservation,
            0,
            1,
            Some("default"),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(prepared_reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.release_orphaned_reservations(10).await.unwrap(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(prepared_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        0,
        "a prepared delivery never proves that a byte was enqueued"
    );

    let delivered_request_id = Uuid::now_v7();
    let delivered_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: delivered_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            protocol: "openai",
            model: "sync-image-reaper",
            request_object: "objects/blake3/delivered-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database
        .prepare_proxy_delivery(
            delivered_request_id,
            key.tenant_id,
            &delivered_reservation,
            0,
            1,
            Some("default"),
        )
        .await
        .unwrap();
    database
        .mark_proxy_delivery_started(delivered_request_id, key.tenant_id, &delivered_reservation)
        .await
        .unwrap();
    sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
        .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
        .bind(delivered_reservation.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(database.release_orphaned_reservations(10).await.unwrap(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(delivered_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        1,
        "a confirmed delivery is conservatively charged to its contract ceiling"
    );
}
