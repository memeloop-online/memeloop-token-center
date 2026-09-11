use super::*;

#[derive(Clone, Serialize)]
pub(crate) struct QuotaResetAuditEvent {
    pub id: String,
    pub event: String,
    pub actor_service_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: i64,
}

#[derive(Clone, Serialize)]
pub(crate) struct QuotaResetOperation {
    pub id: String,
    pub upstream_account_id: String,
    pub state: String,
    pub credential_generation: i64,
    pub available_credits: i64,
    pub applicable_credits: i64,
    pub observed_at: i64,
    pub expires_at: i64,
    pub prepared_by_service_id: String,
    pub confirmed_by_service_id: Option<String>,
    pub confirmed_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_reconciled_at: Option<i64>,
    pub reconciled_available_credits: Option<i64>,
    pub reconciled_applicable_credits: Option<i64>,
    pub error_code: Option<String>,
    pub audit: Vec<QuotaResetAuditEvent>,
    /// The supplier decides which Codex rate limits a credit resets.
    pub effect: &'static str,
    pub consumes_credits: i64,
}

pub(crate) struct PrepareQuotaReset {
    pub id: Uuid,
    pub account: UpstreamAccountView,
    pub actor: String,
    pub confirmation_hash: String,
    pub prepare_idempotency_hash: String,
    pub available: i64,
    pub applicable: i64,
    pub observed_at: i64,
}

pub(crate) struct PrepareQuotaResetResult {
    pub operation: QuotaResetOperation,
    pub replayed: bool,
}

pub(crate) enum QuotaResetClaim {
    Claimed { redeem_request_id: String },
    Replayed(Box<QuotaResetOperation>),
}

