use super::*;

mod bindings;
mod groups;
mod selection;
#[cfg(test)]
mod tests;
mod types;
pub(crate) use types::*;

pub(crate) const CONFIG_KEY: &str = "__mtc_transport_proxy_binding";
const MEMBERS_AAD: &[u8] = b"mtc/transport-proxy-members/v1";

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

fn open_members(row: &AnyRow, key: &[u8]) -> Result<Vec<Member>, AppError> {
    open_private_json(
        &row.try_get::<String, _>("members_ciphertext")?,
        key,
        MEMBERS_AAD,
    )
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
    let rows = sqlx::query("SELECT b.account_id, g.members_ciphertext FROM transport_proxy_bindings b JOIN transport_proxy_groups g ON g.id = b.group_id")
        .fetch_all(&mut **tx).await?;
    let mut bytes = 0usize;
    for row in &rows {
        let proxies = open_members(row, key)?
            .into_iter()
            .map(|member| member.proxy_url)
            .collect::<Vec<_>>();
        bytes += serde_json::to_vec(&serde_json::json!([{
            "account_id":row.try_get::<String, _>("account_id")?,
            "version":i64::MAX,
            "proxies":proxies,
        }]))
        .map_err(|_| AppError::Internal)?
        .len()
            + 512;
    }
    if count > 256 || rows.len() > 256 || bytes > 256 * 1024 {
        return Err(conflict("proxy_group_capacity_exceeded"));
    }
    Ok(())
}
