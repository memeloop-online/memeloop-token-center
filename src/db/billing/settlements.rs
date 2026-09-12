use super::super::*;

impl Database {
    pub async fn list_account_settlements(
        &self,
        account_id: Uuid,
        limit: i64,
        after: Option<(i64, Uuid)>,
        exact: Option<(AccountSettlementKind, Uuid)>,
    ) -> Result<AccountSettlementPage, AppError> {
        if !(1..=500).contains(&limit) || (exact.is_some() && after.is_some()) {
            return Err(AppError::BadRequest("invalid settlement query".into()));
        }
        if let Some((sequence, id)) = after {
            let valid_cursor = sqlx::query(
                "SELECT settlement_id FROM account_settlement_feed WHERE account_id = $1 AND settlement_sequence = $2 AND settlement_id = $3",
            )
            .bind(account_id.to_string())
            .bind(sequence)
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .is_some();
            if !valid_cursor {
                return Err(AppError::BadRequest(
                    "after cursor does not identify an account settlement".into(),
                ));
            }
        }
        let (after_sequence, after_id) = after
            .map(|(sequence, id)| (sequence, id.to_string()))
            .unwrap_or_else(|| (-1, "00000000-0000-0000-0000-000000000000".to_owned()));
        let (exact_kind, exact_request_id) = exact
            .map(|(kind, id)| (settlement_kind_name(kind), id.to_string()))
            .unwrap_or(("", String::new()));
        let fetch_limit = if exact.is_some() {
            2
        } else {
            limit.saturating_add(1)
        };
        let rows = sqlx::query(
            "SELECT settlement_id, settlement_sequence, request_id, request_kind, account_id, key_id, model, cost_micros, currency, settled_at, completed_at, input_tokens, cached_input_tokens, cache_write_tokens, output_tokens FROM account_settlement_feed WHERE account_id = $1 AND (settlement_sequence > $2 OR (settlement_sequence = $2 AND settlement_id > $3)) AND ($4 = '' OR (request_kind = $4 AND request_id = $5)) ORDER BY settlement_sequence ASC, settlement_id ASC LIMIT $6",
        )
        .bind(account_id.to_string())
        .bind(after_sequence)
        .bind(after_id)
        .bind(exact_kind)
        .bind(exact_request_id)
        .bind(fetch_limit)
        .fetch_all(&self.pool)
        .await?;
        if exact.is_some() && rows.len() > 1 {
            return Err(AppError::Internal);
        }
        let mut items = rows
            .iter()
            .map(account_settlement_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = exact.is_none() && items.len() > limit as usize;
        if has_more {
            items.truncate(limit as usize);
        }
        let next_cursor = has_more.then(|| {
            let last = items.last().expect("a page with more rows is non-empty");
            AccountSettlementCursor {
                after_sequence: last.settlement_sequence,
                after_id: last.settlement_id,
            }
        });
        Ok(AccountSettlementPage { items, next_cursor })
    }
}

fn settlement_kind_name(kind: AccountSettlementKind) -> &'static str {
    match kind {
        AccountSettlementKind::Text => "text",
        AccountSettlementKind::Generation => "generation",
    }
}

pub(crate) async fn publish_text_settlement_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT r.id AS request_id, 'text' AS request_kind, u.account_id, r.key_id, r.model, r.cost_micros, r.completed_at, r.input_tokens, r.cached_input_tokens, r.cache_write_tokens, r.output_tokens, u.enforcement_mode, l.id AS settlement_id, l.amount_micros, l.currency, l.created_at AS settled_at FROM request_records r JOIN usage_reservations u ON u.id = r.reservation_id LEFT JOIN ledger_entries l ON l.account_id = u.account_id AND l.key_id = u.key_id AND l.kind = 'usage' AND l.source = u.id WHERE r.id = $1 AND r.completed_at IS NOT NULL",
    )
    .bind(request_id.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    publish_settlement_row(tx, row).await
}

pub(crate) async fn publish_generation_settlement_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT g.id AS request_id, 'generation' AS request_kind, u.account_id, g.key_id, g.public_model AS model, g.cost_micros, g.completed_at, CAST(NULL AS BIGINT) AS input_tokens, CAST(NULL AS BIGINT) AS cached_input_tokens, CAST(NULL AS BIGINT) AS cache_write_tokens, CAST(NULL AS BIGINT) AS output_tokens, u.enforcement_mode, l.id AS settlement_id, l.amount_micros, l.currency, l.created_at AS settled_at FROM generation_jobs g JOIN usage_reservations u ON u.id = g.reservation_id LEFT JOIN ledger_entries l ON l.account_id = u.account_id AND l.key_id = u.key_id AND l.kind = 'usage' AND l.source = u.id WHERE g.id = $1 AND g.completed_at IS NOT NULL",
    )
    .bind(request_id.to_string())
    .fetch_optional(&mut **tx)
    .await?;
    publish_settlement_row(tx, row).await
}

