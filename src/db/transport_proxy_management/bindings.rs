use super::*;

impl Database {
    pub(crate) async fn get_transport_binding(
        &self,
        id: Uuid,
        external: &str,
    ) -> Result<Value, AppError> {
        let row = sqlx::query(BINDING_SELECT)
            .bind(id.to_string())
            .bind(external)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        binding_view(&row, external)
    }

    pub(crate) async fn bind_transport_group(
        &self,
        id: Uuid,
        body: BindGroup,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        self.change_transport_binding(
            id,
            &body.tenant_external_id,
            Some(body.group_id),
            body.initial_member_id,
            body.expected_group_version,
            body.expected_binding_version,
            body.expected_credential_generation,
            body.expected_updated_at,
            actor,
            key,
        )
        .await
    }

    pub(crate) async fn unbind_transport_group(
        &self,
        id: Uuid,
        body: UnbindGroup,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        self.change_transport_binding(
            id,
            &body.tenant_external_id,
            None,
            body.single_proxy_member_id,
            body.expected_group_version,
            body.expected_binding_version,
            body.expected_credential_generation,
            body.expected_updated_at,
            actor,
            key,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn change_transport_binding(
        &self,
        id: Uuid,
        external: &str,
        target: Option<Uuid>,
        member: Uuid,
        group_version: i64,
        expected_binding: i64,
        generation: i64,
        updated_at: i64,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, external).await?;
        let current = sqlx::query(BINDING_SELECT)
            .bind(id.to_string())
            .bind(external)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let version: i64 = current.try_get("binding_version")?;
        if version != expected_binding {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        let current_group: Option<String> = current.try_get("group_id")?;
        let group_id = target
            .map(|id| id.to_string())
            .or_else(|| current_group.clone())
            .ok_or_else(invalid)?;
        let row =
            sqlx::query("SELECT * FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2")
                .bind(&group_id)
                .bind(&tenant_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
        if row.try_get::<i64, _>("version")? != group_version {
            return Err(conflict("proxy_group_version_conflict"));
        }
        let members = open_members(&row, key)?;
        let requested = members
            .iter()
            .find(|candidate| candidate.id == member)
            .ok_or_else(invalid)?;
        let initial: Option<String> = current.try_get("initial_member_id")?;
        let selected = if target.is_some() && current_group.as_deref() == Some(group_id.as_str()) {
            members
                .iter()
                .find(|candidate| Some(candidate.id.to_string()) == initial)
                .ok_or_else(invalid)?
        } else {
            requested
        };
        let version = version.checked_add(1).ok_or(AppError::Internal)?;
        let stamp = BindingStamp {
            selection_version: revision(&mut tx).await?,
            binding_version: version,
            group_id: target,
            group_version: target.map(|_| group_version),
        };
        self.write_transport_account(
            &mut tx,
            id,
            &tenant_id,
            &stamp,
            &selected.proxy_url,
            Some((generation, updated_at)),
            key,
        )
        .await?;
        if target.is_some() {
            let previous = if current_group.as_deref() == Some(group_id.as_str()) {
                members.as_slice()
            } else {
                &[]
            };
            selection::rebase_selection(&mut tx, id, previous, &members, selected.id, None, key)
                .await?;
        }
        sqlx::query("INSERT INTO transport_proxy_bindings (account_id, group_id, version, initial_member_id, replacement_member_id, updated_at) VALUES ($1, $2, $3, $4, NULL, $5) ON CONFLICT (account_id) DO UPDATE SET group_id = excluded.group_id, version = excluded.version, initial_member_id = excluded.initial_member_id, replacement_member_id = NULL, updated_at = excluded.updated_at")
            .bind(id.to_string()).bind(target.map(|id| id.to_string())).bind(version)
            .bind(target.map(|_| selected.id.to_string())).bind(unix_millis()).execute(&mut *tx).await?;
        budget(&mut tx, key).await?;
        audit(
            &mut tx,
            &tenant_id,
            id,
            if target.is_some() { "bind" } else { "unbind" },
            version,
            actor,
        )
        .await?;
        let row = sqlx::query(BINDING_SELECT)
            .bind(id.to_string())
            .bind(external)
            .fetch_one(&mut *tx)
            .await?;
        let result = binding_view(&row, external)?;
        tx.commit().await?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn write_transport_account(
        &self,
        tx: &mut Transaction<'_, Any>,
        id: Uuid,
        tenant_id: &str,
        stamp: &BindingStamp,
        proxy: &str,
        expected: Option<(i64, i64)>,
        key: &[u8],
    ) -> Result<(), AppError> {
        let query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND a.tenant_id = $2 FOR UPDATE OF a, c"
            }
            DatabaseBackend::Sqlite => {
                "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 AND a.tenant_id = $2"
            }
        };
        let row = sqlx::query(query)
            .bind(id.to_string())
            .bind(tenant_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
        if row.try_get::<String, _>("driver")? != crate::oauth::codex_device::PROVIDER_DRIVER
            || row.try_get::<String, _>("auth_kind")? != "oauth"
        {
            return Err(invalid());
        }
        let generation: i64 = row.try_get("credential_generation")?;
        let updated_at: i64 = row.try_get("updated_at")?;
        if expected.is_some_and(|expected| expected != (generation, updated_at)) {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        let next_generation = generation
            .checked_add(1)
            .filter(|generation| *generation <= i64::from(u32::MAX))
            .ok_or(AppError::Internal)?;
        let now = unix_millis().max(updated_at.saturating_add(1));
        let refresh_query = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT request_started_at, pending_credential_ciphertext, lease_expires_at FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND credential_generation = $2 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT request_started_at, pending_credential_ciphertext, lease_expires_at FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND credential_generation = $2"
            }
        };
        let refresh = sqlx::query(refresh_query)
            .bind(id.to_string())
            .bind(generation)
            .fetch_optional(&mut **tx)
            .await?;
        if let Some(refresh) = refresh {
            let dispatched = refresh
                .try_get::<Option<i64>, _>("request_started_at")?
                .is_some();
            if refresh
                .try_get::<Option<String>, _>("pending_credential_ciphertext")?
                .is_some()
                || (dispatched && refresh.try_get::<i64, _>("lease_expires_at")? > now)
            {
                return Err(conflict("proxy_group_binding_conflict"));
            }
            if dispatched {
                sqlx::query("UPDATE upstream_oauth_refresh_leases SET credential_generation = $1, idempotency_key = $2 WHERE account_id = $3 AND credential_generation = $4")
                    .bind(next_generation).bind(format!("transport-proxy-fence-{}", Uuid::now_v7())).bind(id.to_string()).bind(generation).execute(&mut **tx).await?;
            }
        }
        let credential = open_credential(&row.try_get::<String, _>("credential_ciphertext")?, key)?
            .with_transport_proxy(proxy.to_owned())?;
        let ciphertext = seal_credential(&credential, key)?;
        let mut config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
            .map_err(|_| AppError::Internal)?;
        config.as_object_mut().ok_or(AppError::Internal)?.insert(
            CONFIG_KEY.into(),
            serde_json::to_value(stamp).map_err(|_| AppError::Internal)?,
        );
        sqlx::query("UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL")
            .bind(now).bind(id.to_string()).bind(generation).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(Uuid::now_v7().to_string()).bind(id.to_string()).bind(next_generation).bind(ciphertext).bind(credential.expires_at()).bind(now).execute(&mut **tx).await?;
        let changed = sqlx::query("UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2, config_json = $3 WHERE id = $4 AND credential_generation = $5 AND updated_at = $6")
            .bind(next_generation).bind(now).bind(config.to_string()).bind(id.to_string()).bind(generation).bind(updated_at).execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        Ok(())
    }
}

const BINDING_SELECT: &str = "SELECT a.id, a.credential_generation, a.updated_at, COALESCE(b.version, 0) AS binding_version, b.group_id, g.version AS group_version, b.initial_member_id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN transport_proxy_bindings b ON b.account_id = a.id LEFT JOIN transport_proxy_groups g ON g.id = b.group_id WHERE a.id = $1 AND t.external_id = $2";

fn binding_view(row: &AnyRow, external: &str) -> Result<Value, AppError> {
    let group_id: Option<String> = row.try_get("group_id")?;
    Ok(serde_json::json!({
        "account_id":row.try_get::<String,_>("id")?,"tenant_external_id":external,
        "binding_version":row.try_get::<i64,_>("binding_version")?,"group_id":group_id,
        "group_version":row.try_get::<Option<i64>,_>("group_version")?,
        "initial_member_id":row.try_get::<Option<String>,_>("initial_member_id")?,
        "credential_generation":row.try_get::<i64,_>("credential_generation")?,"updated_at":row.try_get::<i64,_>("updated_at")?,
        "runtime":{"scope":"this_process","configuration_state":"pending","applied_binding_version":null,"applied_group_version":null,"selected_member_id":null,"observed_at":unix_millis()}
    }))
}
