use std::collections::BTreeSet;

use super::super::*;

#[derive(Clone, Debug, Serialize)]
pub struct ReconcileEntitlementInput {
    pub tenant_external_id: String,
    pub account_id: Uuid,
    pub provider: String,
    pub external_subscription_id: String,
    pub external_cycle_id: String,
    pub period_start: i64,
    pub period_end: i64,
    pub currency: String,
    pub desired_micros: i64,
    pub version: i64,
    pub source: String,
    pub proration_json: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CancelEntitlementInput {
    pub tenant_external_id: String,
    pub provider: String,
    pub external_subscription_id: String,
    pub external_cycle_id: Option<String>,
    pub version: i64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplaceEntitlementInput {
    pub tenant_external_id: String,
    pub provider: String,
    pub external_subscription_id: String,
    pub version: i64,
    pub source: String,
    pub replacement: ReconcileEntitlementInput,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementOperation {
    Reconcile(ReconcileEntitlementInput),
    Cancel(CancelEntitlementInput),
    Replace(ReplaceEntitlementInput),
}

#[derive(Clone, Debug)]
pub enum CloudRoutingGrantSnapshot {
    /// The normalized contract. Empty vectors intentionally deny every route.
    Explicit {
        route_ids: Vec<Uuid>,
        route_group_ids: Vec<Uuid>,
    },
    /// Compatibility for older Cloud senders. Models are resolved to the
    /// tenant's routes once, while this transaction is committed.
    LegacyAllowedModels(Vec<String>),
    /// Cancellation always removes authorization regardless of payload data.
    DenyAll,
}

#[derive(Clone, Debug)]
pub struct ApplyCloudEntitlementInput {
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub provider: String,
    pub external_subscription_id: String,
    pub currency: String,
    pub provisioning_idempotency_key: String,
    pub reconciliation_idempotency_key: String,
    pub operation: EntitlementOperation,
    pub policy: KeyPolicy,
    pub routing: CloudRoutingGrantSnapshot,
    pub audit: CloudSubscriptionEventInput,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyCloudEntitlementResult {
    pub credential: ProvisionedCloudCredential,
    pub entitlement: EntitlementReconcileResult,
    pub policy: KeyPolicy,
}

impl EntitlementOperation {
    fn tenant_external_id(&self) -> &str {
        match self {
            Self::Reconcile(input) => &input.tenant_external_id,
            Self::Cancel(input) => &input.tenant_external_id,
            Self::Replace(input) => &input.tenant_external_id,
        }
    }
}

impl Database {
    pub async fn list_entitlements(
        &self,
        tenant_external_id: Option<&str>,
        provider: Option<&str>,
        external_subscription_id: Option<&str>,
    ) -> Result<Vec<EntitlementView>, AppError> {
        let rows = sqlx::query(
            "SELECT e.id AS entitlement_id, c.id AS cycle_id, t.external_id AS tenant_external_id, e.account_id, e.provider, e.external_subscription_id, c.external_cycle_id, c.period_start, c.period_end, c.currency, c.desired_micros, c.consumed_micros, c.funded_micros, e.status, e.version, e.replaced_by_entitlement_id, e.created_at, e.updated_at FROM subscription_entitlements e JOIN tenants t ON t.id = e.tenant_id JOIN entitlement_cycles c ON c.id = e.current_cycle_id WHERE ($1 = '' OR t.external_id = $1) AND ($2 = '' OR e.provider = $2) AND ($3 = '' OR e.external_subscription_id = $3) ORDER BY e.updated_at DESC, e.id DESC LIMIT 500",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(provider.unwrap_or_default())
        .bind(external_subscription_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(entitlement_view).collect()
    }

    /// Self-service projection bound to the stable key identity. The join goes
    /// through the durable credit account, so rotating credential material does
    /// not move subscription history and no caller-supplied tenant is trusted.
    pub async fn list_cloud_entitlements_for_key(
        &self,
        key_id: Uuid,
    ) -> Result<Vec<EntitlementView>, AppError> {
        let rows = sqlx::query(
            "SELECT e.id AS entitlement_id, c.id AS cycle_id, t.external_id AS tenant_external_id, e.account_id, e.provider, e.external_subscription_id, c.external_cycle_id, c.period_start, c.period_end, c.currency, c.desired_micros, c.consumed_micros, c.funded_micros, e.status, e.version, e.replaced_by_entitlement_id, e.created_at, e.updated_at FROM key_records k JOIN subscription_entitlements e ON e.tenant_id = k.tenant_id AND e.account_id = k.account_id JOIN tenants t ON t.id = e.tenant_id JOIN entitlement_cycles c ON c.id = e.current_cycle_id WHERE k.id = $1 AND e.provider = 'memeloop-cloud' ORDER BY e.updated_at DESC, e.id DESC LIMIT 100",
        )
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(entitlement_view).collect()
    }

    /// Reconciles an external subscription snapshot against durable account
    /// credit. The tenant and idempotency key form the replay namespace, while
    /// the stable provider/subscription identity survives credential rotation.
    pub async fn reconcile_entitlement(
        &self,
        mut operation: EntitlementOperation,
        idempotency_key: &str,
    ) -> Result<EntitlementReconcileResult, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        canonicalize_entitlement_operation(&mut operation)?;
        validate_entitlement_operation(&operation)?;
        let tenant_external_id = operation.tenant_external_id().to_owned();
        let mut tx = self.begin_write_transaction().await?;
        let tenant = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant_id: String = tenant.try_get("id")?;
        let (result, _) = reconcile_entitlement_in_transaction(
            &mut tx,
            self.backend,
            &tenant_id,
            operation,
            idempotency_key,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Atomically applies the MemeLoop Cloud financial snapshot, stable
    /// credential identity, rate policy, normalized routing grants, replay
    /// record, and audit event. No externally visible partial state survives a
    /// failed webhook.
    pub async fn apply_cloud_entitlement(
        &self,
        mut input: ApplyCloudEntitlementInput,
        pepper: &[u8],
    ) -> Result<ApplyCloudEntitlementResult, AppError> {
        validate_idempotency_key(
            &input.reconciliation_idempotency_key,
            "reconciliation identity",
        )?;
        validate_key_policy(&input.policy)?;
        validate_cloud_routing_snapshot(&input.routing)?;
        canonicalize_entitlement_operation(&mut input.operation)?;
        validate_entitlement_operation(&input.operation)?;
        let (active_snapshot, version) = match &input.operation {
            EntitlementOperation::Reconcile(operation) => {
                if operation.tenant_external_id != input.tenant_external_id
                    || operation.provider != input.provider
                    || operation.external_subscription_id != input.external_subscription_id
                    || !operation.currency.eq_ignore_ascii_case(&input.currency)
                {
                    return Err(AppError::BadRequest(
                        "Cloud entitlement identity fields must match".into(),
                    ));
                }
                (true, operation.version)
            }
            EntitlementOperation::Cancel(operation) => {
                if operation.tenant_external_id != input.tenant_external_id
                    || operation.provider != input.provider
                    || operation.external_subscription_id != input.external_subscription_id
                {
                    return Err(AppError::BadRequest(
                        "Cloud entitlement identity fields must match".into(),
                    ));
                }
                (false, operation.version)
            }
            EntitlementOperation::Replace(_) => {
                return Err(AppError::BadRequest(
                    "MemeLoop Cloud snapshots do not replace subscription identity".into(),
                ));
            }
        };
        if input.audit.tenant_external_id != input.tenant_external_id
            || input.audit.principal_external_id != input.principal_external_id
            || input.audit.version != version
            || input.audit.subscription_status
                != if active_snapshot {
                    "active"
                } else {
                    "cancelled"
                }
        {
            return Err(AppError::BadRequest(
                "Cloud audit identity must match the entitlement snapshot".into(),
            ));
        }
        if !active_snapshot && !matches!(input.routing, CloudRoutingGrantSnapshot::DenyAll) {
            return Err(AppError::BadRequest(
                "cancelled Cloud snapshots cannot grant model routes".into(),
            ));
        }

        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            let subscription_lock = serde_json::to_string(&(
                &input.tenant_external_id,
                &input.provider,
                &input.external_subscription_id,
            ))
            .map_err(|_| AppError::Internal)?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948316))")
                .bind(subscription_lock)
                .execute(&mut *tx)
                .await?;
        }
        let bound_principal = sqlx::query(
            "SELECT p.external_id FROM subscription_entitlements e JOIN tenants t ON t.id = e.tenant_id JOIN key_records k ON k.tenant_id = e.tenant_id AND k.account_id = e.account_id JOIN principals p ON p.id = k.principal_id WHERE t.external_id = $1 AND e.provider = $2 AND e.external_subscription_id = $3",
        )
        .bind(&input.tenant_external_id)
        .bind(&input.provider)
        .bind(&input.external_subscription_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = bound_principal {
            let bound_principal: String = row.try_get("external_id")?;
            if bound_principal != input.principal_external_id {
                return Err(AppError::Forbidden);
            }
        }
        let (tenant_id, credential) = self
            .provision_or_load_cloud_credential_in_transaction(
                &mut tx,
                CloudCredentialProvisioningInput {
                    tenant_external_id: &input.tenant_external_id,
                    principal_external_id: &input.principal_external_id,
                    currency: &input.currency,
                    provisioning_idempotency_key: &input.provisioning_idempotency_key,
                    create_if_missing: active_snapshot,
                    pepper,
                    now,
                },
            )
            .await?;
        if let EntitlementOperation::Reconcile(operation) = &mut input.operation {
            operation.account_id = credential.account_id;
        }
        let (entitlement, replayed) = reconcile_entitlement_in_transaction(
            &mut tx,
            self.backend,
            &tenant_id,
            input.operation,
            &input.reconciliation_idempotency_key,
        )
        .await?;

        if replayed {
            // A reconciliation replay is bound to its stored entitlement row,
            // not to the mutable current subscription version. A later webhook
            // may already have advanced that version, changed policy, or
            // rotated the credential while this event identity must remain an
            // idempotent success with no financial or routing mutation.
            let policy_json: String = sqlx::query(
                "SELECT k.policy_json FROM key_records k JOIN subscription_entitlements e ON e.tenant_id = k.tenant_id AND e.account_id = k.account_id WHERE k.id = $1 AND k.tenant_id = $2 AND e.provider = $3 AND e.external_subscription_id = $4 AND e.id = $5",
            )
            .bind(credential.key_id.to_string())
            .bind(&tenant_id)
            .bind(&input.provider)
            .bind(&input.external_subscription_id)
            .bind(entitlement.entitlement.entitlement_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| {
                AppError::Conflict(
                    "credential no longer belongs to the replayed entitlement".into(),
                )
            })?
            .try_get("policy_json")?;
            let effective_policy =
                serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;
            input.audit.key_id = credential.key_id;
            input.audit.entitlement_id = entitlement.entitlement.entitlement_id;
            super::cloud::record_cloud_subscription_event_in_transaction(&mut tx, input.audit, now)
                .await?;
            tx.commit().await?;
            return Ok(ApplyCloudEntitlementResult {
                credential,
                entitlement,
                policy: effective_policy,
            });
        }

        let key_row = sqlx::query(
            "SELECT k.status, k.policy_json FROM key_records k JOIN subscription_entitlements e ON e.tenant_id = k.tenant_id AND e.account_id = k.account_id WHERE k.id = $1 AND k.tenant_id = $2 AND e.provider = $3 AND e.external_subscription_id = $4 AND e.version = $5",
        )
        .bind(credential.key_id.to_string())
        .bind(&tenant_id)
        .bind(&input.provider)
        .bind(&input.external_subscription_id)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            AppError::Conflict(
                "credential policy snapshot is stale or belongs to another entitlement".into(),
            )
        })?;
        let key_status: String = key_row.try_get("status")?;
        let mut effective_policy: KeyPolicy =
            serde_json::from_str(&key_row.try_get::<String, _>("policy_json")?)
                .map_err(|_| AppError::Internal)?;

        // Subscription automation owns financial snapshots, but it does not own
        // the operator-controlled credential lifecycle. A disabled stable key
        // must be explicitly reactivated by an operator before a later active
        // snapshot may fund it or alter its access policy.
        if active_snapshot && key_status != "active" {
            return Err(AppError::Conflict(
                "suspended or revoked credentials cannot receive active subscription updates"
                    .into(),
            ));
        }

        if active_snapshot {
            // `allowed_models` remains accepted only as an input compatibility
            // source. Clearing it before persistence prevents it becoming a
            // second runtime authority beside normalized grants.
            input.policy.allowed_models.clear();
            let policy_json =
                serde_json::to_string(&input.policy).map_err(|_| AppError::Internal)?;
            let updated = sqlx::query(
                "UPDATE key_records SET policy_json = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4",
            )
            .bind(policy_json)
            .bind(now)
            .bind(credential.key_id.to_string())
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "credential policy snapshot is stale".into(),
                ));
            }
            effective_policy = input.policy;
        }

        let (route_ids, route_group_ids, legacy_models) = if active_snapshot {
            match &input.routing {
                CloudRoutingGrantSnapshot::Explicit {
                    route_ids,
                    route_group_ids,
                } => (route_ids.as_slice(), route_group_ids.as_slice(), None),
                CloudRoutingGrantSnapshot::LegacyAllowedModels(models) => {
                    (&[][..], &[][..], Some(models.as_slice()))
                }
                CloudRoutingGrantSnapshot::DenyAll => (&[][..], &[][..], None),
            }
        } else {
            (&[][..], &[][..], None)
        };
        replace_key_routing_grants_in_transaction(
            &mut tx,
            &tenant_id,
            credential.key_id,
            route_ids,
            route_group_ids,
            legacy_models,
            now,
        )
        .await?;

        input.audit.key_id = credential.key_id;
        input.audit.entitlement_id = entitlement.entitlement.entitlement_id;
        super::cloud::record_cloud_subscription_event_in_transaction(&mut tx, input.audit, now)
            .await?;
        tx.commit().await?;
        Ok(ApplyCloudEntitlementResult {
            credential,
            entitlement,
            policy: effective_policy,
        })
    }
}

