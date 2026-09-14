use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    db: Database,
    key: AuthenticatedKey,
    reservation: UsageReservation,
    request_id: Uuid,
    actor: Uuid,
    tenant: String,
    revision: String,
    idempotency: Option<GenerationJobIdempotency>,
}

impl Fixture {
    async fn new(with_idempotency: bool) -> Self {
        let (directory, db, key, price) =
            synchronous_image_atomic_fixture("image-human-resolution.db").await;
        let request_id = Uuid::now_v7();
        let idempotency = with_idempotency.then(|| GenerationJobIdempotency {
            key: "human-resolution".into(),
            request_hash: "a".repeat(64),
        });
        let reservation = match db
            .start_synchronous_image_request(StartSynchronousImageRequest {
                routing_snapshot: None,
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 0,
                output_token_ceiling: 1,
                idempotency: idempotency.as_ref(),
                protocol: "openai-image",
                model: "atomic-image",
                request_object: "objects/blake3/audit-request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap()
        {
            StartSynchronousImageResult::Started(r) => r,
            other => panic!("{other:?}"),
        };
        db.arm_synchronous_image_submission(
            key.key_id,
            idempotency.as_ref().map(|i| i.key.as_str()),
            request_id,
            reservation.id,
        )
        .await
        .unwrap();
        db.quarantine_synchronous_image_submission(
            key.key_id,
            idempotency.as_ref().map(|i| i.key.as_str()),
            request_id,
            reservation.id,
        )
        .await
        .unwrap();
        let tenant: String = sqlx::query_scalar("SELECT external_id FROM tenants WHERE id = $1")
            .bind(key.tenant_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let actor = db
            .create_service_token(
                CreateServiceTokenInput {
                    name: "human-reconciler".into(),
                    scopes: vec!["generations:reconcile".into()],
                    tenant_external_id: Some(tenant.clone()),
                },
                b"service test pepper",
            )
            .await
            .unwrap()
            .service_id;
        let revision = db
            .image_generation_quarantine(&tenant, request_id)
            .await
            .unwrap()
            .revision;
        Self {
            _directory: directory,
            db,
            key,
            reservation,
            request_id,
            actor,
            tenant,
            revision,
            idempotency,
        }
    }

    fn input(&self) -> ResolveImageGenerationQuarantine<'_> {
        ResolveImageGenerationQuarantine {
            tenant_external_id: &self.tenant,
            request_id: self.request_id,
            actor_service_id: self.actor,
            idempotency_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            expected_revision: &self.revision,
            action: "settle_confirmed",
            confirmed_cost_micros: 500,
            currency: "USD",
            evidence_digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        }
    }
}

#[tokio::test]
async fn manual_image_resolution_is_atomic_replay_safe_and_fences_late_completion() {
    for with_idempotency in [true, false] {
        for action in ["not_delivered", "settle_confirmed"] {
            let f = Fixture::new(with_idempotency).await;
            let pending =
                f.db.image_generation_quarantine(&f.tenant, f.request_id)
                    .await
                    .unwrap();
            assert_eq!(pending.status, "awaiting_confirmation");
            assert!(pending.resolution.is_none());
            assert_eq!(
                f.db.list_image_generation_quarantine(&f.tenant, 10, None)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let mut input = f.input();
            input.action = action;
            input.confirmed_cost_micros = if action == "not_delivered" { 0 } else { 500 };
            let result =
                f.db.resolve_image_generation_quarantine(input)
                    .await
                    .unwrap();
            assert_eq!(result.resulting_status, "failed");
            let resolved =
                f.db.image_generation_quarantine(&f.tenant, f.request_id)
                    .await
                    .unwrap();
            assert_eq!(resolved.status, "resolved");
            assert_eq!(resolved.resolution.as_ref(), Some(&result));
            assert_ne!(resolved.revision, pending.revision);
            assert!(
                f.db.list_image_generation_quarantine(&f.tenant, 10, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let mut replay = f.input();
            replay.action = action;
            replay.confirmed_cost_micros = result.confirmed_cost_micros;
            assert_eq!(
                f.db.resolve_image_generation_quarantine(replay)
                    .await
                    .unwrap(),
                result
            );
            let mut mismatch = f.input();
            mismatch.evidence_digest =
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
            assert!(matches!(
                f.db.resolve_image_generation_quarantine(mismatch).await,
                Err(AppError::Conflict(_))
            ));
            let late =
                f.db.finish_synchronous_image_request(FinishSynchronousImageRequest {
                    key_id: f.key.key_id,
                    idempotency_key: f.idempotency.as_ref().map(|i| i.key.as_str()),
                    request_id: f.request_id,
                    reservation: &f.reservation,
                    status_code: 200,
                    duration_ms: 1,
                    input_tokens: 0,
                    output_tokens: 1,
                    error_code: None,
                    response_object: "objects/blake3/late-result",
                    assets: &[],
                })
                .await;
            if with_idempotency {
                assert!(matches!(
                    late,
                    Ok(FinishSynchronousImageResult::Replay(
                        SynchronousImageIdempotencyClaim::Failed { .. }
                    ))
                ));
            } else {
                assert!(matches!(late, Err(AppError::Conflict(_))));
            }
            let row = sqlx::query("SELECT r.actual_micros,r.status,q.cost_micros,q.status_code,q.request_object,q.response_object FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE q.id = $1")
                .bind(f.request_id.to_string()).fetch_one(&f.db.pool).await.unwrap();
            assert_eq!(row.get::<String, _>("status"), "settled");
            assert_eq!(
                row.get::<i64, _>("actual_micros"),
                result.confirmed_cost_micros
            );
            assert_eq!(
                row.get::<i64, _>("cost_micros"),
                result.confirmed_cost_micros
            );
            assert_eq!(row.get::<i64, _>("status_code"), 502);
            assert_eq!(
                row.get::<String, _>("request_object"),
                "objects/blake3/audit-request"
            );
            assert!(
                row.get::<String, _>("response_object")
                    .starts_with("gap://")
            );
            let receipts: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM image_generation_quarantine_resolutions")
                    .fetch_one(&f.db.pool)
                    .await
                    .unwrap();
            assert_eq!(receipts, 1);
            if let Some(idempotency) = &f.idempotency {
                assert!(matches!(
                    f.db.claim_synchronous_image_idempotency(
                        f.key.key_id,
                        idempotency,
                        Uuid::now_v7()
                    )
                    .await
                    .unwrap(),
                    SynchronousImageIdempotencyClaim::Failed { .. }
                ));
            }
            // Even an exact replay must revalidate the still-authorized actor.
            f.db.set_service_token_status(f.actor, "revoked")
                .await
                .unwrap();
            assert!(matches!(
                f.db.resolve_image_generation_quarantine(f.input()).await,
                Err(AppError::Forbidden)
            ));
        }
    }
}

#[tokio::test]
async fn manual_image_resolution_rejects_stale_authority_amount_currency_and_revision() {
    let f = Fixture::new(true).await;
    assert!(
        f.db.list_image_generation_quarantine("foreign-tenant", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        f.db.image_generation_quarantine("foreign-tenant", f.request_id)
            .await,
        Err(AppError::NotFound)
    ));
    let mut stale = f.input();
    stale.expected_revision = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(stale).await,
        Err(AppError::Conflict(_))
    ));
    let mut currency = f.input();
    currency.currency = "EUR";
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(currency).await,
        Err(AppError::BadRequest(_))
    ));
    let mut invalid = f.input();
    invalid.action = "not_delivered";
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(invalid).await,
        Err(AppError::BadRequest(_))
    ));
    let mut excessive = f.input();
    excessive.confirmed_cost_micros = 1001;
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(excessive).await,
        Err(AppError::Conflict(_))
    ));
    let mut overflow = f.input();
    overflow.confirmed_cost_micros = i64::MAX;
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(overflow).await,
        Err(AppError::BadRequest(_))
    ));
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM image_generation_quarantine_resolutions")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(
        receipts, 0,
        "failed amount confirmation must roll back audit and settlement"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM usage_reservations WHERE id = $1")
        .bind(f.reservation.id.to_string())
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "reserved");
    sqlx::query(
        "UPDATE service_credentials SET scopes_json = '[]' WHERE service_principal_id = $1",
    )
    .bind(f.actor.to_string())
    .execute(&f.db.pool)
    .await
    .unwrap();
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(f.input()).await,
        Err(AppError::Forbidden)
    ));
    sqlx::query("UPDATE service_credentials SET scopes_json = '[\"generations:reconcile\"]', tenant_external_id = 'foreign-tenant' WHERE service_principal_id = $1").bind(f.actor.to_string()).execute(&f.db.pool).await.unwrap();
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(f.input()).await,
        Err(AppError::Forbidden)
    ));
    sqlx::query(
        "UPDATE service_credentials SET tenant_external_id = NULL WHERE service_principal_id = $1",
    )
    .bind(f.actor.to_string())
    .execute(&f.db.pool)
    .await
    .unwrap();
    assert!(
        matches!(
            f.db.resolve_image_generation_quarantine(f.input()).await,
            Err(AppError::Forbidden)
        ),
        "global service must not reconcile a tenant's money"
    );
    sqlx::query("UPDATE service_credentials SET tenant_external_id = $2, revoked_at = 1 WHERE service_principal_id = $1").bind(f.actor.to_string()).bind(&f.tenant).execute(&f.db.pool).await.unwrap();
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(f.input()).await,
        Err(AppError::Forbidden)
    ));
}

