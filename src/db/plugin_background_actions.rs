use super::*;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BackgroundActionGrant {
    pub plugin_id: String,
    pub tenant_id: String,
    pub tenant_external_id: String,
    pub upstream_account_id: String,
    pub enabled: bool,
    pub revision: i64,
    #[serde(skip)]
    pub actor_service_id: String,
    #[serde(skip)]
    pub actor_generation: i64,
    #[serde(skip)]
    pub bootstrap_authority_hash: String,
    pub last_attempt_at: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_result: Option<String>,
    pub last_operation_id: Option<String>,
}

fn grant(row: AnyRow) -> Result<BackgroundActionGrant, AppError> {
    Ok(BackgroundActionGrant {
        plugin_id: row.try_get("plugin_id")?,
        tenant_id: row.try_get("tenant_id")?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        upstream_account_id: row.try_get("upstream_account_id")?,
        enabled: row.try_get::<i64, _>("enabled")? == 1,
        revision: row.try_get("revision")?,
        actor_service_id: row.try_get("actor_service_id")?,
        actor_generation: row.try_get("actor_generation")?,
        bootstrap_authority_hash: row.try_get("bootstrap_authority_hash")?,
        last_attempt_at: row.try_get("last_attempt_at")?,
        last_error_code: row.try_get("last_error_code")?,
        last_result: row.try_get("last_result")?,
        last_operation_id: row.try_get("last_operation_id")?,
    })
}

