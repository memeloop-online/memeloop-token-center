use std::collections::BTreeSet;

use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid, unix_millis};
use super::grant_revisions::{
    bump_route_grant_revisions, compare_and_bump_credential_grant_revision,
    credential_grant_revision, lock_routing_relation_writes,
};
use super::types::{CredentialRoutingView, ReplaceCredentialRoutingInput};

const MAX_GRANT_MEMBERS: usize = 500;

impl Database {
    pub async fn credential_routing(
        &self,
        key_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<CredentialRoutingView, AppError> {
        let tenant_id: String = sqlx::query(
            "SELECT k.tenant_id FROM key_records k \
             JOIN tenants t ON t.id = k.tenant_id \
             WHERE k.id = $1 AND t.external_id = $2",
        )
        .bind(key_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("tenant_id")?;
        self.credential_routing_view(key_id, &tenant_id).await
    }

    pub async fn replace_credential_routing(
        &self,
        key_id: Uuid,
        input: ReplaceCredentialRoutingInput,
    ) -> Result<CredentialRoutingView, AppError> {
        let route_ids = bounded_unique_ids(input.route_ids, "routes")?;
        let route_group_ids = bounded_unique_ids(input.route_group_ids, "route groups")?;
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, &input.tenant_external_id).await?;
        let key_exists = sqlx::query("SELECT 1 FROM key_records WHERE id = $1 AND tenant_id = $2")
            .bind(key_id.to_string())
            .bind(&tenant_id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some();
        if !key_exists {
            return Err(AppError::NotFound);
        }
        require_members_tenant(&mut tx, "model_routes", &route_ids, &tenant_id).await?;
        require_members_tenant(&mut tx, "route_groups", &route_group_ids, &tenant_id).await?;

        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let old_route_ids =
            select_key_grant_ids_in_transaction(&mut tx, &tenant_id, key_id, true).await?;
        let old_route_group_ids =
            select_key_grant_ids_in_transaction(&mut tx, &tenant_id, key_id, false).await?;
        let direct_grants_changed = !same_ids(&old_route_ids, &route_ids)
            || !same_ids(&old_route_group_ids, &route_group_ids);
        compare_and_bump_credential_grant_revision(
            &mut tx,
            &tenant_id,
            key_id,
            input.expected_grant_revision,
            direct_grants_changed,
        )
        .await?;
        if !direct_grants_changed {
            tx.commit().await?;
            return self.credential_routing_view(key_id, &tenant_id).await;
        }

        sqlx::query("DELETE FROM routing_grants WHERE tenant_id = $1 AND key_id = $2")
            .bind(&tenant_id)
            .bind(key_id.to_string())
            .execute(&mut *tx)
            .await?;
        let now = unix_millis();
        for route_id in &route_ids {
            sqlx::query("INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ($1, $2, $3, NULL, $4)")
                .bind(&tenant_id).bind(key_id.to_string()).bind(route_id.to_string()).bind(now).execute(&mut *tx).await?;
        }
        for group_id in &route_group_ids {
            sqlx::query("INSERT INTO routing_grants (tenant_id, key_id, model_route_id, route_group_id, created_at) VALUES ($1, $2, NULL, $3, $4)")
                .bind(&tenant_id).bind(key_id.to_string()).bind(group_id.to_string()).bind(now).execute(&mut *tx).await?;
        }
        let old_route_ids = old_route_ids.into_iter().collect::<BTreeSet<_>>();
        let new_route_ids = route_ids.iter().copied().collect::<BTreeSet<_>>();
        let affected_route_ids = old_route_ids
            .symmetric_difference(&new_route_ids)
            .copied()
            .collect::<Vec<_>>();
        bump_route_grant_revisions(&mut tx, &tenant_id, &affected_route_ids).await?;

        tx.commit().await?;
        self.credential_routing_view(key_id, &tenant_id).await
    }

    async fn credential_routing_view(
        &self,
        key_id: Uuid,
        tenant_id: &str,
    ) -> Result<CredentialRoutingView, AppError> {
        let route_ids = select_key_grant_ids(&self.pool, tenant_id, key_id, true).await?;
        let route_group_ids = select_key_grant_ids(&self.pool, tenant_id, key_id, false).await?;
        let rows = sqlx::query("SELECT model_route_id AS id FROM routing_grants WHERE tenant_id = $1 AND key_id = $2 AND model_route_id IS NOT NULL UNION SELECT m.model_route_id AS id FROM routing_grants g JOIN model_route_group_memberships m ON m.tenant_id = g.tenant_id AND m.route_group_id = g.route_group_id WHERE g.tenant_id = $1 AND g.key_id = $2 AND g.route_group_id IS NOT NULL ORDER BY id")
            .bind(tenant_id).bind(key_id.to_string()).fetch_all(&self.pool).await?;
        let effective_route_ids = rows
            .into_iter()
            .map(|row| parse_uuid(row.try_get("id")?))
            .collect::<Result<Vec<_>, _>>()?;
        let updated_at: i64 =
            sqlx::query("SELECT updated_at FROM key_records WHERE id = $1 AND tenant_id = $2")
                .bind(key_id.to_string())
                .bind(tenant_id)
                .fetch_one(&self.pool)
                .await?
                .try_get("updated_at")?;
        let grant_revision = credential_grant_revision(&self.pool, tenant_id, key_id).await?;
        Ok(CredentialRoutingView {
            key_id,
            route_ids,
            route_group_ids,
            effective_route_ids,
            updated_at,
            grant_revision,
        })
    }
}

async fn tenant_id(tx: &mut Transaction<'_, Any>, external_id: &str) -> Result<String, AppError> {
    sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
        .bind(external_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("id")
        .map_err(Into::into)
}

async fn require_members_tenant(
    tx: &mut Transaction<'_, Any>,
    table: &str,
    ids: &[Uuid],
    tenant_id: &str,
) -> Result<(), AppError> {
    let sql = match table {
        "route_groups" => "SELECT 1 FROM route_groups WHERE tenant_id = $1 AND id = $2",
        "model_routes" => "SELECT 1 FROM model_routes WHERE tenant_id = $1 AND id = $2",
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

async fn select_key_grant_ids(
    pool: &sqlx::AnyPool,
    tenant_id: &str,
    key_id: Uuid,
    exact: bool,
) -> Result<Vec<Uuid>, AppError> {
    let (column, predicate) = grant_column_and_predicate(exact);
    let sql = format!(
        "SELECT {column} AS id FROM routing_grants WHERE tenant_id = $1 AND key_id = $2 AND {predicate} ORDER BY {column}"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(tenant_id)
        .bind(key_id.to_string())
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| parse_uuid(row.try_get("id")?))
        .collect()
}

async fn select_key_grant_ids_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    key_id: Uuid,
    exact: bool,
) -> Result<Vec<Uuid>, AppError> {
    let (column, predicate) = grant_column_and_predicate(exact);
    let sql = format!(
        "SELECT {column} AS id FROM routing_grants WHERE tenant_id = $1 AND key_id = $2 AND {predicate} ORDER BY {column}"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(tenant_id)
        .bind(key_id.to_string())
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| parse_uuid(row.try_get("id")?))
        .collect()
}

fn grant_column_and_predicate(exact: bool) -> (&'static str, &'static str) {
    if exact {
        ("model_route_id", "model_route_id IS NOT NULL")
    } else {
        ("route_group_id", "route_group_id IS NOT NULL")
    }
}

fn bounded_unique_ids(ids: Vec<Uuid>, label: &str) -> Result<Vec<Uuid>, AppError> {
    if ids.len() > MAX_GRANT_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "{label} cannot contain more than {MAX_GRANT_MEMBERS} entries"
        )));
    }
    Ok(ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn same_ids(left: &[Uuid], right: &[Uuid]) -> bool {
    left.iter().copied().collect::<BTreeSet<_>>() == right.iter().copied().collect::<BTreeSet<_>>()
}
