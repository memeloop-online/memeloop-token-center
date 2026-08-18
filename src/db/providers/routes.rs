use super::super::*;

pub struct CreateModelRouteInput {
    pub tenant_external_id: String,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
}

#[derive(Clone, Debug)]
pub struct UpdateModelRouteInput {
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub expected_updated_at: i64,
}

impl Database {
    pub async fn list_model_routes(
        &self,
        tenant_external_id: Option<&str>,
    ) -> Result<Vec<ModelRouteView>, AppError> {
        let rows = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE ($1 = '' OR t.external_id = $1) ORDER BY r.public_model, r.priority, r.id",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(model_route_view).collect()
    }
    pub async fn create_model_route(
        &self,
        input: CreateModelRouteInput,
    ) -> Result<ModelRouteView, AppError> {
        validate_model_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let now = unix_millis();
        let route_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        let account_tenant: String = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = $1 AND status = 'active'",
        )
        .bind(input.upstream_account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("tenant_id")?;
        if account_tenant != tenant_id {
            return Err(AppError::Forbidden);
        }
        let inserted = sqlx::query(
            "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9) ON CONFLICT(tenant_id, public_model, protocol, priority) DO NOTHING",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .bind(input.public_model.trim())
        .bind(input.upstream_account_id.to_string())
        .bind(input.upstream_model.trim())
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.priority = $4",
            )
            .bind(&tenant_id)
            .bind(input.public_model.trim())
            .bind(&input.protocol)
            .bind(input.priority)
            .fetch_one(&mut *tx)
            .await?;
            let existing = model_route_view(existing)?;
            if existing.upstream_account_id == input.upstream_account_id
                && existing.upstream_model == input.upstream_model.trim()
            {
                tx.commit().await?;
                return Ok(existing);
            }
            return Err(AppError::Conflict(
                "another route already uses this public model, protocol, and priority".into(),
            ));
        }
        tx.commit().await?;
        Ok(ModelRouteView {
            id: route_id,
            tenant_id: parse_uuid(tenant_id)?,
            tenant_external_id: Some(input.tenant_external_id),
            public_model: input.public_model.trim().to_owned(),
            upstream_account_id: input.upstream_account_id,
            upstream_model: input.upstream_model.trim().to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: true,
            created_at: now,
            updated_at: now,
        })
    }
    pub async fn update_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        input: UpdateModelRouteInput,
    ) -> Result<ModelRouteView, AppError> {
        validate_model_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let public_model = input.public_model.trim();
        let upstream_model = input.upstream_model.trim();
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_view = model_route_view(current)?;
        let unchanged = current_view.public_model == public_model
            && current_view.upstream_account_id == input.upstream_account_id
            && current_view.upstream_model == upstream_model
            && current_view.protocol == input.protocol
            && current_view.priority == input.priority;
        if unchanged {
            tx.commit().await?;
            return Ok(current_view);
        }
        if current_view.updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        let account_tenant = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = $1 AND status = 'active'",
        )
        .bind(input.upstream_account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get::<String, _>("tenant_id")?;
        if account_tenant != current_view.tenant_id.to_string() {
            return Err(AppError::Forbidden);
        }
        let duplicate = sqlx::query(
            "SELECT id FROM model_routes WHERE tenant_id = $1 AND public_model = $2 AND protocol = $3 AND priority = $4 AND id <> $5",
        )
        .bind(current_view.tenant_id.to_string())
        .bind(public_model)
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(route_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if duplicate {
            return Err(AppError::Conflict(
                "another route already uses this public model, protocol, and priority".into(),
            ));
        }
        let updated_at = unix_millis().max(current_view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE model_routes SET public_model = $1, upstream_account_id = $2, upstream_model = $3, protocol = $4, priority = $5, updated_at = $6 WHERE id = $7 AND tenant_id = $8 AND updated_at = $9",
        )
        .bind(public_model)
        .bind(input.upstream_account_id.to_string())
        .bind(upstream_model)
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(updated_at)
        .bind(route_id.to_string())
        .bind(current_view.tenant_id.to_string())
        .bind(input.expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        tx.commit().await?;
        Ok(ModelRouteView {
            id: route_id,
            tenant_id: current_view.tenant_id,
            tenant_external_id: current_view.tenant_external_id,
            public_model: public_model.to_owned(),
            upstream_account_id: input.upstream_account_id,
            upstream_model: upstream_model.to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: current_view.enabled,
            created_at: current_view.created_at,
            updated_at,
        })
    }
    pub async fn set_model_route_enabled(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        enabled: bool,
        expected_updated_at: i64,
    ) -> Result<ModelRouteView, AppError> {
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT r.id, r.tenant_id, t.external_id AS tenant_external_id, r.public_model, r.upstream_account_id, r.upstream_model, r.protocol, r.priority, r.enabled, r.created_at, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let mut route = model_route_view(current)?;
        if route.enabled == enabled {
            tx.commit().await?;
            return Ok(route);
        }
        if route.updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before changing its status".into(),
            ));
        }
        let updated_at = unix_millis().max(route.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE model_routes SET enabled = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND updated_at = $5",
        )
        .bind(i64::from(enabled))
        .bind(updated_at)
        .bind(route_id.to_string())
        .bind(route.tenant_id.to_string())
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before changing its status".into(),
            ));
        }
        tx.commit().await?;
        route.enabled = enabled;
        route.updated_at = updated_at;
        Ok(route)
    }
    pub async fn delete_model_route(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let route = sqlx::query(
            "SELECT r.tenant_id, r.enabled, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(route) = route else {
            tx.commit().await?;
            return Ok(());
        };
        let enabled = route.try_get::<i64, _>("enabled")? != 0;
        let updated_at: i64 = route.try_get("updated_at")?;
        if enabled {
            return Err(AppError::Conflict(
                "disable the model route before deleting it".into(),
            ));
        }
        if updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before deleting it".into(),
            ));
        }
        let referenced =
            sqlx::query("SELECT id FROM request_records WHERE model_route_id = $1 LIMIT 1")
                .bind(route_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if referenced {
            return Err(AppError::Conflict(
                "the route has request history and must be retained in a disabled state".into(),
            ));
        }
        let changed = sqlx::query(
            "DELETE FROM model_routes WHERE id = $1 AND tenant_id = $2 AND enabled = 0 AND updated_at = $3",
        )
        .bind(route_id.to_string())
        .bind(route.try_get::<String, _>("tenant_id")?)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before deleting it".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        self.resolve_upstream_with_hint(tenant_id, public_model, protocol, None, key_material)
            .await
    }
    pub async fn resolve_upstream_with_hint(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        upstream_account_id: Option<Uuid>,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let sql = if upstream_account_id.is_some() {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND a.id = $4 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        } else {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        };
        let query = sqlx::query(sql)
            .bind(tenant_id.to_string())
            .bind(public_model)
            .bind(protocol);
        let query = if let Some(account_id) = upstream_account_id {
            query.bind(account_id.to_string())
        } else {
            query
        };
        let row = query.fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let config_json: String = row.try_get("config_json")?;
        let config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let base_url = validate_config(&config)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        Ok(Some(ResolvedUpstream {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            driver: row.try_get("driver")?,
            base_url,
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential: open_credential(&ciphertext, key_material)?,
        }))
    }
}

