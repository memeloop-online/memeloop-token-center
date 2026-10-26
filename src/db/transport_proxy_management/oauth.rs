use super::*;

impl Database {
    pub(crate) async fn snapshot_oauth_transport_group(
        &self,
        reference: OAuthGroupReference,
        external: &str,
        account_binding: Option<(Uuid, BindingStamp)>,
        key: &[u8],
    ) -> Result<OAuthGroupSnapshot, AppError> {
        self.snapshot_oauth_transport_group_selected(
            reference,
            external,
            account_binding,
            None,
            key,
        )
        .await
    }

    pub(crate) async fn snapshot_oauth_transport_group_selected(
        &self,
        reference: OAuthGroupReference,
        external: &str,
        account_binding: Option<(Uuid, BindingStamp)>,
        selected_proxy: Option<&str>,
        key: &[u8],
    ) -> Result<OAuthGroupSnapshot, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, external).await?;
        let row =
            sqlx::query("SELECT * FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2")
                .bind(reference.group_id.to_string())
                .bind(tenant_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
        if row.try_get::<i64, _>("version")? != reference.expected_group_version {
            return Err(conflict("proxy_group_version_conflict"));
        }
        let members = open_members(&row, key)?;
        let member = members
            .iter()
            .find(|member| member.id == reference.initial_member_id)
            .ok_or_else(invalid)?;
        let selected = match selected_proxy {
            Some(proxy) => members
                .iter()
                .find(|member| member.proxy_url == proxy)
                .ok_or_else(invalid)?,
            None => member,
        };
        let account_revision = if let Some((account_id, _)) = &account_binding {
            let row = sqlx::query("SELECT a.credential_generation, a.updated_at FROM upstream_accounts a WHERE a.id = $1 AND a.tenant_id = $2")
                .bind(account_id.to_string()).bind(tenant(&mut tx, external).await?)
                .fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
            Some((
                row.try_get("credential_generation")?,
                row.try_get("updated_at")?,
            ))
        } else {
            None
        };
        let snapshot = OAuthGroupSnapshot {
            reference,
            tenant_external_id: external.into(),
            proxy_url: member.proxy_url.clone(),
            selected_member_id: selected.id,
            selected_proxy_url: selected.proxy_url.clone(),
            account_binding,
            account_revision,
            session_id: None,
        };
        self.validate_oauth_transport_group_tx(&mut tx, &snapshot, key)
            .await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub(crate) async fn validate_oauth_transport_group(
        &self,
        snapshot: &OAuthGroupSnapshot,
        key: &[u8],
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        self.validate_oauth_transport_group_tx(&mut tx, snapshot, key)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn validate_oauth_transport_group_tx(
        &self,
        tx: &mut Transaction<'_, Any>,
        snapshot: &OAuthGroupSnapshot,
        key: &[u8],
    ) -> Result<Vec<Member>, AppError> {
        let tenant_id = tenant(tx, &snapshot.tenant_external_id).await?;
        let row =
            sqlx::query("SELECT * FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2")
                .bind(snapshot.reference.group_id.to_string())
                .bind(tenant_id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(AppError::NotFound)?;
        if row.try_get::<i64, _>("version")? != snapshot.reference.expected_group_version {
            return Err(conflict("proxy_group_version_conflict"));
        }
        let members = open_members(&row, key)?;
        if !members.iter().any(|member| {
            member.id == snapshot.selected_member_id
                && member.proxy_url == snapshot.selected_proxy_url
        }) {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        if !members.iter().any(|member| {
            member.id == snapshot.reference.initial_member_id
                && member.proxy_url == snapshot.proxy_url
        }) {
            return Err(conflict("proxy_group_binding_conflict"));
        }
        if let Some((account_id, stamp)) = &snapshot.account_binding {
            let row = sqlx::query("SELECT a.config_json, a.credential_generation, a.updated_at, a.oauth_session_id, b.version AS binding_version, b.group_id, g.version AS group_version FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id JOIN transport_proxy_bindings b ON b.account_id = a.id JOIN transport_proxy_groups g ON g.id = b.group_id WHERE a.id = $1 AND t.external_id = $2")
                .bind(account_id.to_string()).bind(&snapshot.tenant_external_id)
                .fetch_optional(&mut **tx).await?.ok_or(AppError::NotFound)?;
            let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
                .map_err(|_| AppError::internal("oauth_validate_oauth_transport_group_tx_1"))?;
            let installed_session = snapshot.session_id.is_some_and(|session| {
                row.try_get::<Option<String>, _>("oauth_session_id")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(session.to_string().as_str())
            });
            if config.get(CONFIG_KEY)
                != Some(&serde_json::to_value(stamp).map_err(|_| AppError::internal("oauth_validate_oauth_transport_group_tx_2"))?)
                || stamp.group_id != Some(snapshot.reference.group_id)
                || stamp.group_version != Some(snapshot.reference.expected_group_version)
                || (!installed_session
                    && snapshot.account_revision
                        != Some((
                            row.try_get("credential_generation")?,
                            row.try_get("updated_at")?,
                        )))
                || row.try_get::<i64, _>("binding_version")? != stamp.binding_version
                || row.try_get::<String, _>("group_id")? != snapshot.reference.group_id.to_string()
                || row.try_get::<i64, _>("group_version")?
                    != snapshot.reference.expected_group_version
            {
                return Err(conflict("proxy_group_binding_conflict"));
            }
        }
        Ok(members)
    }

    pub(crate) async fn install_oauth_transport_group_tx(
        &self,
        tx: &mut Transaction<'_, Any>,
        account_id: Uuid,
        snapshot: &OAuthGroupSnapshot,
        key: &[u8],
    ) -> Result<BindingStamp, AppError> {
        let members = self
            .validate_oauth_transport_group_tx(tx, snapshot, key)
            .await?;
        if snapshot.account_binding.is_some() {
            return Err(invalid());
        }
        let stamp = BindingStamp {
            selection_version: revision(tx).await?,
            binding_version: 1,
            group_id: Some(snapshot.reference.group_id),
            group_version: Some(snapshot.reference.expected_group_version),
        };
        sqlx::query("INSERT INTO transport_proxy_bindings (account_id, group_id, version, initial_member_id, replacement_member_id, updated_at) VALUES ($1, $2, 1, $3, NULL, $4)")
            .bind(account_id.to_string()).bind(snapshot.reference.group_id.to_string())
            .bind(snapshot.reference.initial_member_id.to_string()).bind(unix_millis())
            .execute(&mut **tx).await?;
        selection::rebase_selection(
            tx,
            account_id,
            &[],
            &members,
            snapshot.reference.initial_member_id,
            None,
            key,
        )
        .await?;
        budget(tx, key).await?;
        let tenant_id = tenant(tx, &snapshot.tenant_external_id).await?;
        let actor: Option<String> = sqlx::query_scalar("SELECT s.operator_service_id FROM upstream_accounts a JOIN oauth_login_sessions s ON s.id = a.oauth_session_id WHERE a.id = $1")
            .bind(account_id.to_string()).fetch_optional(&mut **tx).await?.flatten();
        audit(
            tx,
            &tenant_id,
            account_id,
            "bind",
            1,
            actor.map(parse_uuid).transpose()?,
        )
        .await?;
        Ok(stamp)
    }
}

pub(crate) async fn lock_oauth_transport_group(
    tx: &mut Transaction<'_, Any>,
) -> Result<(), AppError> {
    lock(tx).await
}
