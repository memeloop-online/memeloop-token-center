use std::collections::BTreeMap;

use super::*;

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Group {
    account_id: Uuid,
    version: i64,
    proxies: Vec<String>,
}

pub(crate) struct TransportProxyGroups {
    groups: BTreeMap<Uuid, (Group, String)>,
}

pub(crate) struct ProxySelection {
    pub(crate) credential: UpstreamCredential,
    pub(crate) generation: i64,
    ticket: Option<Ticket>,
}

struct Ticket {
    account_id: Uuid,
    group_version: i64,
    fingerprint: String,
    credential_generation: i64,
    selected: usize,
    count: usize,
}

fn unavailable() -> AppError {
    AppError::Conflict("account transport proxy selection is unavailable".into())
}

impl TransportProxyGroups {
    pub(crate) fn parse(input: &str, key: &[u8]) -> Result<Self, AppError> {
        if input.len() > 256 * 1024 {
            return Err(unavailable());
        }
        let values: Vec<Group> = serde_json::from_str(input).map_err(|_| unavailable())?;
        if values.len() > 256 {
            return Err(unavailable());
        }
        let mut groups = BTreeMap::new();
        for group in values {
            if group.version <= 0 || !(1..=4).contains(&group.proxies.len()) {
                return Err(unavailable());
            }
            for (index, proxy) in group.proxies.iter().enumerate() {
                crate::provider::validate_codex_proxy_url(proxy).map_err(|_| unavailable())?;
                if group.proxies[..index].contains(proxy) {
                    return Err(unavailable());
                }
            }
            let mut digest = Sha256::new();
            digest.update(b"mtc/transport-proxy-group/v1\0");
            digest.update(key);
            digest.update(serde_json::to_vec(&group).map_err(|_| unavailable())?);
            let fingerprint = format!("{:x}", digest.finalize());
            if groups
                .insert(group.account_id, (group, fingerprint))
                .is_some()
            {
                return Err(unavailable());
            }
        }
        Ok(Self { groups })
    }

    pub(crate) async fn select(
        &self,
        database: &Database,
        account_id: Uuid,
        credential_generation: i64,
        credential: &UpstreamCredential,
    ) -> Result<ProxySelection, AppError> {
        let single = || ProxySelection {
            credential: credential.clone(),
            generation: 0,
            ticket: None,
        };
        let Some((group, fingerprint)) = self.groups.get(&account_id) else {
            return Ok(single());
        };
        let Some(base_index) = credential.proxy().and_then(|(proxy, _)| {
            group
                .proxies
                .iter()
                .position(|candidate| candidate == proxy)
        }) else {
            return Ok(single());
        };
        let read = || {
            sqlx::query(
            "SELECT s.selected_index, s.selection_generation FROM upstream_transport_proxy_selections s JOIN upstream_accounts a ON a.id = s.account_id AND a.credential_generation = s.credential_generation WHERE s.account_id = $1 AND s.group_version = $2 AND s.group_fingerprint = $3 AND s.credential_generation = $4 AND s.base_index = $5",
        )
        .bind(account_id.to_string())
        .bind(group.version)
        .bind(fingerprint)
        .bind(credential_generation)
        .bind(base_index as i64)
        };
        let row = if let Some(row) = read().fetch_optional(&database.pool).await? {
            row
        } else {
            sqlx::query(
            "INSERT INTO upstream_transport_proxy_selections (account_id, group_version, group_fingerprint, credential_generation, base_index, selected_index, selection_generation) SELECT id, $2, $3, $4, $5, $5, 1 FROM upstream_accounts WHERE id = $1 AND credential_generation = $4 ON CONFLICT (account_id) DO UPDATE SET group_version = excluded.group_version, group_fingerprint = excluded.group_fingerprint, credential_generation = excluded.credential_generation, base_index = excluded.base_index, selected_index = CASE WHEN upstream_transport_proxy_selections.group_version <> excluded.group_version OR upstream_transport_proxy_selections.base_index <> excluded.base_index THEN excluded.base_index ELSE upstream_transport_proxy_selections.selected_index END, selection_generation = upstream_transport_proxy_selections.selection_generation + 1 WHERE upstream_transport_proxy_selections.credential_generation <= excluded.credential_generation AND (upstream_transport_proxy_selections.group_version < excluded.group_version OR (upstream_transport_proxy_selections.group_version = excluded.group_version AND upstream_transport_proxy_selections.group_fingerprint = excluded.group_fingerprint AND upstream_transport_proxy_selections.credential_generation < excluded.credential_generation))",
        )
        .bind(account_id.to_string())
        .bind(group.version)
        .bind(fingerprint)
        .bind(credential_generation)
        .bind(base_index as i64)
        .execute(&database.pool)
        .await?;
            read()
                .fetch_optional(&database.pool)
                .await?
                .ok_or_else(unavailable)?
        };
        let selected =
            usize::try_from(row.try_get::<i64, _>("selected_index")?).map_err(|_| unavailable())?;
        let proxy = group.proxies.get(selected).ok_or_else(unavailable)?;
        Ok(ProxySelection {
            credential: credential.clone().with_transport_proxy(proxy.clone())?,
            generation: row.try_get("selection_generation")?,
            ticket: Some(Ticket {
                account_id,
                group_version: group.version,
                fingerprint: fingerprint.clone(),
                credential_generation,
                selected,
                count: group.proxies.len(),
            }),
        })
    }
}

impl ProxySelection {
    pub(crate) fn member(&self) -> Option<usize> {
        self.ticket
            .as_ref()
            .filter(|ticket| ticket.count > 1)
            .map(|ticket| ticket.selected)
    }

    pub(crate) async fn advance_after_connect_failure(
        &self,
        database: &Database,
        attempted: &[usize],
    ) -> Result<bool, AppError> {
        let Some(ticket) = &self.ticket else {
            return Ok(false);
        };
        let next = (1..ticket.count)
            .map(|offset| (ticket.selected + offset) % ticket.count)
            .find(|candidate| !attempted.contains(candidate));
        let Some(next) = next else {
            return Ok(false);
        };
        let mut transaction = database.begin_write_transaction().await?;
        let account_query = match database.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT credential_generation FROM upstream_accounts WHERE id = $1 FOR SHARE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT credential_generation FROM upstream_accounts WHERE id = $1"
            }
        };
        let current: Option<i64> = sqlx::query_scalar(account_query)
            .bind(ticket.account_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?;
        if current != Some(ticket.credential_generation) {
            transaction.rollback().await?;
            return Ok(false);
        }
        let changed = sqlx::query(
            "UPDATE upstream_transport_proxy_selections SET selected_index = $1, selection_generation = selection_generation + 1 WHERE account_id = $2 AND group_version = $3 AND group_fingerprint = $4 AND credential_generation = $5 AND selection_generation = $6 AND selected_index = $7 AND EXISTS (SELECT 1 FROM upstream_accounts a WHERE a.id = $2 AND a.credential_generation = $5)",
        )
        .bind(next as i64)
        .bind(ticket.account_id.to_string())
        .bind(ticket.group_version)
        .bind(&ticket.fingerprint)
        .bind(ticket.credential_generation)
        .bind(self.generation)
        .bind(ticket.selected as i64)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(changed.rows_affected() == 1)
    }
}