fn validate_cloud_routing_snapshot(routing: &CloudRoutingGrantSnapshot) -> Result<(), AppError> {
    let CloudRoutingGrantSnapshot::Explicit {
        route_ids,
        route_group_ids,
    } = routing
    else {
        return Ok(());
    };
    if route_ids.len() > 500
        || route_group_ids.len() > 500
        || route_ids.iter().copied().collect::<BTreeSet<_>>().len() != route_ids.len()
        || route_group_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != route_group_ids.len()
    {
        return Err(AppError::BadRequest(
            "Cloud routing grants must contain at most 500 unique routes and route groups".into(),
        ));
    }
    Ok(())
}

pub(super) async fn reconcile_entitlement_in_transaction(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    tenant_id: &str,
    operation: EntitlementOperation,
    idempotency_key: &str,
) -> Result<(EntitlementReconcileResult, bool), AppError> {
    let canonical = serde_json::to_vec(&operation).map_err(|_| AppError::Internal)?;
    let request_hash = format!("{:x}", Sha256::digest(&canonical));
    let now = unix_millis();
    let reconciliation_id = Uuid::now_v7();
    if matches!(backend, DatabaseBackend::PostgreSql) {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948315))")
            .bind(format!("{tenant_id}:{idempotency_key}"))
            .execute(&mut **tx)
            .await?;
    } else {
        // `BEGIN IMMEDIATE` already serializes SQLite writers. Touching the
        // tenant also makes the lock ordering explicit for both call paths.
        sqlx::query("UPDATE tenants SET created_at = created_at WHERE id = $1")
            .bind(tenant_id)
            .execute(&mut **tx)
            .await?;
    }
    if let Some(replay) = sqlx::query(
        "SELECT request_hash, response_json FROM entitlement_reconciliations WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await?
    {
        let existing_hash: String = replay.try_get("request_hash")?;
        if existing_hash != request_hash {
            return Err(AppError::Conflict(
                "Idempotency-Key was already used for a different entitlement request".into(),
            ));
        }
        let response_json: String = replay.try_get("response_json")?;
        return serde_json::from_str(&response_json)
            .map(|response| (response, true))
            .map_err(|_| AppError::Internal);
    }

    let result = match operation {
        EntitlementOperation::Reconcile(input) => {
            reconcile_entitlement_snapshot(tx, tenant_id, &input, reconciliation_id, now).await?
        }
        EntitlementOperation::Cancel(input) => {
            cancel_entitlement_snapshot(tx, tenant_id, &input, reconciliation_id, now).await?
        }
        EntitlementOperation::Replace(input) => {
            replace_entitlement_snapshot(tx, tenant_id, &input, reconciliation_id, now).await?
        }
    };
    let response_json = serde_json::to_string(&result).map_err(|_| AppError::Internal)?;
    sqlx::query(
        "INSERT INTO entitlement_reconciliations (id, tenant_id, idempotency_key, request_hash, response_json, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(reconciliation_id.to_string())
    .bind(tenant_id)
    .bind(idempotency_key)
    .bind(request_hash)
    .bind(response_json)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok((result, false))
}

async fn lock_entitlement_account(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    tenant_id: &str,
    currency: Option<&str>,
) -> Result<String, AppError> {
    let locked = sqlx::query(
        "UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1 AND tenant_id = $2",
    )
    .bind(account_id.to_string())
    .bind(tenant_id)
    .execute(&mut **tx)
    .await?;
    if locked.rows_affected() != 1 {
        return Err(AppError::Forbidden);
    }
    let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = $1")
        .bind(account_id.to_string())
        .fetch_one(&mut **tx)
        .await?;
    let account_currency: String = row.try_get("currency")?;
    if currency.is_some_and(|value| !account_currency.eq_ignore_ascii_case(value)) {
        return Err(AppError::Conflict(
            "entitlement currency does not match the stable credit account".into(),
        ));
    }
    Ok(account_currency)
}

async fn entitlement_result_view(
    tx: &mut Transaction<'_, Any>,
    entitlement_id: &str,
) -> Result<EntitlementView, AppError> {
    let row = sqlx::query(
        "SELECT e.id AS entitlement_id, c.id AS cycle_id, t.external_id AS tenant_external_id, e.account_id, e.provider, e.external_subscription_id, c.external_cycle_id, c.period_start, c.period_end, c.currency, c.desired_micros, c.consumed_micros, c.funded_micros, e.status, e.version, e.replaced_by_entitlement_id, e.created_at, e.updated_at FROM subscription_entitlements e JOIN tenants t ON t.id = e.tenant_id JOIN entitlement_cycles c ON c.id = e.current_cycle_id WHERE e.id = $1",
    )
    .bind(entitlement_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    entitlement_view(row)
}

#[allow(clippy::too_many_arguments)]
async fn adjust_entitlement_cycle(
    tx: &mut Transaction<'_, Any>,
    account_id: Uuid,
    cycle_id: &str,
    desired_micros: i64,
    status: &str,
    revoke_remaining: bool,
    proration_json: Option<&str>,
    source: &str,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    let row = sqlx::query(
        "SELECT currency, funded_micros, consumed_micros FROM entitlement_cycles WHERE id = $1",
    )
    .bind(cycle_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let currency: String = row.try_get("currency")?;
    let funded_micros: i64 = row.try_get("funded_micros")?;
    let consumed_micros: i64 = row.try_get("consumed_micros")?;
    let target_funded = if revoke_remaining {
        consumed_micros
    } else {
        desired_micros.max(consumed_micros)
    };
    let delta = target_funded.saturating_sub(funded_micros);
    let account_update = if delta < 0 {
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3 AND available_micros >= $4",
        )
        .bind(delta)
        .bind(now)
        .bind(account_id.to_string())
        .bind(delta.saturating_abs())
        .execute(&mut **tx)
        .await?
    } else {
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3",
        )
        .bind(delta)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?
    };
    if account_update.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "entitlement remaining credit is no longer revocable".into(),
        ));
    }
    sqlx::query(
        "UPDATE entitlement_cycles SET desired_micros = $1, funded_micros = $2, status = $3, proration_json = $4, updated_at = $5 WHERE id = $6",
    )
    .bind(desired_micros)
    .bind(target_funded)
    .bind(status)
    .bind(proration_json)
    .bind(now)
    .bind(cycle_id)
    .execute(&mut **tx)
    .await?;
    if delta != 0 {
        sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, entitlement_cycle_id, created_at) VALUES ($1, $2, 'entitlement_adjustment', $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(delta)
        .bind(currency)
        .bind(source)
        .bind(format!("entitlement:{reconciliation_id}:{cycle_id}"))
        .bind(cycle_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
    }
    Ok(delta)
}

