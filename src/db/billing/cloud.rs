use super::super::*;

#[derive(Clone, Debug)]
pub struct CloudSubscriptionEventInput {
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub key_id: Uuid,
    pub entitlement_id: Uuid,
    pub event_key_hash: String,
    pub request_hash: String,
    pub version: i64,
    pub subscription_status: String,
}

impl Database {
    /// Records only authenticated, fully applied Cloud events. Raw provider
    /// event IDs are represented by a one-way digest so logs and database
    /// exports do not disclose external customer metadata.
    pub async fn record_cloud_subscription_event(
        &self,
        input: CloudSubscriptionEventInput,
    ) -> Result<(), AppError> {
        if input.event_key_hash.len() != 64
            || input.request_hash.len() != 64
            || !input
                .event_key_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !input
                .request_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || input.version <= 0
            || !matches!(input.subscription_status.as_str(), "active" | "cancelled")
        {
            return Err(AppError::BadRequest(
                "invalid MemeLoop Cloud event audit metadata".into(),
            ));
        }
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let inserted = sqlx::query(
            "INSERT INTO memeloop_cloud_subscription_events (id, tenant_id, principal_id, key_id, entitlement_id, event_key_hash, request_hash, version, subscription_status, created_at) SELECT $1, t.id, p.id, k.id, e.id, $2, $3, $4, $5, $6 FROM tenants t JOIN principals p ON p.tenant_id = t.id JOIN key_records k ON k.tenant_id = t.id AND k.principal_id = p.id JOIN subscription_entitlements e ON e.tenant_id = t.id AND e.account_id = k.account_id WHERE t.external_id = $7 AND p.external_id = $8 AND k.id = $9 AND e.id = $10 ON CONFLICT(tenant_id, event_key_hash) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&input.event_key_hash)
        .bind(&input.request_hash)
        .bind(input.version)
        .bind(&input.subscription_status)
        .bind(now)
        .bind(&input.tenant_external_id)
        .bind(&input.principal_external_id)
        .bind(input.key_id.to_string())
        .bind(input.entitlement_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            let tenant = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
                .bind(&input.tenant_external_id)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AppError::Forbidden)?;
            let tenant_id: String = tenant.try_get("id")?;
            let existing = sqlx::query(
                "SELECT request_hash, key_id, entitlement_id, version, subscription_status FROM memeloop_cloud_subscription_events WHERE tenant_id = $1 AND event_key_hash = $2",
            )
            .bind(tenant_id)
            .bind(&input.event_key_hash)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(existing) = existing else {
                return Err(AppError::Forbidden);
            };
            if existing.try_get::<String, _>("request_hash")? != input.request_hash
                || existing.try_get::<String, _>("key_id")? != input.key_id.to_string()
                || existing.try_get::<String, _>("entitlement_id")?
                    != input.entitlement_id.to_string()
                || existing.try_get::<i64, _>("version")? != input.version
                || existing.try_get::<String, _>("subscription_status")?
                    != input.subscription_status
            {
                return Err(AppError::Conflict(
                    "MemeLoop Cloud event identity was already used for another state".into(),
                ));
            }
        }
        transaction.commit().await?;
        Ok(())
    }
}
