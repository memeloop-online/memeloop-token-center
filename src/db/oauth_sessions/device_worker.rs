use super::*;

impl Database {
    pub async fn codex_login_progress(
        &self,
        reference: &OAuthLoginSessionReference,
        now: i64,
    ) -> Result<OAuthLoginClaim, AppError> {
        let row = sqlx::query("SELECT s.tenant_external_id, s.operator_service_id, CASE WHEN s.operator_is_bootstrap THEN 1 ELSE 0 END AS operator_is_bootstrap_int, s.expires_at, s.status, s.next_poll_at, s.result_account_id, (SELECT a.id FROM upstream_accounts a WHERE a.oauth_session_id = s.id) AS recovered_account_id FROM oauth_login_sessions s WHERE s.id = $1 AND s.flow_kind = 'openai_codex_device'")
            .bind(reference.session_id.to_string()).fetch_optional(&self.pool).await?.ok_or(AppError::Forbidden)?;
        require_matching_reference(&row, reference)?;
        if now >= reference.expires_at.saturating_add(24 * 60 * 60 * 1000) {
            return Err(AppError::BadRequest("OAuth login recovery expired".into()));
        }
        if let Some(account_id) = row
            .try_get::<Option<String>, _>("result_account_id")?
            .or(row.try_get::<Option<String>, _>("recovered_account_id")?)
        {
            return Ok(OAuthLoginClaim::Consumed {
                account_id: parse_uuid(account_id)?,
            });
        }
        let status: String = row.try_get("status")?;
        if status == "failed"
            || (reference.expires_at <= now && matches!(status.as_str(), "pending" | "polling"))
        {
            return Err(AppError::BadRequest("OAuth login expired or failed".into()));
        }
        Ok(OAuthLoginClaim::Pending {
            retry_after_seconds: millis_until(now, row.try_get::<i64, _>("next_poll_at")?)
                .clamp(1, 5),
        })
    }

    pub async fn defer_codex_login_worker(
        &self,
        session_id: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        sqlx::query("UPDATE oauth_login_sessions SET next_poll_at = $1 WHERE id = $2 AND flow_kind = 'openai_codex_device' AND status IN ('pending', 'ready') AND next_poll_at <= $3")
            .bind(now.saturating_add(15_000)).bind(session_id.to_string()).bind(now).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn due_codex_login_sessions(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>, AppError> {
        sqlx::query("UPDATE oauth_login_sessions SET status = 'failed', lease_owner = NULL, lease_expires_at = NULL, updated_at = $1 WHERE id IN (SELECT id FROM oauth_login_sessions WHERE flow_kind = 'openai_codex_device' AND expires_at <= $1 AND (status = 'pending' OR (status = 'polling' AND lease_expires_at <= $1)) ORDER BY expires_at, id LIMIT $2)")
            .bind(now).bind(limit.clamp(1, 100)).execute(&self.pool).await?;
        let rows = sqlx::query("SELECT id FROM oauth_login_sessions WHERE flow_kind = 'openai_codex_device' AND next_poll_at <= $1 AND ((expires_at > $1 AND (status = 'pending' OR (status = 'polling' AND lease_expires_at <= $1))) OR (expires_at > $2 AND (status = 'ready' OR (status = 'finalizing' AND lease_expires_at <= $1)))) ORDER BY next_poll_at, id LIMIT $3")
            .bind(now).bind(now.saturating_sub(24 * 60 * 60 * 1000)).bind(limit.clamp(1, 100)).fetch_all(&self.pool).await?;
        rows.into_iter()
            .map(|row| parse_uuid(row.try_get("id")?))
            .collect()
    }

    pub async fn codex_login_session_reference(
        &self,
        session_id: Uuid,
    ) -> Result<(OAuthLoginSessionReference, String), AppError> {
        let row = sqlx::query("SELECT tenant_external_id, operator_service_id, expires_at, state_ciphertext FROM oauth_login_sessions WHERE id = $1 AND flow_kind = 'openai_codex_device'")
            .bind(session_id.to_string()).fetch_optional(&self.pool).await?.ok_or(AppError::Forbidden)?;
        Ok((
            OAuthLoginSessionReference {
                session_id,
                flow_kind: "openai_codex_device".into(),
                tenant_external_id: row.try_get("tenant_external_id")?,
                operator_service_id: row
                    .try_get::<Option<String>, _>("operator_service_id")?
                    .map(parse_uuid)
                    .transpose()?,
                expires_at: row.try_get("expires_at")?,
            },
            row.try_get("state_ciphertext")?,
        ))
    }

    pub async fn oauth_login_worker_authority(
        &self,
        reference: &OAuthLoginSessionReference,
    ) -> Result<AuthenticatedService, AppError> {
        let Some(service_id) = reference.operator_service_id else {
            return Ok(AuthenticatedService::bootstrap());
        };
        let row = sqlx::query("SELECT p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL LEFT JOIN tenants t ON t.external_id = c.tenant_external_id WHERE p.id = $1 AND p.status = 'active' AND (c.tenant_external_id IS NULL OR t.status = 'active')")
            .bind(service_id.to_string()).fetch_optional(&self.pool).await?.ok_or(AppError::Forbidden)?;
        let scopes: Vec<String> = serde_json::from_str(&row.try_get::<String, _>("scopes_json")?)
            .map_err(|_| AppError::Forbidden)?;
        let service = AuthenticatedService {
            service_id: Some(service_id),
            credential_generation: Some(row.try_get("credential_generation")?),
            scopes,
            tenant_external_id: row.try_get("tenant_external_id")?,
        };
        if !service.allows("oauth:write")
            || service
                .tenant_external_id
                .as_ref()
                .is_some_and(|tenant| tenant != &reference.tenant_external_id)
        {
            return Err(AppError::Forbidden);
        }
        Ok(service)
    }

    pub async fn claim_codex_login_poll(
        &self,
        reference: &OAuthLoginSessionReference,
        now: i64,
        interval: u64,
    ) -> Result<OAuthLoginClaim, AppError> {
        if reference.flow_kind != "openai_codex_device"
            || now >= reference.expires_at.saturating_add(24 * 60 * 60 * 1000)
        {
            return Err(AppError::BadRequest("OAuth login recovery expired".into()));
        }
        self.claim_oauth_login_poll_impl(reference, now, interval, true, 90_000)
            .await
    }
}
