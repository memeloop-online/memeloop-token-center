use std::collections::BTreeSet;

use serde_json::Value;
use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, DatabaseBackend, unix_millis};
use super::grant_revisions::bump_credential_grant_revisions;

const ASSOCIATION_CHUNK_SIZE: usize = 64;
const CONSERVATIVE_CUSTOM_MODEL_RESERVATION_BOUND: i64 = 1_000_000_000;

pub(super) struct RouteRelationSnapshot {
    pub(super) credential_ids: Vec<Uuid>,
    pub(super) route_group_ids: Vec<Uuid>,
}

pub(super) struct RouteAssociationReplacement<'a> {
    pub(super) route_id: Uuid,
    pub(super) tenant_id: &'a str,
    pub(super) upstream_model: &'a str,
    pub(super) upstream_ids: &'a [Uuid],
    pub(super) included_provider_group_ids: &'a [Uuid],
    pub(super) excluded_provider_group_ids: &'a [Uuid],
    pub(super) route_group_ids: &'a [Uuid],
    pub(super) credential_ids: &'a [Uuid],
    pub(super) custom_model_confirmed: bool,
}

pub(super) async fn route_relation_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<RouteRelationSnapshot, AppError> {
    Ok(RouteRelationSnapshot {
        credential_ids: select_relation_ids(
            tx,
            "SELECT key_id AS id FROM routing_grants WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY key_id",
            tenant_id,
            route_id,
        )
        .await?,
        route_group_ids: select_relation_ids(
            tx,
            "SELECT route_group_id AS id FROM model_route_group_memberships WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY route_group_id",
            tenant_id,
            route_id,
        )
        .await?,
    })
}

pub(super) async fn finish_route_relation_replace(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    old: RouteRelationSnapshot,
    credential_ids: &[Uuid],
    route_group_ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    let old_credentials = old.credential_ids.into_iter().collect::<BTreeSet<_>>();
    let new_credentials = credential_ids.iter().copied().collect::<BTreeSet<_>>();
    let affected_credentials = old_credentials
        .symmetric_difference(&new_credentials)
        .copied()
        .collect::<Vec<_>>();
    bump_credential_grant_revisions(tx, tenant_id, &affected_credentials).await?;
    let old_route_groups = old.route_group_ids.into_iter().collect::<BTreeSet<_>>();
    let new_route_groups = route_group_ids.iter().copied().collect::<BTreeSet<_>>();
    let affected_route_groups = old_route_groups
        .symmetric_difference(&new_route_groups)
        .copied()
        .collect::<Vec<_>>();
    bump_route_group_relation_timestamps(tx, tenant_id, &affected_route_groups, now).await
}

pub(crate) async fn bump_route_group_relation_timestamps(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_group_ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    for group_id in route_group_ids {
        let updated = sqlx::query(
            "UPDATE route_groups SET updated_at = CASE WHEN updated_at >= $1 THEN updated_at + 1 ELSE $1 END \
             WHERE tenant_id = $2 AND id = $3 AND updated_at < 9223372036854775807",
        )
        .bind(now)
        .bind(tenant_id)
        .bind(group_id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "route group changed; reload before saving".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn bump_model_route_relation_timestamps(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    for route_id in route_ids {
        let updated = sqlx::query(
            "UPDATE model_routes SET updated_at = CASE WHEN updated_at >= $1 THEN updated_at + 1 ELSE $1 END \
             WHERE tenant_id = $2 AND id = $3 AND updated_at < 9223372036854775807",
        )
        .bind(now)
        .bind(tenant_id)
        .bind(route_id.to_string())
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "model route changed; reload before saving".into(),
            ));
        }
    }
    Ok(())
}

async fn select_relation_ids(
    tx: &mut Transaction<'_, Any>,
    sql: &'static str,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query(sql)
        .bind(tenant_id)
        .bind(route_id.to_string())
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| super::super::parse_uuid(row.try_get("id")?))
        .collect()
}