async fn reconcile_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &ReconcileEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    lock_entitlement_account(tx, input.account_id, tenant_id, Some(&input.currency)).await?;
    let existing = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?;
    let entitlement_id = if let Some(row) = existing {
        let existing_id: String = row.try_get("id")?;
        let existing_account: String = row.try_get("account_id")?;
        let status: String = row.try_get("status")?;
        let version: i64 = row.try_get("version")?;
        if parse_uuid(existing_account)? != input.account_id {
            return Err(AppError::Conflict(
                "a stable subscription cannot be moved to another credit account; use replace"
                    .into(),
            ));
        }
        if status == "replaced" {
            return Err(AppError::Conflict(
                "a replaced subscription cannot be reactivated".into(),
            ));
        }
        if input.version <= version {
            return Err(AppError::Conflict(format!(
                "entitlement version must be greater than {version}"
            )));
        }
        existing_id
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8)",
        )
        .bind(&id)
        .bind(tenant_id)
        .bind(input.account_id.to_string())
        .bind(&input.provider)
        .bind(&input.external_subscription_id)
        .bind(input.version)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        id
    };

    let existing_cycle = sqlx::query(
        "SELECT id, period_start, period_end, currency FROM entitlement_cycles WHERE entitlement_id = $1 AND external_cycle_id = $2",
    )
    .bind(&entitlement_id)
    .bind(&input.external_cycle_id)
    .fetch_optional(&mut **tx)
    .await?;
    let cycle_id = if let Some(row) = existing_cycle {
        if row.try_get::<i64, _>("period_start")? != input.period_start
            || row.try_get::<i64, _>("period_end")? != input.period_end
            || !row
                .try_get::<String, _>("currency")?
                .eq_ignore_ascii_case(&input.currency)
        {
            return Err(AppError::Conflict(
                "a stable billing-cycle identity cannot change period or currency".into(),
            ));
        }
        row.try_get("id")?
    } else {
        let id = Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, proration_json, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 0, 0, 0, 'active', $7, $8, $9)",
        )
        .bind(&id)
        .bind(&entitlement_id)
        .bind(&input.external_cycle_id)
        .bind(input.period_start)
        .bind(input.period_end)
        .bind(input.currency.to_uppercase())
        .bind(input.proration_json.as_deref())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        id
    };

    let stale_cycles = sqlx::query(
        "SELECT id, desired_micros, proration_json FROM entitlement_cycles WHERE entitlement_id = $1 AND id <> $2 AND status = 'active'",
    )
    .bind(&entitlement_id)
    .bind(&cycle_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut ledger_delta = 0_i64;
    for stale in stale_cycles {
        let stale_id: String = stale.try_get("id")?;
        let desired: i64 = stale.try_get("desired_micros")?;
        let proration: Option<String> = stale.try_get("proration_json")?;
        ledger_delta = ledger_delta.saturating_add(
            adjust_entitlement_cycle(
                tx,
                input.account_id,
                &stale_id,
                desired,
                "expired",
                true,
                proration.as_deref(),
                &input.source,
                reconciliation_id,
                now,
            )
            .await?,
        );
    }
    ledger_delta = ledger_delta.saturating_add(
        adjust_entitlement_cycle(
            tx,
            input.account_id,
            &cycle_id,
            input.desired_micros,
            "active",
            false,
            input.proration_json.as_deref(),
            &input.source,
            reconciliation_id,
            now,
        )
        .await?,
    );
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'active', version = $1, current_cycle_id = $2, replaced_by_entitlement_id = NULL, updated_at = $3 WHERE id = $4",
    )
    .bind(input.version)
    .bind(&cycle_id)
    .bind(now)
    .bind(&entitlement_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &entitlement_id).await?,
        ledger_delta: micros_to_decimal_string(ledger_delta),
        replaced_entitlement_id: None,
    })
}