#[allow(clippy::too_many_arguments)]
async fn insert_audit(
    tx: &mut Transaction<'_, Any>,
    operation_id: &str,
    tenant: &str,
    account: &str,
    event: &str,
    actor: Option<&str>,
    error_code: Option<&str>,
    created_at: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO upstream_quota_reset_audit (
             id, operation_id, tenant_id, upstream_account_id, event,
             actor_service_id, error_code, created_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(operation_id)
    .bind(tenant)
    .bind(account)
    .bind(event)
    .bind(actor)
    .bind(error_code)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Database {
    pub(crate) async fn quota_reset_prepare_replay(
        &self,
        account: &UpstreamAccountView,
        actor: &str,
        prepare_idempotency_hash: &str,
    ) -> Result<Option<QuotaResetOperation>, AppError> {
        let tenant = account.tenant_id.to_string();
        let account_id = account.id.to_string();
        let id = sqlx::query_scalar::<_, String>(
            "SELECT id FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2
               AND actor_service_id = $3 AND prepare_idempotency_hash = $4",
        )
        .bind(&tenant)
        .bind(&account_id)
        .bind(actor)
        .bind(prepare_idempotency_hash)
        .fetch_optional(&self.pool)
        .await?;
        match id {
            Some(id) => self
                .quota_reset_operation(&tenant, &account_id, &id)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub(crate) async fn quota_reset_operation_for_tenant(
        &self,
        tenant_external: &str,
        account: &str,
        id: &str,
    ) -> Result<QuotaResetOperation, AppError> {
        let row = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant: String = row.try_get("id")?;
        self.quota_reset_operation(&tenant, account, id).await
    }

    pub(crate) async fn prepare_quota_reset(
        &self,
        input: PrepareQuotaReset,
    ) -> Result<PrepareQuotaResetResult, AppError> {
        let now = unix_millis();
        let id = input.id.to_string();
        let account = input.account.id.to_string();
        let tenant = input.account.tenant_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Expiration cannot reopen submitted/unknown operations.
        sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'expired', updated_at = $1
             WHERE upstream_account_id = $2 AND state = 'prepared' AND expires_at <= $1",
        )
        .bind(now)
        .bind(&account)
        .execute(&mut *tx)
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO upstream_quota_reset_operations (
                 id, tenant_id, upstream_account_id, credential_generation,
                 transport_updated_at, actor_service_id, confirmation_hash,
                 redeem_request_id, state, available_credits, applicable_credits,
                 observed_at, expires_at, created_at, updated_at,
                 prepare_idempotency_hash
             ) VALUES (
                 $1, $2, $3, $4, $5, $6, $7, $8, 'prepared', $9, $10,
                 $11, $12, $13, $13, $14
             ) ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(&tenant)
        .bind(&account)
        .bind(input.account.credential_generation)
        .bind(input.account.updated_at)
        .bind(&input.actor)
        .bind(input.confirmation_hash)
        .bind(Uuid::now_v7().to_string())
        .bind(input.available)
        .bind(input.applicable)
        .bind(input.observed_at)
        .bind(now + 120_000)
        .bind(now)
        .bind(&input.prepare_idempotency_hash)
        .execute(&mut *tx)
        .await?;
        let replayed = inserted.rows_affected() == 0;
        let operation_id = if replayed {
            sqlx::query_scalar::<_, String>(
                "SELECT id FROM upstream_quota_reset_operations
                 WHERE tenant_id = $1 AND upstream_account_id = $2
                   AND actor_service_id = $3 AND prepare_idempotency_hash = $4",
            )
            .bind(&tenant)
            .bind(&account)
            .bind(&input.actor)
            .bind(&input.prepare_idempotency_hash)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                AppError::Conflict(
                    "an unresolved quota reset operation already exists for this account".into(),
                )
            })?
        } else {
            insert_audit(
                &mut tx,
                &id,
                &tenant,
                &account,
                "prepared",
                Some(&input.actor),
                None,
                now,
            )
            .await?;
            id
        };
        tx.commit().await?;
        Ok(PrepareQuotaResetResult {
            operation: self
                .quota_reset_operation(&tenant, &account, &operation_id)
                .await?,
            replayed,
        })
    }

    pub(crate) async fn quota_reset_operation(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
    ) -> Result<QuotaResetOperation, AppError> {
        let now = unix_millis();
        sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'expired', updated_at = $1
             WHERE tenant_id = $2 AND upstream_account_id = $3 AND id = $4
               AND state = 'prepared' AND expires_at <= $1",
        )
        .bind(now)
        .bind(tenant)
        .bind(account)
        .bind(id)
        .execute(&self.pool)
        .await?;
        let row = sqlx::query(
            "SELECT id, upstream_account_id, state, credential_generation,
                    available_credits, applicable_credits, observed_at, expires_at,
                    actor_service_id, confirmed_by_service_id, confirmed_at,
                    created_at, updated_at, last_reconciled_at,
                    reconciled_available_credits, reconciled_applicable_credits,
                    error_code
             FROM upstream_quota_reset_operations
             WHERE tenant_id = $1 AND upstream_account_id = $2 AND id = $3",
        )
        .bind(tenant)
        .bind(account)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let audit = sqlx::query(
            "SELECT id, event, actor_service_id, error_code, created_at
             FROM upstream_quota_reset_audit
             WHERE operation_id = $1 AND tenant_id = $2 AND upstream_account_id = $3
             ORDER BY created_at, id
             LIMIT 32",
        )
        .bind(id)
        .bind(tenant)
        .bind(account)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| -> Result<QuotaResetAuditEvent, sqlx::Error> {
            Ok(QuotaResetAuditEvent {
                id: row.try_get("id")?,
                event: row.try_get("event")?,
                actor_service_id: row.try_get("actor_service_id")?,
                error_code: row.try_get("error_code")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
        Ok(QuotaResetOperation {
            id: row.try_get("id")?,
            upstream_account_id: row.try_get("upstream_account_id")?,
            state: row.try_get("state")?,
            credential_generation: row.try_get("credential_generation")?,
            available_credits: row.try_get("available_credits")?,
            applicable_credits: row.try_get("applicable_credits")?,
            observed_at: row.try_get("observed_at")?,
            expires_at: row.try_get("expires_at")?,
            prepared_by_service_id: row.try_get("actor_service_id")?,
            confirmed_by_service_id: row.try_get("confirmed_by_service_id")?,
            confirmed_at: row.try_get("confirmed_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            last_reconciled_at: row.try_get("last_reconciled_at")?,
            reconciled_available_credits: row.try_get("reconciled_available_credits")?,
            reconciled_applicable_credits: row.try_get("reconciled_applicable_credits")?,
            error_code: row.try_get("error_code")?,
            audit,
            effect: "supplier_defined_codex_rate_limits",
            consumes_credits: 1,
        })
    }

    /// CAS is committed before dispatch. An exact confirmation replay returns
    /// durable state and can never send a second supplier request.
    pub(crate) async fn claim_quota_reset(
        &self,
        account: &UpstreamAccountView,
        id: &str,
        actor: &str,
        confirmation_hash: &str,
        idempotency_hash: &str,
    ) -> Result<QuotaResetClaim, AppError> {
        let now = unix_millis();
        let tenant = account.tenant_id.to_string();
        let account_id = account.id.to_string();
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = 'submitted', confirm_idempotency_hash = $1,
                 confirmed_by_service_id = $2, confirmed_at = $3, updated_at = $3
             WHERE id = $4 AND tenant_id = $5 AND upstream_account_id = $6
               AND state = 'prepared' AND expires_at > $3
               AND credential_generation = $7 AND transport_updated_at = $8
               AND actor_service_id = $2 AND confirmation_hash = $9
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_id AND account.tenant_id = $5
                   AND account.credential_generation = $7 AND account.updated_at = $8
               )",
        )
        .bind(idempotency_hash)
        .bind(actor)
        .bind(now)
        .bind(id)
        .bind(&tenant)
        .bind(&account_id)
        .bind(account.credential_generation)
        .bind(account.updated_at)
        .bind(confirmation_hash)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 0 {
            let exact_replay = sqlx::query_scalar::<_, String>(
                "SELECT state FROM upstream_quota_reset_operations
                 WHERE id = $1 AND tenant_id = $2 AND upstream_account_id = $3
                   AND actor_service_id = $4 AND confirmation_hash = $5
                   AND confirmed_by_service_id = $4 AND confirm_idempotency_hash = $6
                   AND state IN ('submitted', 'accepted', 'unknown')",
            )
            .bind(id)
            .bind(&tenant)
            .bind(&account_id)
            .bind(actor)
            .bind(confirmation_hash)
            .bind(idempotency_hash)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            if exact_replay.is_some() {
                return Ok(QuotaResetClaim::Replayed(Box::new(
                    self.quota_reset_operation(&tenant, &account_id, id).await?,
                )));
            }
            return Err(AppError::Conflict(
                "reset confirmation is expired, mismatched, or uses another Idempotency-Key".into(),
            ));
        }
        let redeem_request_id: String = sqlx::query_scalar(
            "SELECT redeem_request_id FROM upstream_quota_reset_operations WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        insert_audit(
            &mut tx,
            id,
            &tenant,
            &account_id,
            "confirmation_claimed",
            Some(actor),
            None,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(QuotaResetClaim::Claimed { redeem_request_id })
    }

    pub(crate) async fn finish_quota_reset(
        &self,
        id: &str,
        accepted: bool,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let state = if accepted { "accepted" } else { "unknown" };
        let event = if accepted {
            "dispatch_accepted"
        } else {
            "dispatch_unknown"
        };
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET state = $1, error_code = $2, updated_at = $3
             WHERE id = $4 AND state = 'submitted'",
        )
        .bind(state)
        .bind(error)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() == 1 {
            let row = sqlx::query(
                "SELECT tenant_id, upstream_account_id, confirmed_by_service_id
                 FROM upstream_quota_reset_operations WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let tenant: String = row.try_get("tenant_id")?;
            let account: String = row.try_get("upstream_account_id")?;
            let actor: Option<String> = row.try_get("confirmed_by_service_id")?;
            insert_audit(
                &mut tx,
                id,
                &tenant,
                &account,
                event,
                actor.as_deref(),
                error,
                now,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn reconcile_quota_reset(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
        actor: &str,
        available: i64,
        applicable: i64,
        observed_at: i64,
    ) -> Result<(), AppError> {
        // Read evidence alone cannot attribute a changed balance to this
        // operation. In particular, unknown never becomes accepted here.
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE upstream_quota_reset_operations
             SET reconciled_available_credits = $1,
                 reconciled_applicable_credits = $2,
                 last_reconciled_at = $3, updated_at = $3
             WHERE tenant_id = $4 AND upstream_account_id = $5 AND id = $6
               AND state IN ('submitted', 'accepted', 'unknown')",
        )
        .bind(available)
        .bind(applicable)
        .bind(observed_at)
        .bind(tenant)
        .bind(account)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "only dispatched resets can be reconciled".into(),
            ));
        }
        insert_audit(
            &mut tx,
            id,
            tenant,
            account,
            "reconciled",
            Some(actor),
            None,
            unix_millis(),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_reset_is_idempotent_audited_and_unknown_blocks_new_operations() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("reset.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        contract(&db).await;
    }

    #[tokio::test]
    async fn postgres_reset_is_idempotent_audited_and_unknown_blocks_new_operations() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        contract(&db).await;
    }

    async fn contract(db: &Database) {
        let now = unix_millis();
        let tenant = Uuid::now_v7();
        let account = UpstreamAccountView {
            id: Uuid::now_v7(),
            tenant_id: tenant,
            tenant_external_id: Some(format!("quota-reset-{tenant}")),
            name: "quota fixture".into(),
            driver: "openai-codex".into(),
            auth_kind: "oauth".into(),
            connection_method: "oauth".into(),
            credential_generation: 1,
            status: "active".into(),
            config: serde_json::json!({}),
            credential_expires_at: None,
            has_proxy: false,
            proxy_scheme: None,
            proxy_remote_dns: false,
            proxy_label: None,
            proxy_fingerprint: None,
            can_update_transport_proxy: true,
            can_refresh: true,
            can_rotate: false,
            can_reauthorize: true,
            route_count: 0,
            created_at: now,
            updated_at: now,
        };
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3)")
            .bind(tenant.to_string())
            .bind(account.tenant_external_id.as_ref().unwrap())
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'quota fixture', 'openai-codex', 'oauth', '{}', 'active', 1, $3, $3)")
            .bind(account.id.to_string()).bind(tenant.to_string()).bind(now).execute(&db.pool).await.unwrap();
        let input = |idempotency: &str| PrepareQuotaReset {
            id: Uuid::now_v7(),
            account: account.clone(),
            actor: "actor".into(),
            confirmation_hash: "confirmation-hash".into(),
            prepare_idempotency_hash: idempotency.into(),
            available: 2,
            applicable: 1,
            observed_at: now,
        };
        let first = db.prepare_quota_reset(input("prepare-key")).await.unwrap();
        assert!(!first.replayed);
        let preflight_replay = db
            .quota_reset_prepare_replay(&account, "actor", "prepare-key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preflight_replay.id, first.operation.id);
        assert!(
            db.quota_reset_prepare_replay(&account, "other-actor", "prepare-key")
                .await
                .unwrap()
                .is_none(),
            "a caller cannot claim another actor's prepare replay"
        );
        let replay = db.prepare_quota_reset(input("prepare-key")).await.unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.operation.id, first.operation.id);
        let operation = first.operation;

        let mut changed_account = account.clone();
        changed_account.credential_generation += 1;
        assert!(
            db.claim_quota_reset(
                &changed_account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key"
            )
            .await
            .is_err()
        );
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "other",
                "confirmation-hash",
                "confirm-key"
            )
            .await
            .is_err()
        );
        assert!(
            db.claim_quota_reset(&account, &operation.id, "actor", "bad", "confirm-key")
                .await
                .is_err()
        );
        assert!(
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &account.id.to_string(),
                &operation.id,
                "actor",
                99,
                99,
                now + 1
            )
            .await
            .is_err()
        );
        let claim = db
            .claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key",
            )
            .await
            .unwrap();
        assert!(matches!(claim, QuotaResetClaim::Claimed { .. }));
        let replay = db
            .claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "confirm-key",
            )
            .await
            .unwrap();
        assert!(matches!(replay, QuotaResetClaim::Replayed(_)));
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "different-confirm-key"
            )
            .await
            .is_err(),
            "a different confirmation key cannot replay or dispatch"
        );
        assert!(
            db.prepare_quota_reset(input("new-prepare-key"))
                .await
                .is_err(),
            "an unresolved operation blocks a differently keyed preparation"
        );

        db.finish_quota_reset(&operation.id, false, Some("reset_dispatch_unknown"))
            .await
            .unwrap();
        db.reconcile_quota_reset(
            &tenant.to_string(),
            &account.id.to_string(),
            &operation.id,
            "actor",
            1,
            0,
            now + 2,
        )
        .await
        .unwrap();
        let unknown = db
            .quota_reset_operation(&tenant.to_string(), &account.id.to_string(), &operation.id)
            .await
            .unwrap();
        assert_eq!(unknown.state, "unknown");
        assert_eq!(unknown.available_credits, 2);
        assert_eq!(unknown.applicable_credits, 1);
        assert_eq!(unknown.reconciled_available_credits, Some(1));
        assert_eq!(unknown.reconciled_applicable_credits, Some(0));
        assert_eq!(
            unknown
                .audit
                .iter()
                .map(|event| event.event.as_str())
                .collect::<Vec<_>>(),
            [
                "prepared",
                "confirmation_claimed",
                "dispatch_unknown",
                "reconciled"
            ]
        );
        assert!(
            db.prepare_quota_reset(input("after-unknown"))
                .await
                .is_err()
        );
        assert!(
            db.claim_quota_reset(
                &account,
                &operation.id,
                "actor",
                "confirmation-hash",
                "different-confirm-key"
            )
            .await
            .is_err()
        );
    }
}
