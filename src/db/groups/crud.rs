use std::collections::BTreeMap;

use sqlx::{Row, any::AnyRow};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid, unix_millis};
use super::types::{CreateGroupInput, GroupKind, GroupView, UpdateGroupInput};

const MAX_GROUP_LIST_MEMBERS: usize = 10_000;

impl Database {
    pub async fn list_groups(
        &self,
        kind: GroupKind,
        tenant_external_id: &str,
    ) -> Result<Vec<GroupView>, AppError> {
        let (groups, memberships, group_column) = kind.tables();
        let member_column = kind.member_column();
        let sql = format!(
            "SELECT g.id, g.tenant_id, t.external_id AS tenant_external_id, g.name, g.created_at, g.updated_at, (SELECT COUNT(*) FROM {memberships} m WHERE m.tenant_id = g.tenant_id AND m.{group_column} = g.id) AS member_count FROM {groups} g JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $1 ORDER BY g.normalized_name ASC, g.id ASC LIMIT 500"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant_external_id)
            .fetch_all(&self.pool)
            .await?;
        let member_sql = format!(
            "SELECT m.{group_column} AS group_id, m.{member_column} AS member_id FROM {memberships} m JOIN {groups} g ON g.id = m.{group_column} AND g.tenant_id = m.tenant_id JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $1 ORDER BY m.{group_column}, m.{member_column} LIMIT {}",
            MAX_GROUP_LIST_MEMBERS + 1
        );
        let member_rows = sqlx::query(sqlx::AssertSqlSafe(member_sql))
            .bind(tenant_external_id)
            .fetch_all(&self.pool)
            .await?;
        if member_rows.len() > MAX_GROUP_LIST_MEMBERS {
            return Err(AppError::BadRequest(
                "group membership response is too large; narrow the tenant data set".into(),
            ));
        }
        let mut members = BTreeMap::<Uuid, Vec<Uuid>>::new();
        for row in member_rows {
            members
                .entry(parse_uuid(row.try_get("group_id")?)?)
                .or_default()
                .push(parse_uuid(row.try_get("member_id")?)?);
        }
        rows.into_iter()
            .map(|row| {
                let id = parse_uuid(row.try_get("id")?)?;
                group_view(row, members.remove(&id).unwrap_or_default())
            })
            .collect()
    }

    pub async fn create_group(
        &self,
        kind: GroupKind,
        input: CreateGroupInput,
    ) -> Result<GroupView, AppError> {
        let (name, normalized_name) = normalize_group_name(&input.name)?;
        let (groups, _, _) = kind.tables();
        let id = Uuid::now_v7();
        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        let sql = format!(
            "INSERT INTO {groups} (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, normalized_name) DO NOTHING"
        );
        let inserted = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.to_string())
            .bind(&tenant_id)
            .bind(&name)
            .bind(&normalized_name)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        if inserted.rows_affected() == 0 {
            return Err(AppError::Conflict(format!(
                "a {} group with this name already exists",
                kind.name()
            )));
        }
        tx.commit().await?;
        Ok(GroupView {
            id,
            tenant_id: parse_uuid(tenant_id)?,
            tenant_external_id: input.tenant_external_id,
            name,
            member_ids: Vec::new(),
            member_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn update_group(
        &self,
        kind: GroupKind,
        group_id: Uuid,
        input: UpdateGroupInput,
    ) -> Result<GroupView, AppError> {
        let (name, normalized_name) = normalize_group_name(&input.name)?;
        let (groups, _, _) = kind.tables();
        let now = unix_millis().max(input.expected_updated_at.saturating_add(1));
        let sql = format!(
            "UPDATE {groups} SET name = $1, normalized_name = $2, updated_at = $3 WHERE id = $4 AND tenant_id = (SELECT id FROM tenants WHERE external_id = $5) AND updated_at = $6"
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(name)
            .bind(normalized_name)
            .bind(now)
            .bind(group_id.to_string())
            .bind(&input.tenant_external_id)
            .bind(input.expected_updated_at)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            self.require_group(kind, group_id, &input.tenant_external_id)
                .await?;
            return Err(AppError::Conflict(
                "reload the group before saving it again".into(),
            ));
        }
        self.group(kind, group_id, &input.tenant_external_id).await
    }

    pub async fn delete_group(
        &self,
        kind: GroupKind,
        group_id: Uuid,
        tenant_external_id: &str,
        expected_updated_at: i64,
    ) -> Result<(), AppError> {
        let (groups, memberships, group_column) = kind.tables();
        let dependent = match kind {
            GroupKind::Provider => "EXISTS(SELECT 1 FROM model_route_included_provider_groups WHERE provider_group_id = $1) OR EXISTS(SELECT 1 FROM model_route_excluded_provider_groups WHERE provider_group_id = $1)".to_owned(),
            GroupKind::Route => "EXISTS(SELECT 1 FROM routing_grants WHERE route_group_id = $1)".to_owned(),
            GroupKind::Credential => "0 = 1".to_owned(),
        };
        let sql = format!(
            "DELETE FROM {groups} WHERE id = $1 AND tenant_id = (SELECT id FROM tenants WHERE external_id = $2) AND updated_at = $3 AND NOT EXISTS(SELECT 1 FROM {memberships} WHERE {group_column} = $1) AND NOT ({dependent})"
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(group_id.to_string())
            .bind(tenant_external_id)
            .bind(expected_updated_at)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            self.require_group(kind, group_id, tenant_external_id)
                .await?;
            return Err(AppError::Conflict(
                "remove the group from members and routing rules before deleting it".into(),
            ));
        }
        Ok(())
    }

    pub(super) async fn group(
        &self,
        kind: GroupKind,
        group_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<GroupView, AppError> {
        self.list_groups(kind, tenant_external_id)
            .await?
            .into_iter()
            .find(|group| group.id == group_id)
            .ok_or(AppError::NotFound)
    }

    async fn require_group(
        &self,
        kind: GroupKind,
        group_id: Uuid,
        tenant: &str,
    ) -> Result<(), AppError> {
        let (groups, _, _) = kind.tables();
        let sql = format!(
            "SELECT g.id FROM {groups} g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = $1 AND t.external_id = $2"
        );
        if sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(group_id.to_string())
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await?
            .is_none()
        {
            Err(AppError::NotFound)
        } else {
            Ok(())
        }
    }
}

fn normalize_group_name(raw: &str) -> Result<(String, String), AppError> {
    let name = raw.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "group name must contain 1 to 100 non-control bytes".into(),
        ));
    }
    Ok((name.to_owned(), name.to_lowercase()))
}

fn group_view(row: AnyRow, member_ids: Vec<Uuid>) -> Result<GroupView, AppError> {
    Ok(GroupView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        name: row.try_get("name")?,
        member_count: row.try_get("member_count")?,
        member_ids,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}
