use super::super::*;
use super::accounts::upstream_account_view;
use super::*;

pub struct ReauthorizeUpstreamAccountInput {
    pub tenant_external_id: String,
    pub expected_updated_at: i64,
    pub driver: String,
    pub oauth_session_id: Uuid,
    pub oauth_driver: String,
    pub oauth_refresh_url: Option<String>,
    pub provider_config: Option<Value>,
    pub credential: UpstreamCredential,
}

impl Database {
    pub async fn disconnect_upstream_oauth(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
        key_material: &[u8],
    ) -> Result<(UpstreamAccountView, String, UpstreamCredential), AppError> {
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.auth_kind, a.oauth_session_id, a.oauth_driver, a.updated_at, c.credential_ciphertext, c.revoked_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.id = $1 AND t.external_id = $2 FOR UPDATE OF a, c"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.auth_kind, a.oauth_session_id, a.oauth_driver, a.updated_at, c.credential_ciphertext, c.revoked_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.id = $1 AND t.external_id = $2"
            }
        };
        let row = sqlx::query(select)
            .bind(account_id.to_string())
            .bind(tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Forbidden)?;
        let oauth_driver = row
            .try_get::<Option<String>, _>("oauth_driver")?
            .ok_or_else(|| {
                AppError::BadRequest("upstream account has no OAuth lifecycle".into())
            })?;
        if row.try_get::<String, _>("auth_kind")? != "oauth"
            || row
                .try_get::<Option<String>, _>("oauth_session_id")?
                .is_none()
        {
            return Err(AppError::BadRequest(
                "upstream account has no OAuth lifecycle".into(),
            ));
        }
        let current_updated_at: i64 = row.try_get("updated_at")?;
        if current_updated_at != expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before disconnecting it".into(),
            ));
        }
        let credential = open_credential(
            &row.try_get::<String, _>("credential_ciphertext")?,
            key_material,
        )?;
        if row.try_get::<Option<i64>, _>("revoked_at")?.is_none() {
            let updated_at = now.max(current_updated_at.saturating_add(1));
            sqlx::query(
                "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND generation = (SELECT credential_generation FROM upstream_accounts WHERE id = $2) AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(account_id.to_string())
            .execute(&mut *tx)
            .await?;
            let changed = sqlx::query(
                "UPDATE upstream_accounts SET status = 'disabled', updated_at = $1 WHERE id = $2 AND updated_at = $3",
            )
            .bind(updated_at)
            .bind(account_id.to_string())
            .bind(current_updated_at)
            .execute(&mut *tx)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "upstream provider changed while it was being disconnected".into(),
                ));
            }
        }
        tx.commit().await?;
        let view = self
            .upstream_account_for_reauthorization(account_id, tenant_external_id)
            .await?;
        Ok((view, oauth_driver, credential))
    }

    /// Completes an interactive OAuth flow against an existing stable
    /// upstream identity. The opaque OAuth session id is the replay key: a
    /// repeated poll returns the already-installed generation instead of
    /// rotating twice.
    pub async fn reauthorize_upstream_account(
        &self,
        account_id: Uuid,
        input: ReauthorizeUpstreamAccountInput,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        if input.credential.auth_kind() != "oauth" {
            return Err(AppError::BadRequest(
                "interactive authorization must produce an OAuth credential".into(),
            ));
        }
        if !matches!(
            input.oauth_driver.as_str(),
            "cursor"
                | "provider_adapter"
                | "openai_codex_device"
                | "anthropic_claude_manual_pkce"
                | "github_copilot_device"
        ) {
            return Err(AppError::BadRequest(
                "unsupported OAuth reauthorization lifecycle".into(),
            ));
        }
        if input.oauth_refresh_url.is_none() {
            return Err(AppError::BadRequest(
                "OAuth reauthorization refresh endpoint is required".into(),
            ));
        }
        let provider_config_json = input
            .provider_config
            .as_ref()
            .map(|config| {
                if !config.is_object() {
                    return Err(AppError::BadRequest(
                        "upstream provider configuration must be an object".into(),
                    ));
                }
                serde_json::to_string(config).map_err(|_| AppError::BadRequest("config".into()))
            })
            .transpose()?;
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.id, a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND t.external_id = $2"
            }
        };
        let row = sqlx::query(select)
            .bind(account_id.to_string())
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Forbidden)?;
        let current_session = row.try_get::<Option<String>, _>("oauth_session_id")?;
        let completed_session = input.oauth_session_id.to_string();
        if current_session.as_deref() == Some(completed_session.as_str()) {
            let view = upstream_account_view(row)?;
            tx.commit().await?;
            return Ok(view);
        }
        let current_driver: String = row.try_get("driver")?;
        let current_auth_kind: String = row.try_get("auth_kind")?;
        let current_oauth_driver = row.try_get::<Option<String>, _>("oauth_driver")?;
        if current_driver != input.driver
            || current_oauth_driver.as_deref() != Some(input.oauth_driver.as_str())
            || !upstream_can_reauthorize(
                &current_driver,
                &current_auth_kind,
                current_session.as_deref(),
                current_oauth_driver.as_deref(),
            )
        {
            return Err(AppError::BadRequest(
                "upstream account does not support interactive reauthorization".into(),
            ));
        }
        if !matches!(
            row.try_get::<String, _>("status")?.as_str(),
            "active" | "disabled"
        ) {
            return Err(AppError::Forbidden);
        }
        if row.try_get::<i64, _>("updated_at")? != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the upstream provider before authorizing it again".into(),
            ));
        }
        let current_credential = open_credential(
            &row.try_get::<String, _>("credential_ciphertext")?,
            key_material,
        )?;
        let credential = input.credential.preserve_proxy_from(&current_credential);
        credential.validate(now)?;
        let ciphertext = seal_credential(&credential, key_material)?;
        let current_generation: i64 = row.try_get("credential_generation")?;
        let generation = current_generation
            .checked_add(1)
            .ok_or(AppError::Internal)?;
        let updated_at = now.max(input.expected_updated_at.saturating_add(1));
        sqlx::query(
            "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(generation)
        .bind(ciphertext)
        .bind(credential.expires_at())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let changed = sqlx::query(
            "UPDATE upstream_accounts SET auth_kind = 'oauth', credential_generation = $1, oauth_session_id = $2, oauth_driver = $3, oauth_refresh_url = $4, config_json = COALESCE($5, config_json), updated_at = $6 WHERE id = $7 AND updated_at = $8",
        )
        .bind(generation)
        .bind(input.oauth_session_id.to_string())
        .bind(&input.oauth_driver)
        .bind(&input.oauth_refresh_url)
        .bind(provider_config_json)
        .bind(updated_at)
        .bind(account_id.to_string())
        .bind(input.expected_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "upstream provider changed while authorization was completing".into(),
            ));
        }
        tx.commit().await?;
        self.upstream_account_for_reauthorization(account_id, &input.tenant_external_id)
            .await
    }

    pub async fn rotate_upstream_credential(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        let (account, _) = self
            .rotate_upstream_credential_with_outcome(
                account_id,
                credential,
                idempotency_key,
                key_material,
            )
            .await?;
        Ok(account)
    }

    /// Rotates an upstream credential and reports whether this invocation
    /// installed a new credential generation. An exact idempotency replay
    /// returns the stored account view with `changed` set to false.
    pub async fn rotate_upstream_credential_with_outcome(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<(UpstreamAccountView, bool), AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash = upstream_credential_rotation_request_hash(account_id, &credential)?;
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut tx,
            UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
            account_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let view = open_rotation_replay(
                replay,
                UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
                account_id,
                idempotency_key,
                &request_hash,
                key_material,
                now,
            )?;
            tx.commit().await?;
            return Ok((view, false));
        }
        let view = self
            .rotate_upstream_credential_claimed(
                &mut tx,
                account_id,
                credential,
                idempotency_key,
                UPSTREAM_CREDENTIAL_ROTATION_RESOURCE,
                &request_hash,
                expires_at,
                None,
                None,
                key_material,
            )
            .await?;
        tx.commit().await?;
        Ok((view, true))
    }
    /// Claim an OAuth refresh before contacting the authorization server. A
    /// retry with the same key returns the stored result, while a concurrent
    /// retry observes the in-progress tombstone and cannot refresh twice.
    pub async fn begin_upstream_oauth_refresh(
        &self,
        account_id: Uuid,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<Option<UpstreamAccountView>, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash =
            credential_rotation_request_hash(UPSTREAM_OAUTH_REFRESH_RESOURCE, account_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut tx = self.pool.begin().await?;
        let replay = claim_credential_rotation(
            &mut tx,
            UPSTREAM_OAUTH_REFRESH_RESOURCE,
            account_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?;
        if let Some(replay) = replay {
            if replay.response_ciphertext.is_some() {
                let view = open_rotation_replay(
                    replay,
                    UPSTREAM_OAUTH_REFRESH_RESOURCE,
                    account_id,
                    idempotency_key,
                    &request_hash,
                    key_material,
                    now,
                )?;
                tx.commit().await?;
                return Ok(Some(view));
            }
            let pending = sqlx::query(
                "SELECT credential_generation, pending_credential_ciphertext FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(pending) = pending else {
                return Err(AppError::Conflict(
                    "OAuth refresh is already in progress for this Idempotency-Key".into(),
                ));
            };
            let pending_ciphertext: Option<String> =
                pending.try_get("pending_credential_ciphertext")?;
            let Some(pending_ciphertext) = pending_ciphertext else {
                return Err(AppError::Conflict(
                    "OAuth refresh is already in progress for this Idempotency-Key".into(),
                ));
            };
            let credential = open_credential(&pending_ciphertext, key_material)?;
            let expected_generation: i64 = pending.try_get("credential_generation")?;
            let view = self
                .rotate_upstream_credential_claimed(
                    &mut tx,
                    account_id,
                    credential,
                    idempotency_key,
                    UPSTREAM_OAUTH_REFRESH_RESOURCE,
                    &request_hash,
                    replay.expires_at,
                    Some(expected_generation),
                    Some(&pending_ciphertext),
                    key_material,
                )
                .await?;
            sqlx::query(
                "DELETE FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND idempotency_key = $2 AND credential_generation = $3",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .bind(expected_generation)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Some(view));
        }
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT auth_kind, status, credential_generation, oauth_session_id FROM upstream_accounts WHERE id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT auth_kind, status, credential_generation, oauth_session_id FROM upstream_accounts WHERE id = $1"
            }
        };
        let account = sqlx::query(select)
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if account.try_get::<String, _>("auth_kind")? != "oauth"
            || !matches!(
                account.try_get::<String, _>("status")?.as_str(),
                "active" | "disabled"
            )
            || account
                .try_get::<Option<String>, _>("oauth_session_id")?
                .is_none()
        {
            return Err(AppError::BadRequest(
                "upstream account has no managed OAuth lifecycle".into(),
            ));
        }
        let generation: i64 = account.try_get("credential_generation")?;
        let lease_expires_at = now.saturating_add(UPSTREAM_OAUTH_REFRESH_LEASE_MILLIS);
        let leased = sqlx::query(
            "INSERT INTO upstream_oauth_refresh_leases (account_id, credential_generation, idempotency_key, lease_expires_at, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(account_id) DO UPDATE SET credential_generation = excluded.credential_generation, idempotency_key = excluded.idempotency_key, pending_credential_ciphertext = NULL, pending_expires_at = NULL, lease_expires_at = excluded.lease_expires_at, created_at = excluded.created_at WHERE (upstream_oauth_refresh_leases.pending_credential_ciphertext IS NULL AND upstream_oauth_refresh_leases.lease_expires_at <= $5) OR upstream_oauth_refresh_leases.credential_generation <> excluded.credential_generation",
        )
        .bind(account_id.to_string())
        .bind(generation)
        .bind(idempotency_key)
        .bind(lease_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if leased.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "OAuth refresh is already in progress for this credential generation".into(),
            ));
        }
        tx.commit().await?;
        Ok(None)
    }
    pub async fn finish_upstream_oauth_refresh(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        // Seal once: bounded finalize retries reuse identical ciphertext and
        // never contact the authorization server again.
        let pending_ciphertext = seal_credential(&credential, key_material)?;
        for attempt in 0..3 {
            let result = self
                .finish_upstream_oauth_refresh_attempt(
                    account_id,
                    credential.clone(),
                    &pending_ciphertext,
                    idempotency_key,
                    key_material,
                )
                .await;
            match result {
                Ok(view) => return Ok(view),
                Err(error)
                    if attempt < 2
                        && matches!(error, AppError::Internal | AppError::Storage(_)) =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded OAuth finalize loop always returns")
    }
    async fn finish_upstream_oauth_refresh_attempt(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        pending_ciphertext: &str,
        idempotency_key: &str,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        let request_hash =
            credential_rotation_request_hash(UPSTREAM_OAUTH_REFRESH_RESOURCE, account_id);
        if let Some(view) = self
            .stage_upstream_oauth_refresh_pending(
                account_id,
                &credential,
                pending_ciphertext,
                idempotency_key,
                &request_hash,
                key_material,
            )
            .await?
        {
            return Ok(view);
        }
        let mut tx = self.pool.begin().await?;
        let replay_row = sqlx::query(
            "SELECT resource_kind, resource_id, request_hash, response_ciphertext, expires_at FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::BadRequest("OAuth refresh claim is missing; retry authorization with a new Idempotency-Key".into()))?;
        if replay_row.try_get::<String, _>("resource_kind")? != UPSTREAM_OAUTH_REFRESH_RESOURCE
            || replay_row.try_get::<String, _>("resource_id")? != account_id.to_string()
            || replay_row.try_get::<String, _>("request_hash")? != request_hash
        {
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different credential rotation".into(),
            ));
        }
        let expires_at: i64 = replay_row.try_get("expires_at")?;
        let response_ciphertext: Option<String> = replay_row.try_get("response_ciphertext")?;
        if response_ciphertext.is_some() {
            let view = open_rotation_replay(
                RotationReplay {
                    response_ciphertext,
                    expires_at,
                },
                UPSTREAM_OAUTH_REFRESH_RESOURCE,
                account_id,
                idempotency_key,
                &request_hash,
                key_material,
                unix_millis(),
            )?;
            tx.commit().await?;
            return Ok(view);
        }
        let lease = sqlx::query(
            "SELECT credential_generation, pending_credential_ciphertext FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND idempotency_key = $2",
        )
        .bind(account_id.to_string())
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::Conflict("OAuth refresh lease is missing or was superseded".into()))?;
        if lease
            .try_get::<Option<String>, _>("pending_credential_ciphertext")?
            .as_deref()
            != Some(pending_ciphertext)
        {
            return Err(AppError::Conflict(
                "OAuth refresh pending result does not match its lease".into(),
            ));
        }
        let expected_generation: i64 = lease.try_get("credential_generation")?;
        let view = self
            .rotate_upstream_credential_claimed(
                &mut tx,
                account_id,
                credential,
                idempotency_key,
                UPSTREAM_OAUTH_REFRESH_RESOURCE,
                &request_hash,
                expires_at,
                Some(expected_generation),
                Some(pending_ciphertext),
                key_material,
            )
            .await?;
        sqlx::query(
            "DELETE FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND idempotency_key = $2 AND credential_generation = $3",
        )
        .bind(account_id.to_string())
        .bind(idempotency_key)
        .bind(expected_generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(view)
    }
    async fn stage_upstream_oauth_refresh_pending(
        &self,
        account_id: Uuid,
        credential: &UpstreamCredential,
        pending_ciphertext: &str,
        idempotency_key: &str,
        request_hash: &str,
        key_material: &[u8],
    ) -> Result<Option<UpstreamAccountView>, AppError> {
        let mut tx = self.pool.begin().await?;
        let replay = sqlx::query(
            "SELECT resource_kind, resource_id, request_hash, response_ciphertext, expires_at FROM credential_rotation_replays WHERE idempotency_key = $1",
        )
        .bind(idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::BadRequest("OAuth refresh claim is missing; retry authorization with a new Idempotency-Key".into()))?;
        if replay.try_get::<String, _>("resource_kind")? != UPSTREAM_OAUTH_REFRESH_RESOURCE
            || replay.try_get::<String, _>("resource_id")? != account_id.to_string()
            || replay.try_get::<String, _>("request_hash")? != request_hash
        {
            return Err(AppError::BadRequest(
                "Idempotency-Key was already used for a different credential rotation".into(),
            ));
        }
        let expires_at: i64 = replay.try_get("expires_at")?;
        let response_ciphertext: Option<String> = replay.try_get("response_ciphertext")?;
        if response_ciphertext.is_some() {
            let view = open_rotation_replay(
                RotationReplay {
                    response_ciphertext,
                    expires_at,
                },
                UPSTREAM_OAUTH_REFRESH_RESOURCE,
                account_id,
                idempotency_key,
                request_hash,
                key_material,
                unix_millis(),
            )?;
            tx.commit().await?;
            return Ok(Some(view));
        }
        let staged = sqlx::query(
            "UPDATE upstream_oauth_refresh_leases SET pending_credential_ciphertext = $1, pending_expires_at = $2 WHERE account_id = $3 AND idempotency_key = $4 AND (pending_credential_ciphertext IS NULL OR pending_credential_ciphertext = $1)",
        )
        .bind(pending_ciphertext)
        .bind(credential.expires_at())
        .bind(account_id.to_string())
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;
        if staged.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "OAuth refresh lease is missing or contains a different pending result".into(),
            ));
        }
        tx.commit().await?;
        Ok(None)
    }
    pub async fn abort_upstream_oauth_refresh(
        &self,
        account_id: Uuid,
        idempotency_key: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "DELETE FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND idempotency_key = $2 AND pending_credential_ciphertext IS NULL",
        )
        .bind(account_id.to_string())
        .bind(idempotency_key)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM credential_rotation_replays WHERE idempotency_key = $1 AND resource_kind = $2 AND resource_id = $3 AND response_ciphertext IS NULL AND NOT EXISTS (SELECT 1 FROM upstream_oauth_refresh_leases l WHERE l.account_id = $3 AND l.idempotency_key = $1)",
        )
        .bind(idempotency_key)
        .bind(UPSTREAM_OAUTH_REFRESH_RESOURCE)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    /// Returns core-owned OAuth lifecycle metadata. The config fallback is
    /// read-only compatibility for accounts created before schema v32.
    pub async fn upstream_oauth_lifecycle(
        &self,
        account_id: Uuid,
    ) -> Result<(String, String), AppError> {
        let row = sqlx::query(
            "SELECT driver, auth_kind, oauth_session_id, oauth_driver, oauth_refresh_url, config_json FROM upstream_accounts WHERE id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        if row.try_get::<String, _>("auth_kind")? != "oauth"
            || row
                .try_get::<Option<String>, _>("oauth_session_id")?
                .is_none()
        {
            return Err(AppError::BadRequest(
                "upstream account has no managed OAuth lifecycle".into(),
            ));
        }
        if let (Some(driver), Some(refresh_url)) = (
            row.try_get::<Option<String>, _>("oauth_driver")?,
            row.try_get::<Option<String>, _>("oauth_refresh_url")?,
        ) {
            return Ok((driver, refresh_url));
        }
        let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
            .map_err(|_| AppError::Internal)?;
        let driver = config
            .pointer("/oauth/driver")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("upstream OAuth driver is missing".into()))?;
        let refresh_url = config
            .pointer("/oauth/refresh_url")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("upstream OAuth refresh URL is missing".into()))?;
        Ok((driver.to_owned(), refresh_url.to_owned()))
    }
    pub async fn list_managed_oauth_refresh_candidates(
        &self,
        refresh_before: i64,
        limit: i64,
    ) -> Result<Vec<(Uuid, i64)>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id, a.credential_generation FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.status = 'active' AND a.auth_kind = 'oauth' AND a.oauth_session_id IS NOT NULL AND a.oauth_refresh_url IS NOT NULL AND a.driver <> 'cpa-gemini-oauth-legacy' AND c.expires_at IS NOT NULL AND c.expires_at <= $1 ORDER BY c.expires_at, a.id LIMIT $2",
        )
        .bind(refresh_before)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    parse_uuid(row.try_get("id")?)?,
                    row.try_get("credential_generation")?,
                ))
            })
            .collect()
    }
    #[allow(clippy::too_many_arguments)]
    async fn rotate_upstream_credential_claimed(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        account_id: Uuid,
        credential: UpstreamCredential,
        idempotency_key: &str,
        resource_kind: &str,
        request_hash: &str,
        replay_expires_at: i64,
        expected_generation: Option<i64>,
        prepared_ciphertext: Option<&str>,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        let now = unix_millis();
        let ciphertext = match prepared_ciphertext {
            Some(ciphertext) => ciphertext.to_owned(),
            None => seal_credential(&credential, key_material)?,
        };
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 FOR UPDATE OF a"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.tenant_id, t.external_id AS tenant_external_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.created_at, a.updated_at, (SELECT COUNT(*) FROM model_routes r WHERE r.upstream_account_id = a.id) AS route_count FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1"
            }
        };
        let row = sqlx::query(select)
            .bind(account_id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if !matches!(status.as_str(), "active" | "disabled") {
            return Err(AppError::Forbidden);
        }
        let auth_kind = credential.auth_kind().to_owned();
        let current_generation: i64 = row.try_get("credential_generation")?;
        if expected_generation.is_some_and(|expected| expected != current_generation) {
            return Err(AppError::Conflict(
                "OAuth credential changed while refresh was in progress".into(),
            ));
        }
        let generation = current_generation + 1;
        let updated_at = now.max(row.try_get::<i64, _>("updated_at")?.saturating_add(1));
        sqlx::query(
            "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(generation)
        .bind(ciphertext)
        .bind(credential.expires_at())
        .bind(now)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE upstream_accounts SET auth_kind = $1, credential_generation = $2, oauth_session_id = CASE WHEN $1 = 'oauth' THEN oauth_session_id ELSE NULL END, oauth_driver = CASE WHEN $1 = 'oauth' THEN oauth_driver ELSE NULL END, oauth_refresh_url = CASE WHEN $1 = 'oauth' THEN oauth_refresh_url ELSE NULL END, updated_at = $3 WHERE id = $4",
        )
        .bind(&auth_kind)
        .bind(generation)
        .bind(updated_at)
        .bind(account_id.to_string())
        .execute(&mut **tx)
        .await?;

        let config_json: String = row.try_get("config_json")?;
        let view = UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            tenant_external_id: Some(row.try_get("tenant_external_id")?),
            name: row.try_get("name")?,
            driver: row.try_get("driver")?,
            auth_kind: auth_kind.clone(),
            connection_method: upstream_connection_method(
                row.try_get::<String, _>("driver")?.as_str(),
                credential.auth_kind(),
            ),
            credential_generation: generation,
            status,
            config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
            credential_expires_at: credential.expires_at(),
            can_refresh: auth_kind == "oauth"
                && row
                    .try_get::<Option<String>, _>("oauth_session_id")?
                    .is_some()
                && row
                    .try_get::<Option<String>, _>("oauth_refresh_url")?
                    .is_some()
                && !matches!(
                    row.try_get::<String, _>("driver")?.as_str(),
                    "cpa-gemini-oauth-legacy"
                ),
            can_rotate: auth_kind != "none",
            can_reauthorize: upstream_can_reauthorize(
                &row.try_get::<String, _>("driver")?,
                &auth_kind,
                row.try_get::<Option<String>, _>("oauth_session_id")?
                    .as_deref(),
                row.try_get::<Option<String>, _>("oauth_driver")?.as_deref(),
            ),
            route_count: row.try_get("route_count")?,
            created_at: row.try_get("created_at")?,
            updated_at,
        };
        store_credential_rotation_response(
            tx,
            idempotency_key,
            &view,
            resource_kind,
            account_id,
            request_hash,
            replay_expires_at,
            key_material,
        )
        .await?;
        Ok(view)
    }
}

fn upstream_credential_rotation_request_hash(
    account_id: Uuid,
    credential: &UpstreamCredential,
) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(credential).map_err(|_| AppError::Internal)?;
    let mut hash = Sha256::new();
    hash.update(b"memeloop-token-center/upstream-credential-rotation-request/v1\0");
    hash.update(account_id.as_bytes());
    hash.update([0]);
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
}