impl Database {
    pub(crate) async fn authorize_background_action_tick(
        &self,
        grant: &BackgroundActionGrant,
        owner: &str,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        require_grant(&mut tx, self.backend, grant, owner, i64::MAX, unix_millis()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Recover a pre-dispatch crash without creating a new business operation.
    /// Only a demonstrably unconfirmed prepare can be refreshed. Submitted and
    /// ambiguous operations retain their original irreversible dispatch claim.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn refresh_background_quota_reset_prepare(
        &self,
        account: &UpstreamAccountView,
        id: &str,
        available: i64,
        applicable: i64,
        observed_at: i64,
        guard: &crate::upstream_quota::reset::BackgroundResetGuard,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let now = unix_millis();
        require_grant(
            &mut tx,
            self.backend,
            &guard.grant,
            &guard.lease_owner,
            guard.deadline_ms,
            now,
        )
        .await?;
        sqlx::query("UPDATE upstream_quota_reset_operations SET state='prepared',available_credits=$1,applicable_credits=$2,observed_at=$3,credential_generation=$4,transport_updated_at=$5,expires_at=$6,updated_at=$7 WHERE id=$8 AND tenant_id=$9 AND upstream_account_id=$10 AND actor_service_id=$11 AND state IN ('prepared','expired') AND confirmed_at IS NULL AND confirm_idempotency_hash IS NULL AND NOT EXISTS (SELECT 1 FROM upstream_quota_reset_audit audit WHERE audit.operation_id=$8 AND audit.event='confirmation_claimed') AND NOT EXISTS (SELECT 1 FROM upstream_quota_reset_operations other WHERE other.upstream_account_id=$10 AND other.id<>$8 AND other.state IN ('prepared','submitted','accepted','unknown') AND other.settled_at IS NULL)")
            .bind(available).bind(applicable).bind(observed_at).bind(account.credential_generation).bind(account.updated_at)
            .bind(now.saturating_add(120_000).min(guard.deadline_ms)).bind(now).bind(id).bind(&guard.grant.tenant_id).bind(&guard.grant.upstream_account_id).bind(format!("plugin:{}",guard.grant.plugin_id))
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn background_action_grant(
        &self,
        plugin: &str,
        tenant: &str,
        account: &str,
    ) -> Result<Option<BackgroundActionGrant>, AppError> {
        sqlx::query("SELECT g.*, t.external_id AS tenant_external_id FROM plugin_background_action_grants g JOIN tenants t ON t.id=g.tenant_id WHERE g.plugin_id=$1 AND t.external_id=$2 AND g.upstream_account_id=$3")
            .bind(plugin).bind(tenant).bind(account)
            .fetch_optional(&self.pool).await?.map(grant).transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn put_background_action_grant(
        &self,
        plugin: &str,
        tenant: &str,
        account: Uuid,
        enabled: bool,
        expected_revision: i64,
        actor: &AuthenticatedService,
        bootstrap_hash: &str,
    ) -> Result<BackgroundActionGrant, AppError> {
        if expected_revision < 0
            || !actor.allows("providers:write")
            || !actor.allows("plugins:write")
            || actor
                .tenant_external_id
                .as_deref()
                .is_some_and(|scope| scope != tenant)
        {
            return Err(AppError::Forbidden);
        }
        let account_id = account.to_string();
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id: String = sqlx::query_scalar("SELECT a.tenant_id FROM upstream_accounts a JOIN tenants t ON t.id=a.tenant_id WHERE a.id=$1 AND t.external_id=$2 AND ($3=0 OR (t.status='active' AND a.driver='openai-codex' AND a.status='active'))")
            .bind(&account_id).bind(tenant).bind(i64::from(enabled)).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let actor_id = actor
            .service_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "bootstrap".into());
        let generation = actor.credential_generation.unwrap_or(0);
        sqlx::query("INSERT INTO plugin_background_action_grants (plugin_id,tenant_id,upstream_account_id,actor_service_id,actor_generation,updated_at) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(plugin).bind(&tenant_id).bind(&account_id).bind(&actor_id).bind(generation).bind(unix_millis()).execute(&mut *tx).await?;
        let changed = sqlx::query("UPDATE plugin_background_action_grants SET enabled=$1,revision=revision+1,actor_service_id=$2,actor_generation=$3,bootstrap_authority_hash=$4,next_attempt_at=0,lease_owner=NULL,lease_until=0,updated_at=$5 WHERE plugin_id=$6 AND tenant_id=$7 AND upstream_account_id=$8 AND revision=$9")
            .bind(i64::from(enabled)).bind(&actor_id).bind(generation).bind(bootstrap_hash).bind(unix_millis())
            .bind(plugin).bind(&tenant_id).bind(&account_id).bind(expected_revision).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "background action authorization revision changed".into(),
            ));
        }
        require_actor(&mut tx, self.backend, &actor_id, generation, tenant).await?;
        tx.commit().await?;
        self.background_action_grant(plugin, tenant, &account_id)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub(crate) async fn due_background_action_grants(
        &self,
        plugin: &str,
        now: i64,
    ) -> Result<Vec<BackgroundActionGrant>, AppError> {
        sqlx::query("SELECT g.*,t.external_id AS tenant_external_id FROM plugin_background_action_grants g JOIN tenants t ON t.id=g.tenant_id JOIN upstream_accounts a ON a.id=g.upstream_account_id AND a.tenant_id=g.tenant_id WHERE g.plugin_id=$1 AND g.enabled=1 AND g.next_attempt_at<=$2 AND g.lease_until<=$2 AND t.status='active' AND a.status='active' AND a.driver='openai-codex' ORDER BY g.next_attempt_at,g.upstream_account_id LIMIT 100")
            .bind(plugin).bind(now).fetch_all(&self.pool).await?.into_iter().map(grant).collect()
    }

    pub(crate) async fn claim_background_action_tick(
        &self,
        grant: &BackgroundActionGrant,
        owner: &str,
        now: i64,
        interval_ms: i64,
    ) -> Result<bool, AppError> {
        let changed=sqlx::query("UPDATE plugin_background_action_grants SET lease_owner=$1,lease_until=$2,next_attempt_at=$3,last_attempt_at=$4 WHERE plugin_id=$5 AND tenant_id=$6 AND upstream_account_id=$7 AND revision=$8 AND enabled=1 AND lease_until<=$4 AND next_attempt_at<=$4")
            .bind(owner).bind(now.saturating_add(120_000)).bind(now.saturating_add(interval_ms)).bind(now)
            .bind(&grant.plugin_id).bind(&grant.tenant_id).bind(&grant.upstream_account_id).bind(grant.revision).execute(&self.pool).await?;
        Ok(changed.rows_affected() == 1)
    }

    pub(crate) async fn finish_background_action_tick(
        &self,
        grant: &BackgroundActionGrant,
        owner: &str,
        error: Option<&str>,
        result: Option<&str>,
        operation: Option<&str>,
    ) -> Result<(), AppError> {
        sqlx::query("UPDATE plugin_background_action_grants SET lease_owner=NULL,lease_until=0,last_error_code=$1,last_result=$7,last_operation_id=COALESCE($8,last_operation_id) WHERE plugin_id=$2 AND tenant_id=$3 AND upstream_account_id=$4 AND revision=$5 AND lease_owner=$6")
            .bind(error).bind(&grant.plugin_id).bind(&grant.tenant_id).bind(&grant.upstream_account_id).bind(grant.revision).bind(owner).bind(result).bind(operation).execute(&self.pool).await?;
        Ok(())
    }
}

/// Serialize dispatch with runtime publication, including insertion of the first
/// head. A row lock alone cannot fence an absent head. The short SHARE table lock
/// conflicts with publication's INSERT/UPDATE and is held only through the local
/// claim transaction; no supplier or component execution occurs under it.
pub(super) async fn require_runtime(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    expected: Option<i64>,
) -> Result<(), AppError> {
    if matches!(backend, DatabaseBackend::PostgreSql) {
        sqlx::query("LOCK TABLE application_plugin_head IN SHARE MODE")
            .execute(&mut **tx)
            .await?;
    }
    let current: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM application_plugin_head WHERE scope='global'")
            .fetch_optional(&mut **tx)
            .await?;
    if current != expected {
        return Err(AppError::Conflict(
            "plugin runtime changed before background dispatch".into(),
        ));
    }
    Ok(())
}

/// Called inside the reset dispatch transaction. Lock the grant and actor so
/// revocation/rotation either precedes the claim or follows its committed fence.
pub(super) async fn require_grant(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    expected: &BackgroundActionGrant,
    owner: &str,
    deadline: i64,
    now: i64,
) -> Result<(), AppError> {
    let query = if matches!(backend, DatabaseBackend::PostgreSql) {
        "SELECT revision FROM plugin_background_action_grants WHERE plugin_id=$1 AND tenant_id=$2 AND upstream_account_id=$3 AND revision=$4 AND enabled=1 AND lease_owner=$5 AND lease_until>$6 FOR UPDATE"
    } else {
        "SELECT revision FROM plugin_background_action_grants WHERE plugin_id=$1 AND tenant_id=$2 AND upstream_account_id=$3 AND revision=$4 AND enabled=1 AND lease_owner=$5 AND lease_until>$6"
    };
    let revision = sqlx::query_scalar::<_, i64>(query)
        .bind(&expected.plugin_id)
        .bind(&expected.tenant_id)
        .bind(&expected.upstream_account_id)
        .bind(expected.revision)
        .bind(owner)
        .bind(now)
        .fetch_optional(&mut **tx)
        .await?;
    if revision.is_none() || now >= deadline {
        return Err(AppError::Conflict(
            "background action authorization or deadline changed".into(),
        ));
    }
    require_actor(
        tx,
        backend,
        &expected.actor_service_id,
        expected.actor_generation,
        &expected.tenant_external_id,
    )
    .await
}

async fn require_actor(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    actor: &str,
    generation: i64,
    tenant: &str,
) -> Result<(), AppError> {
    if actor == "bootstrap" {
        return Ok(());
    }
    let query = if matches!(backend, DatabaseBackend::PostgreSql) {
        "SELECT c.scopes_json,c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id=p.id AND c.generation=p.credential_generation AND c.revoked_at IS NULL WHERE p.id=$1 AND p.status='active' AND p.credential_generation=$2 FOR UPDATE OF p,c"
    } else {
        "SELECT c.scopes_json,c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id=p.id AND c.generation=p.credential_generation AND c.revoked_at IS NULL WHERE p.id=$1 AND p.status='active' AND p.credential_generation=$2"
    };
    let row = sqlx::query(query)
        .bind(actor)
        .bind(generation)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::Forbidden)?;
    let scopes: Vec<String> = serde_json::from_str(&row.try_get::<String, _>("scopes_json")?)
        .map_err(|_| AppError::Forbidden)?;
    let scope: Option<String> = row.try_get("tenant_external_id")?;
    if !scopes.iter().any(|scope| scope == "providers:write")
        || !scopes.iter().any(|scope| scope == "plugins:write")
        || scope.as_deref().is_some_and(|scope| scope != tenant)
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}