async fn cancel_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &CancelEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    let row = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let entitlement_id: String = row.try_get("id")?;
    let account_id = parse_uuid(row.try_get("account_id")?)?;
    lock_entitlement_account(tx, account_id, tenant_id, None).await?;
    let row = sqlx::query(
        "SELECT status, version, current_cycle_id FROM subscription_entitlements WHERE id = $1",
    )
    .bind(&entitlement_id)
    .fetch_one(&mut **tx)
    .await?;
    let version: i64 = row.try_get("version")?;
    let status: String = row.try_get("status")?;
    let cycle_id: String = row.try_get("current_cycle_id")?;
    if status == "replaced" || input.version <= version {
        return Err(AppError::Conflict(format!(
            "cancellation version must be greater than {version}"
        )));
    }
    let cycle = sqlx::query(
        "SELECT external_cycle_id, desired_micros, proration_json FROM entitlement_cycles WHERE id = $1",
    )
    .bind(&cycle_id)
    .fetch_one(&mut **tx)
    .await?;
    let external_cycle_id: String = cycle.try_get("external_cycle_id")?;
    if input
        .external_cycle_id
        .as_deref()
        .is_some_and(|expected| expected != external_cycle_id)
    {
        return Err(AppError::Conflict(
            "cancellation billing-cycle identity is stale".into(),
        ));
    }
    let desired: i64 = cycle.try_get("desired_micros")?;
    let proration: Option<String> = cycle.try_get("proration_json")?;
    let delta = adjust_entitlement_cycle(
        tx,
        account_id,
        &cycle_id,
        desired,
        "cancelled",
        true,
        proration.as_deref(),
        &input.source,
        reconciliation_id,
        now,
    )
    .await?;
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'cancelled', version = $1, updated_at = $2 WHERE id = $3",
    )
    .bind(input.version)
    .bind(now)
    .bind(&entitlement_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &entitlement_id).await?,
        ledger_delta: micros_to_decimal_string(delta),
        replaced_entitlement_id: None,
    })
}

