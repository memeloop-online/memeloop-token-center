//! One-time schema126 -> latest observation conversion. Legacy decoding belongs
//! here, never in the request path. Operators must drain old consumers first.
use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::db::requests::conversations::prefix_storage::{self, Fingerprint};

const PAGE_SIZE: i64 = 64;

fn invalid() -> sqlx::Error {
    sqlx::Error::Protocol(
        "observation prefix migration failed ownership/content verification".into(),
    )
}

pub(super) async fn convert(transaction: &mut Transaction<'_, Any>) -> Result<(), sqlx::Error> {
    // Match the live writer lock order. This prevents writes during conversion;
    // draining old readers/importers remains an explicit cutover prerequisite.
    lock_request_stats_projection_rebuild_in_transaction(transaction)
        .await
        .map_err(|_| invalid())?;
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        sqlx::query("LOCK TABLE conversation_observations IN ACCESS EXCLUSIVE MODE")
            .execute(&mut **transaction)
            .await?;
    }
    let mut after: Option<String> = None;
    loop {
        let rows = sqlx::query("SELECT o.id, o.atom_hashes_json, o.leaf_node_hash, c.tenant_id, k.tenant_id AS key_tenant_id FROM conversation_observations o LEFT JOIN conversation_clusters c ON c.id = o.cluster_id LEFT JOIN key_records k ON k.id = o.key_id WHERE (CAST($1 AS TEXT) IS NULL OR o.id > $1) ORDER BY o.id LIMIT $2")
            .bind(&after).bind(PAGE_SIZE).fetch_all(&mut **transaction).await?;
        if rows.is_empty() {
            break;
        }
        after = Some(rows.last().ok_or_else(invalid)?.try_get("id")?);
        let mut tenants: BTreeMap<String, Vec<AnyRow>> = BTreeMap::new();
        for row in rows {
            let tenant: Option<String> = row.try_get("tenant_id")?;
            let key_tenant: Option<String> = row.try_get("key_tenant_id")?;
            if tenant.is_none() || tenant != key_tenant {
                return Err(invalid());
            }
            tenants
                .entry(tenant.ok_or_else(invalid)?)
                .or_default()
                .push(row);
        }
        for (tenant, rows) in tenants {
            let mut roots = BTreeSet::new();
            for row in &rows {
                if let Some(leaf) = row.try_get::<Option<String>, _>("leaf_node_hash")? {
                    roots.insert(leaf);
                }
            }
            let paths = prefix_storage::load_paths(transaction, &tenant, roots)
                .await
                .map_err(|_| invalid())?;
            // Verify semantic ownership without rewriting or scanning bodies.
            let atoms: Vec<_> = paths
                .nodes
                .values()
                .map(|node| node.atom.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            for batch in atoms.chunks(800) {
                let placeholders = (2..batch.len() + 2)
                    .map(|index| format!("${index}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let statement = format!(
                    "SELECT COUNT(*) FROM semantic_atoms WHERE tenant_id = $1 AND content_hash IN ({placeholders})"
                );
                let mut query =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(statement)).bind(&tenant);
                for hash in batch {
                    query = query.bind(hash);
                }
                if query.fetch_one(&mut **transaction).await? != batch.len() as i64 {
                    return Err(invalid());
                }
            }
            for row in rows {
                let leaf: Option<String> = row.try_get("leaf_node_hash")?;
                let full = paths.hashes(leaf.as_deref(), None).map_err(|_| invalid())?;
                let fingerprint = Fingerprint::from_hashes(&full[..full.len().min(1024)]);
                let encoded: String = row.try_get("atom_hashes_json")?;
                if encoded.trim_start().starts_with('[') {
                    let legacy: Vec<String> =
                        serde_json::from_str(&encoded).map_err(|_| invalid())?;
                    // Prior writers stored either a complete list or the first
                    // 1024 hashes. Accept neither a partial lie nor an empty
                    // sentinel for a nonempty leaf.
                    if legacy != full && legacy != full[..full.len().min(1024)] {
                        return Err(invalid());
                    }
                } else {
                    let current = prefix_storage::decode(&encoded).map_err(|_| invalid())?;
                    if current.context_prefix_v1 != fingerprint.context_prefix_v1
                        || current.atom_count != fingerprint.atom_count
                    {
                        return Err(invalid());
                    }
                }
                let latest = serde_json::to_string(&fingerprint).map_err(|_| invalid())?;
                sqlx::query(
                    "UPDATE conversation_observations SET atom_hashes_json = $1 WHERE id = $2",
                )
                .bind(latest)
                .bind(row.try_get::<String, _>("id")?)
                .execute(&mut **transaction)
                .await?;
            }
        }
    }
    let guard = if transaction.as_mut().backend_name() == "PostgreSQL" {
        include_str!("../../../migrations/postgres/0127_conversation_prefix_guard.sql")
    } else {
        include_str!("../../../migrations/sqlite/0127_conversation_prefix_guard.sql")
    };
    sqlx::raw_sql(guard).execute(&mut **transaction).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
