use super::super::*;

impl Database {
    const MAX_LISTED_MODEL_PRICES: i64 = 1_000;

    pub async fn list_generation_prices(
        &self,
        currency: &str,
    ) -> Result<Vec<GenerationPrice>, AppError> {
        validate_currency(currency)?;
        let rows = sqlx::query(
            "SELECT id, model, currency, billing_unit, micros_per_unit FROM generation_prices WHERE currency = $1 ORDER BY model",
        )
        .bind(currency.to_uppercase())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_price_view).collect()
    }

    pub async fn upsert_model_price(
        &self,
        model: &str,
        currency: &str,
        input_per_million: Decimal,
        output_per_million: Decimal,
    ) -> Result<ModelPrice, AppError> {
        self.upsert_model_price_tier(
            model,
            currency,
            "default",
            input_per_million,
            input_per_million,
            input_per_million,
            output_per_million,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_model_price_tier(
        &self,
        model: &str,
        currency: &str,
        service_tier: &str,
        input_per_million: Decimal,
        cached_input_per_million: Decimal,
        cache_write_per_million: Decimal,
        output_per_million: Decimal,
        cache_price_estimated: bool,
    ) -> Result<ModelPrice, AppError> {
        validate_currency(currency)?;
        validate_service_tier(service_tier)?;
        let input_micros = decimal_to_micros(input_per_million)?;
        let cached_input_micros = decimal_to_micros(cached_input_per_million)?;
        let cache_write_micros = decimal_to_micros(cache_write_per_million)?;
        let output_micros = decimal_to_micros(output_per_million)?;
        if [
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
        ]
        .into_iter()
        .any(|price| price < 0)
        {
            return Err(AppError::BadRequest(
                "model prices cannot be negative".into(),
            ));
        }
        let currency = currency.to_uppercase();
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        if service_tier == "default" {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, 'manual', $6) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        let tier_updated = upsert_price_tier(
            &mut tx,
            model,
            &currency,
            service_tier,
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
            "manual",
            now,
            cache_price_estimated,
        )
        .await?;
        if !tier_updated {
            return Err(AppError::BadRequest(
                "create the default service tier before an additional tier".into(),
            ));
        }
        tx.commit().await?;
        self.model_price(model, &currency).await
    }

    pub async fn upsert_synced_model_price(
        &self,
        model: &str,
        currency: &str,
        input_per_million: Decimal,
        output_per_million: Decimal,
        source: &str,
    ) -> Result<ModelPriceView, AppError> {
        self.upsert_synced_model_price_tier(
            model,
            currency,
            "default",
            input_per_million,
            input_per_million,
            input_per_million,
            output_per_million,
            source,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_synced_model_price_tier(
        &self,
        model: &str,
        currency: &str,
        service_tier: &str,
        input_per_million: Decimal,
        cached_input_per_million: Decimal,
        cache_write_per_million: Decimal,
        output_per_million: Decimal,
        source: &str,
        cache_price_estimated: bool,
    ) -> Result<ModelPriceView, AppError> {
        validate_currency(currency)?;
        validate_service_tier(service_tier)?;
        if !matches!(source, "models.dev" | "litellm" | "openrouter") {
            return Err(AppError::BadRequest("unsupported price source".into()));
        }
        let input_micros = decimal_to_micros(input_per_million)?;
        let cached_input_micros = decimal_to_micros(cached_input_per_million)?;
        let cache_write_micros = decimal_to_micros(cache_write_per_million)?;
        let output_micros = decimal_to_micros(output_per_million)?;
        if [
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
        ]
        .into_iter()
        .any(|price| price < 0)
        {
            return Err(AppError::BadRequest(
                "model prices cannot be negative".into(),
            ));
        }
        let currency = currency.to_uppercase();
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        if service_tier == "default" {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(source)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        } else if sqlx::query("SELECT id FROM model_prices WHERE model = $1 AND currency = $2")
            .bind(model)
            .bind(&currency)
            .fetch_optional(&mut *tx)
            .await?
            .is_none()
        {
            sqlx::query(
                "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(model)
            .bind(&currency)
            .bind(input_micros)
            .bind(output_micros)
            .bind(source)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            upsert_price_tier(
                &mut tx,
                model,
                &currency,
                "default",
                input_micros,
                cached_input_micros,
                cache_write_micros,
                output_micros,
                source,
                now,
                true,
            )
            .await?;
        }
        upsert_price_tier(
            &mut tx,
            model,
            &currency,
            service_tier,
            input_micros,
            cached_input_micros,
            cache_write_micros,
            output_micros,
            source,
            now,
            cache_price_estimated,
        )
        .await?;
        tx.commit().await?;
        self.model_price_view(model, &currency).await
    }

    pub async fn list_model_prices(&self, currency: &str) -> Result<Vec<ModelPriceView>, AppError> {
        self.list_model_prices_page(currency, Self::MAX_LISTED_MODEL_PRICES as usize, 0)
            .await
    }

    pub async fn list_model_prices_page(
        &self,
        currency: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ModelPriceView>, AppError> {
        validate_currency(currency)?;
        if limit == 0 || limit > Self::MAX_LISTED_MODEL_PRICES as usize {
            return Err(AppError::BadRequest(format!(
                "model price page limit must be between 1 and {}",
                Self::MAX_LISTED_MODEL_PRICES
            )));
        }
        let limit = i64::try_from(limit).map_err(|_| AppError::Internal)?;
        let offset = i64::try_from(offset)
            .map_err(|_| AppError::BadRequest("model price offset is too large".into()))?;
        let rows = sqlx::query(
            "WITH limited_prices AS (SELECT model, currency, input_micros_per_million, output_micros_per_million, source, updated_at FROM model_prices WHERE currency = $1 ORDER BY model ASC LIMIT $2 OFFSET $3) SELECT p.model, p.currency, p.input_micros_per_million, p.output_micros_per_million, p.source, p.updated_at, t.service_tier AS tier_service_tier, t.input_micros_per_million AS tier_input_micros_per_million, t.cached_input_micros_per_million AS tier_cached_input_micros_per_million, t.cache_write_micros_per_million AS tier_cache_write_micros_per_million, t.output_micros_per_million AS tier_output_micros_per_million, t.source AS tier_source, t.updated_at AS tier_updated_at, t.cache_price_estimated AS tier_cache_price_estimated FROM limited_prices p LEFT JOIN model_price_tiers t ON t.model = p.model AND t.currency = p.currency ORDER BY p.model ASC, CASE WHEN t.service_tier = 'default' THEN 0 ELSE 1 END, t.service_tier",
        )
        .bind(currency.to_uppercase())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        model_price_views_from_joined_rows(rows)
    }

    pub async fn model_price_views_for_models(
        &self,
        currency: &str,
        models: &[String],
    ) -> Result<Vec<ModelPriceView>, AppError> {
        use std::fmt::Write as _;

        validate_currency(currency)?;
        if models.len() > crate::pricing::MAX_SYNC_MODELS {
            return Err(AppError::BadRequest(format!(
                "at most {} model prices can be loaded at once",
                crate::pricing::MAX_SYNC_MODELS
            )));
        }
        if models.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = "SELECT p.model, p.currency, p.input_micros_per_million, p.output_micros_per_million, p.source, p.updated_at, t.service_tier AS tier_service_tier, t.input_micros_per_million AS tier_input_micros_per_million, t.cached_input_micros_per_million AS tier_cached_input_micros_per_million, t.cache_write_micros_per_million AS tier_cache_write_micros_per_million, t.output_micros_per_million AS tier_output_micros_per_million, t.source AS tier_source, t.updated_at AS tier_updated_at, t.cache_price_estimated AS tier_cache_price_estimated FROM model_prices p LEFT JOIN model_price_tiers t ON t.model = p.model AND t.currency = p.currency WHERE p.currency = $1 AND p.model IN (".to_owned();
        for index in 0..models.len() {
            if index > 0 {
                statement.push_str(", ");
            }
            write!(statement, "${}", index + 2).map_err(|_| AppError::Internal)?;
        }
        statement.push_str(") ORDER BY p.model ASC, CASE WHEN t.service_tier = 'default' THEN 0 ELSE 1 END, t.service_tier");
        // SQL safety boundary: only monotonically generated `$N` placeholders are appended to
        // the literal statement. Currency and every model value remain binds. QueryBuilder<Any>
        // cannot be used here because PostgreSQL and SQLite use different native placeholders.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(currency.to_uppercase());
        for model in models {
            query = query.bind(model);
        }
        let rows = query.fetch_all(&self.pool).await?;
        model_price_views_from_joined_rows(rows)
    }

    pub async fn model_price_view(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<ModelPriceView, AppError> {
        let row = sqlx::query(
            "SELECT model, currency, input_micros_per_million, output_micros_per_million, source, updated_at FROM model_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        self.model_price_view_from_base_row(row).await
    }

    pub async fn pricing_models(
        &self,
        tenant_external_id: Option<&str>,
    ) -> Result<Vec<String>, AppError> {
        let rows = if let Some(tenant) = tenant_external_id {
            sqlx::query(
                "SELECT model FROM (SELECT model FROM model_prices UNION SELECT a.model FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 UNION SELECT g.public_model AS model FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $2 UNION SELECT r.public_model AS model FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $3) discovered_models ORDER BY model ASC LIMIT $4",
            )
            .bind(tenant)
            .bind(tenant)
            .bind(tenant)
            .bind((crate::pricing::MAX_SYNC_MODELS + 1) as i64)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT model FROM (SELECT model FROM model_prices UNION SELECT model FROM usage_daily_aggregates UNION SELECT public_model AS model FROM generation_jobs UNION SELECT public_model AS model FROM model_routes) discovered_models ORDER BY model ASC LIMIT $1",
            )
            .bind((crate::pricing::MAX_SYNC_MODELS + 1) as i64)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| row.try_get("model").map_err(AppError::from))
            .collect()
    }

    pub async fn model_price(&self, model: &str, currency: &str) -> Result<ModelPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, input_micros_per_million, output_micros_per_million FROM model_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        let tiers = self.model_price_tiers(model, currency).await?;
        Ok(ModelPrice {
            id: parse_uuid(row.try_get("id")?)?,
            input_micros_per_million: row.try_get("input_micros_per_million")?,
            output_micros_per_million: row.try_get("output_micros_per_million")?,
            tiers,
        })
    }

    async fn model_price_tiers(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<Vec<ModelPriceTier>, AppError> {
        let rows = sqlx::query(
            "SELECT service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source FROM model_price_tiers WHERE model = $1 AND currency = $2 ORDER BY CASE WHEN service_tier = 'default' THEN 0 ELSE 1 END, service_tier",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ModelPriceTier {
                    service_tier: row.try_get("service_tier")?,
                    input_micros_per_million: row.try_get("input_micros_per_million")?,
                    cached_input_micros_per_million: row
                        .try_get("cached_input_micros_per_million")?,
                    cache_write_micros_per_million: row
                        .try_get("cache_write_micros_per_million")?,
                    output_micros_per_million: row.try_get("output_micros_per_million")?,
                    source: row.try_get("source")?,
                })
            })
            .collect()
    }

    async fn model_price_view_from_base_row(
        &self,
        row: AnyRow,
    ) -> Result<ModelPriceView, AppError> {
        let model: String = row.try_get("model")?;
        let currency: String = row.try_get("currency")?;
        let tier_rows = sqlx::query(
            "SELECT service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source, updated_at, cache_price_estimated FROM model_price_tiers WHERE model = $1 AND currency = $2 ORDER BY CASE WHEN service_tier = 'default' THEN 0 ELSE 1 END, service_tier",
        )
        .bind(&model)
        .bind(&currency)
        .fetch_all(&self.pool)
        .await?;
        let tiers = tier_rows
            .into_iter()
            .map(|tier| {
                Ok(ModelPriceTierView {
                    service_tier: tier.try_get("service_tier")?,
                    input_per_million: micros_to_decimal_string(
                        tier.try_get("input_micros_per_million")?,
                    ),
                    cached_input_per_million: micros_to_decimal_string(
                        tier.try_get("cached_input_micros_per_million")?,
                    ),
                    cache_write_per_million: micros_to_decimal_string(
                        tier.try_get("cache_write_micros_per_million")?,
                    ),
                    output_per_million: micros_to_decimal_string(
                        tier.try_get("output_micros_per_million")?,
                    ),
                    source: tier.try_get("source")?,
                    updated_at: tier.try_get("updated_at")?,
                    cache_price_estimated: tier.try_get::<i64, _>("cache_price_estimated")? != 0,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(ModelPriceView {
            model,
            currency,
            input_per_million: micros_to_decimal_string(row.try_get("input_micros_per_million")?),
            output_per_million: micros_to_decimal_string(row.try_get("output_micros_per_million")?),
            source: row.try_get("source")?,
            updated_at: row.try_get("updated_at")?,
            tiers,
        })
    }

    pub async fn upsert_generation_price(
        &self,
        model: &str,
        currency: &str,
        billing_unit: &str,
        price_per_unit: Decimal,
    ) -> Result<GenerationPrice, AppError> {
        validate_currency(currency)?;
        if !matches!(billing_unit, "job" | "second" | "image" | "megapixel") {
            return Err(AppError::BadRequest(
                "billing_unit must be job, second, image, or megapixel".into(),
            ));
        }
        let micros_per_unit = decimal_to_micros(price_per_unit)?;
        if micros_per_unit < 0 {
            return Err(AppError::BadRequest(
                "generation price cannot be negative".into(),
            ));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO generation_prices (id, model, currency, billing_unit, micros_per_unit, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(model, currency) DO UPDATE SET billing_unit = excluded.billing_unit, micros_per_unit = excluded.micros_per_unit, updated_at = excluded.updated_at",
        )
        .bind(id.to_string())
        .bind(model)
        .bind(currency.to_uppercase())
        .bind(billing_unit)
        .bind(micros_per_unit)
        .bind(unix_millis())
        .execute(&self.pool)
        .await?;
        self.generation_price(model, currency).await
    }

    pub async fn generation_price(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<GenerationPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, model, currency, billing_unit, micros_per_unit FROM generation_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
        Ok(GenerationPrice {
            id: parse_uuid(row.try_get("id")?)?,
            model: row.try_get("model")?,
            currency: row.try_get("currency")?,
            billing_unit: row.try_get("billing_unit")?,
            price_per_unit: micros_to_decimal_string(micros_per_unit),
            micros_per_unit,
        })
    }
}

fn model_price_views_from_joined_rows(rows: Vec<AnyRow>) -> Result<Vec<ModelPriceView>, AppError> {
    let mut prices = Vec::<ModelPriceView>::new();
    for row in rows {
        let model: String = row.try_get("model")?;
        if prices.last().is_none_or(|price| price.model != model) {
            prices.push(ModelPriceView {
                model,
                currency: row.try_get("currency")?,
                input_per_million: micros_to_decimal_string(
                    row.try_get("input_micros_per_million")?,
                ),
                output_per_million: micros_to_decimal_string(
                    row.try_get("output_micros_per_million")?,
                ),
                source: row.try_get("source")?,
                updated_at: row.try_get("updated_at")?,
                tiers: Vec::new(),
            });
        }
        let Some(service_tier) = row.try_get::<Option<String>, _>("tier_service_tier")? else {
            continue;
        };
        prices
            .last_mut()
            .ok_or(AppError::Internal)?
            .tiers
            .push(ModelPriceTierView {
                service_tier,
                input_per_million: micros_to_decimal_string(
                    row.try_get("tier_input_micros_per_million")?,
                ),
                cached_input_per_million: micros_to_decimal_string(
                    row.try_get("tier_cached_input_micros_per_million")?,
                ),
                cache_write_per_million: micros_to_decimal_string(
                    row.try_get("tier_cache_write_micros_per_million")?,
                ),
                output_per_million: micros_to_decimal_string(
                    row.try_get("tier_output_micros_per_million")?,
                ),
                source: row.try_get("tier_source")?,
                updated_at: row.try_get("tier_updated_at")?,
                cache_price_estimated: row.try_get::<i64, _>("tier_cache_price_estimated")? != 0,
            });
    }
    Ok(prices)
}

fn generation_price_view(row: AnyRow) -> Result<GenerationPrice, AppError> {
    let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
    Ok(GenerationPrice {
        id: parse_uuid(row.try_get("id")?)?,
        model: row.try_get("model")?,
        currency: row.try_get("currency")?,
        billing_unit: row.try_get("billing_unit")?,
        price_per_unit: micros_to_decimal_string(micros_per_unit),
        micros_per_unit,
    })
}

#[allow(clippy::too_many_arguments)]
async fn upsert_price_tier(
    tx: &mut Transaction<'_, Any>,
    model: &str,
    currency: &str,
    service_tier: &str,
    input_micros: i64,
    cached_input_micros: i64,
    cache_write_micros: i64,
    output_micros: i64,
    source: &str,
    now: i64,
    cache_price_estimated: bool,
) -> Result<bool, AppError> {
    // Keep the base-price existence check and tier write in one write statement. In SQLite, a
    // deferred transaction that first reads and then upgrades to a writer can fail immediately
    // with SQLITE_BUSY when a background worker owns the writer slot, bypassing busy_timeout.
    let result = sqlx::query(
        "INSERT INTO model_price_tiers (id, model, currency, service_tier, input_micros_per_million, cached_input_micros_per_million, cache_write_micros_per_million, output_micros_per_million, source, updated_at, cache_price_estimated) SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11 FROM model_prices WHERE model = $2 AND currency = $3 ON CONFLICT(model, currency, service_tier) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, cached_input_micros_per_million = excluded.cached_input_micros_per_million, cache_write_micros_per_million = excluded.cache_write_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, source = excluded.source, updated_at = excluded.updated_at, cache_price_estimated = excluded.cache_price_estimated",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(model)
    .bind(currency)
    .bind(service_tier)
    .bind(input_micros)
    .bind(cached_input_micros)
    .bind(cache_write_micros)
    .bind(output_micros)
    .bind(source)
    .bind(now)
    .bind(i64::from(cache_price_estimated))
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}
