use super::*;
use crate::db::transport_proxy_groups::strategy::AutoConfig;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateProxySelection {
    pub tenant_external_id: String,
    pub expected_selection_version: i64,
    pub expected_binding_version: i64,
    pub expected_credential_generation: i64,
    pub expected_updated_at: i64,
    pub expected_group_version: Option<i64>,
    pub mode: String,
    pub proxy_id: Option<Uuid>,
    pub group_id: Option<Uuid>,
    pub plugin_id: Option<String>,
    pub plugin_version: Option<String>,
    #[serde(default = "empty_config")]
    pub config: Value,
}

fn empty_config() -> Value {
    serde_json::json!({})
}

impl UpdateProxySelection {
    pub(crate) fn validate(&self, plugins: &crate::plugin::PluginRuntime) -> Result<(), AppError> {
        if !self.config.is_object()
            || serde_json::to_string(&self.config)
                .map_err(|_| invalid())?
                .len()
                > 16384
        {
            return Err(invalid());
        }
        match self.mode.as_str() {
            "manual"
                if self.proxy_id.is_some()
                    && self.group_id.is_none()
                    && self.plugin_id.is_none()
                    && self.plugin_version.is_none()
                    && self.config == empty_config() =>
            {
                Ok(())
            }
            "auto"
                if self.proxy_id.is_none()
                    && self.group_id.is_some()
                    && self.expected_group_version.is_some()
                    && self.plugin_id.is_none()
                    && self.plugin_version.is_none() =>
            {
                serde_json::from_value::<AutoConfig>(self.config.clone())
                    .map_err(|_| invalid())?
                    .validate()
            }
            "plugin"
                if self.proxy_id.is_none()
                    && self.group_id.is_some()
                    && self.expected_group_version.is_some() =>
            {
                plugins.validate_network_proxy_configuration(
                    self.plugin_id.as_deref().ok_or_else(invalid)?,
                    self.plugin_version.as_deref().ok_or_else(invalid)?,
                    &self.config,
                )
            }
            _ => Err(invalid()),
        }
    }
}

impl Database {
    pub(crate) async fn get_network_proxy_selection(
        &self,
        account: Uuid,
        external: &str,
    ) -> Result<Value, AppError> {
        let row = sqlx::query("SELECT a.id, a.credential_generation, a.updated_at AS account_updated_at, COALESCE(b.version, 0) AS binding_version, p.version, p.mode, p.proxy_id, p.group_id, p.plugin_id, p.plugin_version, p.config_json FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN transport_proxy_bindings b ON b.account_id = a.id LEFT JOIN network_proxy_selection_policies p ON p.account_id = a.id WHERE a.id = $1 AND t.external_id = $2")
            .bind(account.to_string()).bind(external).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        Ok(
            serde_json::json!({"account_id": account, "tenant_external_id": external,
            "selection_version": row.try_get::<Option<i64>, _>("version")?.unwrap_or(0),
            "binding_version": row.try_get::<i64, _>("binding_version")?,
            "credential_generation": row.try_get::<i64, _>("credential_generation")?,
            "account_updated_at": row.try_get::<i64, _>("account_updated_at")?,
            "desired": {"mode": row.try_get::<Option<String>, _>("mode")?.unwrap_or_else(|| "legacy".into()),
                "proxy_id": row.try_get::<Option<String>, _>("proxy_id")?, "group_id": row.try_get::<Option<String>, _>("group_id")?,
                "plugin_id": row.try_get::<Option<String>, _>("plugin_id")?, "plugin_version": row.try_get::<Option<String>, _>("plugin_version")?,
                "config": serde_json::from_str::<Value>(&row.try_get::<Option<String>, _>("config_json")?.unwrap_or_else(|| "{}".into())).map_err(|_| invalid())?}}),
        )
    }