#[tokio::test]
async fn concurrent_image_resolution_has_one_receipt_and_one_charge() {
    let f = Fixture::new(true).await;
    let first = f.input();
    let mut second = f.input();
    second.idempotency_hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let (first, second) = tokio::join!(
        f.db.resolve_image_generation_quarantine(first),
        f.db.resolve_image_generation_quarantine(second)
    );
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    assert!(
        matches!(first, Err(AppError::Conflict(_))) || matches!(second, Err(AppError::Conflict(_)))
    );
    let receipts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM image_generation_quarantine_resolutions")
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(receipts, 1);
    let cost: i64 =
        sqlx::query_scalar("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(f.reservation.id.to_string())
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(cost, 500);
}

#[tokio::test]
async fn manual_image_confirmation_rejects_accounting_overflow_without_sqlite_real_promotion() {
    for target in ["lifetime", "day", "account", "pending_metered"] {
        let f = Fixture::new(true).await;
        let near_max = i64::MAX - 100;
        match target {
            "lifetime" => {
                sqlx::query(
                    "UPDATE key_budget_state SET settled_lifetime_micros = $1 WHERE key_id = $2",
                )
                .bind(near_max)
                .bind(f.key.key_id.to_string())
                .execute(&f.db.pool)
                .await
                .unwrap();
            }
            "day" => {
                sqlx::query("INSERT INTO key_budget_daily_rollups (key_id,day_bucket,settled_micros) VALUES ($1,$2,$3) ON CONFLICT(key_id,day_bucket) DO UPDATE SET settled_micros = excluded.settled_micros")
                .bind(f.key.key_id.to_string()).bind(unix_millis() / 86_400_000).bind(near_max).execute(&f.db.pool).await.unwrap();
            }
            "pending_metered" => {
                sqlx::query("INSERT INTO metered_usage_projection_outbox (reservation_id, account_id, key_id, actual_micros, created_at) VALUES ($1,$2,$3,$4,1)")
                    .bind(f.reservation.id.to_string()).bind(f.reservation.account_id.to_string()).bind(f.key.key_id.to_string()).bind(near_max).execute(&f.db.pool).await.unwrap();
            }
            _ => {
                sqlx::query("INSERT INTO account_usage_state (account_id,settled_lifetime_micros,updated_at) VALUES ($1,$2,1) ON CONFLICT(account_id) DO UPDATE SET settled_lifetime_micros = excluded.settled_lifetime_micros")
                .bind(f.reservation.account_id.to_string()).bind(near_max).execute(&f.db.pool).await.unwrap();
            }
        }
        assert!(
            matches!(
                f.db.resolve_image_generation_quarantine(f.input()).await,
                Err(AppError::Conflict(_))
            ),
            "{target} overflow must reject confirmation"
        );
        let receipt_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM image_generation_quarantine_resolutions")
                .fetch_one(&f.db.pool)
                .await
                .unwrap();
        assert_eq!(receipt_count, 0);
        let status: String =
            sqlx::query_scalar("SELECT status FROM usage_reservations WHERE id = $1")
                .bind(f.reservation.id.to_string())
                .fetch_one(&f.db.pool)
                .await
                .unwrap();
        assert_eq!(status, "reserved");
        let stored: i64 = match target {
            "lifetime" => sqlx::query_scalar("SELECT settled_lifetime_micros FROM key_budget_state WHERE key_id = $1").bind(f.key.key_id.to_string()).fetch_one(&f.db.pool).await.unwrap(),
            "day" => sqlx::query_scalar("SELECT settled_micros FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket = $2").bind(f.key.key_id.to_string()).bind(unix_millis() / 86_400_000).fetch_one(&f.db.pool).await.unwrap(),
            "pending_metered" => sqlx::query_scalar("SELECT actual_micros FROM metered_usage_projection_outbox WHERE reservation_id = $1").bind(f.reservation.id.to_string()).fetch_one(&f.db.pool).await.unwrap(),
            _ => sqlx::query_scalar("SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1").bind(f.reservation.account_id.to_string()).fetch_one(&f.db.pool).await.unwrap(),
        };
        assert_eq!(stored, near_max, "unchanged i64 proves no REAL promotion");
    }
    let f = Fixture::new(false).await;
    sqlx::query(
        "UPDATE usage_reservations SET enforcement_mode = 'metered_unlimited' WHERE id = $1",
    )
    .bind(f.reservation.id.to_string())
    .execute(&f.db.pool)
    .await
    .unwrap();
    let mut excessive = f.input();
    excessive.confirmed_cost_micros = i64::MAX;
    assert!(matches!(
        f.db.resolve_image_generation_quarantine(excessive).await,
        Err(AppError::BadRequest(_))
    ));
}