async fn replace_entitlement_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    input: &ReplaceEntitlementInput,
    reconciliation_id: Uuid,
    now: i64,
) -> Result<EntitlementReconcileResult, AppError> {
    let old = sqlx::query(
        "SELECT id, account_id, status, version, current_cycle_id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.provider)
    .bind(&input.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let old_id: String = old.try_get("id")?;
    let account_id = parse_uuid(old.try_get("account_id")?)?;
    lock_entitlement_account(tx, account_id, tenant_id, Some(&input.replacement.currency)).await?;
    let old = sqlx::query(
        "SELECT status, version, current_cycle_id FROM subscription_entitlements WHERE id = $1",
    )
    .bind(&old_id)
    .fetch_one(&mut **tx)
    .await?;
    let old_status: String = old.try_get("status")?;
    let old_version: i64 = old.try_get("version")?;
    let old_cycle_id: String = old.try_get("current_cycle_id")?;
    if old_status == "replaced" || input.version <= old_version {
        return Err(AppError::Conflict(format!(
            "replacement version must be greater than {old_version}"
        )));
    }
    if input.replacement.tenant_external_id != input.tenant_external_id
        || input.replacement.account_id != account_id
    {
        return Err(AppError::Conflict(
            "replacement must retain the tenant and stable credit account".into(),
        ));
    }
    if input.replacement.provider == input.provider
        && input.replacement.external_subscription_id == input.external_subscription_id
    {
        return Err(AppError::Conflict(
            "replacement subscription identity must be different".into(),
        ));
    }
    if sqlx::query(
        "SELECT id FROM subscription_entitlements WHERE tenant_id = $1 AND provider = $2 AND external_subscription_id = $3",
    )
    .bind(tenant_id)
    .bind(&input.replacement.provider)
    .bind(&input.replacement.external_subscription_id)
    .fetch_optional(&mut **tx)
    .await?
    .is_some()
    {
        return Err(AppError::Conflict(
            "replacement subscription identity already exists".into(),
        ));
    }
    let old_cycle =
        sqlx::query("SELECT desired_micros, proration_json FROM entitlement_cycles WHERE id = $1")
            .bind(&old_cycle_id)
            .fetch_one(&mut **tx)
            .await?;
    let old_desired: i64 = old_cycle.try_get("desired_micros")?;
    let old_proration: Option<String> = old_cycle.try_get("proration_json")?;
    let mut ledger_delta = adjust_entitlement_cycle(
        tx,
        account_id,
        &old_cycle_id,
        old_desired,
        "replaced",
        true,
        old_proration.as_deref(),
        &input.source,
        reconciliation_id,
        now,
    )
    .await?;

    let new_id = Uuid::now_v7().to_string();
    let new_cycle_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO subscription_entitlements (id, tenant_id, account_id, provider, external_subscription_id, status, version, current_cycle_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 'active', $6, $7, $8, $9)",
    )
    .bind(&new_id)
    .bind(tenant_id)
    .bind(account_id.to_string())
    .bind(&input.replacement.provider)
    .bind(&input.replacement.external_subscription_id)
    .bind(input.replacement.version)
    .bind(&new_cycle_id)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO entitlement_cycles (id, entitlement_id, external_cycle_id, period_start, period_end, currency, desired_micros, funded_micros, consumed_micros, status, proration_json, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 0, 0, 0, 'active', $7, $8, $9)",
    )
    .bind(&new_cycle_id)
    .bind(&new_id)
    .bind(&input.replacement.external_cycle_id)
    .bind(input.replacement.period_start)
    .bind(input.replacement.period_end)
    .bind(input.replacement.currency.to_uppercase())
    .bind(input.replacement.proration_json.as_deref())
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    ledger_delta = ledger_delta.saturating_add(
        adjust_entitlement_cycle(
            tx,
            account_id,
            &new_cycle_id,
            input.replacement.desired_micros,
            "active",
            false,
            input.replacement.proration_json.as_deref(),
            &input.source,
            reconciliation_id,
            now,
        )
        .await?,
    );
    sqlx::query(
        "UPDATE subscription_entitlements SET status = 'replaced', version = $1, replaced_by_entitlement_id = $2, updated_at = $3 WHERE id = $4",
    )
    .bind(input.version)
    .bind(&new_id)
    .bind(now)
    .bind(&old_id)
    .execute(&mut **tx)
    .await?;
    Ok(EntitlementReconcileResult {
        entitlement: entitlement_result_view(tx, &new_id).await?,
        ledger_delta: micros_to_decimal_string(ledger_delta),
        replaced_entitlement_id: Some(parse_uuid(old_id)?),
    })
}

