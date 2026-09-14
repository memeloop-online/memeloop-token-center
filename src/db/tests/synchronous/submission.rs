use super::*;

#[tokio::test]
async fn durable_send_blocks_duplicate_owner_takeover_and_generic_refund() {
    for with_idempotency in [true, false] {
        let (_directory, database, key, price) =
            synchronous_image_atomic_fixture("image-send-fence.db").await;
        let request_id = Uuid::now_v7();
        let idempotency = GenerationJobIdempotency {
            key: "send-once".into(),
            request_hash: "a".repeat(64),
        };
        let idempotency = with_idempotency.then_some(&idempotency);
        let snapshot = serde_json::json!({"fixture":"pinned strategy"});
        let reservation = match database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                routing_snapshot: Some(&snapshot),
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency,
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/sent-request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap()
        {
            StartSynchronousImageResult::Started(reservation) => reservation,
            other => panic!("unexpected start: {other:?}"),
        };
        let stored: String =
            sqlx::query_scalar("SELECT routing_snapshot FROM request_records WHERE id = $1")
                .bind(request_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).unwrap(),
            snapshot
        );
        assert!(
            !database
                .confirm_synchronous_image_submission_started(
                    key.key_id,
                    request_id,
                    reservation.id
                )
                .await
                .unwrap()
        );
        assert!(matches!(
            database
                .confirm_synchronous_image_submission_started(
                    key.key_id,
                    request_id,
                    Uuid::now_v7()
                )
                .await,
            Err(AppError::NotFound)
        ));
        assert!(
            database
                .arm_synchronous_image_submission(
                    Uuid::now_v7(),
                    idempotency.map(|i| i.key.as_str()),
                    request_id,
                    reservation.id
                )
                .await
                .is_err()
        );
        assert!(
            database
                .arm_synchronous_image_submission(
                    key.key_id,
                    idempotency.map(|i| i.key.as_str()),
                    request_id,
                    Uuid::now_v7()
                )
                .await
                .is_err()
        );
        if with_idempotency {
            assert!(
                database
                    .arm_synchronous_image_submission(key.key_id, None, request_id, reservation.id)
                    .await
                    .is_err()
            );
        }
        database
            .arm_synchronous_image_submission(
                key.key_id,
                idempotency.map(|i| i.key.as_str()),
                request_id,
                reservation.id,
            )
            .await
            .unwrap();
        let generic_expiry = database
            .finish_proxy_request(FinishProxyRequest {
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id,
                tenant_id: key.tenant_id,
                reservation: &reservation,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                requested_service_tier: None,
                status_code: 504,
                duration_ms: 100,
                usage: TokenUsage::default(),
                charge_contract_ceiling: false,
                error_code: Some("request_expired"),
                response_object: "gap://expired/response",
                conversation: None,
            })
            .await;
        assert!(
            database
                .confirm_synchronous_image_submission_started(
                    key.key_id,
                    request_id,
                    reservation.id
                )
                .await
                .unwrap()
        );
        assert!(
            matches!(generic_expiry, Err(AppError::Conflict(_))),
            "a reaper selected before arm must still be fenced at settlement"
        );
        assert!(
            database
                .arm_synchronous_image_submission(
                    key.key_id,
                    idempotency.map(|i| i.key.as_str()),
                    request_id,
                    reservation.id
                )
                .await
                .is_err()
        );
        let old = unix_millis() - 31 * 60 * 1000;
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(old)
            .bind(reservation.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        if let Some(idempotency) = idempotency {
            sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = $1 WHERE request_id = $2")
                .bind(old).bind(request_id.to_string()).execute(&database.pool).await.unwrap();
            assert!(
                matches!(database.lookup_synchronous_image_idempotency(key.key_id, idempotency).await.unwrap(), Some(SynchronousImageIdempotencyClaim::Uncertain { request_id: id }) if id == request_id)
            );
            assert!(matches!(
                database
                    .claim_synchronous_image_idempotency(key.key_id, idempotency, Uuid::now_v7())
                    .await
                    .unwrap(),
                SynchronousImageIdempotencyClaim::Uncertain { .. }
            ));
            assert!(matches!(
                database
                    .start_synchronous_image_request(StartSynchronousImageRequest {
                        routing_snapshot: None,
                        request_id: Uuid::now_v7(),
                        key: &key,
                        price: &price,
                        input_token_ceiling: 0,
                        output_token_ceiling: 1,
                        idempotency: Some(idempotency),
                        protocol: "openai-image",
                        model: "atomic-image",
                        request_object: "objects/blake3/retry",
                        upstream_account_id: None,
                        model_route_id: None,
                    })
                    .await
                    .unwrap(),
                StartSynchronousImageResult::Replay(
                    SynchronousImageIdempotencyClaim::Uncertain { .. }
                )
            ));
        }
        database
            .quarantine_synchronous_image_submission(
                key.key_id,
                idempotency.map(|i| i.key.as_str()),
                request_id,
                reservation.id,
            )
            .await
            .unwrap();
        assert_eq!(
            database.release_orphaned_reservations(100).await.unwrap(),
            0
        );
        let row = sqlx::query("SELECT q.submission_started_at, q.submission_uncertain_at, q.completed_at, q.error_code, q.request_object, r.status FROM request_records q JOIN usage_reservations r ON r.id = q.reservation_id WHERE q.id = $1")
            .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(row.get::<Option<i64>, _>("submission_started_at").is_some());
        assert!(
            row.get::<Option<i64>, _>("submission_uncertain_at")
                .is_some()
        );
        assert!(row.get::<Option<i64>, _>("completed_at").is_none());
        assert_eq!(row.get::<String, _>("status"), "reserved");
        assert_eq!(
            row.get::<String, _>("error_code"),
            "image_submission_uncertain"
        );
        assert_eq!(
            row.get::<String, _>("request_object"),
            "objects/blake3/sent-request"
        );
    }
}

#[tokio::test]
async fn expired_owner_cannot_arm_send_and_snapshot_limit_precedes_reservation() {
    let (_directory, database, key, price) =
        synchronous_image_atomic_fixture("image-send-expired.db").await;
    let request_id = Uuid::now_v7();
    let idempotency = GenerationJobIdempotency {
        key: "expired-send".into(),
        request_hash: "a".repeat(64),
    };
    let oversized = serde_json::Value::String("x".repeat(1024 * 1024));
    assert!(matches!(
        database
            .start_synchronous_image_request(StartSynchronousImageRequest {
                routing_snapshot: Some(&oversized),
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: Some(&idempotency),
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_reservations")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let reservation = match database
        .start_synchronous_image_request(StartSynchronousImageRequest {
            routing_snapshot: None,
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 0,
            output_token_ceiling: 1,
            idempotency: Some(&idempotency),
            protocol: "openai-image",
            model: "atomic-image",
            request_object: "objects/blake3/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap()
    {
        StartSynchronousImageResult::Started(reservation) => reservation,
        other => panic!("{other:?}"),
    };
    sqlx::query(
        "UPDATE synchronous_image_idempotency SET lease_expires_at = 1 WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    assert!(
        database
            .arm_synchronous_image_submission(
                key.key_id,
                Some(&idempotency.key),
                request_id,
                reservation.id
            )
            .await
            .is_err()
    );
}