    pub(crate) async fn update_network_proxy_selection(
        &self,
        account: Uuid,
        body: UpdateProxySelection,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        normalize_legacy_members(&self.pool, key).await?;
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, &body.tenant_external_id).await?;
        let current = sqlx::query("SELECT a.credential_generation, a.updated_at, COALESCE(b.version, 0) AS binding_version, b.group_id, COALESCE(p.version, 0) AS policy_version FROM upstream_accounts a LEFT JOIN transport_proxy_bindings b ON b.account_id = a.id LEFT JOIN network_proxy_selection_policies p ON p.account_id = a.id WHERE a.id = $1 AND a.tenant_id = $2")
            .bind(account.to_string()).bind(&tenant_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        if current.try_get::<i64, _>("policy_version")? != body.expected_selection_version
            || current.try_get::<i64, _>("binding_version")? != body.expected_binding_version
            || current.try_get::<i64, _>("credential_generation")?
                != body.expected_credential_generation
            || current.try_get::<i64, _>("updated_at")? != body.expected_updated_at
        {
            return Err(conflict("network_proxy_selection_version_conflict"));
        }
        let previous = if let Some(group) = current.try_get::<Option<String>, _>("group_id")? {
            load_group_members(&mut *tx, &group, key).await?
        } else {
            Vec::new()
        };
        let (next, group_version) = if let Some(group) = body.group_id {
            let version: i64 = sqlx::query_scalar(
                "SELECT version FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2",
            )
            .bind(group.to_string())
            .bind(&tenant_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
            if Some(version) != body.expected_group_version {
                return Err(conflict("proxy_group_version_conflict"));
            }
            (
                load_group_members(&mut *tx, &group.to_string(), key).await?,
                Some(version),
            )
        } else {
            let row = sqlx::query("SELECT id, name, version, proxy_url FROM network_proxies WHERE id = $1 AND tenant_id = $2")
                .bind(body.proxy_id.ok_or_else(invalid)?.to_string()).bind(&tenant_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
            (
                vec![Member {
                    id: parse_uuid(row.try_get("id")?)?,
                    label: row.try_get("name")?,
                    version: row.try_get("version")?,
                    proxy_url: row.try_get::<String, _>("proxy_url")?,
                }],
                None,
            )
        };
        let initial = previous
            .iter()
            .find_map(|old| next.iter().find(|candidate| candidate.id == old.id))
            .or_else(|| next.first())
            .ok_or_else(invalid)?;
        let binding_version = body
            .expected_binding_version
            .checked_add(1)
            .ok_or_else(invalid)?;
        let stamp = BindingStamp {
            selection_version: revision(&mut tx).await?,
            binding_version,
            group_id: body.group_id,
            group_version,
        };
        self.write_transport_account(
            &mut tx,
            account,
            &tenant_id,
            &stamp,
            &initial.proxy_url,
            Some((
                body.expected_credential_generation,
                body.expected_updated_at,
            )),
            key,
        )
        .await?;
        selection::rebase_selection(&mut tx, account, &previous, &next, initial.id, None, key)
            .await?;
        sqlx::query("INSERT INTO transport_proxy_bindings (account_id, group_id, version, initial_member_id, replacement_member_id, updated_at) VALUES ($1,$2,$3,$4,NULL,$5) ON CONFLICT (account_id) DO UPDATE SET group_id = excluded.group_id, version = excluded.version, initial_member_id = excluded.initial_member_id, replacement_member_id = NULL, updated_at = excluded.updated_at")
            .bind(account.to_string()).bind(body.group_id.map(|id| id.to_string())).bind(binding_version).bind(initial.id.to_string()).bind(unix_millis()).execute(&mut *tx).await?;
        let version = body
            .expected_selection_version
            .checked_add(1)
            .ok_or_else(invalid)?;
        sqlx::query("INSERT INTO network_proxy_selection_policies (account_id, tenant_id, version, mode, proxy_id, group_id, plugin_id, plugin_version, config_json, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (account_id) DO UPDATE SET version = excluded.version, mode = excluded.mode, proxy_id = excluded.proxy_id, group_id = excluded.group_id, plugin_id = excluded.plugin_id, plugin_version = excluded.plugin_version, config_json = excluded.config_json, updated_at = excluded.updated_at")
            .bind(account.to_string()).bind(&tenant_id).bind(version).bind(&body.mode).bind(body.proxy_id.map(|id| id.to_string()))
            .bind(body.group_id.map(|id| id.to_string())).bind(body.plugin_id).bind(body.plugin_version).bind(body.config.to_string()).bind(unix_millis()).execute(&mut *tx).await?;
        budget(&mut tx, key).await?;
        audit(
            &mut tx,
            &tenant_id,
            account,
            "selection_update",
            version,
            actor,
        )
        .await?;
        tx.commit().await?;
        self.get_network_proxy_selection(account, &body.tenant_external_id)
            .await
    }
}
