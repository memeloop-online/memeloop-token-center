use super::super::*;

impl Database {
    pub async fn delete_expired_rate_windows(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(2 * 24 * 60 * 60 * 1_000);
        let rows = sqlx::query(
            "DELETE FROM rate_limit_windows WHERE (key_id, window_start) IN (SELECT key_id, window_start FROM rate_limit_windows WHERE window_start < $1 ORDER BY window_start ASC, key_id ASC LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit.clamp(1, 100_000))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }

    pub async fn delete_expired_budget_rollups(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff_day = unix_millis().saturating_sub(7 * 86_400_000) / 86_400_000;
        let cutoff_at = cutoff_day.saturating_mul(86_400_000);
        let limit = limit.clamp(1, 100_000);
        let mut tx = self.begin_write_transaction().await?;
        let events = sqlx::query(
            "DELETE FROM key_budget_usage_events WHERE usage_entry_id IN (SELECT usage_entry_id FROM key_budget_usage_events WHERE settled_at < $1 ORDER BY settled_at ASC, usage_entry_id ASC LIMIT $2)",
        )
        .bind(cutoff_at)
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        let daily = sqlx::query(
            "DELETE FROM key_budget_daily_rollups WHERE (key_id, day_bucket) IN (SELECT key_id, day_bucket FROM key_budget_daily_rollups WHERE day_bucket < $1 ORDER BY day_bucket ASC, key_id ASC LIMIT $2)",
        )
        .bind(cutoff_day)
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(events.rows_affected().saturating_add(daily.rows_affected()))
    }

    pub async fn reserve_usage(
        &self,
        key: &AuthenticatedKey,
        price: &ModelPrice,
        input_token_ceiling: i64,
        output_token_ceiling: i64,
    ) -> Result<UsageReservation, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let reservation = reserve_usage_in_transaction(
            &mut tx,
            key,
            price,
            input_token_ceiling,
            output_token_ceiling,
            unix_millis(),
        )
        .await?;
        tx.commit().await?;
        Ok(reservation)
    }

    pub async fn settle_usage(
        &self,
        reservation: &UsageReservation,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Result<i64, AppError> {
        self.settle_token_usage(
            reservation,
            &TokenUsage {
                input_tokens,
                output_tokens,
                ..TokenUsage::default()
            },
        )
        .await
    }

    pub async fn settle_token_usage(
        &self,
        reservation: &UsageReservation,
        usage: &TokenUsage,
    ) -> Result<i64, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let actual_micros =
            settle_token_usage_in_transaction(&mut tx, reservation, usage, unix_millis()).await?;
        tx.commit().await?;
        Ok(actual_micros)
    }

    pub async fn release_orphaned_reservations(&self, limit: i64) -> Result<u64, AppError> {
        let now = unix_millis();
        let cutoff = now.saturating_sub(30 * 60 * 1_000);
        let rows = sqlx::query(
            "SELECT r.id, r.account_id, r.key_id, r.enforcement_mode, r.reserved_micros, r.reserved_tokens, r.rate_window_start, q.id AS request_id, q.created_at AS request_created_at, q.tenant_id AS request_tenant_id, q.error_code AS pending_error_code, q.input_tokens AS pending_input_tokens, q.output_tokens AS pending_output_tokens, q.service_tier AS pending_service_tier FROM usage_reservations r LEFT JOIN request_records q ON q.reservation_id = r.id WHERE (r.status = 'reserved' OR (r.status = 'settled' AND q.id IS NOT NULL)) AND r.created_at < $1 AND q.completed_at IS NULL AND NOT EXISTS (SELECT 1 FROM generation_jobs g WHERE g.reservation_id = r.id) AND NOT EXISTS (SELECT 1 FROM synchronous_image_idempotency s WHERE s.reservation_id = r.id AND s.status = 'pending' AND s.lease_expires_at > $2) ORDER BY r.created_at, r.id LIMIT $3",
        )
        .bind(cutoff)
        .bind(now)
        .bind(limit.clamp(1, 1_000))
        .fetch_all(&self.pool)
        .await?;
        let mut released = 0_u64;
        for row in rows {
            let request_id = row
                .try_get::<Option<String>, _>("request_id")?
                .map(parse_uuid)
                .transpose()?;
            let request_created_at = row.try_get::<Option<i64>, _>("request_created_at")?;
            let request_tenant_id = row
                .try_get::<Option<String>, _>("request_tenant_id")?
                .map(parse_uuid)
                .transpose()?;
            let reservation = UsageReservation {
                id: parse_uuid(row.try_get("id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                enforcement_mode: EnforcementMode::from_storage(
                    row.try_get::<String, _>("enforcement_mode")?.as_str(),
                )
                .ok_or(AppError::Internal)?,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: 0,
                price_tiers: Vec::new(),
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            };
            if let Some(request_id) = request_id {
                let tenant_id = request_tenant_id.ok_or(AppError::Internal)?;
                let response_object = format!("gap://{request_id}/response");
                let delivery_started = row
                    .try_get::<Option<String>, _>("pending_error_code")?
                    .as_deref()
                    == Some("delivery_started");
                let input_token_ceiling = if delivery_started {
                    row.try_get::<Option<i64>, _>("pending_input_tokens")?
                        .ok_or(AppError::Internal)?
                } else {
                    reservation.reserved_tokens
                };
                let output_token_ceiling = if delivery_started {
                    row.try_get::<Option<i64>, _>("pending_output_tokens")?
                        .ok_or(AppError::Internal)?
                } else {
                    0
                };
                let requested_service_tier = delivery_started
                    .then(|| row.try_get::<Option<String>, _>("pending_service_tier"))
                    .transpose()?
                    .flatten();
                let result = self
                    .finish_proxy_request(FinishProxyRequest {
                        request_id,
                        tenant_id,
                        reservation: &reservation,
                        input_token_ceiling,
                        output_token_ceiling,
                        requested_service_tier: requested_service_tier.as_deref(),
                        status_code: 504,
                        duration_ms: request_created_at
                            .map(|created_at| now.saturating_sub(created_at))
                            .unwrap_or_default(),
                        usage: if delivery_started {
                            TokenUsage {
                                input_tokens: input_token_ceiling,
                                output_tokens: output_token_ceiling,
                                ..TokenUsage::default()
                            }
                        } else {
                            TokenUsage::default()
                        },
                        charge_contract_ceiling: delivery_started,
                        error_code: Some("request_expired"),
                        response_object: &response_object,
                        conversation: None,
                    })
                    .await?;
                if matches!(result, FinishProxyRequestResult::Finished { .. }) {
                    released = released.saturating_add(1);
                }
            } else {
                self.settle_usage(&reservation, 0, 0).await?;
                released = released.saturating_add(1);
            }
        }
        Ok(released)
    }
}

fn validate_token_usage(usage: &TokenUsage) -> Result<(), AppError> {
    let values = [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
    ];
    if values
        .into_iter()
        .any(|tokens| !(0..=1_000_000_000).contains(&tokens))
    {
        return Err(AppError::BadRequest(
            "upstream token usage is outside the supported range".into(),
        ));
    }
    if let Some(tier) = usage.service_tier.as_deref() {
        validate_service_tier(tier)?;
    }
    Ok(())
}

pub fn normalize_proxy_usage(
    usage: &TokenUsage,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<&str>,
) -> Result<TokenUsage, AppError> {
    validate_token_usage(usage)
        .map_err(|_| AppError::Upstream("upstream returned invalid token usage".to_owned()))?;
    if input_token_ceiling < 0 || output_token_ceiling < 0 {
        return Err(AppError::Internal);
    }
    let total_input = usage
        .input_tokens
        .checked_add(usage.cached_input_tokens)
        .and_then(|tokens| tokens.checked_add(usage.cache_write_tokens))
        .ok_or_else(|| AppError::Upstream("upstream returned invalid token usage".to_owned()))?;
    if total_input > input_token_ceiling || usage.output_tokens > output_token_ceiling {
        return Err(AppError::Upstream(
            "upstream returned invalid token usage".to_owned(),
        ));
    }

    if let Some(requested) = requested_service_tier {
        validate_service_tier(requested).map_err(|_| AppError::Internal)?;
    }
    let reported = usage.service_tier.as_deref();
    let tier_matches = match requested_service_tier {
        // The Responses contract defaults an omitted tier to `auto`, and a
        // compatible upstream may report that alias even when the caller's
        // admitted and priced contract is the standard/default tier. Keep the
        // narrow alias compatible without accepting a different paid tier.
        None | Some("default") => {
            reported.is_none() || matches!(reported, Some("default" | "auto"))
        }
        Some("auto") => true,
        Some("standard_only") => {
            reported.is_none() || matches!(reported, Some("default" | "standard_only"))
        }
        Some(requested) => reported.is_none() || reported == Some(requested),
    };
    if !tier_matches {
        return Err(AppError::Upstream(
            "upstream returned an unrequested service tier".to_owned(),
        ));
    }
    let mut normalized = usage.clone();
    if normalized.service_tier.is_none()
        || reported == Some("auto") && matches!(requested_service_tier, None | Some("default"))
    {
        normalized.service_tier = requested_service_tier.map(str::to_owned);
    }
    Ok(normalized)
}

pub(crate) async fn lock_key_budget_state(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<(i64, i64), AppError> {
    let key_id = key_id.to_string();
    let row = sqlx::query(
        "UPDATE key_budget_state SET updated_at = $1 WHERE key_id = $2 RETURNING settled_lifetime_micros, reserved_micros",
    )
        .bind(now)
        .bind(&key_id)
        .fetch_optional(&mut **tx)
        .await?;
    // Key creation and the budget-rollup migration establish this row as one
    // invariant. Fail closed if it is absent. A speculative INSERT on every
    // request makes PostgreSQL wait on a concurrent updater merely to prove
    // the existing unique-index entry conflicts.
    let row = row.ok_or(AppError::NotFound)?;
    Ok((
        row.try_get("settled_lifetime_micros")?,
        row.try_get("reserved_micros")?,
    ))
}

fn retry_after_until(reset_at: i64, now: i64) -> u64 {
    u64::try_from(reset_at.saturating_sub(now).saturating_add(999) / 1_000)
        .unwrap_or(1)
        .max(1)
}

pub(crate) async fn reserve_usage_in_transaction(
    tx: &mut Transaction<'_, Any>,
    key: &AuthenticatedKey,
    price: &ModelPrice,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    now: i64,
) -> Result<UsageReservation, AppError> {
    let maximum_input_price = price
        .tiers
        .iter()
        .flat_map(|tier| {
            [
                tier.input_micros_per_million,
                tier.cached_input_micros_per_million,
                tier.cache_write_micros_per_million,
            ]
        })
        .max()
        .unwrap_or(price.input_micros_per_million)
        .max(price.input_micros_per_million);
    let maximum_output_price = price
        .tiers
        .iter()
        .map(|tier| tier.output_micros_per_million)
        .max()
        .unwrap_or(price.output_micros_per_million)
        .max(price.output_micros_per_million);
    let reserved_micros = priced_tokens(input_token_ceiling, maximum_input_price)
        .checked_add(priced_tokens(output_token_ceiling, maximum_output_price))
        .ok_or(AppError::LimitExceeded {
            reason: LimitReason::BalanceExhausted,
            retry_after_seconds: None,
        })?;
    let reserved_tokens = input_token_ceiling
        .checked_add(output_token_ceiling)
        .ok_or(AppError::LimitExceeded {
            reason: LimitReason::TpmExhausted,
            retry_after_seconds: Some(60),
        })?;
    let window_start = now / 60_000 * 60_000;
    if !key.policy.enforcement_mode.enforces_prepaid_limits() {
        let id = Uuid::now_v7();
        let price_snapshot_json = serde_json::to_string(price).map_err(|_| AppError::Internal)?;
        sqlx::query(
            "INSERT INTO usage_reservations (id, account_id, key_id, price_id, reserved_micros, reserved_tokens, rate_window_start, status, created_at, price_snapshot_json, enforcement_mode) VALUES ($1, $2, $3, $4, $5, $6, $7, 'reserved', $8, $9, $10)",
        )
        .bind(id.to_string())
        .bind(key.account_id.to_string())
        .bind(key.key_id.to_string())
        .bind(price.id.to_string())
        .bind(reserved_micros)
        .bind(reserved_tokens)
        .bind(window_start)
        .bind(now)
        .bind(price_snapshot_json)
        .bind(key.policy.enforcement_mode.as_str())
        .execute(&mut **tx)
        .await?;
        return Ok(UsageReservation {
            id,
            account_id: key.account_id,
            key_id: key.key_id,
            enforcement_mode: key.policy.enforcement_mode,
            reserved_micros,
            input_micros_per_million: price.input_micros_per_million,
            output_micros_per_million: price.output_micros_per_million,
            price_tiers: price.tiers.clone(),
            rate_window_start: window_start,
            reserved_tokens,
        });
    }
    if reserved_tokens > key.policy.tokens_per_minute as i64 {
        return Err(AppError::LimitExceeded {
            reason: LimitReason::TpmExhausted,
            retry_after_seconds: Some(retry_after_until(now / 60_000 * 60_000 + 60_000, now)),
        });
    }
    let (settled_lifetime_micros, active_reserved) =
        lock_key_budget_state(tx, key.key_id, now).await?;

    let daily_settled = if key.policy.daily_budget.is_some() {
        key_budget_daily_settled(tx, key.key_id, now).await?
    } else {
        0
    };
    let weekly_settled = if key.policy.weekly_budget.is_some() {
        key_budget_rolling_weekly_settled(tx, key.key_id, now).await?
    } else {
        0
    };
    for (configured_budget, settled, reason, retry_after_seconds) in [
        (
            key.policy.daily_budget.as_deref(),
            daily_settled,
            LimitReason::DailyBudgetExhausted,
            Some(retry_after_until((now / 86_400_000 + 1) * 86_400_000, now)),
        ),
        (
            key.policy.weekly_budget.as_deref(),
            weekly_settled,
            LimitReason::WeeklyBudgetExhausted,
            Some(1),
        ),
        (
            key.policy.lifetime_budget.as_deref(),
            settled_lifetime_micros,
            LimitReason::LifetimeBudgetExhausted,
            None,
        ),
    ] {
        let Some(configured_budget) = configured_budget else {
            continue;
        };
        let budget_micros = decimal_to_micros(
            Decimal::from_str_exact(configured_budget).map_err(|_| AppError::Internal)?,
        )?;
        if settled
            .saturating_add(active_reserved)
            .saturating_add(reserved_micros)
            > budget_micros
        {
            let retry_after_seconds = if reason == LimitReason::WeeklyBudgetExhausted {
                let cutoff = now.saturating_sub(7 * 86_400_000);
                let oldest: Option<i64> = sqlx::query(
                    "SELECT MIN(settled_at) AS oldest FROM key_budget_usage_events WHERE key_id = $1 AND settled_at >= $2",
                )
                .bind(key.key_id.to_string())
                .bind(cutoff)
                .fetch_one(&mut **tx)
                .await?
                .try_get("oldest")?;
                oldest
                    .map(|settled_at| {
                        retry_after_until(settled_at.saturating_add(7 * 86_400_000), now)
                    })
                    .or(Some(1))
            } else {
                retry_after_seconds
            };
            return Err(AppError::LimitExceeded {
                reason,
                retry_after_seconds,
            });
        }
    }

    let active_requests: i64 = sqlx::query(
        "SELECT COUNT(*) AS active FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'",
    )
    .bind(key.key_id.to_string())
    .fetch_one(&mut **tx)
    .await?
    .try_get("active")?;
    if active_requests >= i64::from(key.policy.max_concurrency) {
        return Err(AppError::LimitExceeded {
            reason: LimitReason::ConcurrencyExhausted,
            retry_after_seconds: Some(1),
        });
    }

    let rate_result = sqlx::query(
        "INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES ($1, $2, 1, $3) ON CONFLICT(key_id, window_start) DO UPDATE SET requests = rate_limit_windows.requests + 1, tokens = rate_limit_windows.tokens + $4 WHERE rate_limit_windows.requests < $5 AND rate_limit_windows.tokens + $6 <= $7",
    )
    .bind(key.key_id.to_string())
    .bind(window_start)
    .bind(reserved_tokens)
    .bind(reserved_tokens)
    .bind(i64::from(key.policy.requests_per_minute))
    .bind(reserved_tokens)
    .bind(key.policy.tokens_per_minute as i64)
    .execute(&mut **tx)
    .await?;
    if rate_result.rows_affected() == 0 {
        let window = sqlx::query(
            "SELECT requests, tokens FROM rate_limit_windows WHERE key_id = $1 AND window_start = $2",
        )
        .bind(key.key_id.to_string())
        .bind(window_start)
        .fetch_one(&mut **tx)
        .await?;
        let reason =
            if window.try_get::<i64, _>("requests")? >= i64::from(key.policy.requests_per_minute) {
                LimitReason::RpmExhausted
            } else {
                LimitReason::TpmExhausted
            };
        return Err(AppError::LimitExceeded {
            reason,
            retry_after_seconds: Some(retry_after_until(window_start + 60_000, now)),
        });
    }

    let balance_result = sqlx::query(
        "UPDATE credit_accounts SET available_micros = available_micros - $1, reserved_micros = reserved_micros + $2, updated_at = $3 WHERE id = $4 AND currency = $5 AND available_micros >= $6",
    )
    .bind(reserved_micros)
    .bind(reserved_micros)
    .bind(now)
    .bind(key.account_id.to_string())
    .bind(&key.currency)
    .bind(reserved_micros)
    .execute(&mut **tx)
    .await?;
    if balance_result.rows_affected() == 0 {
        return Err(AppError::LimitExceeded {
            reason: LimitReason::BalanceExhausted,
            retry_after_seconds: None,
        });
    }
    sqlx::query(
        "UPDATE key_budget_state SET reserved_micros = reserved_micros + $1, updated_at = $2 WHERE key_id = $3",
    )
    .bind(reserved_micros)
    .bind(now)
    .bind(key.key_id.to_string())
    .execute(&mut **tx)
    .await?;
    let id = Uuid::now_v7();
    let price_snapshot_json = serde_json::to_string(price).map_err(|_| AppError::Internal)?;
    sqlx::query(
        "INSERT INTO usage_reservations (id, account_id, key_id, price_id, reserved_micros, reserved_tokens, rate_window_start, status, created_at, price_snapshot_json, enforcement_mode) VALUES ($1, $2, $3, $4, $5, $6, $7, 'reserved', $8, $9, $10)",
    )
    .bind(id.to_string())
    .bind(key.account_id.to_string())
    .bind(key.key_id.to_string())
    .bind(price.id.to_string())
    .bind(reserved_micros)
    .bind(reserved_tokens)
    .bind(window_start)
    .bind(now)
    .bind(price_snapshot_json)
    .bind(key.policy.enforcement_mode.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(UsageReservation {
        id,
        account_id: key.account_id,
        key_id: key.key_id,
        enforcement_mode: key.policy.enforcement_mode,
        reserved_micros,
        input_micros_per_million: price.input_micros_per_million,
        output_micros_per_million: price.output_micros_per_million,
        price_tiers: price.tiers.clone(),
        rate_window_start: window_start,
        reserved_tokens,
    })
}

pub(crate) async fn settle_token_usage_in_transaction(
    tx: &mut Transaction<'_, Any>,
    reservation: &UsageReservation,
    usage: &TokenUsage,
    now: i64,
) -> Result<i64, AppError> {
    settle_token_usage_in_transaction_with_charge(tx, reservation, usage, now, None).await
}

pub(crate) async fn settle_token_usage_in_transaction_with_charge(
    tx: &mut Transaction<'_, Any>,
    reservation: &UsageReservation,
    usage: &TokenUsage,
    now: i64,
    forced_actual_micros: Option<i64>,
) -> Result<i64, AppError> {
    validate_token_usage(usage)?;
    let calculated_micros = match forced_actual_micros {
        Some(forced) if (0..=reservation.reserved_micros).contains(&forced) => forced,
        Some(_) => return Err(AppError::Internal),
        None => price_token_usage(reservation, usage)?,
    };
    if !reservation.enforcement_mode.enforces_prepaid_limits() {
        let claimed = sqlx::query(
            "UPDATE usage_reservations SET actual_micros = $1, status = 'settled', settled_at = $2 WHERE id = $3 AND key_id = $4 AND account_id = $5 AND enforcement_mode = 'metered_unlimited' AND status = 'reserved'",
        )
        .bind(calculated_micros)
        .bind(now)
        .bind(reservation.id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.account_id.to_string())
        .execute(&mut **tx)
        .await?;
        if claimed.rows_affected() == 0 {
            return sqlx::query(
                "SELECT actual_micros FROM usage_reservations WHERE id = $1 AND key_id = $2 AND account_id = $3 AND enforcement_mode = 'metered_unlimited' AND status = 'settled'",
            )
            .bind(reservation.id.to_string())
            .bind(reservation.key_id.to_string())
            .bind(reservation.account_id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("actual_micros")
            .map_err(AppError::from);
        }

        let usage_ledger_entry_id = Uuid::now_v7();
        let idempotency_key = format!("metered-usage:{}", reservation.id);
        let usage_ledger = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, idempotency_key, created_at) SELECT $1, $2, $3, 'usage', $4, currency, $5, $6, $7 FROM credit_accounts WHERE id = $8 ON CONFLICT DO NOTHING",
        )
        .bind(usage_ledger_entry_id.to_string())
        .bind(reservation.account_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(-calculated_micros)
        .bind(reservation.id.to_string())
        .bind(&idempotency_key)
        .bind(now)
        .bind(reservation.account_id.to_string())
        .execute(&mut **tx)
        .await?;
        if usage_ledger.rows_affected() != 1 {
            let replay_matches: i64 = sqlx::query(
                "SELECT COUNT(*) AS matching FROM ledger_entries WHERE idempotency_key = $1 AND account_id = $2 AND key_id = $3 AND kind = 'usage' AND amount_micros = $4 AND source = $5",
            )
            .bind(&idempotency_key)
            .bind(reservation.account_id.to_string())
            .bind(reservation.key_id.to_string())
            .bind(-calculated_micros)
            .bind(reservation.id.to_string())
            .fetch_one(&mut **tx)
            .await?
            .try_get("matching")?;
            if replay_matches != 1 {
                return Err(AppError::Conflict(
                    "metered usage ledger ownership mismatch".into(),
                ));
            }
        }
        sqlx::query(
            "INSERT INTO metered_usage_projection_outbox (reservation_id, account_id, key_id, actual_micros, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(reservation_id) DO NOTHING",
        )
        .bind(reservation.id.to_string())
        .bind(reservation.account_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(calculated_micros)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        return Ok(calculated_micros);
    }
    let (settled_lifetime_micros, reserved_micros) =
        lock_key_budget_state(tx, reservation.key_id, now).await?;
    sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
        .bind(reservation.account_id.to_string())
        .execute(&mut **tx)
        .await?;
    let settlement_context =
        sqlx::query("SELECT a.available_micros, k.policy_json FROM credit_accounts a JOIN key_records k ON k.id = $1 AND k.account_id = a.id WHERE a.id = $2")
            .bind(reservation.key_id.to_string())
            .bind(reservation.account_id.to_string())
            .fetch_one(&mut **tx)
            .await?;
    let available_micros: i64 = settlement_context.try_get("available_micros")?;
    let policy_json: String = settlement_context.try_get("policy_json")?;
    let policy: KeyPolicy = serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;
    let mut maximum_charge = available_micros
        .max(0)
        .saturating_add(reservation.reserved_micros);
    let other_active_reserved = reserved_micros
        .saturating_sub(reservation.reserved_micros)
        .max(0);
    let daily_settled = if policy.daily_budget.is_some() {
        key_budget_daily_settled(tx, reservation.key_id, now).await?
    } else {
        0
    };
    let weekly_settled = if policy.weekly_budget.is_some() {
        key_budget_rolling_weekly_settled(tx, reservation.key_id, now).await?
    } else {
        0
    };
    for (configured_budget, settled) in [
        (policy.daily_budget.as_deref(), daily_settled),
        (policy.weekly_budget.as_deref(), weekly_settled),
        (policy.lifetime_budget.as_deref(), settled_lifetime_micros),
    ] {
        let Some(configured_budget) = configured_budget else {
            continue;
        };
        let budget_micros = decimal_to_micros(
            Decimal::from_str_exact(configured_budget).map_err(|_| AppError::Internal)?,
        )?;
        maximum_charge = maximum_charge.min(
            budget_micros
                .saturating_sub(settled)
                .saturating_sub(other_active_reserved)
                .max(0),
        );
    }
    let actual_micros = calculated_micros.min(maximum_charge);
    if actual_micros != calculated_micros {
        tracing::warn!(
            reservation_id = %reservation.id,
            calculated_micros,
            charged_micros = actual_micros,
            "upstream usage exceeded the account hard balance limit"
        );
    }
    let released = reservation
        .reserved_micros
        .saturating_sub(actual_micros)
        .max(0);
    let overage = actual_micros
        .saturating_sub(reservation.reserved_micros)
        .max(0);
    let claimed = sqlx::query(
        "UPDATE usage_reservations SET actual_micros = $1, status = 'settled', settled_at = $2 WHERE id = $3 AND status = 'reserved'",
    )
    .bind(actual_micros)
    .bind(now)
    .bind(reservation.id.to_string())
    .execute(&mut **tx)
    .await?;
    if claimed.rows_affected() == 0 {
        return sqlx::query(
            "SELECT actual_micros FROM usage_reservations WHERE id = $1 AND status = 'settled'",
        )
        .bind(reservation.id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("actual_micros")
        .map_err(AppError::from);
    }
    let budget_state = sqlx::query(
        "UPDATE key_budget_state SET settled_lifetime_micros = settled_lifetime_micros + $1, reserved_micros = reserved_micros - $2, updated_at = $3 WHERE key_id = $4 AND reserved_micros >= $5",
    )
    .bind(actual_micros)
    .bind(reservation.reserved_micros)
    .bind(now)
    .bind(reservation.key_id.to_string())
    .bind(reservation.reserved_micros)
    .execute(&mut **tx)
    .await?;
    if budget_state.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    sqlx::query(
        "INSERT INTO key_budget_daily_rollups (key_id, day_bucket, settled_micros) VALUES ($1, $2, $3) ON CONFLICT(key_id, day_bucket) DO UPDATE SET settled_micros = key_budget_daily_rollups.settled_micros + excluded.settled_micros",
    )
    .bind(reservation.key_id.to_string())
    .bind(now / 86_400_000)
    .bind(actual_micros)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE credit_accounts SET available_micros = available_micros + $1 - $2, reserved_micros = reserved_micros - $3, updated_at = $4 WHERE id = $5",
    )
    .bind(released)
    .bind(overage)
    .bind(reservation.reserved_micros)
    .bind(now)
    .bind(reservation.account_id.to_string())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO account_usage_state (account_id, settled_lifetime_micros, updated_at) VALUES ($1, $2, $3) ON CONFLICT(account_id) DO UPDATE SET settled_lifetime_micros = account_usage_state.settled_lifetime_micros + excluded.settled_lifetime_micros, updated_at = excluded.updated_at",
    )
    .bind(reservation.account_id.to_string())
    .bind(actual_micros)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let actual_tokens = usage.total_tokens();
    sqlx::query(
        "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens - $1 + $2 < 0 THEN 0 ELSE tokens - $3 + $4 END WHERE key_id = $5 AND window_start = $6",
    )
    .bind(reservation.reserved_tokens)
    .bind(actual_tokens)
    .bind(reservation.reserved_tokens)
    .bind(actual_tokens)
    .bind(reservation.key_id.to_string())
    .bind(reservation.rate_window_start)
    .execute(&mut **tx)
    .await?;
    let usage_ledger_entry_id = Uuid::now_v7();
    let usage_ledger = sqlx::query(
        "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT $1, $2, $3, 'usage', $4, currency, $5, $6 FROM credit_accounts WHERE id = $7",
    )
    .bind(usage_ledger_entry_id.to_string())
    .bind(reservation.account_id.to_string())
    .bind(reservation.key_id.to_string())
    .bind(-actual_micros)
    .bind(reservation.id.to_string())
    .bind(now)
    .bind(reservation.account_id.to_string())
    .execute(&mut **tx)
    .await?;
    if usage_ledger.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    sqlx::query(
        "INSERT INTO key_budget_usage_events (usage_entry_id, reservation_id, key_id, account_id, amount_micros, settled_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(usage_ledger_entry_id.to_string())
    .bind(reservation.id.to_string())
    .bind(reservation.key_id.to_string())
    .bind(reservation.account_id.to_string())
    .bind(actual_micros)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    let mut entitlement_usage = actual_micros;
    if entitlement_usage > 0 {
        let cycles = sqlx::query(
            "SELECT c.id, c.funded_micros - c.consumed_micros AS remaining_micros FROM entitlement_cycles c JOIN subscription_entitlements e ON e.id = c.entitlement_id WHERE e.account_id = $1 AND e.status = 'active' AND e.current_cycle_id = c.id AND c.status = 'active' AND c.period_start <= $2 AND c.period_end > $3 AND c.funded_micros > c.consumed_micros ORDER BY c.period_end ASC, c.id ASC",
        )
        .bind(reservation.account_id.to_string())
        .bind(now)
        .bind(now)
        .fetch_all(&mut **tx)
        .await?;
        for cycle in cycles {
            if entitlement_usage == 0 {
                break;
            }
            let cycle_id: String = cycle.try_get("id")?;
            let remaining_micros: i64 = cycle.try_get("remaining_micros")?;
            let allocated = entitlement_usage.min(remaining_micros.max(0));
            if allocated == 0 {
                continue;
            }
            sqlx::query(
                "UPDATE entitlement_cycles SET consumed_micros = consumed_micros + $1, updated_at = $2 WHERE id = $3 AND funded_micros - consumed_micros >= $4",
            )
            .bind(allocated)
            .bind(now)
            .bind(&cycle_id)
            .bind(allocated)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO entitlement_usage_allocations (id, entitlement_cycle_id, usage_ledger_entry_id, amount_micros, created_at) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(cycle_id)
            .bind(usage_ledger_entry_id.to_string())
            .bind(allocated)
            .bind(now)
            .execute(&mut **tx)
            .await?;
            entitlement_usage = entitlement_usage.saturating_sub(allocated);
        }
    }
    Ok(actual_micros)
}

async fn key_budget_daily_settled(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    Ok(sqlx::query(
        "SELECT COALESCE((SELECT settled_micros FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket = $2), 0) AS amount",
    )
    .bind(key_id.to_string())
    .bind(now / 86_400_000)
    .fetch_one(&mut **tx)
    .await?
    .try_get("amount")?)
}

async fn key_budget_rolling_weekly_settled(
    tx: &mut Transaction<'_, Any>,
    key_id: Uuid,
    now: i64,
) -> Result<i64, AppError> {
    let cutoff = now.saturating_sub(7 * 86_400_000);
    let first_full_day = cutoff / 86_400_000 + 1;
    let first_full_day_at = first_full_day.saturating_mul(86_400_000);
    Ok(sqlx::query(
        "SELECT CAST(COALESCE((SELECT SUM(settled_micros) FROM key_budget_daily_rollups WHERE key_id = $1 AND day_bucket >= $2), 0) + COALESCE((SELECT SUM(amount_micros) FROM key_budget_usage_events WHERE key_id = $3 AND settled_at >= $4 AND settled_at < $5), 0) AS BIGINT) AS amount",
    )
    .bind(key_id.to_string())
    .bind(first_full_day)
    .bind(key_id.to_string())
    .bind(cutoff)
    .bind(first_full_day_at)
    .fetch_one(&mut **tx)
    .await?
    .try_get("amount")?)
}

pub(crate) fn proxy_contract_ceiling_micros(
    reservation: &UsageReservation,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<&str>,
) -> Result<i64, AppError> {
    if input_token_ceiling < 0
        || output_token_ceiling < 0
        || input_token_ceiling.checked_add(output_token_ceiling)
            != Some(reservation.reserved_tokens)
    {
        return Err(AppError::Internal);
    }
    if let Some(tier) = requested_service_tier {
        validate_service_tier(tier).map_err(|_| AppError::Internal)?;
    }

    let fallback = ModelPriceTier {
        service_tier: "default".to_owned(),
        input_micros_per_million: reservation.input_micros_per_million,
        cached_input_micros_per_million: reservation.input_micros_per_million,
        cache_write_micros_per_million: reservation.input_micros_per_million,
        output_micros_per_million: reservation.output_micros_per_million,
        source: "legacy-snapshot".to_owned(),
    };
    let default_tier = reservation
        .price_tiers
        .iter()
        .find(|tier| tier.service_tier == "default")
        .unwrap_or(&fallback);
    let selected: Vec<&ModelPriceTier> = match requested_service_tier {
        Some("auto") => {
            if reservation.price_tiers.is_empty() {
                vec![default_tier]
            } else {
                reservation.price_tiers.iter().collect()
            }
        }
        Some("standard_only") => {
            let tiers = reservation
                .price_tiers
                .iter()
                .filter(|tier| matches!(tier.service_tier.as_str(), "default" | "standard_only"))
                .collect::<Vec<_>>();
            if tiers.is_empty() {
                vec![default_tier]
            } else {
                tiers
            }
        }
        Some(requested) => match reservation
            .price_tiers
            .iter()
            .find(|tier| tier.service_tier == requested)
        {
            Some(tier) => vec![tier],
            None if reservation.price_tiers.is_empty() => vec![default_tier],
            None => reservation.price_tiers.iter().collect(),
        },
        None => vec![default_tier],
    };
    let maximum_input_price = selected
        .iter()
        .flat_map(|tier| {
            [
                tier.input_micros_per_million,
                tier.cached_input_micros_per_million,
                tier.cache_write_micros_per_million,
            ]
        })
        .max()
        .ok_or(AppError::Internal)?;
    let maximum_output_price = selected
        .iter()
        .map(|tier| tier.output_micros_per_million)
        .max()
        .ok_or(AppError::Internal)?;
    priced_tokens(input_token_ceiling, maximum_input_price)
        .checked_add(priced_tokens(output_token_ceiling, maximum_output_price))
        .map(|charge| charge.min(reservation.reserved_micros))
        .ok_or(AppError::Internal)
}

pub(crate) fn price_token_usage(
    reservation: &UsageReservation,
    usage: &TokenUsage,
) -> Result<i64, AppError> {
    let fallback = ModelPriceTier {
        service_tier: "default".to_owned(),
        input_micros_per_million: reservation.input_micros_per_million,
        cached_input_micros_per_million: reservation.input_micros_per_million,
        cache_write_micros_per_million: reservation.input_micros_per_million,
        output_micros_per_million: reservation.output_micros_per_million,
        source: "legacy-snapshot".to_owned(),
    };
    let requested = usage.service_tier.as_deref().unwrap_or("default");
    let exact = reservation
        .price_tiers
        .iter()
        .find(|tier| tier.service_tier == requested);
    let conservative;
    let tier = if let Some(exact) = exact {
        exact
    } else if reservation.price_tiers.is_empty() {
        &fallback
    } else {
        conservative = ModelPriceTier {
            service_tier: requested.to_owned(),
            input_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.input_micros_per_million)
                .max()
                .unwrap_or(fallback.input_micros_per_million),
            cached_input_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.cached_input_micros_per_million)
                .max()
                .unwrap_or(fallback.cached_input_micros_per_million),
            cache_write_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.cache_write_micros_per_million)
                .max()
                .unwrap_or(fallback.cache_write_micros_per_million),
            output_micros_per_million: reservation
                .price_tiers
                .iter()
                .map(|tier| tier.output_micros_per_million)
                .max()
                .unwrap_or(fallback.output_micros_per_million),
            source: "conservative-snapshot".to_owned(),
        };
        &conservative
    };
    [
        priced_tokens(usage.input_tokens, tier.input_micros_per_million),
        priced_tokens(
            usage.cached_input_tokens,
            tier.cached_input_micros_per_million,
        ),
        priced_tokens(
            usage.cache_write_tokens,
            tier.cache_write_micros_per_million,
        ),
        priced_tokens(usage.output_tokens, tier.output_micros_per_million),
    ]
    .into_iter()
    .try_fold(0_i64, i64::checked_add)
    .ok_or(AppError::Internal)
}
