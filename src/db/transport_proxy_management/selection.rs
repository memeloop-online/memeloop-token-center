use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn rebase_selection(
    tx: &mut Transaction<'_, Any>,
    account: Uuid,
    previous: &[Member],
    next: &[Member],
    initial: Uuid,
    replacement: Option<Uuid>,
    key: &[u8],
) -> Result<(), AppError> {
    let generation: i64 =
        sqlx::query_scalar("SELECT credential_generation FROM upstream_accounts WHERE id = $1")
            .bind(account.to_string())
            .fetch_one(&mut **tx)
            .await?;
    let durable = sqlx::query("SELECT group_version, group_fingerprint, selected_index, selection_generation FROM upstream_transport_proxy_selections WHERE account_id = $1")
        .bind(account.to_string()).fetch_optional(&mut **tx).await?;
    let base = next
        .iter()
        .position(|member| member.id == initial)
        .ok_or_else(invalid)?;
    let mut selected = base;
    let mut epoch = 1;
    if let Some(row) = durable {
        epoch = row
            .try_get::<i64, _>("selection_generation")?
            .checked_add(1)
            .ok_or(AppError::Internal)?;
        let version: i64 = row.try_get("group_version")?;
        let expected = TransportProxyGroups::fingerprint(
            account,
            version,
            previous
                .iter()
                .map(|member| member.proxy_url.clone())
                .collect(),
            key,
        )?;
        if row.try_get::<String, _>("group_fingerprint")? == expected {
            let index = usize::try_from(row.try_get::<i64, _>("selected_index")?)
                .map_err(|_| AppError::Internal)?;
            selected = previous
                .get(index)
                .and_then(|old| {
                    next.iter()
                        .position(|member| old.id == member.id && old.proxy_url == member.proxy_url)
                })
                .or_else(|| {
                    next.iter()
                        .position(|member| Some(member.id) == replacement)
                })
                .unwrap_or(base);
        }
    }
    if epoch >= 1 << 28 {
        return Err(conflict("proxy_group_capacity_exceeded"));
    }
    let fingerprint = TransportProxyGroups::fingerprint(
        account,
        generation,
        next.iter().map(|member| member.proxy_url.clone()).collect(),
        key,
    )?;
    sqlx::query("INSERT INTO upstream_transport_proxy_selections (account_id, group_version, group_fingerprint, credential_generation, base_index, selected_index, selection_generation) VALUES ($1, $2, $3, $2, $4, $5, $6) ON CONFLICT (account_id) DO UPDATE SET group_version = excluded.group_version, group_fingerprint = excluded.group_fingerprint, credential_generation = excluded.credential_generation, base_index = excluded.base_index, selected_index = excluded.selected_index, selection_generation = excluded.selection_generation")
        .bind(account.to_string()).bind(generation).bind(fingerprint).bind(base as i64).bind(selected as i64).bind(epoch).execute(&mut **tx).await?;
    Ok(())
}
