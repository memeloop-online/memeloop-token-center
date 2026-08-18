use super::super::*;

const KEY_PROVISIONING_AAD: &[u8] = b"memeloop-token-center/key-provisioning-response/v1";
const KEY_ROTATION_RESOURCE: &str = "key";

pub struct CreateKeyInput {
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub alias: String,
    pub currency: String,
    pub policy: KeyPolicy,
    pub initial_balance: Decimal,
    pub idempotency_key: Option<String>,
}

impl Database {
    pub async fn list_managed_keys(
        &self,
        tenant_external_id: Option<&str>,
        principal_external_id: Option<&str>,
    ) -> Result<Vec<ManagedKeyView>, AppError> {
        let rows = sqlx::query(
            "SELECT k.id, k.account_id, t.external_id AS tenant_external_id, p.external_id AS principal_external_id, k.alias, k.currency, k.status, k.credential_generation, COALESCE((SELECT c.fingerprint FROM key_credentials c WHERE c.key_id = k.id AND c.generation = k.credential_generation AND c.revoked_at IS NULL ORDER BY c.id LIMIT 1), (SELECT lc.fingerprint FROM legacy_key_credentials lc WHERE lc.key_id = k.id AND lc.generation = k.credential_generation AND lc.revoked_at IS NULL ORDER BY lc.id LIMIT 1)) AS fingerprint, k.created_at, k.updated_at, k.policy_json, a.available_micros, a.reserved_micros FROM key_records k JOIN tenants t ON t.id = k.tenant_id JOIN principals p ON p.id = k.principal_id JOIN credit_accounts a ON a.id = k.account_id WHERE ($1 = '' OR t.external_id = $1) AND ($2 = '' OR p.external_id = $2) ORDER BY k.created_at DESC, k.id DESC LIMIT 500",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(principal_external_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(managed_key_view).collect()
    }
    pub async fn set_key_status(&self, key_id: Uuid, status: &str) -> Result<String, AppError> {
        if !matches!(status, "active" | "suspended" | "revoked") {
            return Err(AppError::BadRequest(
                "credential status must be active, suspended, or revoked".into(),
            ));
        }
        let mut transaction = self.begin_write_transaction().await?;
        let current = sqlx::query("SELECT status FROM key_records WHERE id = $1")
            .bind(key_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("status")? == "revoked" && status != "revoked" {
            return Err(AppError::BadRequest(
                "a revoked credential cannot be reactivated".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE key_records SET status = $1, updated_at = $2 WHERE id = $3 AND NOT (status = 'revoked' AND $4 <> 'revoked')",
        )
        .bind(status)
        .bind(unix_millis())
        .bind(key_id.to_string())
        .bind(status)
        .execute(&mut *transaction)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if status == "revoked" {
            let now = unix_millis();
            sqlx::query(
                "UPDATE key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE legacy_key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(key_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(status.to_owned())
    }
    pub async fn create_key(
        &self,
        input: CreateKeyInput,
        pepper: &[u8],
    ) -> Result<IssuedKey, AppError> {
        validate_currency(&input.currency)?;
        validate_key_input(&input)?;
        let idempotency_key = input
            .idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if input.idempotency_key.is_some() && idempotency_key.is_none() {
            return Err(AppError::BadRequest(
                "Idempotency-Key cannot be empty".into(),
            ));
        }
        if idempotency_key.is_some_and(|value| {
            value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(AppError::BadRequest(
                "Idempotency-Key must be at most 200 visible ASCII characters".into(),
            ));
        }
        let provisioning_request_hash = idempotency_key.map(|_| {
            let canonical = serde_json::to_vec(&serde_json::json!({
                "tenant_external_id": input.tenant_external_id.trim(),
                "principal_external_id": input.principal_external_id.trim(),
                "alias": input.alias.trim(),
                "currency": input.currency.to_uppercase(),
                "policy": input.policy,
                "initial_balance": input.initial_balance.normalize().to_string()
            }))
            .expect("key provisioning request is JSON serializable");
            format!("{:x}", Sha256::digest(canonical))
        });
        let now = unix_millis();
        let tenant_id = Uuid::now_v7();
        let principal_id = Uuid::now_v7();
        let account_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        let issued = crypto::issue_credential(key_id, pepper);
        let policy_json = serde_json::to_string(&input.policy).map_err(|_| AppError::Internal)?;
        let initial_balance_micros = decimal_to_micros(input.initial_balance)?;
        let mut tx = self.pool.begin().await?;

        if let Some(idempotency_key) = idempotency_key {
            if matches!(self.backend, DatabaseBackend::PostgreSql) {
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948312))")
                    .bind(format!(
                        "{}:{idempotency_key}",
                        input.tenant_external_id.trim()
                    ))
                    .execute(&mut *tx)
                    .await?;
            }
            let existing = sqlx::query(
                "SELECT k.provisioning_request_hash, k.issued_key_ciphertext FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.provisioning_idempotency_key = $1 AND t.external_id = $2",
            )
            .bind(idempotency_key)
            .bind(input.tenant_external_id.trim())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                let existing_hash: Option<String> =
                    existing.try_get("provisioning_request_hash")?;
                if existing_hash.as_deref() != provisioning_request_hash.as_deref() {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used with a different key request".into(),
                    ));
                }
                let ciphertext: Option<String> = existing.try_get("issued_key_ciphertext")?;
                let issued = open_private_json(
                    ciphertext.as_deref().ok_or_else(|| {
                        AppError::BadRequest(
                            "idempotent key provisioning response is no longer available; rotate the key"
                                .into(),
                        )
                    })?,
                    pepper,
                    KEY_PROVISIONING_AAD,
                )?;
                tx.commit().await?;
                return Ok(issued);
            }
        }

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_id.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;

        sqlx::query(
            "INSERT INTO principals (id, tenant_id, external_id, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT(tenant_id, external_id) DO NOTHING",
        )
        .bind(principal_id.to_string())
        .bind(&tenant_id)
        .bind(&input.principal_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let principal_id: String =
            sqlx::query("SELECT id FROM principals WHERE tenant_id = $1 AND external_id = $2")
                .bind(&tenant_id)
                .bind(&input.principal_external_id)
                .fetch_one(&mut *tx)
                .await?
                .try_get("id")?;

        sqlx::query(
            "INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 0, $6, $7)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(input.currency.to_uppercase())
        .bind(initial_balance_micros)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO account_usage_state (account_id, settled_lifetime_micros, updated_at) VALUES ($1, 0, $2)",
        )
        .bind(account_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let issued_key = IssuedKey {
            key_id,
            account_id,
            alias: input.alias.clone(),
            currency: input.currency.to_uppercase(),
            credential_generation: 1,
            key: issued.secret.clone(),
            fingerprint: issued.fingerprint.clone(),
        };
        let issued_key_ciphertext = idempotency_key
            .map(|_| seal_private_json(&issued_key, pepper, KEY_PROVISIONING_AAD))
            .transpose()?;
        sqlx::query(
            "INSERT INTO key_records (id, tenant_id, principal_id, account_id, alias, currency, policy_json, status, credential_generation, provisioning_idempotency_key, provisioning_request_hash, issued_key_ciphertext, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', 1, $8, $9, $10, $11, $12)",
        )
        .bind(key_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(account_id.to_string())
        .bind(&input.alias)
        .bind(input.currency.to_uppercase())
        .bind(policy_json)
        .bind(idempotency_key)
        .bind(provisioning_request_hash)
        .bind(issued_key_ciphertext)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO key_budget_state (key_id, settled_lifetime_micros, reserved_micros, updated_at) VALUES ($1, 0, 0, $2)",
        )
        .bind(key_id.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        insert_credential(&mut tx, &issued, 1, now).await?;
        if initial_balance_micros != 0 {
            sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'grant', $4, $5, 'initial', $6)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account_id.to_string())
            .bind(key_id.to_string())
            .bind(initial_balance_micros)
            .bind(input.currency.to_uppercase())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(issued_key)
    }
    pub async fn update_key_policy(
        &self,
        key_id: Uuid,
        policy: KeyPolicy,
    ) -> Result<KeyPolicy, AppError> {
        validate_policy_budgets(&policy)?;
        let policy_json = serde_json::to_string(&policy).map_err(|_| AppError::Internal)?;
        let result = sqlx::query(
            "UPDATE key_records SET policy_json = $1, updated_at = $2 WHERE id = $3 AND status = 'active'",
        )
        .bind(policy_json)
        .bind(unix_millis())
        .bind(key_id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(policy)
    }
    pub async fn rotate_key(
        &self,
        key_id: Uuid,
        idempotency_key: &str,
        pepper: &[u8],
    ) -> Result<IssuedKey, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash = credential_rotation_request_hash(KEY_ROTATION_RESOURCE, key_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut tx,
            KEY_ROTATION_RESOURCE,
            key_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let issued = open_rotation_replay(
                replay,
                KEY_ROTATION_RESOURCE,
                key_id,
                idempotency_key,
                &request_hash,
                pepper,
                now,
            )?;
            tx.commit().await?;
            return Ok(issued);
        }

        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = $1"
            }
        };
        let row = sqlx::query(select)
            .bind(key_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status != "active" {
            return Err(AppError::Forbidden);
        }
        let generation: i64 = row.try_get::<i64, _>("credential_generation")? + 1;
        let account_id: String = row.try_get("account_id")?;
        let alias: String = row.try_get("alias")?;
        let currency: String = row.try_get("currency")?;
        let issued = crypto::issue_credential(key_id, pepper);

        sqlx::query(
            "UPDATE key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE legacy_key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        insert_credential(&mut tx, &issued, generation, now).await?;
        sqlx::query(
            "UPDATE key_records SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        let response = IssuedKey {
            key_id,
            account_id: Uuid::parse_str(&account_id).map_err(|_| AppError::Internal)?,
            alias,
            currency,
            credential_generation: generation,
            key: issued.secret,
            fingerprint: issued.fingerprint,
        };
        store_credential_rotation_response(
            &mut tx,
            idempotency_key,
            &response,
            KEY_ROTATION_RESOURCE,
            key_id,
            &request_hash,
            expires_at,
            pepper,
        )
        .await?;
        tx.commit().await?;
        Ok(response)
    }
    pub async fn authenticate_key(
        &self,
        value: &str,
        pepper: &[u8],
    ) -> Result<AuthenticatedKey, AppError> {
        let Some(parsed) = crypto::parse_credential(value) else {
            return super::legacy::authenticate_legacy_key(self, value, pepper).await;
        };
        let row = sqlx::query(
            "SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id AND c.generation = k.credential_generation WHERE k.id = $1 AND c.revoked_at IS NULL",
        )
        .bind(parsed.key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        super::authenticated_key_from_row(row, value, pepper)
    }
    pub async fn key_view(&self, key: &AuthenticatedKey) -> Result<KeyView, AppError> {
        let row = sqlx::query(
            "SELECT k.created_at, a.available_micros FROM key_records k JOIN credit_accounts a ON a.id = k.account_id WHERE k.id = $1",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(KeyView {
            key_id: key.key_id,
            alias: key.alias.clone(),
            currency: key.currency.clone(),
            credential_generation: key.credential_generation,
            created_at: row.try_get("created_at")?,
            policy: key.policy.clone(),
            available_balance: micros_to_decimal_string(row.try_get("available_micros")?),
        })
    }
    pub async fn rename_key(&self, key_id: Uuid, alias: &str) -> Result<KeyAliasView, AppError> {
        let alias = alias.trim();
        validate_key_alias(alias)?;
        let updated_at = unix_millis();
        let updated =
            sqlx::query("UPDATE key_records SET alias = $1, updated_at = $2 WHERE id = $3")
                .bind(alias)
                .bind(updated_at)
                .bind(key_id.to_string())
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        Ok(KeyAliasView {
            key_id,
            alias: alias.to_owned(),
            updated_at,
        })
    }
    pub async fn key_limit_snapshot(&self, key_id: Uuid) -> Result<KeyLimitSnapshot, AppError> {
        let captured_at = unix_millis();
        let key_id_string = key_id.to_string();
        let context = sqlx::query(
            "SELECT k.currency, k.policy_json, a.available_micros, a.reserved_micros AS account_reserved_micros, COALESCE(s.settled_lifetime_micros, 0) AS settled_lifetime_micros, COALESCE(s.reserved_micros, 0) AS key_reserved_micros FROM key_records k JOIN credit_accounts a ON a.id = k.account_id LEFT JOIN key_budget_state s ON s.key_id = k.id WHERE k.id = $1",
        )
        .bind(&key_id_string)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let policy_json: String = context.try_get("policy_json")?;
        let policy: KeyPolicy =
            serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;
        let key_reserved_micros: i64 = context.try_get("key_reserved_micros")?;
        let settled_lifetime_micros: i64 = context.try_get("settled_lifetime_micros")?;
        let daily_settled: i64 = sqlx::query(
            "SELECT COALESCE((SELECT settled_micros FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket = $2), 0) AS amount",
        )
        .bind(&key_id_string)
        .bind(captured_at / 86_400_000)
        .fetch_one(&self.pool)
        .await?
        .try_get("amount")?;
        let weekly_cutoff = captured_at.saturating_sub(7 * 86_400_000);
        let weekly = sqlx::query(
            "SELECT CAST(COALESCE(SUM(amount_micros), 0) AS BIGINT) AS amount, MIN(settled_at) AS oldest FROM key_budget_usage_events WHERE key_id = $1 AND settled_at >= $2",
        )
        .bind(&key_id_string)
        .bind(weekly_cutoff)
        .fetch_one(&self.pool)
        .await?;
        let weekly_settled: i64 = weekly.try_get("amount")?;
        let weekly_oldest: Option<i64> = weekly.try_get("oldest")?;
        let window_start = captured_at / 60_000 * 60_000;
        let rate = sqlx::query(
            "SELECT requests, tokens FROM rate_limit_windows WHERE key_id = $1 AND window_start = $2",
        )
        .bind(&key_id_string)
        .bind(window_start)
        .fetch_optional(&self.pool)
        .await?;
        let requests_used = rate
            .as_ref()
            .map(|row| row.try_get::<i64, _>("requests"))
            .transpose()?
            .unwrap_or(0)
            .max(0) as u64;
        let tokens_used = rate
            .as_ref()
            .map(|row| row.try_get::<i64, _>("tokens"))
            .transpose()?
            .unwrap_or(0)
            .max(0) as u64;
        let active = sqlx::query(
            "SELECT COUNT(*) AS active FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'",
        )
        .bind(&key_id_string)
        .fetch_one(&self.pool)
        .await?
        .try_get::<i64, _>("active")?
        .max(0) as u64;
        let rpm_limit = u64::from(policy.requests_per_minute);
        let tpm_limit = policy.tokens_per_minute;
        let concurrency_limit = u64::from(policy.max_concurrency);
        Ok(KeyLimitSnapshot {
            key_id,
            captured_at,
            currency: context.try_get("currency")?,
            available_balance: micros_to_decimal_string(context.try_get("available_micros")?),
            reserved_balance: micros_to_decimal_string(context.try_get("account_reserved_micros")?),
            rpm: KeyRateLimitSnapshot {
                limit: rpm_limit,
                used: requests_used,
                remaining: rpm_limit.saturating_sub(requests_used),
                reset_at: window_start + 60_000,
            },
            tpm: KeyRateLimitSnapshot {
                limit: tpm_limit,
                used: tokens_used,
                remaining: tpm_limit.saturating_sub(tokens_used),
                reset_at: window_start + 60_000,
            },
            concurrency: KeyConcurrencySnapshot {
                limit: policy.max_concurrency,
                active,
                remaining: concurrency_limit.saturating_sub(active),
            },
            daily_budget: key_budget_snapshot(
                policy.daily_budget.as_deref(),
                daily_settled,
                key_reserved_micros,
                Some((captured_at / 86_400_000 + 1) * 86_400_000),
            )?,
            weekly_budget: key_budget_snapshot(
                policy.weekly_budget.as_deref(),
                weekly_settled,
                key_reserved_micros,
                weekly_oldest.map(|oldest| oldest.saturating_add(7 * 86_400_000)),
            )?,
            lifetime_budget: key_budget_snapshot(
                policy.lifetime_budget.as_deref(),
                settled_lifetime_micros,
                key_reserved_micros,
                None,
            )?,
        })
    }
    pub async fn require_key_tenant(
        &self,
        key_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.id = $1 AND t.external_id = $2",
        )
        .bind(key_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }
    pub async fn expire_key_provisioning_responses(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(24 * 60 * 60 * 1_000);
        let now = unix_millis();
        let limit = limit.clamp(1, 10_000);
        let mut transaction = self.pool.begin().await?;
        let provisioning = sqlx::query(
            "UPDATE key_records SET issued_key_ciphertext = NULL WHERE id IN (SELECT id FROM key_records WHERE issued_key_ciphertext IS NOT NULL AND created_at < $1 ORDER BY created_at, id LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&mut *transaction)
        .await?;
        let rotations = sqlx::query(
            "UPDATE credential_rotation_replays SET response_ciphertext = NULL WHERE idempotency_key IN (SELECT idempotency_key FROM credential_rotation_replays WHERE response_ciphertext IS NOT NULL AND expires_at <= $1 ORDER BY expires_at, idempotency_key LIMIT $2)",
        )
        .bind(now)
        .bind(limit)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(provisioning
            .rows_affected()
            .saturating_add(rotations.rows_affected()))
    }
}

async fn insert_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO key_credentials (id, key_id, generation, secret_hash, fingerprint, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(issued.credential_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(generation)
    .bind(issued.secret_hash.clone())
    .bind(&issued.fingerprint)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
fn managed_key_view(row: AnyRow) -> Result<ManagedKeyView, AppError> {
    let policy_json: String = row.try_get("policy_json")?;
    Ok(ManagedKeyView {
        key_id: parse_uuid(row.try_get("id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        principal_external_id: row.try_get("principal_external_id")?,
        alias: row.try_get("alias")?,
        currency: row.try_get("currency")?,
        status: row.try_get("status")?,
        credential_generation: row.try_get("credential_generation")?,
        fingerprint: row.try_get("fingerprint")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
        available_balance: micros_to_decimal_string(row.try_get("available_micros")?),
        reserved_balance: micros_to_decimal_string(row.try_get("reserved_micros")?),
    })
}
fn validate_policy_budgets(policy: &KeyPolicy) -> Result<(), AppError> {
    if policy.requests_per_minute == 0
        || policy.tokens_per_minute == 0
        || policy.max_concurrency == 0
    {
        return Err(AppError::BadRequest(
            "RPM, TPM, and maximum concurrency must be positive".into(),
        ));
    }
    if policy.allowed_models.len() > 500
        || policy.allowed_models.iter().any(|model| {
            model.trim().is_empty()
                || model.chars().count() > 200
                || model.chars().any(char::is_control)
        })
    {
        return Err(AppError::BadRequest(
            "allowed models must contain at most 500 non-empty model names".into(),
        ));
    }
    if policy.tokens_per_minute > JSON_SAFE_INTEGER_MAX {
        return Err(AppError::BadRequest(format!(
            "TPM must not exceed the JSON safe integer maximum {JSON_SAFE_INTEGER_MAX}"
        )));
    }
    for value in [
        policy.daily_budget.as_deref(),
        policy.weekly_budget.as_deref(),
        policy.lifetime_budget.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let decimal = Decimal::from_str_exact(value)
            .map_err(|_| AppError::BadRequest("key budgets must be decimal strings".into()))?;
        if decimal.is_sign_negative() {
            return Err(AppError::BadRequest(
                "key budgets cannot be negative".into(),
            ));
        }
        decimal_to_micros(decimal)?;
    }
    Ok(())
}
fn validate_key_input(input: &CreateKeyInput) -> Result<(), AppError> {
    for (field, value) in [
        ("tenant_external_id", input.tenant_external_id.as_str()),
        (
            "principal_external_id",
            input.principal_external_id.as_str(),
        ),
        ("alias", input.alias.as_str()),
    ] {
        let value = value.trim();
        if value.is_empty() || value.chars().count() > 200 || value.chars().any(char::is_control) {
            return Err(AppError::BadRequest(format!(
                "{field} must contain 1 to 200 non-control characters"
            )));
        }
    }
    validate_policy_budgets(&input.policy)
}
fn validate_key_alias(alias: &str) -> Result<(), AppError> {
    if alias.is_empty() || alias.chars().count() > 200 || alias.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "alias must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}
fn key_budget_snapshot(
    configured: Option<&str>,
    settled_micros: i64,
    reserved_micros: i64,
    reset_at: Option<i64>,
) -> Result<KeyBudgetSnapshot, AppError> {
    let limit_micros = configured
        .map(|value| {
            Decimal::from_str_exact(value)
                .map_err(|_| AppError::Internal)
                .and_then(decimal_to_micros)
        })
        .transpose()?;
    Ok(KeyBudgetSnapshot {
        limit: configured.map(str::to_owned),
        settled: micros_to_decimal_string(settled_micros.max(0)),
        reserved: micros_to_decimal_string(reserved_micros.max(0)),
        remaining: limit_micros.map(|limit| {
            micros_to_decimal_string(
                limit
                    .saturating_sub(settled_micros)
                    .saturating_sub(reserved_micros)
                    .max(0),
            )
        }),
        reset_at: configured.and(reset_at),
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::*;

    #[tokio::test]
    async fn sqlite_key_rotation_is_concurrent_idempotent_and_resource_bound() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("key-rotation.db").display()
        );
        let database = Database::connect_with_max(&database_url, 8).await.unwrap();
        database.migrate().await.unwrap();
        let pepper: &'static [u8] = b"a rotation credential pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "stable-identity".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        allowed_models: vec!["codex-test".to_owned()],
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
        let key_id = issued.key_id;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let database = database.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                database
                    .rotate_key(key_id, "same-key-rotation", pepper)
                    .await
            }));
        }
        let mut rotated = Vec::new();
        for task in tasks {
            rotated.push(task.await.unwrap().unwrap());
        }
        assert!(rotated.iter().all(|value| {
            value.key_id == issued.key_id
                && value.account_id == issued.account_id
                && value.credential_generation == 2
                && value.key == rotated[0].key
        }));
        assert!(matches!(
            database.authenticate_key(&issued.key, pepper).await,
            Err(AppError::Unauthorized)
        ));
        let authenticated = database
            .authenticate_key(&rotated[0].key, pepper)
            .await
            .unwrap();
        assert_eq!(authenticated.account_id, issued.account_id);
        assert!(authenticated.policy.allows_model("codex-test"));

        let service = database
            .create_service_token(
                CreateServiceTokenInput {
                    name: "resource-binding-test".to_owned(),
                    scopes: vec!["keys:write".to_owned()],
                    tenant_external_id: None,
                },
                pepper,
            )
            .await
            .unwrap();
        assert!(matches!(
            database
                .rotate_service_token(service.service_id, "same-key-rotation", pepper)
                .await,
            Err(AppError::BadRequest(_))
        ));

        let replay_row = sqlx::query(
            "SELECT request_hash, response_ciphertext FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind("same-key-rotation")
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let request_hash: String = replay_row.try_get("request_hash").unwrap();
        let ciphertext: String = replay_row.try_get("response_ciphertext").unwrap();
        assert_eq!(request_hash.len(), 64);
        assert!(!ciphertext.contains(&rotated[0].key));

        sqlx::query(
            "UPDATE credential_rotation_replays SET expires_at = $1 WHERE idempotency_key = $2",
        )
        .bind(unix_millis().saturating_sub(1))
        .bind("same-key-rotation")
        .execute(&database.pool)
        .await
        .unwrap();
        database
            .expire_key_provisioning_responses(100)
            .await
            .unwrap();
        let expired: Option<String> = sqlx::query(
            "SELECT response_ciphertext FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind("same-key-rotation")
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("response_ciphertext")
        .unwrap();
        assert!(expired.is_none());
        assert!(matches!(
            database
                .rotate_key(issued.key_id, "same-key-rotation", pepper)
                .await,
            Err(AppError::BadRequest(_))
        ));
        let generation: i64 =
            sqlx::query("SELECT credential_generation FROM key_records WHERE id = $1")
                .bind(issued.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("credential_generation")
                .unwrap();
        assert_eq!(generation, 2);
    }
    #[tokio::test]
    async fn key_provisioning_replays_one_encrypted_response_for_an_idempotency_key() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("key-idempotency.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let request = |alias: &str| CreateKeyInput {
            tenant_external_id: "tenant".to_owned(),
            principal_external_id: "member".to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy::default(),
            initial_balance: Decimal::ONE,
            idempotency_key: Some("registration-event-1".to_owned()),
        };

        let first = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        let replay = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        assert_eq!(replay.key_id, first.key_id);
        assert_eq!(replay.account_id, first.account_id);
        assert_eq!(replay.key, first.key);
        assert!(matches!(
            database.create_key(request("different"), pepper).await,
            Err(AppError::BadRequest(_))
        ));

        let count: i64 = sqlx::query("SELECT COUNT(*) AS count FROM key_records")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("count")
            .unwrap();
        assert_eq!(count, 1);
        let ciphertext: String =
            sqlx::query("SELECT issued_key_ciphertext FROM key_records WHERE id = $1")
                .bind(first.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("issued_key_ciphertext")
                .unwrap();
        assert!(!ciphertext.contains(&first.key));

        let other_tenant = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "other-tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "primary".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: Some("registration-event-1".to_owned()),
                },
                pepper,
            )
            .await
            .unwrap();
        assert_ne!(other_tenant.key_id, first.key_id);
    }
    #[tokio::test]
    async fn alias_and_limit_snapshot_keep_stable_identity_and_authoritative_concurrency() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("credential-limits.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"credential limits pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "credential-limits".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "before".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy {
                        requests_per_minute: 3,
                        tokens_per_minute: 1_000,
                        max_concurrency: 1,
                        daily_budget: Some("0.01".to_owned()),
                        weekly_budget: Some("0.02".to_owned()),
                        lifetime_budget: Some("0.03".to_owned()),
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::TWO,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let before = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let renamed = database
            .rename_key(issued.key_id, "  after  ")
            .await
            .unwrap();
        assert_eq!(renamed.key_id, issued.key_id);
        assert_eq!(renamed.alias, "after");
        let after = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        assert_eq!(after.key_id, before.key_id);
        assert_eq!(after.account_id, before.account_id);
        assert_eq!(after.credential_generation, before.credential_generation);
        assert_eq!(
            serde_json::to_value(&after.policy).unwrap(),
            serde_json::to_value(&before.policy).unwrap()
        );
        assert_eq!(after.alias, "after");

        let price = database
            .upsert_model_price("credential-limits", "USD", Decimal::ZERO, Decimal::ONE)
            .await
            .unwrap();
        let reservation = database
            .reserve_usage(&after, &price, 0, 100)
            .await
            .unwrap();

        let snapshot = database.key_limit_snapshot(issued.key_id).await.unwrap();
        assert_eq!(snapshot.key_id, issued.key_id);
        assert_eq!(snapshot.available_balance, "1.9999");
        assert_eq!(snapshot.reserved_balance, "0.0001");
        assert_eq!(snapshot.rpm.limit, 3);
        assert_eq!(snapshot.rpm.used, 1);
        assert_eq!(snapshot.tpm.limit, 1_000);
        assert_eq!(snapshot.tpm.used, 100);
        assert_eq!(snapshot.concurrency.limit, 1);
        assert_eq!(snapshot.concurrency.active, 1);
        assert_eq!(snapshot.concurrency.remaining, 0);
        assert_eq!(snapshot.daily_budget.limit.as_deref(), Some("0.01"));
        assert_eq!(snapshot.daily_budget.settled, "0");
        assert_eq!(snapshot.daily_budget.reserved, "0.0001");
        assert_eq!(snapshot.daily_budget.remaining.as_deref(), Some("0.0099"));
        assert!(matches!(
            database.reserve_usage(&after, &price, 0, 100).await,
            Err(AppError::LimitExceeded {
                reason: LimitReason::ConcurrencyExhausted,
                retry_after_seconds: Some(1),
            })
        ));

        database.settle_usage(&reservation, 0, 100).await.unwrap();
        let settled = database.key_limit_snapshot(issued.key_id).await.unwrap();
        assert_eq!(settled.concurrency.active, 0);
        assert_eq!(settled.concurrency.remaining, 1);
        assert_eq!(settled.daily_budget.settled, "0.0001");
        assert_eq!(settled.daily_budget.reserved, "0");
        let next = database
            .reserve_usage(&after, &price, 0, 100)
            .await
            .unwrap();
        database.settle_usage(&next, 0, 100).await.unwrap();
    }
}
