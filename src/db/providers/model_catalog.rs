use super::super::*;

pub const MODEL_CATALOG_TTL_MILLIS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredUpstreamModel {
    pub model_id: String,
    pub protocol: String,
    pub context_window: Option<i64>,
    pub reservation_token_bound: Option<i64>,
    pub reservation_bound_source: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamModelView {
    pub id: String,
    pub protocol: String,
    pub context_window: Option<i64>,
    pub reservation_token_bound: Option<i64>,
    pub reservation_bound_source: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamModelCatalogView {
    pub account_id: Uuid,
    pub status: String,
    pub credential_generation: i64,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub error_code: Option<String>,
    pub models: Vec<UpstreamModelView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AggregatedUpstreamModelView {
    pub id: String,
    pub protocol: String,
    pub supported_account_count: i64,
    pub eligible_account_count: i64,
    pub complete_coverage: bool,
    pub context_window: Option<i64>,
    pub reservation_token_bound: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AggregatedUpstreamModelCatalogView {
    pub data: Vec<AggregatedUpstreamModelView>,
    pub eligible_account_count: i64,
    pub unknown_account_count: i64,
    pub stale_account_count: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceModelCatalogResult {
    Replaced,
    CredentialGenerationChanged,
    LeaseChanged,
}

impl Database {
    pub async fn upstream_account_tenant_external_id(
        &self,
        account_id: Uuid,
    ) -> Result<String, AppError> {
        sqlx::query_scalar(
            "SELECT t.external_id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)
    }

    pub async fn claim_upstream_model_catalog_sync(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        credential_generation: i64,
        lease_id: Uuid,
    ) -> Result<bool, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let account_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.tenant_id, a.credential_generation FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.tenant_id, a.credential_generation FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2"
            }
        };
        let account = sqlx::query(account_sql)
            .bind(account_id.to_string())
            .bind(tenant_external_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant_id: String = account.try_get("tenant_id")?;
        let current_generation: i64 = account.try_get("credential_generation")?;
        if current_generation != credential_generation {
            transaction.rollback().await?;
            return Ok(false);
        }
        let now = unix_millis();
        let lease = sqlx::query(
            "SELECT credential_generation, sync_lease_expires_at FROM upstream_model_catalog_state WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(lease) = lease {
            let lease_generation: i64 = lease.try_get("credential_generation")?;
            let lease_expires_at: Option<i64> = lease.try_get("sync_lease_expires_at")?;
            if lease_generation == credential_generation
                && lease_expires_at.is_some_and(|expires_at| expires_at > now)
            {
                transaction.rollback().await?;
                return Ok(false);
            }
        }
        sqlx::query(
            "INSERT INTO upstream_model_catalog_state (upstream_account_id, tenant_id, current_snapshot_id, credential_generation, status, last_attempt_at, last_success_at, expires_at, last_error_code, sync_lease_id, sync_lease_expires_at) VALUES ($1, $2, NULL, $3, 'syncing', $4, NULL, NULL, NULL, $5, $6) ON CONFLICT(upstream_account_id) DO UPDATE SET credential_generation = excluded.credential_generation, status = 'syncing', last_attempt_at = excluded.last_attempt_at, last_error_code = NULL, sync_lease_id = excluded.sync_lease_id, sync_lease_expires_at = excluded.sync_lease_expires_at",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(credential_generation)
        .bind(now)
        .bind(lease_id.to_string())
        .bind(now.saturating_add(30_000))
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn model_catalog_lease_matches(
        &self,
        transaction: &mut Transaction<'static, Any>,
        account_id: Uuid,
        lease_id: Uuid,
    ) -> Result<bool, AppError> {
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT sync_lease_id FROM upstream_model_catalog_state WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
        .flatten();
        Ok(stored.as_deref() == Some(lease_id.to_string().as_str()))
    }

    pub async fn replace_upstream_model_catalog(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        credential_generation: i64,
        lease_id: Uuid,
        source_kind: &str,
        models: &[DiscoveredUpstreamModel],
    ) -> Result<ReplaceModelCatalogResult, AppError> {
        if models.len() > 10_000
            || !matches!(source_kind, "openai_v1" | "component" | "codex_models")
        {
            return Err(AppError::BadRequest(
                "invalid model catalog snapshot".into(),
            ));
        }
        let mut transaction = self.begin_write_transaction().await?;
        let account_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.tenant_id, a.credential_generation, a.driver, a.config_json, a.updated_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.tenant_id, a.credential_generation, a.driver, a.config_json, a.updated_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2"
            }
        };
        let account = sqlx::query(account_sql)
            .bind(account_id.to_string())
            .bind(tenant_external_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant_id: String = account.try_get("tenant_id")?;
        let current_generation: i64 = account.try_get("credential_generation")?;
        let driver: String = account.try_get("driver")?;
        let config_json: String = account.try_get("config_json")?;
        let account_updated_at: i64 = account.try_get("updated_at")?;
        if current_generation != credential_generation {
            transaction.rollback().await?;
            return Ok(ReplaceModelCatalogResult::CredentialGenerationChanged);
        }
        if !self
            .model_catalog_lease_matches(&mut transaction, account_id, lease_id)
            .await?
        {
            transaction.rollback().await?;
            return Ok(ReplaceModelCatalogResult::LeaseChanged);
        }

        let now = unix_millis();
        let snapshot_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO upstream_model_catalog_snapshots (id, tenant_id, upstream_account_id, credential_generation, source_kind, fetched_at, model_count) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(snapshot_id.to_string())
        .bind(&tenant_id)
        .bind(account_id.to_string())
        .bind(credential_generation)
        .bind(source_kind)
        .bind(now)
        .bind(models.len() as i64)
        .execute(&mut *transaction)
        .await?;

        if !models.is_empty() {
            let model_values = models
                .iter()
                .map(|model| {
                    serde_json::json!({
                        "model_id": model.model_id,
                        "protocol": model.protocol,
                        "context_window": model.context_window,
                        "reservation_token_bound": model.reservation_token_bound,
                        "reservation_bound_source": model.reservation_bound_source,
                    })
                })
                .collect::<Vec<_>>();
            let models_json =
                serde_json::to_string(&model_values).map_err(|_| AppError::Internal)?;
            let insert_sql = match self.backend {
                DatabaseBackend::PostgreSql => {
                    "INSERT INTO upstream_models (snapshot_id, tenant_id, upstream_account_id, model_id, protocol, context_window, reservation_token_bound, reservation_bound_source, created_at) SELECT $2, $3, $4, model.model_id, model.protocol, model.context_window, model.reservation_token_bound, model.reservation_bound_source, $5 FROM jsonb_to_recordset(CAST($1 AS jsonb)) AS model(model_id TEXT, protocol TEXT, context_window BIGINT, reservation_token_bound BIGINT, reservation_bound_source TEXT)"
                }
                DatabaseBackend::Sqlite => {
                    "INSERT INTO upstream_models (snapshot_id, tenant_id, upstream_account_id, model_id, protocol, context_window, reservation_token_bound, reservation_bound_source, created_at) SELECT $2, $3, $4, json_extract(value, '$.model_id'), json_extract(value, '$.protocol'), json_extract(value, '$.context_window'), json_extract(value, '$.reservation_token_bound'), json_extract(value, '$.reservation_bound_source'), $5 FROM json_each($1)"
                }
            };
            sqlx::query(insert_sql)
                .bind(models_json)
                .bind(snapshot_id.to_string())
                .bind(&tenant_id)
                .bind(account_id.to_string())
                .bind(now)
                .execute(&mut *transaction)
                .await?;
        }

        if matches!(driver.as_str(), "openai-codex" | "cpa-codex-oauth") {
            let mut config: Value =
                serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
            let object = config.as_object_mut().ok_or(AppError::Internal)?;
            let limits = models
                .iter()
                .map(|model| {
                    model
                        .reservation_token_bound
                        .map(|limit| (model.model_id.clone(), Value::from(limit)))
                        .ok_or(AppError::Internal)
                })
                .collect::<Result<serde_json::Map<String, Value>, AppError>>()?;
            if limits.is_empty() {
                return Err(AppError::BadRequest(
                    "Codex model catalog did not contain a trusted model".into(),
                ));
            }
            object.remove("output_token_limits");
            object.insert("reservation_token_bounds".to_owned(), Value::Object(limits));
            let next_config = serde_json::to_string(&config).map_err(|_| AppError::Internal)?;
            sqlx::query(
                "UPDATE upstream_accounts SET config_json = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND credential_generation = $5",
            )
            .bind(next_config)
            .bind(unix_millis().max(account_updated_at.saturating_add(1)))
            .bind(account_id.to_string())
            .bind(&tenant_id)
            .bind(credential_generation)
            .execute(&mut *transaction)
            .await?;
        }

        sqlx::query(
            "INSERT INTO upstream_model_catalog_state (upstream_account_id, tenant_id, current_snapshot_id, credential_generation, status, last_attempt_at, last_success_at, expires_at, last_error_code, sync_lease_id, sync_lease_expires_at) VALUES ($1, $2, $3, $4, 'ready', $5, $5, $6, NULL, NULL, NULL) ON CONFLICT(upstream_account_id) DO UPDATE SET tenant_id = excluded.tenant_id, current_snapshot_id = excluded.current_snapshot_id, credential_generation = excluded.credential_generation, status = excluded.status, last_attempt_at = excluded.last_attempt_at, last_success_at = excluded.last_success_at, expires_at = excluded.expires_at, last_error_code = NULL, sync_lease_id = NULL, sync_lease_expires_at = NULL",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(snapshot_id.to_string())
        .bind(credential_generation)
        .bind(now)
        .bind(now.saturating_add(MODEL_CATALOG_TTL_MILLIS))
        .execute(&mut *transaction)
        .await?;

        // A catalog is a cache, not an audit log. Keep one atomic snapshot so
        // provider churn cannot grow the control database without bound.
        sqlx::query(
            "DELETE FROM upstream_model_catalog_snapshots WHERE upstream_account_id = $1 AND id <> $2",
        )
        .bind(account_id.to_string())
        .bind(snapshot_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ReplaceModelCatalogResult::Replaced)
    }

    pub async fn record_upstream_model_catalog_failure(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        credential_generation: i64,
        lease_id: Uuid,
        error_code: &'static str,
    ) -> Result<ReplaceModelCatalogResult, AppError> {
        if !matches!(
            error_code,
            "unsupported"
                | "destination_invalid"
                | "credential_invalid"
                | "connection_failed"
                | "authentication_failed"
                | "rate_limited"
                | "upstream_unavailable"
                | "redirect_rejected"
                | "response_too_large"
                | "invalid_response"
        ) {
            return Err(AppError::Internal);
        }
        let mut transaction = self.begin_write_transaction().await?;
        let account_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.tenant_id, a.credential_generation FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.tenant_id, a.credential_generation FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2"
            }
        };
        let account = sqlx::query(account_sql)
            .bind(account_id.to_string())
            .bind(tenant_external_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let tenant_id: String = account.try_get("tenant_id")?;
        let current_generation: i64 = account.try_get("credential_generation")?;
        if current_generation != credential_generation {
            transaction.rollback().await?;
            return Ok(ReplaceModelCatalogResult::CredentialGenerationChanged);
        }
        if !self
            .model_catalog_lease_matches(&mut transaction, account_id, lease_id)
            .await?
        {
            transaction.rollback().await?;
            return Ok(ReplaceModelCatalogResult::LeaseChanged);
        }
        let now = unix_millis();
        sqlx::query(
            "INSERT INTO upstream_model_catalog_state (upstream_account_id, tenant_id, current_snapshot_id, credential_generation, status, last_attempt_at, last_success_at, expires_at, last_error_code, sync_lease_id, sync_lease_expires_at) VALUES ($1, $2, NULL, $3, 'error', $4, NULL, NULL, $5, NULL, NULL) ON CONFLICT(upstream_account_id) DO UPDATE SET credential_generation = excluded.credential_generation, status = CASE WHEN upstream_model_catalog_state.current_snapshot_id IS NULL THEN 'error' ELSE 'stale' END, last_attempt_at = excluded.last_attempt_at, last_error_code = excluded.last_error_code, sync_lease_id = NULL, sync_lease_expires_at = NULL",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(credential_generation)
        .bind(now)
        .bind(error_code)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(ReplaceModelCatalogResult::Replaced)
    }

    pub async fn upstream_model_catalog(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        query: Option<&str>,
        limit: i64,
    ) -> Result<UpstreamModelCatalogView, AppError> {
        let state = sqlx::query(
            "SELECT a.credential_generation, s.status, s.last_attempt_at, s.last_success_at, s.expires_at, s.last_error_code, s.current_snapshot_id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_model_catalog_state s ON s.upstream_account_id = a.id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let snapshot_id: Option<String> = state.try_get("current_snapshot_id")?;
        let pattern = format!("%{}%", escape_like(query.unwrap_or_default().trim()));
        let rows = if let Some(snapshot_id) = snapshot_id.as_deref() {
            sqlx::query(
                "SELECT model_id, protocol, context_window, reservation_token_bound, reservation_bound_source FROM upstream_models WHERE snapshot_id = $1 AND LOWER(model_id) LIKE LOWER($2) ESCAPE '\\' ORDER BY model_id, protocol LIMIT $3",
            )
            .bind(snapshot_id)
            .bind(pattern)
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };
        let models = rows
            .into_iter()
            .map(|row| {
                Ok(UpstreamModelView {
                    id: row.try_get("model_id")?,
                    protocol: row.try_get("protocol")?,
                    context_window: row.try_get("context_window")?,
                    reservation_token_bound: row.try_get("reservation_token_bound")?,
                    reservation_bound_source: row.try_get("reservation_bound_source")?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let mut status: Option<String> = state.try_get("status")?;
        let expires_at: Option<i64> = state.try_get("expires_at")?;
        if status.as_deref() == Some("ready")
            && expires_at.is_some_and(|expiry| expiry <= unix_millis())
        {
            status = Some("stale".to_owned());
        }
        Ok(UpstreamModelCatalogView {
            account_id,
            status: status.unwrap_or_else(|| "unknown".to_owned()),
            credential_generation: state.try_get("credential_generation")?,
            last_attempt_at: state.try_get("last_attempt_at")?,
            last_success_at: state.try_get("last_success_at")?,
            expires_at,
            error_code: state.try_get("last_error_code")?,
            models,
        })
    }

    pub async fn aggregate_upstream_models(
        &self,
        tenant_external_id: &str,
        explicit_accounts: &[Uuid],
        included_provider_groups: &[Uuid],
        excluded_provider_groups: &[Uuid],
        query: Option<&str>,
        limit: i64,
    ) -> Result<AggregatedUpstreamModelCatalogView, AppError> {
        if explicit_accounts.len() > 100
            || included_provider_groups.len() > 100
            || excluded_provider_groups.len() > 100
        {
            return Err(AppError::BadRequest(
                "model catalog selection is too large".into(),
            ));
        }
        if explicit_accounts.is_empty() && included_provider_groups.is_empty() {
            return Ok(AggregatedUpstreamModelCatalogView::default());
        }
        let explicit_json = uuid_list_json(explicit_accounts)?;
        let included_json = uuid_list_json(included_provider_groups)?;
        let excluded_json = uuid_list_json(excluded_provider_groups)?;
        let candidates_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT DISTINCT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $1 AND a.status = 'active' AND (a.id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($2 AS jsonb)) AS selected(item)) OR EXISTS (SELECT 1 FROM upstream_account_provider_groups apg WHERE apg.tenant_id = a.tenant_id AND apg.upstream_account_id = a.id AND apg.provider_group_id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($3 AS jsonb)) AS selected(item)))) AND NOT EXISTS (SELECT 1 FROM upstream_account_provider_groups apg WHERE apg.tenant_id = a.tenant_id AND apg.upstream_account_id = a.id AND apg.provider_group_id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($4 AS jsonb)) AS selected(item))) ORDER BY a.id LIMIT 501"
            }
            DatabaseBackend::Sqlite => {
                "SELECT DISTINCT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE t.external_id = $1 AND a.status = 'active' AND (a.id IN (SELECT value FROM json_each($2)) OR EXISTS (SELECT 1 FROM upstream_account_provider_groups apg WHERE apg.tenant_id = a.tenant_id AND apg.upstream_account_id = a.id AND apg.provider_group_id IN (SELECT value FROM json_each($3)))) AND NOT EXISTS (SELECT 1 FROM upstream_account_provider_groups apg WHERE apg.tenant_id = a.tenant_id AND apg.upstream_account_id = a.id AND apg.provider_group_id IN (SELECT value FROM json_each($4))) ORDER BY a.id LIMIT 501"
            }
        };
        let account_ids = sqlx::query(candidates_sql)
            .bind(tenant_external_id)
            .bind(explicit_json)
            .bind(included_json)
            .bind(excluded_json)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| row.try_get::<String, _>("id"))
            .collect::<Result<Vec<_>, _>>()?;
        if account_ids.len() > 500 {
            return Err(AppError::BadRequest(
                "model catalog candidate set is too large".into(),
            ));
        }
        if account_ids.is_empty() {
            return Ok(AggregatedUpstreamModelCatalogView::default());
        }

        let account_ids_json =
            serde_json::to_string(&account_ids).map_err(|_| AppError::Internal)?;
        let states_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.id, a.credential_generation, s.status, s.current_snapshot_id, s.expires_at, snapshot.credential_generation AS snapshot_generation FROM upstream_accounts a LEFT JOIN upstream_model_catalog_state s ON s.upstream_account_id = a.id LEFT JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = s.current_snapshot_id AND snapshot.upstream_account_id = a.id WHERE a.id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($1 AS jsonb)) AS selected(item))"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.id, a.credential_generation, s.status, s.current_snapshot_id, s.expires_at, snapshot.credential_generation AS snapshot_generation FROM upstream_accounts a LEFT JOIN upstream_model_catalog_state s ON s.upstream_account_id = a.id LEFT JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = s.current_snapshot_id AND snapshot.upstream_account_id = a.id WHERE a.id IN (SELECT value FROM json_each($1))"
            }
        };
        let now = unix_millis();
        let mut unknown_account_count = 0_i64;
        let mut stale_account_count = 0_i64;
        for row in sqlx::query(states_sql)
            .bind(&account_ids_json)
            .fetch_all(&self.pool)
            .await?
        {
            let status: Option<String> = row.try_get("status")?;
            let snapshot_id: Option<String> = row.try_get("current_snapshot_id")?;
            let account_generation: i64 = row.try_get("credential_generation")?;
            let snapshot_generation: Option<i64> = row.try_get("snapshot_generation")?;
            let expires_at: Option<i64> = row.try_get("expires_at")?;
            if snapshot_id.is_none() || snapshot_generation != Some(account_generation) {
                unknown_account_count += 1;
            } else if status.as_deref() != Some("ready")
                || expires_at.is_some_and(|expiry| expiry <= now)
            {
                stale_account_count += 1;
            }
        }

        let pattern = format!("%{}%", escape_like(query.unwrap_or_default().trim()));
        let aggregate_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT m.model_id, m.protocol, COUNT(DISTINCT m.upstream_account_id) AS supported_count, MIN(m.context_window) AS minimum_context_window, COUNT(m.context_window) AS context_count, MIN(m.reservation_token_bound) AS minimum_reservation_bound, COUNT(m.reservation_token_bound) AS bound_count FROM upstream_models m JOIN upstream_model_catalog_state s ON s.current_snapshot_id = m.snapshot_id AND s.upstream_account_id = m.upstream_account_id JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = m.snapshot_id AND snapshot.upstream_account_id = m.upstream_account_id JOIN upstream_accounts account ON account.id = m.upstream_account_id AND account.credential_generation = s.credential_generation AND account.credential_generation = snapshot.credential_generation WHERE m.upstream_account_id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($1 AS jsonb)) AS selected(item)) AND LOWER(m.model_id) LIKE LOWER($2) ESCAPE '\\' GROUP BY m.model_id, m.protocol ORDER BY m.model_id, m.protocol LIMIT $3"
            }
            DatabaseBackend::Sqlite => {
                "SELECT m.model_id, m.protocol, COUNT(DISTINCT m.upstream_account_id) AS supported_count, MIN(m.context_window) AS minimum_context_window, COUNT(m.context_window) AS context_count, MIN(m.reservation_token_bound) AS minimum_reservation_bound, COUNT(m.reservation_token_bound) AS bound_count FROM upstream_models m JOIN upstream_model_catalog_state s ON s.current_snapshot_id = m.snapshot_id AND s.upstream_account_id = m.upstream_account_id JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = m.snapshot_id AND snapshot.upstream_account_id = m.upstream_account_id JOIN upstream_accounts account ON account.id = m.upstream_account_id AND account.credential_generation = s.credential_generation AND account.credential_generation = snapshot.credential_generation WHERE m.upstream_account_id IN (SELECT value FROM json_each($1)) AND LOWER(m.model_id) LIKE LOWER($2) ESCAPE '\\' GROUP BY m.model_id, m.protocol ORDER BY m.model_id, m.protocol LIMIT $3"
            }
        };
        let eligible_count = account_ids.len() as i64;
        let data = sqlx::query(aggregate_sql)
            .bind(account_ids_json)
            .bind(pattern)
            .bind(limit.clamp(1, 200))
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                let supported_count: i64 = row.try_get("supported_count")?;
                let context_count: i64 = row.try_get("context_count")?;
                let bound_count: i64 = row.try_get("bound_count")?;
                Ok(AggregatedUpstreamModelView {
                    id: row.try_get("model_id")?,
                    protocol: row.try_get("protocol")?,
                    supported_account_count: supported_count,
                    eligible_account_count: eligible_count,
                    complete_coverage: supported_count == eligible_count,
                    context_window: if context_count == supported_count {
                        row.try_get("minimum_context_window")?
                    } else {
                        None
                    },
                    reservation_token_bound: if bound_count == supported_count {
                        row.try_get("minimum_reservation_bound")?
                    } else {
                        None
                    },
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(AggregatedUpstreamModelCatalogView {
            data,
            eligible_account_count: eligible_count,
            unknown_account_count,
            stale_account_count,
        })
    }

    pub async fn validate_managed_codex_route_catalog(
        &self,
        tenant_external_id: &str,
        account_ids: &[Uuid],
        upstream_model: &str,
    ) -> Result<(), AppError> {
        for account_id in account_ids {
            let row = sqlx::query(
                "SELECT a.driver, EXISTS (SELECT 1 FROM upstream_model_catalog_state s JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = s.current_snapshot_id AND snapshot.upstream_account_id = s.upstream_account_id AND snapshot.credential_generation = a.credential_generation JOIN upstream_models m ON m.snapshot_id = snapshot.id WHERE s.upstream_account_id = a.id AND s.credential_generation = a.credential_generation AND m.model_id = $1) AS catalogued FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $2 AND t.external_id = $3",
            )
            .bind(upstream_model)
            .bind(account_id.to_string())
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
            let driver: String = row.try_get("driver")?;
            let catalogued: bool = row.try_get("catalogued")?;
            if matches!(driver.as_str(), "openai-codex" | "cpa-codex-oauth") && !catalogued {
                return Err(AppError::BadRequest(
                    "managed Codex routes require a synchronized model catalog entry".into(),
                ));
            }
        }
        Ok(())
    }

    pub async fn filter_accounts_supporting_upstream_model(
        &self,
        tenant_external_id: &str,
        account_ids: &[Uuid],
        upstream_model: &str,
    ) -> Result<Vec<Uuid>, AppError> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }
        if account_ids.len() > 500 {
            return Err(AppError::BadRequest(
                "route candidate set is too large".into(),
            ));
        }
        let account_ids_json = uuid_list_json(account_ids)?;
        let filter_sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT DISTINCT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_model_catalog_state s ON s.upstream_account_id = a.id AND s.credential_generation = a.credential_generation JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = s.current_snapshot_id AND snapshot.upstream_account_id = a.id AND snapshot.credential_generation = a.credential_generation JOIN upstream_models m ON m.snapshot_id = snapshot.id AND m.upstream_account_id = a.id WHERE t.external_id = $1 AND m.model_id = $2 AND a.id IN (SELECT selected.item FROM jsonb_array_elements_text(CAST($3 AS jsonb)) AS selected(item)) ORDER BY a.id"
            }
            DatabaseBackend::Sqlite => {
                "SELECT DISTINCT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_model_catalog_state s ON s.upstream_account_id = a.id AND s.credential_generation = a.credential_generation JOIN upstream_model_catalog_snapshots snapshot ON snapshot.id = s.current_snapshot_id AND snapshot.upstream_account_id = a.id AND snapshot.credential_generation = a.credential_generation JOIN upstream_models m ON m.snapshot_id = snapshot.id AND m.upstream_account_id = a.id WHERE t.external_id = $1 AND m.model_id = $2 AND a.id IN (SELECT value FROM json_each($3)) ORDER BY a.id"
            }
        };
        sqlx::query(filter_sql)
            .bind(tenant_external_id)
            .bind(upstream_model)
            .bind(account_ids_json)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| parse_uuid(row.try_get::<String, _>("id")?))
            .collect()
    }
}

fn uuid_list_json(values: &[Uuid]) -> Result<String, AppError> {
    serde_json::to_string(&values.iter().map(Uuid::to_string).collect::<Vec<String>>())
        .map_err(|_| AppError::Internal)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
