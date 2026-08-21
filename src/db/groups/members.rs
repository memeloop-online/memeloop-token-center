use std::collections::BTreeSet;

use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::routing::{bump_model_route_relation_timestamps, lock_routing_relation_writes};
use super::super::{AppError, Database, unix_millis};
use super::types::{GroupKind, GroupView, ReplaceGroupMembersInput};

const MAX_GROUP_MEMBERS: usize = 500;
const MEMBERSHIP_CHUNK_SIZE: usize = 100;

impl Database {
    pub async fn replace_group_members(
        &self,
        kind: GroupKind,
        group_id: Uuid,
        input: ReplaceGroupMembersInput,
    ) -> Result<GroupView, AppError> {
        let member_ids = bounded_unique_ids(input.member_ids)?;
        let (groups, memberships, group_column) = kind.tables();
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        if kind == GroupKind::Route {
            lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        }
        let group_sql = format!("SELECT updated_at FROM {groups} WHERE id = $1 AND tenant_id = $2");
        let updated_at: i64 = sqlx::query(sqlx::AssertSqlSafe(group_sql))
            .bind(group_id.to_string())
            .bind(&tenant_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("updated_at")?;
        if updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the group before changing its members".into(),
            ));
        }
        let old_route_ids = if kind == GroupKind::Route {
            select_route_group_members(&mut tx, &tenant_id, group_id).await?
        } else {
            Vec::new()
        };
        require_members_tenant(&mut tx, kind.member_table(), &member_ids, &tenant_id).await?;
        let delete_sql =
            format!("DELETE FROM {memberships} WHERE tenant_id = $1 AND {group_column} = $2");
        sqlx::query(sqlx::AssertSqlSafe(delete_sql))
            .bind(&tenant_id)
            .bind(group_id.to_string())
            .execute(&mut *tx)
            .await?;
        let now = unix_millis().max(updated_at.saturating_add(1));
        insert_members(&mut tx, kind, &tenant_id, group_id, &member_ids, now).await?;
        let update_sql = format!(
            "UPDATE {groups} SET updated_at = $1 WHERE id = $2 AND tenant_id = $3 AND updated_at = $4"
        );
        let changed = sqlx::query(sqlx::AssertSqlSafe(update_sql))
            .bind(now)
            .bind(group_id.to_string())
            .bind(&tenant_id)
            .bind(updated_at)
            .execute(&mut *tx)
            .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the group before changing its members".into(),
            ));
        }
        if kind == GroupKind::Route {
            let affected_route_ids = old_route_ids
                .into_iter()
                .chain(member_ids.iter().copied())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            bump_model_route_relation_timestamps(&mut tx, &tenant_id, &affected_route_ids, now)
                .await?;
        }
        tx.commit().await?;
        self.group(kind, group_id, &input.tenant_external_id).await
    }
}

async fn select_route_group_members(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    group_id: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query(
        "SELECT model_route_id AS id FROM model_route_group_memberships \
         WHERE tenant_id = $1 AND route_group_id = $2 ORDER BY model_route_id",
    )
    .bind(tenant_id)
    .bind(group_id.to_string())
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| super::super::parse_uuid(row.try_get("id")?))
    .collect()
}

fn bounded_unique_ids(ids: Vec<Uuid>) -> Result<Vec<Uuid>, AppError> {
    if ids.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "group members cannot contain more than {MAX_GROUP_MEMBERS} entries"
        )));
    }
    Ok(ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

async fn require_members_tenant(
    tx: &mut Transaction<'_, Any>,
    table: &str,
    ids: &[Uuid],
    tenant_id: &str,
) -> Result<(), AppError> {
    let prefix = match table {
        "upstream_accounts" => {
            "SELECT COUNT(*) AS found FROM upstream_accounts WHERE tenant_id = $1 AND id IN "
        }
        "model_routes" => {
            "SELECT COUNT(*) AS found FROM model_routes WHERE tenant_id = $1 AND id IN "
        }
        "key_records" => {
            "SELECT COUNT(*) AS found FROM key_records WHERE tenant_id = $1 AND id IN "
        }
        _ => return Err(AppError::Internal),
    };
    for chunk in ids.chunks(MEMBERSHIP_CHUNK_SIZE) {
        let sql = format!("{prefix}{}", placeholder_tuple(2, chunk.len()));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(tenant_id);
        for id in chunk {
            query = query.bind(id.to_string());
        }
        let found: i64 = query.fetch_one(&mut **tx).await?.try_get("found")?;
        if found != i64::try_from(chunk.len()).map_err(|_| AppError::Internal)? {
            return Err(AppError::NotFound);
        }
    }
    Ok(())
}

async fn insert_members(
    tx: &mut Transaction<'_, Any>,
    kind: GroupKind,
    tenant_id: &str,
    group_id: Uuid,
    member_ids: &[Uuid],
    now: i64,
) -> Result<(), AppError> {
    let prefix = match kind {
        GroupKind::Provider => {
            "INSERT INTO upstream_account_provider_groups (tenant_id, provider_group_id, upstream_account_id, created_at) VALUES "
        }
        GroupKind::Route => {
            "INSERT INTO model_route_group_memberships (tenant_id, route_group_id, model_route_id, created_at) VALUES "
        }
        GroupKind::Credential => {
            "INSERT INTO credential_group_memberships (tenant_id, credential_group_id, key_id, created_at) VALUES "
        }
    };
    for chunk in member_ids.chunks(MEMBERSHIP_CHUNK_SIZE) {
        let sql = format!("{prefix}{}", values_placeholders(4, chunk.len()));
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for member_id in chunk {
            query = query
                .bind(tenant_id)
                .bind(group_id.to_string())
                .bind(member_id.to_string())
                .bind(now);
        }
        query.execute(&mut **tx).await?;
    }
    Ok(())
}

fn placeholder_tuple(first: usize, count: usize) -> String {
    let placeholders = (first..first + count)
        .map(|index| format!("${index}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("({placeholders})")
}

fn values_placeholders(width: usize, rows: usize) -> String {
    (0..rows)
        .map(|row| placeholder_tuple(row * width + 1, width))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_hundred_members_use_ten_bounded_statements() {
        let validation_statements = MAX_GROUP_MEMBERS.div_ceil(MEMBERSHIP_CHUNK_SIZE);
        let insert_statements = MAX_GROUP_MEMBERS.div_ceil(MEMBERSHIP_CHUNK_SIZE);
        assert_eq!(validation_statements + insert_statements, 10);
        assert_eq!(
            values_placeholders(4, 2),
            "($1, $2, $3, $4), ($5, $6, $7, $8)"
        );
    }
}