fn model_route_view(row: AnyRow) -> Result<ModelRouteView, AppError> {
    Ok(ModelRouteView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id").ok(),
        public_model: row.try_get("public_model")?,
        upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
        upstream_model: row.try_get("upstream_model")?,
        protocol: row.try_get("protocol")?,
        priority: row.try_get("priority")?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
fn validate_model_route_fields(
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
) -> Result<(), AppError> {
    let public_model = public_model.trim();
    let upstream_model = upstream_model.trim();
    if public_model.is_empty() || upstream_model.is_empty() {
        return Err(AppError::BadRequest(
            "public_model and upstream_model are required".into(),
        ));
    }
    if public_model.len() > 200 || upstream_model.len() > 500 {
        return Err(AppError::BadRequest(
            "public_model and upstream_model exceed their length limit".into(),
        ));
    }
    if public_model.chars().any(char::is_control) || upstream_model.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "model names must not contain control characters".into(),
        ));
    }
    if !matches!(protocol, "openai" | "anthropic" | "generation") {
        return Err(AppError::BadRequest(
            "route protocol must be openai, anthropic, or generation".into(),
        ));
    }
    if !(-1_000_000..=1_000_000).contains(&priority) {
        return Err(AppError::BadRequest(
            "route priority must be between -1000000 and 1000000".into(),
        ));
    }
    Ok(())
}
