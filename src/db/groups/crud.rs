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
        let sql = list_groups_sql(kind);
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant_external_id)
            .fetch_all(&self.pool)
            .await?;
        let (groups, memberships, group_column) = kind.tables();
        let member_column = kind.member_column();
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
            route_reference_count: 0,
            enabled_route_reference_count: 0,
            credential_grant_count: 0,
            active_credential_grant_count: 0,
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

fn list_groups_sql(kind: GroupKind) -> String {
    let (groups, memberships, group_column) = kind.tables();
    let (impact_ctes, impact_columns, impact_join) = match kind {
        GroupKind::Provider => (
            ",
             provider_group_route_refs AS (
                 SELECT included.tenant_id, included.model_route_id,
                        included.provider_group_id AS group_id
                 FROM model_route_included_provider_groups included
                 JOIN group_page g
                   ON g.tenant_id = included.tenant_id
                  AND g.id = included.provider_group_id
                 UNION
                 SELECT excluded.tenant_id, excluded.model_route_id,
                        excluded.provider_group_id AS group_id
                 FROM model_route_excluded_provider_groups excluded
                 JOIN group_page g
                   ON g.tenant_id = excluded.tenant_id
                  AND g.id = excluded.provider_group_id
             ),
             impact_counts AS (
                 SELECT route_ref.tenant_id, route_ref.group_id,
                        COUNT(*) AS route_reference_count,
                        SUM(CASE WHEN route.enabled = 1 THEN 1 ELSE 0 END)
                            AS enabled_route_reference_count
                 FROM provider_group_route_refs route_ref
                 JOIN model_routes route
                   ON route.tenant_id = route_ref.tenant_id
                  AND route.id = route_ref.model_route_id
                 GROUP BY route_ref.tenant_id, route_ref.group_id
             )",
            "COALESCE(impact.route_reference_count, CAST(0 AS BIGINT))
                 AS route_reference_count,
             COALESCE(impact.enabled_route_reference_count, CAST(0 AS BIGINT))
                 AS enabled_route_reference_count,
             CAST(0 AS BIGINT) AS credential_grant_count,
             CAST(0 AS BIGINT) AS active_credential_grant_count",
            "LEFT JOIN impact_counts impact
               ON impact.tenant_id = g.tenant_id AND impact.group_id = g.id",
        ),
        GroupKind::Route => (
            ",
             impact_counts AS (
                 SELECT grant_row.tenant_id, grant_row.route_group_id AS group_id,
                        COUNT(*) AS credential_grant_count,
                        SUM(CASE WHEN key_record.status = 'active' THEN 1 ELSE 0 END)
                            AS active_credential_grant_count
                 FROM routing_grants grant_row
                 JOIN group_page g
                   ON g.tenant_id = grant_row.tenant_id
                  AND g.id = grant_row.route_group_id
                 LEFT JOIN key_records key_record
                   ON key_record.tenant_id = grant_row.tenant_id
                  AND key_record.id = grant_row.key_id
                 WHERE grant_row.route_group_id IS NOT NULL
                 GROUP BY grant_row.tenant_id, grant_row.route_group_id
             )",
            "CAST(0 AS BIGINT) AS route_reference_count,
             CAST(0 AS BIGINT) AS enabled_route_reference_count,
             COALESCE(impact.credential_grant_count, CAST(0 AS BIGINT))
                 AS credential_grant_count,
             COALESCE(impact.active_credential_grant_count, CAST(0 AS BIGINT))
                 AS active_credential_grant_count",
            "LEFT JOIN impact_counts impact
               ON impact.tenant_id = g.tenant_id AND impact.group_id = g.id",
        ),
        GroupKind::Credential => (
            "",
            "CAST(0 AS BIGINT) AS route_reference_count,
             CAST(0 AS BIGINT) AS enabled_route_reference_count,
             CAST(0 AS BIGINT) AS credential_grant_count,
             CAST(0 AS BIGINT) AS active_credential_grant_count",
            "",
        ),
    };
    format!(
        "WITH selected_tenant AS (
             SELECT id, external_id FROM tenants WHERE external_id = $1
         ),
         group_page AS MATERIALIZED (
             SELECT g.id, g.tenant_id, tenant.external_id AS tenant_external_id,
                    g.name, g.normalized_name, g.created_at, g.updated_at
             FROM {groups} g
             JOIN selected_tenant tenant ON tenant.id = g.tenant_id
             ORDER BY g.normalized_name ASC, g.id ASC
             LIMIT 500
         ),
         member_counts AS (
             SELECT m.tenant_id, m.{group_column} AS group_id, COUNT(*) AS member_count
             FROM {memberships} m
             JOIN group_page g
               ON g.tenant_id = m.tenant_id AND g.id = m.{group_column}
             GROUP BY m.tenant_id, m.{group_column}
         )
         {impact_ctes}
         SELECT g.id, g.tenant_id, g.tenant_external_id,
                g.name, g.created_at, g.updated_at,
                COALESCE(member_counts.member_count, CAST(0 AS BIGINT)) AS member_count,
                {impact_columns}
         FROM group_page g
         LEFT JOIN member_counts
           ON member_counts.tenant_id = g.tenant_id AND member_counts.group_id = g.id
         {impact_join}
         ORDER BY g.normalized_name ASC, g.id ASC"
    )
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
        route_reference_count: row.try_get("route_reference_count")?,
        enabled_route_reference_count: row.try_get("enabled_route_reference_count")?,
        credential_grant_count: row.try_get("credential_grant_count")?,
        active_credential_grant_count: row.try_get("active_credential_grant_count")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use serde_json::json;

    use super::*;
    use crate::db::{CreateKeyInput, CreateModelRouteInput, CreateUpstreamAccountInput};
    use crate::model::KeyPolicy;
    use crate::provider::UpstreamCredential;

    #[test]
    fn provider_impact_query_is_set_based_for_the_bounded_group_page() {
        let sql = list_groups_sql(GroupKind::Provider);

        assert_eq!(sql.matches("JOIN model_routes route").count(), 1);
        assert_eq!(
            sql.matches("model_route_included_provider_groups").count(),
            1
        );
        assert_eq!(
            sql.matches("model_route_excluded_provider_groups").count(),
            1
        );
        assert!(sql.contains("provider_group_route_refs"));
        assert!(sql.contains(" UNION\n"));
        assert!(
            sql.find("LIMIT 500").unwrap() < sql.find("provider_group_route_refs").unwrap(),
            "the bounded group page must be selected before impact aggregation"
        );
        assert!(!sql.contains("EXISTS"));
        assert!(!sql.contains("provider_group_id = g.id"));
    }

    #[tokio::test]
    async fn grouped_impact_counts_deduplicate_routes_and_preserve_enabled_and_active_subsets() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("group-impact-counts.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let tenant = "group-impact-counts";
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "impact-account".to_owned(),
                    driver: "http-json".to_owned(),
                    config: json!({"base_url": "https://impact.example.test"}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                b"group impact counts test key material",
            )
            .await
            .unwrap();
        let provider_group = database
            .create_group(
                GroupKind::Provider,
                CreateGroupInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "Referenced provider group".to_owned(),
                },
            )
            .await
            .unwrap();
        let other_provider_group = database
            .create_group(
                GroupKind::Provider,
                CreateGroupInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "Other provider group".to_owned(),
                },
            )
            .await
            .unwrap();
        let route_group = database
            .create_group(
                GroupKind::Route,
                CreateGroupInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "Granted route group".to_owned(),
                },
            )
            .await
            .unwrap();
        let mut routes = Vec::new();
        for index in 0..3 {
            routes.push(
                database
                    .create_model_route(CreateModelRouteInput {
                        tenant_external_id: tenant.to_owned(),
                        public_model: format!("impact-model-{index}"),
                        upstream_account_id: account.id,
                        upstream_model: format!("impact-upstream-{index}"),
                        protocol: "openai".to_owned(),
                        priority: index,
                    })
                    .await
                    .unwrap(),
            );
        }
        routes[1] = database
            .set_model_route_enabled(routes[1].id, tenant, false, routes[1].updated_at)
            .await
            .unwrap();
        let tenant_id = account.tenant_id.to_string();
        for (table, route_id, group_id) in [
            (
                "model_route_included_provider_groups",
                routes[0].id,
                provider_group.id,
            ),
            (
                "model_route_excluded_provider_groups",
                routes[0].id,
                provider_group.id,
            ),
            (
                "model_route_included_provider_groups",
                routes[1].id,
                provider_group.id,
            ),
            (
                "model_route_excluded_provider_groups",
                routes[2].id,
                other_provider_group.id,
            ),
        ] {
            let sql = format!(
                "INSERT INTO {table} \
                 (tenant_id, model_route_id, provider_group_id, created_at) \
                 VALUES ($1, $2, $3, 1)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&tenant_id)
                .bind(route_id.to_string())
                .bind(group_id.to_string())
                .execute(&database.pool)
                .await
                .unwrap();
        }

        let active_key = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.to_owned(),
                    principal_external_id: "active-principal".to_owned(),
                    alias: "active-key".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                b"group impact counts downstream pepper",
            )
            .await
            .unwrap();
        let suspended_key = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.to_owned(),
                    principal_external_id: "suspended-principal".to_owned(),
                    alias: "suspended-key".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                b"group impact counts downstream pepper",
            )
            .await
            .unwrap();
        database
            .set_key_status(suspended_key.key_id, "suspended")
            .await
            .unwrap();
        for key_id in [active_key.key_id, suspended_key.key_id] {
            sqlx::query(
                "INSERT INTO routing_grants \
                 (tenant_id, key_id, model_route_id, route_group_id, created_at) \
                 VALUES ($1, $2, NULL, $3, 1)",
            )
            .bind(&tenant_id)
            .bind(key_id.to_string())
            .bind(route_group.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO routing_grants \
             (tenant_id, key_id, model_route_id, route_group_id, created_at) \
             VALUES ($1, $2, $3, NULL, 1)",
        )
        .bind(&tenant_id)
        .bind(active_key.key_id.to_string())
        .bind(routes[0].id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();

        let provider_groups = database
            .list_groups(GroupKind::Provider, tenant)
            .await
            .unwrap();
        let referenced = provider_groups
            .iter()
            .find(|group| group.id == provider_group.id)
            .unwrap();
        assert_eq!(referenced.route_reference_count, 2);
        assert_eq!(referenced.enabled_route_reference_count, 1);
        let other = provider_groups
            .iter()
            .find(|group| group.id == other_provider_group.id)
            .unwrap();
        assert_eq!(other.route_reference_count, 1);
        assert_eq!(other.enabled_route_reference_count, 1);

        let route_groups = database
            .list_groups(GroupKind::Route, tenant)
            .await
            .unwrap();
        let granted = route_groups
            .iter()
            .find(|group| group.id == route_group.id)
            .unwrap();
        assert_eq!(granted.credential_grant_count, 2);
        assert_eq!(granted.active_credential_grant_count, 1);
    }
}
