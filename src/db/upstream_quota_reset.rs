use super::*;

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
    pub last_reconciled_at: Option<i64>,
    pub reconciled_available_credits: Option<i64>,
    pub reconciled_applicable_credits: Option<i64>,
    pub error_code: Option<String>,
    /// The supplier decides which Codex rate limits a credit resets.
    pub effect: &'static str,
    pub consumes_credits: i64,
}

pub(crate) struct PrepareQuotaReset {
    pub account: UpstreamAccountView,
    pub actor: String,
    pub confirmation_hash: String,
    pub available: i64,
    pub applicable: i64,
    pub observed_at: i64,
}

impl Database {
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
    ) -> Result<QuotaResetOperation, AppError> {
        let now = unix_millis();
        let id = Uuid::now_v7().to_string();
        let account = input.account.id.to_string();
        let tenant = input.account.tenant_id.to_string();
        let mut tx = self.pool.begin().await?;
        // Expiration cannot reopen submitted/unknown operations.
        sqlx::query("UPDATE upstream_quota_reset_operations SET state = 'expired', updated_at = $1 WHERE upstream_account_id = $2 AND state = 'prepared' AND expires_at <= $1")
            .bind(now).bind(&account).execute(&mut *tx).await?;
        let result = sqlx::query("INSERT INTO upstream_quota_reset_operations (id, tenant_id, upstream_account_id, credential_generation, transport_updated_at, actor_service_id, confirmation_hash, redeem_request_id, state, available_credits, applicable_credits, observed_at, expires_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'prepared', $9, $10, $11, $12, $13, $13)")
            .bind(&id).bind(&tenant).bind(&account).bind(input.account.credential_generation)
            .bind(input.account.updated_at).bind(input.actor).bind(input.confirmation_hash)
            .bind(Uuid::now_v7().to_string()).bind(input.available).bind(input.applicable)
            .bind(input.observed_at).bind(now + 120_000).bind(now)
            .execute(&mut *tx).await;
        match result {
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Err(AppError::Conflict(
                    "an unresolved quota reset operation already exists for this account".into(),
                ));
            }
            result => {
                result?;
            }
        }
        tx.commit().await?;
        self.quota_reset_operation(&tenant, &account, &id).await
    }

    pub(crate) async fn quota_reset_operation(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
    ) -> Result<QuotaResetOperation, AppError> {
        let row = sqlx::query("SELECT id, upstream_account_id, state, credential_generation, available_credits, applicable_credits, observed_at, expires_at, last_reconciled_at, reconciled_available_credits, reconciled_applicable_credits, error_code FROM upstream_quota_reset_operations WHERE tenant_id = $1 AND upstream_account_id = $2 AND id = $3")
            .bind(tenant).bind(account).bind(id).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        Ok(QuotaResetOperation {
            id: row.try_get("id")?,
            upstream_account_id: row.try_get("upstream_account_id")?,
            state: row.try_get("state")?,
            credential_generation: row.try_get("credential_generation")?,
            available_credits: row.try_get("available_credits")?,
            applicable_credits: row.try_get("applicable_credits")?,
            observed_at: row.try_get("observed_at")?,
            expires_at: row.try_get("expires_at")?,
            last_reconciled_at: row.try_get("last_reconciled_at")?,
            reconciled_available_credits: row.try_get("reconciled_available_credits")?,
            reconciled_applicable_credits: row.try_get("reconciled_applicable_credits")?,
            error_code: row.try_get("error_code")?,
            effect: "supplier_defined_codex_rate_limits",
            consumes_credits: 1,
        })
    }

    /// CAS is committed before dispatch. Losing an ACK or process leaves
    /// submitted/unknown permanently blocking a fresh operation.
    pub(crate) async fn claim_quota_reset(
        &self,
        account: &UpstreamAccountView,
        id: &str,
        actor: &str,
        hash: &str,
    ) -> Result<String, AppError> {
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE upstream_quota_reset_operations SET state = 'submitted', updated_at = $1 WHERE id = $2 AND tenant_id = $3 AND upstream_account_id = $4 AND state = 'prepared' AND expires_at > $1 AND credential_generation = $5 AND transport_updated_at = $6 AND actor_service_id = $7 AND confirmation_hash = $8 AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id = upstream_account_id AND a.tenant_id = $3 AND a.credential_generation = $5 AND a.updated_at = $6)")
            .bind(now).bind(id).bind(account.tenant_id.to_string()).bind(account.id.to_string())
            .bind(account.credential_generation).bind(account.updated_at).bind(actor).bind(hash)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict("reset confirmation is expired, already submitted, or no longer matches this account".into()));
        }
        let row = sqlx::query(
            "SELECT redeem_request_id FROM upstream_quota_reset_operations WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let redeem: String = row.try_get("redeem_request_id")?;
        tx.commit().await?;
        Ok(redeem)
    }

    pub(crate) async fn finish_quota_reset(
        &self,
        id: &str,
        accepted: bool,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        sqlx::query("UPDATE upstream_quota_reset_operations SET state = $1, error_code = $2, updated_at = $3 WHERE id = $4 AND state = 'submitted'")
            .bind(if accepted { "accepted" } else { "unknown" }).bind(error).bind(unix_millis()).bind(id)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn reconcile_quota_reset(
        &self,
        tenant: &str,
        account: &str,
        id: &str,
        available: i64,
        applicable: i64,
        observed_at: i64,
    ) -> Result<(), AppError> {
        // Read evidence alone cannot attribute a changed balance to this
        // operation. In particular, unknown never becomes accepted here.
        let changed = sqlx::query("UPDATE upstream_quota_reset_operations SET reconciled_available_credits = $1, reconciled_applicable_credits = $2, last_reconciled_at = $3, updated_at = $3 WHERE tenant_id = $4 AND upstream_account_id = $5 AND id = $6 AND state IN ('submitted', 'accepted', 'unknown')")
            .bind(available).bind(applicable).bind(observed_at).bind(tenant).bind(account).bind(id)
            .execute(&self.pool).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "only dispatched resets can be reconciled".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_reset_claims_once_and_unknown_blocks_fresh_operations() {
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
    async fn postgres_reset_claims_once_and_unknown_blocks_fresh_operations() {
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
        let input = || PrepareQuotaReset {
            account: account.clone(),
            actor: "actor".into(),
            confirmation_hash: "hash".into(),
            available: 2,
            applicable: 1,
            observed_at: now,
        };
        let operation = db.prepare_quota_reset(input()).await.unwrap();
        let mut changed_account = account.clone();
        changed_account.credential_generation += 1;
        assert!(
            db.claim_quota_reset(&changed_account, &operation.id, "actor", "hash")
                .await
                .is_err()
        );
        changed_account = account.clone();
        changed_account.updated_at += 1;
        assert!(
            db.claim_quota_reset(&changed_account, &operation.id, "actor", "hash")
                .await
                .is_err()
        );
        assert!(
            db.reconcile_quota_reset(
                &tenant.to_string(),
                &account.id.to_string(),
                &operation.id,
                99,
                99,
                now + 1
            )
            .await
            .is_err()
        );
        assert!(
            db.claim_quota_reset(&account, &operation.id, "other", "hash")
                .await
                .is_err()
        );
        assert!(
            db.claim_quota_reset(&account, &operation.id, "actor", "bad")
                .await
                .is_err()
        );
        assert!(
            db.quota_reset_operation(
                &Uuid::now_v7().to_string(),
                &account.id.to_string(),
                &operation.id
            )
            .await
            .is_err()
        );
        let (left, right) = tokio::join!(
            db.claim_quota_reset(&account, &operation.id, "actor", "hash"),
            db.claim_quota_reset(&account, &operation.id, "actor", "hash"),
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert!(db.prepare_quota_reset(input()).await.is_err());
        db.finish_quota_reset(&operation.id, false, Some("reset_dispatch_unknown"))
            .await
            .unwrap();
        assert!(db.prepare_quota_reset(input()).await.is_err());
        db.reconcile_quota_reset(
            &tenant.to_string(),
            &account.id.to_string(),
            &operation.id,
            1,
            0,
            now + 1,
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
        assert!(db.prepare_quota_reset(input()).await.is_err());
        assert!(
            db.claim_quota_reset(&account, &operation.id, "actor", "hash")
                .await
                .is_err()
        );
    }
}