fn entitlement_view(row: AnyRow) -> Result<EntitlementView, AppError> {
    let funded_micros: i64 = row.try_get("funded_micros")?;
    let consumed_micros: i64 = row.try_get("consumed_micros")?;
    Ok(EntitlementView {
        entitlement_id: parse_uuid(row.try_get("entitlement_id")?)?,
        cycle_id: parse_uuid(row.try_get("cycle_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        provider: row.try_get("provider")?,
        external_subscription_id: row.try_get("external_subscription_id")?,
        external_cycle_id: row.try_get("external_cycle_id")?,
        period_start: row.try_get("period_start")?,
        period_end: row.try_get("period_end")?,
        currency: row.try_get("currency")?,
        desired: micros_to_decimal_string(row.try_get("desired_micros")?),
        consumed: micros_to_decimal_string(consumed_micros),
        remaining: micros_to_decimal_string(funded_micros.saturating_sub(consumed_micros).max(0)),
        status: row.try_get("status")?,
        version: row.try_get("version")?,
        replaced_by_entitlement_id: row
            .try_get::<Option<String>, _>("replaced_by_entitlement_id")?
            .map(parse_uuid)
            .transpose()?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn validate_entitlement_identity(value: &str, field: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(AppError::BadRequest(format!(
            "{field} must contain 1 to 200 safe ASCII characters"
        )));
    }
    Ok(())
}

fn validate_reconcile_entitlement(input: &ReconcileEntitlementInput) -> Result<(), AppError> {
    validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
    validate_entitlement_identity(&input.provider, "provider")?;
    validate_entitlement_identity(&input.external_subscription_id, "external_subscription_id")?;
    validate_entitlement_identity(&input.external_cycle_id, "external_cycle_id")?;
    validate_currency(&input.currency)?;
    if input.period_start < 0 || input.period_end <= input.period_start {
        return Err(AppError::BadRequest(
            "billing period_end must be after a non-negative period_start".into(),
        ));
    }
    if input.desired_micros < 0 || input.version <= 0 {
        return Err(AppError::BadRequest(
            "desired entitlement cannot be negative and version must be positive".into(),
        ));
    }
    let source = input.source.trim();
    if source.is_empty() || source.len() > 200 || source.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "source must contain 1 to 200 non-control characters".into(),
        ));
    }
    if input
        .proration_json
        .as_deref()
        .is_some_and(|value| value.len() > 4_096)
    {
        return Err(AppError::BadRequest(
            "proration metadata exceeds 4 KiB".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_entitlement_operation(
    operation: &EntitlementOperation,
) -> Result<(), AppError> {
    match operation {
        EntitlementOperation::Reconcile(input) => validate_reconcile_entitlement(input),
        EntitlementOperation::Cancel(input) => {
            validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
            validate_entitlement_identity(&input.provider, "provider")?;
            validate_entitlement_identity(
                &input.external_subscription_id,
                "external_subscription_id",
            )?;
            if let Some(cycle) = &input.external_cycle_id {
                validate_entitlement_identity(cycle, "external_cycle_id")?;
            }
            if input.version <= 0
                || input.source.trim().is_empty()
                || input.source.len() > 200
                || input.source.chars().any(char::is_control)
            {
                return Err(AppError::BadRequest(
                    "cancellation requires a positive version and valid source".into(),
                ));
            }
            Ok(())
        }
        EntitlementOperation::Replace(input) => {
            validate_entitlement_identity(&input.tenant_external_id, "tenant_external_id")?;
            validate_entitlement_identity(&input.provider, "provider")?;
            validate_entitlement_identity(
                &input.external_subscription_id,
                "external_subscription_id",
            )?;
            if input.version <= 0
                || input.source.trim().is_empty()
                || input.source.len() > 200
                || input.source.chars().any(char::is_control)
            {
                return Err(AppError::BadRequest(
                    "replacement requires a positive version and valid source".into(),
                ));
            }
            validate_reconcile_entitlement(&input.replacement)
        }
    }
}

fn canonicalize_entitlement_operation(
    operation: &mut EntitlementOperation,
) -> Result<(), AppError> {
    fn canonicalize(input: &mut ReconcileEntitlementInput) -> Result<(), AppError> {
        if let Some(raw) = input.proration_json.as_mut() {
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
                AppError::BadRequest("proration metadata must be valid JSON".into())
            })?;
            *raw = serde_json::to_string(&value).map_err(|_| AppError::Internal)?;
        }
        Ok(())
    }
    match operation {
        EntitlementOperation::Reconcile(input) => canonicalize(input),
        EntitlementOperation::Cancel(_) => Ok(()),
        EntitlementOperation::Replace(input) => canonicalize(&mut input.replacement),
    }
}
