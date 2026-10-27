use super::*;
use crate::db::transport_proxy_management::Member;

pub(crate) async fn load_group_members<'connection, Executor>(
    executor: Executor,
    group_id: &str,
    key: &[u8],
) -> Result<Vec<Member>, AppError>
where
    Executor: sqlx::Executor<'connection, Database = Any>,
{
    let rows = sqlx::query("SELECT p.id, p.name, p.version, p.proxy_ciphertext FROM network_proxy_group_members m JOIN network_proxies p ON p.id = m.proxy_id JOIN transport_proxy_groups g ON g.id = m.group_id AND g.tenant_id = p.tenant_id WHERE m.group_id = $1 ORDER BY m.position")
        .bind(group_id).fetch_all(executor).await?;
    rows.into_iter()
        .map(|row| {
            Ok(Member {
                version: row.try_get("version")?,
                id: parse_uuid(row.try_get("id")?)?,
                label: row.try_get("name")?,
                proxy_url: open_private_json(
                    &row.try_get::<String, _>("proxy_ciphertext")?,
                    key,
                    PROXY_AAD,
                )?,
            })
        })
        .collect()
}

pub(crate) async fn normalize_legacy_members(pool: &AnyPool, key: &[u8]) -> Result<(), AppError> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM transport_proxy_groups WHERE members_normalized = 0",
    )
    .fetch_one(pool)
    .await?;
    if pending == 0 {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    proxy_lock(&mut tx).await?;
    let rows = sqlx::query(
        "SELECT * FROM transport_proxy_groups WHERE members_normalized = 0 ORDER BY id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for row in rows {
        let group: String = row.try_get("id")?;
        let tenant: String = row.try_get("tenant_id")?;
        let members: Vec<Member> = open_private_json(
            &row.try_get::<String, _>("members_ciphertext")?,
            key,
            b"mtc/transport-proxy-members/v1",
        )?;
        for (position, member) in members.into_iter().enumerate() {
            let existing = sqlx::query(
                "SELECT tenant_id, proxy_ciphertext FROM network_proxies WHERE id = $1",
            )
            .bind(member.id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                let proxy: String = open_private_json(
                    &existing.try_get::<String, _>("proxy_ciphertext")?,
                    key,
                    PROXY_AAD,
                )?;
                if existing.try_get::<String, _>("tenant_id")? != tenant
                    || proxy != member.proxy_url
                {
                    return Err(AppError::ProxyGroupConflict(
                        "legacy_proxy_identity_conflict",
                    ));
                }
            } else {
                sqlx::query("INSERT INTO network_proxies (id, tenant_id, name, version, proxy_ciphertext, probe_enabled, probe_target, probe_attempts, created_at, updated_at) VALUES ($1,$2,$3,1,$4,$5,$6,$7,$8,$9)")
                    .bind(member.id.to_string()).bind(&tenant).bind(member.label)
                    .bind(seal_private_json(&member.proxy_url, key, PROXY_AAD)?)
                    .bind(i64::from(row.try_get::<Option<String>, _>("probe_url")?.is_some()))
                    .bind(row.try_get::<Option<String>, _>("probe_url")?.unwrap_or_else(|| "https://openai.com/".into()))
                    .bind(row.try_get::<i64, _>("probe_attempts")?).bind(row.try_get::<i64, _>("created_at")?)
                    .bind(row.try_get::<i64, _>("updated_at")?).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO network_proxy_group_members (group_id, proxy_id, position) VALUES ($1,$2,$3)")
                .bind(&group).bind(member.id.to_string()).bind(position as i64).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE transport_proxy_groups SET members_ciphertext = '', members_normalized = 1 WHERE id = $1")
            .bind(group).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn store_group_members(
    tx: &mut Transaction<'_, Any>,
    group: &str,
    tenant: &str,
    members: &[Member],
    probe_target: Option<&str>,
    probe_attempts: i64,
    key: &[u8],
) -> Result<(), AppError> {
    sqlx::query("DELETE FROM network_proxy_group_members WHERE group_id = $1")
        .bind(group)
        .execute(&mut **tx)
        .await?;
    for (position, member) in members.iter().enumerate() {
        let previous = sqlx::query("SELECT * FROM network_proxies WHERE id = $1")
            .bind(member.id.to_string())
            .fetch_optional(&mut **tx)
            .await?;
        if let Some(previous) = previous {
            if previous.try_get::<String, _>("tenant_id")? != tenant {
                return Err(AppError::NotFound);
            }
            let old: String = open_private_json(
                &previous.try_get::<String, _>("proxy_ciphertext")?,
                key,
                PROXY_AAD,
            )?;
            if old != member.proxy_url || previous.try_get::<String, _>("name")? != member.label {
                sqlx::query("UPDATE network_proxies SET name = $1, proxy_ciphertext = $2, version = version + 1, updated_at = $3, last_probe_status = NULL, last_probe_checked_at = NULL WHERE id = $4")
                    .bind(&member.label).bind(seal_private_json(&member.proxy_url, key, PROXY_AAD)?)
                    .bind(unix_millis()).bind(member.id.to_string()).execute(&mut **tx).await?;
            }
        } else {
            sqlx::query("INSERT INTO network_proxies (id, tenant_id, name, version, proxy_ciphertext, probe_enabled, probe_target, probe_attempts, created_at, updated_at) VALUES ($1,$2,$3,1,$4,$5,$6,$7,$8,$8)")
                .bind(member.id.to_string()).bind(tenant).bind(&member.label).bind(seal_private_json(&member.proxy_url, key, PROXY_AAD)?)
                .bind(i64::from(probe_target.is_some())).bind(probe_target.unwrap_or("https://openai.com/"))
                .bind(probe_attempts).bind(unix_millis()).execute(&mut **tx).await?;
        }
        sqlx::query("INSERT INTO network_proxy_group_members (group_id, proxy_id, position) VALUES ($1,$2,$3)")
            .bind(group).bind(member.id.to_string()).bind(position as i64).execute(&mut **tx).await?;
    }
    sqlx::query("UPDATE transport_proxy_groups SET members_ciphertext = '', members_normalized = 1 WHERE id = $1")
        .bind(group).execute(&mut **tx).await?;
    Ok(())
}
