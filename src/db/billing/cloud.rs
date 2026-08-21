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

/// Bounded audit projection for operator and credential self-service reads.
/// The raw webhook event identifier and payload hash are deliberately omitted:
/// callers need lifecycle evidence, not a replayable integration namespace.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CloudSubscriptionEventView {
    pub event_id: Uuid,
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub key_id: Uuid,
    pub entitlement_id: Uuid,
    pub provider: String,
    pub external_subscription_id: String,
    pub version: i64,
    pub subscription_status: String,
    pub created_at: i64,
}

impl Database {
    /// Lists lifecycle events newest first. Every optional selector is applied
    /// in SQL and the result is hard-bounded so this audit endpoint cannot turn
    /// into an unbounded history export.
    pub async fn list_cloud_subscription_events(
        &self,
        tenant_external_id: Option<&str>,
        principal_external_id: Option<&str>,
        key_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<CloudSubscriptionEventView>, AppError> {
        if !(1..=500).contains(&limit) {
            return Err(AppError::BadRequest(
                "limit must be between 1 and 500".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT ev.id AS event_id, t.external_id AS tenant_external_id, p.external_id AS principal_external_id, ev.key_id, ev.entitlement_id, e.provider, e.external_subscription_id, ev.version, ev.subscription_status, ev.created_at FROM memeloop_cloud_subscription_events ev JOIN tenants t ON t.id = ev.tenant_id JOIN principals p ON p.id = ev.principal_id AND p.tenant_id = ev.tenant_id JOIN key_records k ON k.id = ev.key_id AND k.tenant_id = ev.tenant_id AND k.principal_id = ev.principal_id JOIN subscription_entitlements e ON e.id = ev.entitlement_id AND e.tenant_id = ev.tenant_id AND e.account_id = k.account_id WHERE ($1 = '' OR t.external_id = $1) AND ($2 = '' OR p.external_id = $2) AND ($3 = '' OR ev.key_id = $3) ORDER BY ev.created_at DESC, ev.id DESC LIMIT $4",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(principal_external_id.unwrap_or_default())
        .bind(key_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(cloud_subscription_event_view)
            .collect()
    }
}

fn cloud_subscription_event_view(row: AnyRow) -> Result<CloudSubscriptionEventView, AppError> {
    Ok(CloudSubscriptionEventView {
        event_id: parse_uuid(row.try_get("event_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        principal_external_id: row.try_get("principal_external_id")?,
        key_id: parse_uuid(row.try_get("key_id")?)?,
        entitlement_id: parse_uuid(row.try_get("entitlement_id")?)?,
        provider: row.try_get("provider")?,
        external_subscription_id: row.try_get("external_subscription_id")?,
        version: row.try_get("version")?,
        subscription_status: row.try_get("subscription_status")?,
        created_at: row.try_get("created_at")?,
    })
}

fn validate_cloud_subscription_event(input: &CloudSubscriptionEventInput) -> Result<(), AppError> {
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
    Ok(())
}

pub(super) async fn record_cloud_subscription_event_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    input: CloudSubscriptionEventInput,
    now: i64,
) -> Result<(), AppError> {
    validate_cloud_subscription_event(&input)?;
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
        .execute(&mut **transaction)
        .await?;
    if inserted.rows_affected() == 0 {
        let tenant = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or(AppError::Forbidden)?;
        let tenant_id: String = tenant.try_get("id")?;
        let existing = sqlx::query(
                "SELECT request_hash, key_id, entitlement_id, version, subscription_status FROM memeloop_cloud_subscription_events WHERE tenant_id = $1 AND event_key_hash = $2",
            )
            .bind(tenant_id)
            .bind(&input.event_key_hash)
            .fetch_optional(&mut **transaction)
            .await?;
        let Some(existing) = existing else {
            return Err(AppError::Forbidden);
        };
        if existing.try_get::<String, _>("request_hash")? != input.request_hash
            || existing.try_get::<String, _>("key_id")? != input.key_id.to_string()
            || existing.try_get::<String, _>("entitlement_id")? != input.entitlement_id.to_string()
            || existing.try_get::<i64, _>("version")? != input.version
            || existing.try_get::<String, _>("subscription_status")? != input.subscription_status
        {
            return Err(AppError::Conflict(
                "MemeLoop Cloud event identity was already used for another state".into(),
            ));
        }
    }
    Ok(())
}
