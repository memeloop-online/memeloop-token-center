use super::super::*;
use super::*;

pub struct CreateUpstreamAccountInput {
    pub tenant_external_id: String,
    pub name: String,
    pub driver: String,
    pub config: serde_json::Value,
    pub credential: UpstreamCredential,
    pub oauth_session_id: Option<Uuid>,
    /// Core-owned OAuth lifecycle metadata; never embedded in provider config.
    pub oauth_driver: Option<String>,
    pub oauth_refresh_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpdateUpstreamAccountInput {
    pub name: String,
    pub config: serde_json::Value,
    pub expected_updated_at: i64,
}

impl Database {
    pub async fn create_upstream_account(
        &self,
        input: CreateUpstreamAccountInput,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        validate_upstream_account_name(&input.name)?;
        let _ = validate_config(&input.config)?;
        let now = unix_millis();
        let account_id = Uuid::now_v7();
        let tenant_candidate = Uuid::now_v7();
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let credential_ciphertext = seal_credential(&input.credential, key_material)?;
        let auth_kind = input.credential.auth_kind();
        let credential_expires_at = input.credential.expires_at();
        let can_reauthorize = upstream_can_reauthorize(
            &input.driver,
            auth_kind,
            input.oauth_session_id.is_some().then_some("present"),
            input.oauth_driver.as_deref(),
        );
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_candidate.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        if let Some(session_id) = input.oauth_session_id {
            let existing = sqlx::query(
                "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.oauth_session_id = $1",
            )
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                tx.commit().await?;
                return upstream_account_view(existing);
            }
        }
        if sqlx::query("SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2")
            .bind(&tenant_id)
            .bind(input.name.trim())
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "another upstream provider already uses this name".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 'active', 1, $7, $8, $9, $10, $11)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(input.name.trim())
        .bind(&input.driver)
        .bind(auth_kind)
        .bind(config_json)
        .bind(input.oauth_session_id.map(|id| id.to_string()))
        .bind(&input.oauth_driver)
        .bind(&input.oauth_refresh_url)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, $3, $4, $5)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(credential_ciphertext)
        .bind(credential_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(tenant_id)?,
            tenant_external_id: Some(input.tenant_external_id.clone()),
            name: input.name.trim().to_owned(),
            driver: input.driver.clone(),
            auth_kind: auth_kind.to_owned(),
            connection_method: upstream_connection_method(&input.driver, auth_kind),
            credential_generation: 1,
            status: "active".to_owned(),
            config: input.config,
            credential_expires_at,
            can_refresh: auth_kind == "oauth"
                && input.oauth_session_id.is_some()
                && input.oauth_refresh_url.is_some()
                && !matches!(
                    input.driver.as_str(),
                    "cpa-subscription-bridge" | "cpa-gemini-oauth-legacy"
                ),
            can_rotate: auth_kind != "none" && input.driver != "cpa-subscription-bridge",
            can_reauthorize,
            route_count: 0,
            created_at: now,
            updated_at: now,
        })
    }
    pub async fn upstream_account_with_credential(
        &self,
        account_id: Uuid,
        key_material: &[u8],
    ) -> Result<(UpstreamAccountView, UpstreamCredential), AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        Ok((upstream_account_view(row)?, credential))
    }
    pub async fn update_upstream_account(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        input: UpdateUpstreamAccountInput,
    ) -> Result<UpstreamAccountView, AppError> {
        validate_upstream_account_name(&input.name)?;
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_view = upstream_account_view(current)?;
        if current_view.name == input.name.trim() && current_view.config == input.config {
            tx.commit().await?;
            return Ok(current_view);
        }
        if current_view.updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before saving it again".into(),
            ));
        }
        let duplicate = sqlx::query(
            "SELECT id FROM upstream_accounts WHERE tenant_id = $1 AND name = $2 AND id <> $3",
        )
        .bind(current_view.tenant_id.to_string())
        .bind(input.name.trim())
        .bind(account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if duplicate {
            return Err(AppError::Conflict(
                "another upstream provider already uses this name".into(),
            ));
        }
        let updated_at = unix_millis().max(current_view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE upstream_accounts SET name = $1, config_json = $2, updated_at = $3 WHERE id = $4 AND tenant_id = $5 AND updated_at = $6",
        )
        .bind(input.name.trim())
        .bind(config_json)
        .bind(updated_at)
        .bind(account_id.to_string())
        .bind(current_view.tenant_id.to_string())
        .bind(input.expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before saving it again".into(),
            ));
        }
        tx.commit().await?;
        Ok(UpstreamAccountView {
            name: input.name.trim().to_owned(),
            config: input.config,
            updated_at,
            ..current_view
        })
    }
    pub async fn set_upstream_account_status(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        status: &str,
        expected_updated_at: i64,
    ) -> Result<UpstreamAccountView, AppError> {
        if !matches!(status, "active" | "disabled") {
            return Err(AppError::BadRequest(
                "upstream provider status must be active or disabled".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let mut view = upstream_account_view(current)?;
        if view.driver == "cpa-subscription-bridge" && status == "active" {
            return Err(AppError::BadRequest(
                "this legacy upstream type is retired".into(),
            ));
        }
        if view.status == status {
            tx.commit().await?;
            return Ok(view);
        }
        if view.updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before changing its status".into(),
            ));
        }
        let updated_at = unix_millis().max(view.updated_at.saturating_add(1));
        let changed = sqlx::query(
            "UPDATE upstream_accounts SET status = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND updated_at = $5",
        )
        .bind(status)
        .bind(updated_at)
        .bind(account_id.to_string())
        .bind(view.tenant_id.to_string())
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before changing its status".into(),
            ));
        }
        tx.commit().await?;
        view.status = status.to_owned();
        view.updated_at = updated_at;
        Ok(view)
    }
    pub async fn delete_upstream_account(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let account = sqlx::query(
            "SELECT a.tenant_id, a.status, a.updated_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(account) = account else {
            tx.commit().await?;
            return Ok(());
        };
        if account.try_get::<String, _>("status")? != "disabled" {
            return Err(AppError::Conflict(
                "disable the upstream provider before deleting it".into(),
            ));
        }
        if account.try_get::<i64, _>("updated_at")? != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before deleting it".into(),
            ));
        }
        let imported = sqlx::query(
            "SELECT upstream_account_id FROM upstream_account_imports WHERE upstream_account_id = $1 LIMIT 1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if imported {
            return Err(AppError::Conflict(
                "imported upstream providers are retained for audit and cannot be deleted".into(),
            ));
        }
        let has_routes =
            sqlx::query("SELECT id FROM model_routes WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if has_routes {
            return Err(AppError::Conflict(
                "the upstream provider still has model routes and must be retained".into(),
            ));
        }
        let has_request_history =
            sqlx::query("SELECT id FROM request_records WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        let has_generation_history =
            sqlx::query("SELECT id FROM generation_jobs WHERE upstream_account_id = $1 LIMIT 1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if has_request_history || has_generation_history {
            return Err(AppError::Conflict(
                "the upstream provider has request history and must be retained for audit".into(),
            ));
        }
        sqlx::query("DELETE FROM upstream_credentials WHERE upstream_account_id = $1")
            .bind(account_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM credential_rotation_replays WHERE resource_id = $1 AND resource_kind IN ($2, $3)",
        )
        .bind(account_id.to_string())
        .bind(UPSTREAM_CREDENTIAL_ROTATION_RESOURCE)
        .bind(UPSTREAM_OAUTH_REFRESH_RESOURCE)
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query(
            "DELETE FROM upstream_accounts WHERE id = $1 AND tenant_id = $2 AND status = 'disabled' AND updated_at = $3",
        )
        .bind(account_id.to_string())
        .bind(account.try_get::<String, _>("tenant_id")?)
        .bind(expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the upstream provider before deleting it".into(),
            ));
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn list_upstream_accounts(
        &self,
        tenant_external_id: &str,
    ) -> Result<Vec<UpstreamAccountView>, AppError> {
        self.list_upstream_accounts_page(Some(tenant_external_id), None, None, 100)
            .await
    }

    pub async fn list_upstream_accounts_page(
        &self,
        tenant_external_id: Option<&str>,
        before_created_at: Option<i64>,
        before_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<UpstreamAccountView>, AppError> {
        let before_created_at = before_created_at.unwrap_or(i64::MAX);
        let before_id = before_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned());
        let rows = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE ($1 = '' OR t.external_id = $1) AND (a.created_at < $2 OR (a.created_at = $2 AND a.id < $3)) ORDER BY a.created_at DESC, a.id DESC LIMIT $4",
        )
        .bind(tenant_external_id.unwrap_or_default())
        .bind(before_created_at)
        .bind(before_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(upstream_account_view).collect()
    }
    /// Lists every upstream account for a global operator. Tenant-scoped
    /// operators must use `list_upstream_accounts` so the authorization scope
    /// remains visible at the call site.
    pub async fn list_all_upstream_accounts(&self) -> Result<Vec<UpstreamAccountView>, AppError> {
        self.list_upstream_accounts_page(None, None, None, 100)
            .await
    }
    pub async fn require_upstream_tenant(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }
    pub async fn upstream_account_for_reauthorization(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<UpstreamAccountView, AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Forbidden)?;
        upstream_account_view(row)
    }
    pub async fn upstream_driver(&self, account_id: Uuid) -> Result<String, AppError> {
        sqlx::query("SELECT driver FROM upstream_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("driver")
            .map_err(AppError::from)
    }
}

