use super::*;
use crate::db::network_proxies::{
    load_group_members, normalize_legacy_members, store_group_members,
};

mod bindings;
mod groups;
mod oauth;
mod selection;
#[cfg(test)]
mod tests;
mod types;
pub(crate) use oauth::lock_oauth_transport_group;
pub(crate) use types::*;

pub(crate) const CONFIG_KEY: &str = "__mtc_transport_proxy_binding";

fn conflict(code: &'static str) -> AppError {
    AppError::ProxyGroupConflict(code)
}

fn invalid() -> AppError {
    AppError::BadRequest("invalid transport proxy group configuration".into())
}

async fn lock(tx: &mut Transaction<'_, Any>) -> Result<(), AppError> {
    sqlx::query("UPDATE transport_proxy_management_lock SET revision = revision + 1 WHERE id = 1")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn tenant(tx: &mut Transaction<'_, Any>, external_id: &str) -> Result<String, AppError> {
    sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
        .bind(external_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

async fn revision(tx: &mut Transaction<'_, Any>) -> Result<i64, AppError> {
    Ok(
        sqlx::query_scalar("SELECT revision FROM transport_proxy_management_lock WHERE id = 1")
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn audit(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    resource: Uuid,
    action: &str,
    version: i64,
    actor: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO transport_proxy_management_audit (id, tenant_id, resource_id, action, version, actor_service_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(Uuid::now_v7().to_string()).bind(tenant_id).bind(resource.to_string())
        .bind(action).bind(version).bind(actor.map(|id| id.to_string())).bind(unix_millis())
        .execute(&mut **tx).await?;
    Ok(())
}

fn validate_name(value: &str) -> Result<(), AppError> {
    if value.trim() != value
        || !(1..=64).contains(&value.chars().count())
        || value.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    Ok(())
}

async fn available_name(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    name: &str,
    id: Uuid,
) -> Result<(), AppError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transport_proxy_groups WHERE tenant_id = $1 AND name = $2 AND id <> $3")
        .bind(tenant_id).bind(name).bind(id.to_string()).fetch_one(&mut **tx).await?;
    if count != 0 {
        return Err(invalid());
    }
    Ok(())
}

async fn budget(tx: &mut Transaction<'_, Any>, key: &[u8]) -> Result<(), AppError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transport_proxy_groups")
        .fetch_one(&mut **tx)
        .await?;
    let rows = sqlx::query("SELECT b.account_id, g.id FROM transport_proxy_bindings b JOIN transport_proxy_groups g ON g.id = b.group_id")
        .fetch_all(&mut **tx).await?;
    let mut bytes = 0usize;
    for row in &rows {
        let proxies = load_group_members(&mut **tx, &row.try_get::<String, _>("id")?, key)
            .await?
            .into_iter()
            .map(|member| member.proxy_url)
            .collect::<Vec<_>>();
        bytes += serde_json::to_vec(&serde_json::json!([{
            "account_id":row.try_get::<String, _>("account_id")?,
            "version":i64::MAX,
            "proxies":proxies,
        }]))
        .map_err(|_| AppError::internal("transport_proxy_management_mod_budget_1"))?
        .len()
            + 512;
    }
    if count > 256 || rows.len() > 256 || bytes > 256 * 1024 {
        return Err(conflict("proxy_group_capacity_exceeded"));
    }
    Ok(())
}

impl Database {
    pub(crate) async fn refresh_network_proxy_references(
        &self,
        tx: &mut Transaction<'_, Any>,
        proxy_id: Uuid,
        old_proxy: &str,
        old_name: &str,
        exclude_group: Option<&str>,
        key: &[u8],
    ) -> Result<(), AppError> {
        let groups = sqlx::query("SELECT g.id, g.tenant_id, g.version FROM transport_proxy_groups g JOIN network_proxy_group_members m ON m.group_id = g.id WHERE m.proxy_id = $1 ORDER BY g.id")
            .bind(proxy_id.to_string()).fetch_all(&mut **tx).await?;
        for group in groups {
            let id: String = group.try_get("id")?;
            if exclude_group == Some(id.as_str()) {
                continue;
            }
            let tenant_id: String = group.try_get("tenant_id")?;
            let version = group
                .try_get::<i64, _>("version")?
                .checked_add(1)
                .ok_or_else(|| AppError::internal("network_proxy_group_version"))?;
            let next = load_group_members(&mut **tx, &id, key).await?;
            let mut previous = next.clone();
            if let Some(member) = previous.iter_mut().find(|member| member.id == proxy_id) {
                member.proxy_url = old_proxy.to_owned();
                member.label = old_name.to_owned();
            }
            sqlx::query(
                "UPDATE transport_proxy_groups SET version = $1, updated_at = $2 WHERE id = $3",
            )
            .bind(version)
            .bind(unix_millis())
            .bind(&id)
            .execute(&mut **tx)
            .await?;
            let accounts = sqlx::query("SELECT account_id, version, initial_member_id FROM transport_proxy_bindings WHERE group_id = $1 ORDER BY account_id")
                .bind(&id).fetch_all(&mut **tx).await?;
            for account in accounts {
                let account_id = parse_uuid(account.try_get("account_id")?)?;
                let initial = parse_uuid(account.try_get("initial_member_id")?)?;
                let base = next
                    .iter()
                    .find(|member| member.id == initial)
                    .ok_or_else(invalid)?;
                let stamp = BindingStamp {
                    selection_version: revision(tx).await?,
                    binding_version: account.try_get("version")?,
                    group_id: Some(parse_uuid(id.clone())?),
                    group_version: Some(version),
                };
                self.write_transport_account(
                    tx,
                    account_id,
                    &tenant_id,
                    &stamp,
                    &base.proxy_url,
                    None,
                    key,
                )
                .await?;
                selection::rebase_selection(
                    tx,
                    account_id,
                    &previous,
                    &next,
                    initial,
                    Some(initial),
                    key,
                )
                .await?;
            }
        }
        Ok(())
    }
}
