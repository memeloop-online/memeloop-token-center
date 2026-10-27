use super::*;
mod members;
pub(crate) use members::{load_group_members, normalize_legacy_members, store_group_members};

pub(crate) const PROXY_AAD: &[u8] = b"mtc/network-proxy/v1";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProxyProbe {
    pub enabled: bool,
    pub target_url: String,
    pub interval_ms: i64,
    pub attempts: i64,
}

impl Default for ProxyProbe {
    fn default() -> Self {
        Self {
            enabled: true,
            target_url: "https://openai.com/".into(),
            interval_ms: 60_000,
            attempts: 2,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateProxy {
    pub tenant_external_id: String,
    pub name: String,
    pub proxy_url: String,
    #[serde(default)]
    pub probe: ProxyProbe,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateProxy {
    pub tenant_external_id: String,
    pub expected_version: i64,
    pub name: String,
    pub proxy_url: Option<String>,
    pub probe: Option<ProxyProbe>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteProxy {
    pub tenant_external_id: String,
    pub expected_version: i64,
}

fn invalid_proxy() -> AppError {
    AppError::BadRequest("invalid network proxy configuration".into())
}

fn validate_proxy_name(name: &str) -> Result<(), AppError> {
    if name.trim() != name
        || !(1..=64).contains(&name.chars().count())
        || name.chars().any(char::is_control)
    {
        return Err(invalid_proxy());
    }
    Ok(())
}

fn validate_probe(probe: &ProxyProbe) -> Result<(), AppError> {
    let target = url::Url::parse(&probe.target_url).map_err(|_| invalid_proxy())?;
    if probe.target_url.len() > 2048
        || !matches!(target.scheme(), "http" | "https")
        || target.host_str().is_none()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
        || !(10_000..=3_600_000).contains(&probe.interval_ms)
        || !(1..=8).contains(&probe.attempts)
    {
        return Err(invalid_proxy());
    }
    Ok(())
}

pub(crate) fn proxy_view(row: &AnyRow, external: &str, key: &[u8]) -> Result<Value, AppError> {
    let proxy: String = open_private_json(
        &row.try_get::<String, _>("proxy_ciphertext")?,
        key,
        PROXY_AAD,
    )?;
    let parsed = url::Url::parse(&proxy).map_err(|_| invalid_proxy())?;
    Ok(serde_json::json!({
        "id": row.try_get::<String, _>("id")?, "tenant_external_id": external,
        "name": row.try_get::<String, _>("name")?, "version": row.try_get::<i64, _>("version")?,
        "address": {"scheme": parsed.scheme(), "host": parsed.host_str(),
            "port": parsed.port_or_known_default().unwrap_or(1080), "remote_dns": parsed.scheme() == "socks5h",
            "has_auth": !parsed.username().is_empty() || parsed.password().is_some()},
        "probe": {"enabled": row.try_get::<i64, _>("probe_enabled")? != 0,
            "target_url": row.try_get::<String, _>("probe_target")?,
            "interval_ms": row.try_get::<i64, _>("probe_interval_ms")?, "attempts": row.try_get::<i64, _>("probe_attempts")?},
        "active_health": {"status": row.try_get::<Option<String>, _>("last_probe_status")?.unwrap_or_else(|| "unknown".into()),
            "checked_at": row.try_get::<Option<i64>, _>("last_probe_checked_at")?,
            "latency_ms": row.try_get::<Option<i64>, _>("last_probe_latency_ms")?, "scope": "process"},
        "created_at": row.try_get::<i64, _>("created_at")?, "updated_at": row.try_get::<i64, _>("updated_at")?
    }))
}

async fn proxy_lock(tx: &mut Transaction<'_, Any>) -> Result<(), AppError> {
    sqlx::query("UPDATE transport_proxy_management_lock SET revision = revision + 1 WHERE id = 1")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Database {
    pub(crate) async fn list_network_proxies(
        &self,
        external: &str,
        key: &[u8],
    ) -> Result<Value, AppError> {
        normalize_legacy_members(&self.pool, key).await?;
        let rows = sqlx::query("SELECT p.* FROM network_proxies p JOIN tenants t ON t.id = p.tenant_id WHERE t.external_id = $1 ORDER BY p.name, p.id")
            .bind(external).fetch_all(&self.pool).await?;
        let items = rows
            .iter()
            .map(|row| proxy_view(row, external, key))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::json!({"items": items}))
    }

    pub(crate) async fn get_network_proxy(
        &self,
        id: Uuid,
        external: &str,
        key: &[u8],
    ) -> Result<Value, AppError> {
        normalize_legacy_members(&self.pool, key).await?;
        let row = sqlx::query("SELECT p.* FROM network_proxies p JOIN tenants t ON t.id = p.tenant_id WHERE p.id = $1 AND t.external_id = $2")
            .bind(id.to_string()).bind(external).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        let mut result = proxy_view(&row, external, key)?;
        let groups: Vec<String> = sqlx::query_scalar("SELECT group_id FROM network_proxy_group_members WHERE proxy_id = $1 ORDER BY group_id")
            .bind(id.to_string()).fetch_all(&self.pool).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT b.account_id) FROM transport_proxy_bindings b JOIN network_proxy_group_members m ON m.group_id = b.group_id WHERE m.proxy_id = $1")
            .bind(id.to_string()).fetch_one(&self.pool).await?;
        result["group_ids"] = serde_json::json!(groups);
        result["bound_account_count"] = serde_json::json!(count);
        Ok(result)
    }

    pub(crate) async fn create_network_proxy(
        &self,
        body: CreateProxy,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        normalize_legacy_members(&self.pool, key).await?;
        validate_proxy_name(&body.name)?;
        validate_probe(&body.probe)?;
        crate::network::validate_managed_codex_proxy(&body.proxy_url).await?;
        let ciphertext = seal_private_json(&body.proxy_url, key, PROXY_AAD)?;
        let mut tx = self.begin_write_transaction().await?;
        proxy_lock(&mut tx).await?;
        let tenant: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&body.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM network_proxies WHERE tenant_id = $1 AND name = $2",
        )
        .bind(&tenant)
        .bind(&body.name)
        .fetch_one(&mut *tx)
        .await?;
        if count != 0 {
            return Err(AppError::ProxyGroupConflict("network_proxy_name_conflict"));
        }
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO network_proxies (id, tenant_id, name, version, proxy_ciphertext, probe_enabled, probe_target, probe_interval_ms, probe_attempts, created_at, updated_at) VALUES ($1,$2,$3,1,$4,$5,$6,$7,$8,$9,$9)")
            .bind(id.to_string()).bind(&tenant).bind(&body.name).bind(ciphertext)
            .bind(i64::from(body.probe.enabled)).bind(&body.probe.target_url).bind(body.probe.interval_ms).bind(body.probe.attempts)
            .bind(unix_millis()).execute(&mut *tx).await?;
        proxy_audit(&mut tx, &tenant, id, "proxy_create", 1, actor).await?;
        tx.commit().await?;
        self.get_network_proxy(id, &body.tenant_external_id, key)
            .await
    }

    pub(crate) async fn update_network_proxy(
        &self,
        id: Uuid,
        body: UpdateProxy,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        normalize_legacy_members(&self.pool, key).await?;
        validate_proxy_name(&body.name)?;
        if let Some(probe) = &body.probe {
            validate_probe(probe)?;
        }
        if let Some(proxy) = &body.proxy_url {
            crate::network::validate_managed_codex_proxy(proxy).await?;
        }
        let mut tx = self.begin_write_transaction().await?;
        proxy_lock(&mut tx).await?;
        let row = sqlx::query("SELECT p.* FROM network_proxies p JOIN tenants t ON t.id = p.tenant_id WHERE p.id = $1 AND t.external_id = $2")
            .bind(id.to_string()).bind(&body.tenant_external_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let version: i64 = row.try_get("version")?;
        if version != body.expected_version {
            return Err(AppError::ProxyGroupConflict(
                "network_proxy_version_conflict",
            ));
        }
        let tenant: String = row.try_get("tenant_id")?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM network_proxies WHERE tenant_id = $1 AND name = $2 AND id <> $3",
        )
        .bind(&tenant)
        .bind(&body.name)
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if count != 0 {
            return Err(AppError::ProxyGroupConflict("network_proxy_name_conflict"));
        }
        let probe = body.probe.unwrap_or(ProxyProbe {
            enabled: row.try_get::<i64, _>("probe_enabled")? != 0,
            target_url: row.try_get("probe_target")?,
            interval_ms: row.try_get("probe_interval_ms")?,
            attempts: row.try_get("probe_attempts")?,
        });
        let old_proxy: String = open_private_json(
            &row.try_get::<String, _>("proxy_ciphertext")?,
            key,
            PROXY_AAD,
        )?;
        let old_name: String = row.try_get("name")?;
        let ciphertext = body
            .proxy_url
            .as_ref()
            .map(|proxy| seal_private_json(proxy, key, PROXY_AAD))
            .transpose()?
            .unwrap_or(row.try_get("proxy_ciphertext")?);
        let version = version
            .checked_add(1)
            .ok_or_else(|| AppError::internal("network_proxy_version"))?;
        sqlx::query("UPDATE network_proxies SET name = $1, version = $2, proxy_ciphertext = $3, probe_enabled = $4, probe_target = $5, probe_interval_ms = $6, probe_attempts = $7, updated_at = $8, last_probe_status = NULL, last_probe_checked_at = NULL, last_probe_latency_ms = NULL WHERE id = $9")
            .bind(&body.name).bind(version).bind(ciphertext).bind(i64::from(probe.enabled)).bind(probe.target_url)
            .bind(probe.interval_ms).bind(probe.attempts).bind(unix_millis()).bind(id.to_string()).execute(&mut *tx).await?;
        self.refresh_network_proxy_references(&mut tx, id, &old_proxy, &old_name, None, key)
            .await?;
        proxy_audit(&mut tx, &tenant, id, "proxy_update", version, actor).await?;
        tx.commit().await?;
        self.get_network_proxy(id, &body.tenant_external_id, key)
            .await
    }

    pub(crate) async fn delete_network_proxy(
        &self,
        id: Uuid,
        body: DeleteProxy,
        actor: Option<Uuid>,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        proxy_lock(&mut tx).await?;
        let row = sqlx::query("SELECT p.* FROM network_proxies p JOIN tenants t ON t.id = p.tenant_id WHERE p.id = $1 AND t.external_id = $2")
            .bind(id.to_string()).bind(&body.tenant_external_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let version: i64 = row.try_get("version")?;
        if version != body.expected_version {
            return Err(AppError::ProxyGroupConflict(
                "network_proxy_version_conflict",
            ));
        }
        let references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM network_proxy_group_members WHERE proxy_id = $1",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if references != 0 {
            return Err(AppError::ProxyGroupConflict("network_proxy_in_use"));
        }
        sqlx::query("DELETE FROM network_proxies WHERE id = $1")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        proxy_audit(
            &mut tx,
            &row.try_get::<String, _>("tenant_id")?,
            id,
            "proxy_delete",
            version,
            actor,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn proxy_audit(
    tx: &mut Transaction<'_, Any>,
    tenant: &str,
    id: Uuid,
    action: &str,
    version: i64,
    actor: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO transport_proxy_management_audit (id, tenant_id, resource_id, action, version, actor_service_id, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(Uuid::now_v7().to_string()).bind(tenant).bind(id.to_string()).bind(action).bind(version)
        .bind(actor.map(|actor| actor.to_string())).bind(unix_millis()).execute(&mut **tx).await?;
    Ok(())
}