pub(super) async fn replace_route_associations(
    tx: &mut Transaction<'_, Any>,
    replacement: RouteAssociationReplacement<'_>,
) -> Result<(), AppError> {
    let RouteAssociationReplacement {
        route_id,
        tenant_id,
        upstream_model,
        upstream_ids,
        included_provider_group_ids,
        excluded_provider_group_ids,
        route_group_ids,
        credential_ids,
        custom_model_confirmed,
    } = replacement;
    if custom_model_confirmed {
        ensure_explicit_custom_reservation_bounds(tx, tenant_id, upstream_model, upstream_ids)
            .await?;
    }
    for table in [
        "model_route_upstream_accounts",
        "model_route_included_provider_groups",
        "model_route_excluded_provider_groups",
        "model_route_group_memberships",
    ] {
        let sql = format!("DELETE FROM {table} WHERE tenant_id = $1 AND model_route_id = $2");
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant_id)
            .bind(route_id.to_string())
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query("DELETE FROM routing_grants WHERE tenant_id = $1 AND model_route_id = $2")
        .bind(tenant_id)
        .bind(route_id.to_string())
        .execute(&mut **tx)
        .await?;
    let now = unix_millis();
    insert_upstream_accounts(
        tx,
        tenant_id,
        route_id,
        upstream_model,
        upstream_ids,
        now,
        custom_model_confirmed,
    )
    .await?;
    for (table, group_ids) in [
        (
            "model_route_included_provider_groups",
            included_provider_group_ids,
        ),
        (
            "model_route_excluded_provider_groups",
            excluded_provider_group_ids,
        ),
        ("model_route_group_memberships", route_group_ids),
    ] {
        let group_column = if table == "model_route_group_memberships" {
            "route_group_id"
        } else {
            "provider_group_id"
        };
        insert_four_column_associations(
            tx,
            &format!(
                "INSERT INTO {table} (tenant_id, model_route_id, {group_column}, created_at) VALUES "
            ),
            tenant_id,
            route_id,
            group_ids,
            now,
        )
        .await?;
    }
    insert_credential_grants(tx, tenant_id, route_id, credential_ids, now).await?;
    Ok(())
}

/// Establish the transport-side half of an explicit custom Codex route.
///
/// Callers already hold the tenant routing-relation write lock. Keeping this
/// config update in the association transaction gives catalog replacement and
/// association replacement one serialization boundary. If catalog pruning is
/// ordered first, the route recreates a conservative bound; if the route is
/// ordered first, catalog replacement observes the committed association and
/// preserves its bound.
pub(in crate::db) async fn ensure_explicit_custom_reservation_bounds(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    upstream_model: &str,
    account_ids: &[Uuid],
) -> Result<(), AppError> {
    for account_id in account_ids {
        let account = sqlx::query(
            "SELECT driver, config_json, updated_at FROM upstream_accounts WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(account_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let driver: String = account.try_get("driver")?;
        if driver != "openai-codex" {
            continue;
        }
        let config_json: String = account.try_get("config_json")?;
        let previous_updated_at: i64 = account.try_get("updated_at")?;
        let mut config: Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let object = config.as_object_mut().ok_or(AppError::Internal)?;
        let current = object.get("reservation_token_bounds");
        let legacy = object.get("output_token_limits");
        let (mut bounds, needs_normalization) = match (current, legacy) {
            (Some(Value::Object(bounds)), None) => (bounds.clone(), false),
            (None, Some(Value::Object(bounds))) => (bounds.clone(), true),
            (None, None) => (serde_json::Map::new(), true),
            _ => return Err(AppError::Internal),
        };
        let has_valid_bound = bounds.get(upstream_model).is_some_and(|bound| {
            bound.as_i64().is_some_and(|bound| {
                (1..=CONSERVATIVE_CUSTOM_MODEL_RESERVATION_BOUND).contains(&bound)
            })
        });
        if !has_valid_bound {
            if bounds.contains_key(upstream_model) {
                return Err(AppError::Internal);
            }
            bounds.insert(
                upstream_model.to_owned(),
                Value::from(CONSERVATIVE_CUSTOM_MODEL_RESERVATION_BOUND),
            );
        } else if !needs_normalization {
            continue;
        }
        object.remove("output_token_limits");
        object.insert("reservation_token_bounds".to_owned(), Value::Object(bounds));
        let next_config = serde_json::to_string(&config).map_err(|_| AppError::Internal)?;
        let next_updated_at = unix_millis().max(previous_updated_at.saturating_add(1));
        let updated = sqlx::query(
            "UPDATE upstream_accounts SET config_json = $1, updated_at = $2 WHERE tenant_id = $3 AND id = $4 AND updated_at = $5",
        )
        .bind(next_config)
        .bind(next_updated_at)
        .bind(tenant_id)
        .bind(account_id.to_string())
        .bind(previous_updated_at)
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "upstream account changed while saving the route".into(),
            ));
        }
    }
    Ok(())
}

