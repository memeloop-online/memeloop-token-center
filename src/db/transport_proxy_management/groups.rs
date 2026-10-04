use super::*;

impl Database {
    pub(crate) async fn list_transport_groups(
        &self,
        external: &str,
        key: &[u8],
    ) -> Result<Value, AppError> {
        let rows = sqlx::query("SELECT g.*, (SELECT COUNT(*) FROM transport_proxy_bindings b WHERE b.group_id = g.id) AS bound_account_count FROM transport_proxy_groups g JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $1 ORDER BY g.name, g.id")
            .bind(external).fetch_all(&self.pool).await?;
        let items = rows
            .iter()
            .map(|row| view(row, external, key))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::json!({"items":items}))
    }

    pub(crate) async fn get_transport_group(
        &self,
        id: Uuid,
        external: &str,
        key: &[u8],
    ) -> Result<Value, AppError> {
        let row = sqlx::query("SELECT g.*, (SELECT COUNT(*) FROM transport_proxy_bindings b WHERE b.group_id = g.id) AS bound_account_count FROM transport_proxy_groups g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = $1 AND t.external_id = $2")
            .bind(id.to_string()).bind(external).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        view(&row, external, key)
    }

    pub(crate) async fn create_transport_group(
        &self,
        body: CreateGroup,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        validate_name(&body.name)?;
        let members = members(body.members, &[])?;
        validate_endpoints(&members).await?;
        let ciphertext = seal_private_json(&members, key, MEMBERS_AAD)?;
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, &body.tenant_external_id).await?;
        let id = Uuid::now_v7();
        available_name(&mut tx, &tenant_id, &body.name, id).await?;
        let now = unix_millis();
        sqlx::query("INSERT INTO transport_proxy_groups (id, tenant_id, name, version, members_ciphertext, created_at, updated_at) VALUES ($1, $2, $3, 1, $4, $5, $5)")
            .bind(id.to_string()).bind(&tenant_id).bind(&body.name).bind(ciphertext).bind(now).execute(&mut *tx).await?;
        budget(&mut tx, key).await?;
        audit(&mut tx, &tenant_id, id, "create", 1, actor).await?;
        let row = sqlx::query(
            "SELECT g.*, CAST(0 AS BIGINT) AS bound_account_count FROM transport_proxy_groups g WHERE id = $1",
        )
        .bind(id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        let result = view(&row, &body.tenant_external_id, key)?;
        tx.commit().await?;
        Ok(result)
    }

    pub(crate) async fn update_transport_group(
        &self,
        id: Uuid,
        body: UpdateGroup,
        actor: Option<Uuid>,
        key: &[u8],
    ) -> Result<Value, AppError> {
        validate_name(&body.name)?;
        for member in &body.members {
            if let Some(proxy) = &member.proxy_url {
                crate::network::validate_managed_codex_proxy(proxy).await?;
            }
        }
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, &body.tenant_external_id).await?;
        let row =
            sqlx::query("SELECT * FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2")
                .bind(id.to_string())
                .bind(&tenant_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
        let version: i64 = row.try_get("version")?;
        if version != body.expected_version {
            return Err(conflict("proxy_group_version_conflict"));
        }
        let previous = open_members(&row, key)?;
        available_name(&mut tx, &tenant_id, &body.name, id).await?;
        let next = members(body.members, &previous)?;
        let destructive = previous.iter().any(|old| {
            !next
                .iter()
                .any(|member| member.id == old.id && member.proxy_url == old.proxy_url)
        });
        let accounts = sqlx::query("SELECT account_id, version, initial_member_id FROM transport_proxy_bindings WHERE group_id = $1 ORDER BY account_id")
            .bind(id.to_string()).fetch_all(&mut *tx).await?;
        if destructive && !accounts.is_empty() && body.replacement_member_id.is_none() {
            return Err(invalid());
        }
        if let Some(replacement) = body.replacement_member_id
            && !next.iter().any(|member| member.id == replacement)
        {
            return Err(invalid());
        }
        let version = version.checked_add(1).ok_or(AppError::Internal)?;
        let ciphertext = seal_private_json(&next, key, MEMBERS_AAD)?;
        sqlx::query("UPDATE transport_proxy_groups SET name = $1, version = $2, members_ciphertext = $3, updated_at = $4 WHERE id = $5")
            .bind(&body.name).bind(version).bind(ciphertext).bind(unix_millis()).bind(id.to_string()).execute(&mut *tx).await?;
        for account in accounts {
            let account_id = parse_uuid(account.try_get("account_id")?)?;
            let initial = parse_uuid(account.try_get("initial_member_id")?)?;
            let retained = next.iter().find(|member| {
                member.id == initial
                    && previous
                        .iter()
                        .any(|old| old.id == initial && old.proxy_url == member.proxy_url)
            });
            let base = retained
                .or_else(|| {
                    next.iter()
                        .find(|member| Some(member.id) == body.replacement_member_id)
                })
                .ok_or_else(invalid)?;
            let binding_version: i64 = account.try_get("version")?;
            let stamp = BindingStamp {
                binding_version,
                group_id: Some(id),
                group_version: Some(version),
            };
            self.write_transport_account(
                &mut tx,
                account_id,
                &tenant_id,
                &stamp,
                &base.proxy_url,
                None,
                key,
            )
            .await?;
            selection::rebase_selection(
                &mut tx,
                account_id,
                &previous,
                &next,
                base.id,
                body.replacement_member_id,
                key,
            )
            .await?;
            sqlx::query("UPDATE transport_proxy_bindings SET initial_member_id = $1, replacement_member_id = $2, updated_at = $3 WHERE account_id = $4")
                .bind(base.id.to_string()).bind(body.replacement_member_id.map(|id| id.to_string())).bind(unix_millis()).bind(account_id.to_string()).execute(&mut *tx).await?;
        }
        budget(&mut tx, key).await?;
        audit(&mut tx, &tenant_id, id, "update", version, actor).await?;
        let row = sqlx::query("SELECT g.*, (SELECT COUNT(*) FROM transport_proxy_bindings b WHERE b.group_id = g.id) AS bound_account_count FROM transport_proxy_groups g WHERE id = $1")
            .bind(id.to_string()).fetch_one(&mut *tx).await?;
        let result = view(&row, &body.tenant_external_id, key)?;
        tx.commit().await?;
        Ok(result)
    }

    pub(crate) async fn delete_transport_group(
        &self,
        id: Uuid,
        body: DeleteGroup,
        actor: Option<Uuid>,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        lock(&mut tx).await?;
        let tenant_id = tenant(&mut tx, &body.tenant_external_id).await?;
        let version: i64 = sqlx::query_scalar(
            "SELECT version FROM transport_proxy_groups WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id.to_string())
        .bind(&tenant_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        if version != body.expected_version {
            return Err(conflict("proxy_group_version_conflict"));
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM transport_proxy_bindings WHERE group_id = $1")
                .bind(id.to_string())
                .fetch_one(&mut *tx)
                .await?;
        if count != 0 {
            return Err(conflict("proxy_group_in_use"));
        }
        sqlx::query("DELETE FROM transport_proxy_groups WHERE id = $1")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        audit(&mut tx, &tenant_id, id, "delete", version, actor).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn validate_endpoints(members: &[Member]) -> Result<(), AppError> {
    for member in members {
        crate::network::validate_managed_codex_proxy(&member.proxy_url).await?;
    }
    Ok(())
}