async fn publish_settlement_row(
    tx: &mut Transaction<'_, Any>,
    row: Option<AnyRow>,
) -> Result<bool, AppError> {
    let Some(row) = row else {
        return Ok(false);
    };
    let enforcement_mode =
        EnforcementMode::from_storage(row.try_get::<String, _>("enforcement_mode")?.as_str())
            .ok_or(AppError::Internal)?;
    if enforcement_mode == EnforcementMode::MeteredUnlimited {
        return Ok(false);
    }
    let settlement_id: String = row
        .try_get::<Option<String>, _>("settlement_id")?
        .ok_or(AppError::Internal)?;
    let amount_micros: i64 = row
        .try_get::<Option<i64>, _>("amount_micros")?
        .ok_or(AppError::Internal)?;
    let cost_micros: i64 = row.try_get("cost_micros")?;
    if cost_micros < 0 || amount_micros.checked_neg() != Some(cost_micros) {
        return Err(AppError::Internal);
    }
    let account_id: String = row.try_get("account_id")?;
    let request_id: String = row.try_get("request_id")?;
    let request_kind: String = row.try_get("request_kind")?;

    let locked = sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
        .bind(&account_id)
        .execute(&mut **tx)
        .await?;
    if locked.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    let existing = sqlx::query(
        "SELECT settlement_id FROM account_settlement_feed WHERE request_kind = $1 AND request_id = $2",
    )
    .bind(&request_kind)
    .bind(&request_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(existing) = existing {
        return if existing.try_get::<String, _>("settlement_id")? == settlement_id {
            Ok(true)
        } else {
            Err(AppError::Internal)
        };
    }
    let sequence: i64 = sqlx::query_scalar(
        "UPDATE credit_accounts SET settlement_sequence = settlement_sequence + 1 WHERE id = $1 RETURNING settlement_sequence",
    )
    .bind(&account_id)
    .fetch_one(&mut **tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO account_settlement_feed (settlement_id, account_id, settlement_sequence, request_id, request_kind, key_id, model, cost_micros, currency, settled_at, completed_at, input_tokens, cached_input_tokens, cache_write_tokens, output_tokens) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(settlement_id)
    .bind(account_id)
    .bind(sequence)
    .bind(request_id)
    .bind(request_kind)
    .bind(row.try_get::<String, _>("key_id")?)
    .bind(row.try_get::<String, _>("model")?)
    .bind(cost_micros)
    .bind(
        row.try_get::<Option<String>, _>("currency")?
            .ok_or(AppError::Internal)?,
    )
    .bind(
        row.try_get::<Option<i64>, _>("settled_at")?
            .ok_or(AppError::Internal)?,
    )
    .bind(row.try_get::<i64, _>("completed_at")?)
    .bind(row.try_get::<Option<i64>, _>("input_tokens")?)
    .bind(row.try_get::<Option<i64>, _>("cached_input_tokens")?)
    .bind(row.try_get::<Option<i64>, _>("cache_write_tokens")?)
    .bind(row.try_get::<Option<i64>, _>("output_tokens")?)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(true)
}

fn account_settlement_from_row(row: &AnyRow) -> Result<AccountSettlementView, AppError> {
    let cost_micros: i64 = row.try_get("cost_micros")?;
    if cost_micros < 0 {
        return Err(AppError::Internal);
    }
    let kind = match row.try_get::<String, _>("request_kind")?.as_str() {
        "text" => AccountSettlementKind::Text,
        "generation" => AccountSettlementKind::Generation,
        _ => return Err(AppError::Internal),
    };
    Ok(AccountSettlementView {
        settlement_id: parse_uuid(row.try_get("settlement_id")?)?,
        settlement_sequence: row.try_get("settlement_sequence")?,
        request_id: parse_uuid(row.try_get("request_id")?)?,
        kind,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        key_id: parse_uuid(row.try_get("key_id")?)?,
        model: row.try_get("model")?,
        cost: micros_to_decimal_string(cost_micros),
        currency: row.try_get("currency")?,
        settled_at: row.try_get("settled_at")?,
        completed_at: row.try_get("completed_at")?,
        input_tokens: row.try_get("input_tokens")?,
        cached_input_tokens: row.try_get("cached_input_tokens")?,
        cache_write_tokens: row.try_get("cache_write_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
    })
}