async fn insert_upstream_accounts(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    upstream_model: &str,
    account_ids: &[Uuid],
    now: i64,
    custom_model_confirmed: bool,
) -> Result<(), AppError> {
    let catalog_policy = if custom_model_confirmed {
        "explicit_custom"
    } else {
        "required"
    };
    for chunk in account_ids.chunks(ASSOCIATION_CHUNK_SIZE) {
        let sql = format!(
            "INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy) VALUES {}",
            values_placeholders(6, chunk.len(), Some(5))
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for account_id in chunk {
            query = query
                .bind(tenant_id)
                .bind(route_id.to_string())
                .bind(account_id.to_string())
                .bind(upstream_model)
                .bind(now)
                .bind(catalog_policy);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

async fn insert_four_column_associations(
    tx: &mut Transaction<'_, Any>,
    prefix: &str,
    tenant_id: &str,
    route_id: Uuid,
    ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    for chunk in ids.chunks(ASSOCIATION_CHUNK_SIZE) {
        let sql = format!("{prefix}{}", values_placeholders(4, chunk.len(), None));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query
                .bind(tenant_id)
                .bind(route_id.to_string())
                .bind(id.to_string())
                .bind(now);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

async fn insert_credential_grants(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    key_ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    for chunk in key_ids.chunks(ASSOCIATION_CHUNK_SIZE) {
        let sql = format!(
            "INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES {}",
            values_placeholders(4, chunk.len(), Some(4))
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for key_id in chunk {
            query = query
                .bind(tenant_id)
                .bind(key_id.to_string())
                .bind(route_id.to_string())
                .bind(now);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

/// Builds closed, numeric placeholders. `literal_position` inserts the only
/// supported SQL literal (`100` scheduling weight or `NULL` route group)
/// without consuming a bind position.
fn values_placeholders(width: usize, rows: usize, literal_position: Option<usize>) -> String {
    let bound_width = width;
    (0..rows)
        .map(|row| {
            let mut bind = row * bound_width + 1;
            let values = (1..=width + usize::from(literal_position.is_some()))
                .map(|position| {
                    if literal_position == Some(position) {
                        if position == 4 {
                            "NULL".to_owned()
                        } else {
                            "100".to_owned()
                        }
                    } else {
                        let placeholder = format!("${bind}");
                        bind += 1;
                        placeholder
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("({values})")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(in crate::db) async fn ensure_route_has_eligible_candidate(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<(), AppError> {
    let sql = match backend {
        DatabaseBackend::PostgreSql => {
            "SELECT account.id FROM model_route_eligible_upstream_accounts eligible
         JOIN upstream_accounts account ON account.tenant_id = eligible.tenant_id AND account.id = eligible.upstream_account_id AND account.status = 'active'
         JOIN upstream_credentials credential ON credential.upstream_account_id = account.id AND credential.generation = account.credential_generation AND credential.revoked_at IS NULL AND (credential.expires_at IS NULL OR credential.expires_at > $3)
         WHERE eligible.tenant_id = $1 AND eligible.model_route_id = $2
         ORDER BY account.id LIMIT 1 FOR UPDATE OF account, credential"
        }
        DatabaseBackend::Sqlite => {
            "SELECT account.id FROM model_route_eligible_upstream_accounts eligible
         JOIN upstream_accounts account ON account.tenant_id = eligible.tenant_id AND account.id = eligible.upstream_account_id AND account.status = 'active'
         JOIN upstream_credentials credential ON credential.upstream_account_id = account.id AND credential.generation = account.credential_generation AND credential.revoked_at IS NULL AND (credential.expires_at IS NULL OR credential.expires_at > $3)
         WHERE eligible.tenant_id = $1 AND eligible.model_route_id = $2
         ORDER BY account.id LIMIT 1"
        }
    };
    let candidate = sqlx::query(sql)
        .bind(tenant_id)
        .bind(route_id.to_string())
        .bind(unix_millis())
        .fetch_optional(&mut **tx)
        .await?;
    if candidate.is_none() {
        return Err(AppError::BadRequest(
            "the route has no eligible upstream for this model; sync the model catalog or explicitly confirm a custom model".into(),
        ));
    }
    Ok(())
}

pub(super) async fn tenant_id(
    tx: &mut Transaction<'_, Any>,
    external_id: &str,
) -> Result<String, AppError> {
    sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
        .bind(external_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("id")
        .map_err(Into::into)
}

pub(super) async fn require_members_tenant(
    tx: &mut Transaction<'_, Any>,
    table: &str,
    ids: &[Uuid],
    tenant_id: &str,
) -> Result<(), AppError> {
    let sql = match table {
        "upstream_accounts" => "SELECT 1 FROM upstream_accounts WHERE tenant_id = $1 AND id = $2",
        "provider_groups" => "SELECT 1 FROM provider_groups WHERE tenant_id = $1 AND id = $2",
        "route_groups" => "SELECT 1 FROM route_groups WHERE tenant_id = $1 AND id = $2",
        "key_records" => "SELECT 1 FROM key_records WHERE tenant_id = $1 AND id = $2",
        "model_routes" => {
            "SELECT 1 FROM model_routes WHERE archived_at IS NULL AND tenant_id = $1 AND id = $2"
        }
        _ => return Err(AppError::Internal),
    };
    for id in ids {
        if sqlx::query(sql)
            .bind(tenant_id)
            .bind(id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .is_none()
        {
            return Err(AppError::NotFound);
        }
    }
    Ok(())
}

pub(super) async fn select_ids(
    pool: &sqlx::AnyPool,
    sql: &'static str,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query(sql)
        .bind(tenant_id)
        .bind(route_id.to_string())
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| super::super::parse_uuid(row.try_get("id")?))
        .collect()
}

pub(super) async fn candidate_upstream_ids(
    pool: &sqlx::AnyPool,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    let rows = sqlx::query("SELECT candidates.id FROM (SELECT upstream_account_id AS id FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 UNION SELECT m.upstream_account_id AS id FROM model_route_included_provider_groups i JOIN upstream_account_provider_groups m ON m.tenant_id = i.tenant_id AND m.provider_group_id = i.provider_group_id WHERE i.tenant_id = $1 AND i.model_route_id = $2) candidates JOIN upstream_accounts a ON a.id = candidates.id AND a.tenant_id = $1 AND a.status = 'active' WHERE NOT EXISTS (SELECT 1 FROM model_route_excluded_provider_groups e JOIN upstream_account_provider_groups x ON x.tenant_id = e.tenant_id AND x.provider_group_id = e.provider_group_id WHERE e.tenant_id = $1 AND e.model_route_id = $2 AND x.upstream_account_id = candidates.id) ORDER BY candidates.id")
        .bind(tenant_id).bind(route_id.to_string()).fetch_all(pool).await?;
    rows.into_iter()
        .map(|row| super::super::parse_uuid(row.try_get("id")?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_hundred_associations_are_chunked_into_bounded_statements() {
        assert_eq!(500_usize.div_ceil(ASSOCIATION_CHUNK_SIZE), 8);
        assert_eq!(
            values_placeholders(6, 1, Some(5)),
            "($1, $2, $3, $4, 100, $5, $6)"
        );
        assert_eq!(values_placeholders(4, 1, Some(4)), "($1, $2, $3, NULL, $4)");
    }
}