pub(super) fn upstream_account_view(
    row: sqlx::any::AnyRow,
) -> Result<UpstreamAccountView, AppError> {
    let config_json: String = row.try_get("config_json")?;
    let driver: String = row.try_get("driver")?;
    let auth_kind: String = row.try_get("auth_kind")?;
    let oauth_session_id = row
        .try_get::<Option<String>, _>("oauth_session_id")
        .ok()
        .flatten();
    let oauth_driver = row
        .try_get::<Option<String>, _>("oauth_driver")
        .ok()
        .flatten();
    let managed_oauth = oauth_session_id.is_some()
        && row
            .try_get::<Option<String>, _>("oauth_refresh_url")
            .ok()
            .flatten()
            .is_some();
    let can_refresh = auth_kind == "oauth"
        && managed_oauth
        && !matches!(
            driver.as_str(),
            "cpa-subscription-bridge" | "cpa-gemini-oauth-legacy"
        );
    let can_rotate = auth_kind != "none" && driver != "cpa-subscription-bridge";
    let can_reauthorize = upstream_can_reauthorize(
        &driver,
        &auth_kind,
        oauth_session_id.as_deref(),
        oauth_driver.as_deref(),
    );
    Ok(UpstreamAccountView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id").ok(),
        name: row.try_get("name")?,
        connection_method: upstream_connection_method(&driver, &auth_kind),
        driver,
        auth_kind,
        credential_generation: row.try_get("credential_generation")?,
        status: row.try_get("status")?,
        config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
        credential_expires_at: row.try_get("expires_at")?,
        can_refresh,
        can_rotate,
        can_reauthorize,
        route_count: row.try_get("route_count")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
pub(super) fn validate_upstream_account_name(name: &str) -> Result<(), AppError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "upstream provider name must contain 1 to 200 non-control characters".into(),
        ));
    }
    Ok(())
}
