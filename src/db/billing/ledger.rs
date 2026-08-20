use super::super::*;

async fn account_usage_snapshot(
    transaction: &mut Transaction<'_, Any>,
    account_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    // Migration-era and external importers may have created a legitimate
    // credit account without the rollup row introduced in schema v22. Repair
    // that invariant while the account is locked. Deriving the initial value
    // from the durable ledger, instead of blindly assuming zero, preserves the
    // rule that a grant cannot be reversed after settled usage.
    sqlx::query(
        "INSERT INTO account_usage_state (account_id, settled_lifetime_micros, updated_at) SELECT a.id, COALESCE((SELECT SUM(CASE WHEN l.amount_micros < 0 THEN -l.amount_micros ELSE 0 END) FROM ledger_entries l WHERE l.account_id = a.id AND l.kind = 'usage'), 0), $2 FROM credit_accounts a WHERE a.id = $1 ON CONFLICT(account_id) DO NOTHING",
    )
    .bind(account_id.to_string())
    .bind(now)
    .execute(&mut **transaction)
    .await?;

    let state = sqlx::query(
        "SELECT settled_lifetime_micros FROM account_usage_state WHERE account_id = $1",
    )
    .bind(account_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(state.try_get("settled_lifetime_micros")?)
}

impl Database {
    pub async fn require_account_exists(&self, account_id: Uuid) -> Result<(), AppError> {
        let exists = sqlx::query("SELECT id FROM credit_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        exists.then_some(()).ok_or(AppError::NotFound)
    }

    pub async fn list_account_ledger(
        &self,
        account_id: Uuid,
        limit: i64,
    ) -> Result<Vec<LedgerEntryView>, AppError> {
        self.list_account_ledger_page(account_id, limit, None).await
    }

    /// Lists ledger entries using an exclusive descending
    /// `(created_at, entry_id)` cursor. Account ownership is checked by the
    /// HTTP boundary before this method is called; the account predicate is
    /// retained in every page query so entries cannot cross accounts.
    pub async fn list_account_ledger_page(
        &self,
        account_id: Uuid,
        limit: i64,
        before: Option<(i64, Uuid)>,
    ) -> Result<Vec<LedgerEntryView>, AppError> {
        let (before_created_at, before_id) = before
            .map(|(created_at, id)| (created_at, id.to_string()))
            .unwrap_or_else(|| (i64::MAX, "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned()));
        let rows = sqlx::query(
            "SELECT id, kind, amount_micros, currency, source, idempotency_key, created_at FROM ledger_entries WHERE account_id = $1 AND (created_at < $2 OR (created_at = $2 AND id < $3)) ORDER BY created_at DESC, id DESC LIMIT $4",
        )
        .bind(account_id.to_string())
        .bind(before_created_at)
        .bind(before_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(LedgerEntryView {
                    entry_id: parse_uuid(row.try_get("id")?)?,
                    kind: row.try_get("kind")?,
                    amount: micros_to_decimal_string(row.try_get("amount_micros")?),
                    currency: row.try_get("currency")?,
                    source: row.try_get("source")?,
                    idempotency_key: row.try_get("idempotency_key")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    pub async fn grant(
        &self,
        account_id: Uuid,
        amount: Decimal,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }
        let amount_micros = decimal_to_micros(amount)?;
        if amount_micros <= 0 {
            return Err(AppError::BadRequest("grant amount must be positive".into()));
        }
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let account_lock =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_lock.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let currency: String = row.try_get("currency")?;
        let usage_snapshot = account_usage_snapshot(&mut tx, account_id, now).await?;
        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, created_at, account_usage_micros_snapshot) VALUES ($1, $2, 'grant', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(now)
        .bind(usage_snapshot)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT amount_micros, source FROM ledger_entries WHERE idempotency_key = $1 AND account_id = $2 AND kind = 'grant'",
            )
            .bind(idempotency_key)
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Forbidden)?;
            let existing_amount: i64 = existing.try_get("amount_micros")?;
            let existing_source: String = existing.try_get("source")?;
            if existing_amount != amount_micros || existing_source != source {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different grant".into(),
                ));
            }
            tx.commit().await?;
            return Ok(micros_to_decimal_string(existing_amount));
        }
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn reverse_grant(
        &self,
        account_id: Uuid,
        grant_idempotency_key: &str,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(grant_idempotency_key, "grant_idempotency_key")?;
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let account_lock =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_lock.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let original = sqlx::query(
            "SELECT id, amount_micros, currency, account_usage_micros_snapshot FROM ledger_entries WHERE account_id = $1 AND kind = 'grant' AND idempotency_key = $2",
        )
        .bind(account_id.to_string())
        .bind(grant_idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let original_id: String = original.try_get("id")?;
        let amount_micros: i64 = original.try_get("amount_micros")?;
        let currency: String = original.try_get("currency")?;
        let usage_snapshot: i64 = original.try_get("account_usage_micros_snapshot")?;
        let current_usage = account_usage_snapshot(&mut tx, account_id, now).await?;
        if current_usage != usage_snapshot {
            return Err(AppError::BadRequest(
                "grant cannot be automatically reversed after account usage".into(),
            ));
        }

        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, reference_entry_id, created_at) VALUES ($1, $2, 'grant_reversal', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(-amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(&original_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let replay = sqlx::query(
                "SELECT amount_micros, reference_entry_id FROM ledger_entries WHERE account_id = $1 AND kind = 'grant_reversal' AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(replay) = replay {
                let replay_reference: Option<String> = replay.try_get("reference_entry_id")?;
                if replay_reference.as_deref() != Some(original_id.as_str()) {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used for a different grant reversal".into(),
                    ));
                }
                let replay_amount: i64 = replay.try_get("amount_micros")?;
                tx.commit().await?;
                return Ok(micros_to_decimal_string(replay_amount.saturating_abs()));
            }
            let existing_idempotency = sqlx::query(
                "SELECT kind FROM ledger_entries WHERE account_id = $1 AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if existing_idempotency.is_some() {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different ledger operation".into(),
                ));
            }
            return Err(AppError::BadRequest("grant was already reversed".into()));
        }

        let updated = sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros - $1, updated_at = $2 WHERE id = $3 AND currency = $4 AND available_micros >= $5",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .bind(&currency)
        .bind(amount_micros)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            let exists = sqlx::query("SELECT id FROM credit_accounts WHERE id = $1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
            return Err(if exists {
                AppError::QuotaExceeded
            } else {
                AppError::NotFound
            });
        }
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }
}
