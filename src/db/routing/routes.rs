use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid, unix_millis};
use super::associations::{
    RouteAssociationReplacement, candidate_upstream_ids, ensure_route_has_eligible_candidate,
    finish_route_relation_replace, replace_route_associations, require_members_tenant,
    route_relation_snapshot, select_ids, tenant_id,
};
use super::grant_revisions::{
    compare_and_bump_route_grant_revision, lock_routing_relation_writes, route_grant_revision,
};
use super::types::{
    CreateRoutedModelRouteInput, ReplaceRouteRoutingInput, RouteRoutingView,
    UpdateRoutedModelRouteInput,
};
use crate::provider::ModelRouteView;

const MAX_GROUP_NAME_BYTES: usize = 100;
const MAX_GROUP_MEMBERS: usize = 500;

impl Database {
    pub async fn update_routed_model_route(
        &self,
        route_id: Uuid,
        input: UpdateRoutedModelRouteInput,
    ) -> Result<(ModelRouteView, RouteRoutingView), AppError> {
        validate_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let upstream_ids = bounded_unique_ids(input.upstream_account_ids, "upstream accounts")?;
        let included = bounded_unique_ids(
            input.included_provider_group_ids,
            "included provider groups",
        )?;
        let excluded = bounded_unique_ids(
            input.excluded_provider_group_ids,
            "excluded provider groups",
        )?;
        let mut route_groups = bounded_unique_ids(input.route_group_ids, "route groups")?;
        let credential_ids = bounded_unique_ids(input.granted_credential_ids, "credentials")?;
        let route_group_names = bounded_group_names(input.route_group_names)?;
        if upstream_ids.is_empty() && included.is_empty() {
            return Err(AppError::BadRequest(
                "a route requires an explicit upstream account or included provider group".into(),
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, &input.tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let current = sqlx::query("SELECT public_model, upstream_model, protocol, priority, created_at, enabled, updated_at FROM model_routes WHERE id = $1 AND tenant_id = $2")
            .bind(route_id.to_string()).bind(&tenant_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        let current_updated_at: i64 = current.try_get("updated_at")?;
        if current_updated_at != input.expected_updated_at {
            let base_is_replay = current.try_get::<String, _>("public_model")?
                == input.public_model.trim()
                && current.try_get::<String, _>("upstream_model")? == input.upstream_model.trim()
                && current.try_get::<String, _>("protocol")? == input.protocol
                && current.try_get::<i64, _>("priority")? == input.priority;
            let created_at: i64 = current.try_get("created_at")?;
            let enabled = current.try_get::<i64, _>("enabled")? != 0;
            let names_resolved = resolve_existing_route_group_names(
                &mut tx,
                &tenant_id,
                &route_group_names,
                &mut route_groups,
            )
            .await?;
            route_groups = bounded_unique_ids(route_groups, "route groups")?;
            if base_is_replay
                && names_resolved
                && route_relations_match_in_transaction(
                    &mut tx,
                    &tenant_id,
                    route_id,
                    &upstream_ids,
                    &included,
                    &excluded,
                    &route_groups,
                    &credential_ids,
                    input.custom_model_confirmed,
                )
                .await?
            {
                tx.commit().await?;
                let routing = self
                    .route_routing(route_id, &input.tenant_external_id)
                    .await?;
                return Ok((
                    ModelRouteView {
                        id: route_id,
                        tenant_id: parse_uuid(tenant_id)?,
                        tenant_external_id: Some(input.tenant_external_id),
                        public_model: input.public_model.trim().to_owned(),
                        upstream_account_id: upstream_ids
                            .first()
                            .copied()
                            .unwrap_or_else(Uuid::nil),
                        upstream_model: input.upstream_model.trim().to_owned(),
                        protocol: input.protocol,
                        priority: input.priority,
                        enabled,
                        created_at,
                        updated_at: current_updated_at,
                    },
                    routing,
                ));
            }
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        require_members_tenant(&mut tx, "upstream_accounts", &upstream_ids, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &included, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &excluded, &tenant_id).await?;
        require_members_tenant(&mut tx, "route_groups", &route_groups, &tenant_id).await?;
        require_members_tenant(&mut tx, "key_records", &credential_ids, &tenant_id).await?;
        let now = unix_millis().max(current_updated_at.saturating_add(1));
        create_or_find_route_groups(
            &mut tx,
            &tenant_id,
            route_group_names,
            &mut route_groups,
            now,
        )
        .await?;
        route_groups = bounded_unique_ids(route_groups, "route groups")?;
        let old_relations = route_relation_snapshot(&mut tx, &tenant_id, route_id).await?;
        let direct_grants_changed = !same_ids(&old_relations.credential_ids, &credential_ids);
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            input.expected_grant_revision,
            direct_grants_changed,
        )
        .await?;
        let legacy_account_id = upstream_ids.first().copied().unwrap_or_else(Uuid::nil);
        let changed = sqlx::query("UPDATE model_routes SET public_model = $1, upstream_account_id = $2, upstream_model = $3, protocol = $4, priority = $5, updated_at = $6 WHERE id = $7 AND tenant_id = $8 AND updated_at = $9")
            .bind(input.public_model.trim()).bind(legacy_account_id.to_string()).bind(input.upstream_model.trim())
            .bind(&input.protocol).bind(input.priority).bind(now).bind(route_id.to_string()).bind(&tenant_id).bind(current_updated_at)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before saving it again".into(),
            ));
        }
        replace_route_associations(
            &mut tx,
            RouteAssociationReplacement {
                route_id,
                tenant_id: &tenant_id,
                upstream_model: input.upstream_model.trim(),
                upstream_ids: &upstream_ids,
                included_provider_group_ids: &included,
                excluded_provider_group_ids: &excluded,
                route_group_ids: &route_groups,
                credential_ids: &credential_ids,
                custom_model_confirmed: input.custom_model_confirmed,
            },
        )
        .await?;
        finish_route_relation_replace(
            &mut tx,
            &tenant_id,
            old_relations,
            &credential_ids,
            &route_groups,
            now,
        )
        .await?;
        ensure_route_has_eligible_candidate(&mut tx, &tenant_id, route_id).await?;
        let created_at: i64 = current.try_get("created_at")?;
        let enabled = current.try_get::<i64, _>("enabled")? != 0;
        tx.commit().await?;
        let route = ModelRouteView {
            id: route_id,
            tenant_id: parse_uuid(tenant_id.clone())?,
            tenant_external_id: Some(input.tenant_external_id),
            public_model: input.public_model.trim().to_owned(),
            upstream_account_id: legacy_account_id,
            upstream_model: input.upstream_model.trim().to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled,
            created_at,
            updated_at: now,
        };
        let routing = self.route_routing_view(route_id, &tenant_id, now).await?;
        Ok((route, routing))
    }

    pub async fn create_routed_model_route(
        &self,
        input: CreateRoutedModelRouteInput,
    ) -> Result<(ModelRouteView, RouteRoutingView), AppError> {
        validate_route_fields(
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
        )?;
        let upstream_ids = bounded_unique_ids(input.upstream_account_ids, "upstream accounts")?;
        let included = bounded_unique_ids(
            input.included_provider_group_ids,
            "included provider groups",
        )?;
        let excluded = bounded_unique_ids(
            input.excluded_provider_group_ids,
            "excluded provider groups",
        )?;
        let mut route_groups = bounded_unique_ids(input.route_group_ids, "route groups")?;
        let credential_ids = bounded_unique_ids(input.granted_credential_ids, "credentials")?;
        if upstream_ids.is_empty() && included.is_empty() {
            return Err(AppError::BadRequest(
                "a route requires an explicit upstream account or included provider group".into(),
            ));
        }
        let route_group_names = bounded_group_names(input.route_group_names)?;
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, &input.tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        require_members_tenant(&mut tx, "upstream_accounts", &upstream_ids, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &included, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &excluded, &tenant_id).await?;
        require_members_tenant(&mut tx, "route_groups", &route_groups, &tenant_id).await?;
        require_members_tenant(&mut tx, "key_records", &credential_ids, &tenant_id).await?;
        let now = unix_millis();
        create_or_find_route_groups(
            &mut tx,
            &tenant_id,
            route_group_names,
            &mut route_groups,
            now,
        )
        .await?;
        route_groups = bounded_unique_ids(route_groups, "route groups")?;
        if let Some(existing) = find_equivalent_route_in_transaction(
            &mut tx,
            &tenant_id,
            &input.tenant_external_id,
            &input.public_model,
            &input.upstream_model,
            &input.protocol,
            input.priority,
            input.custom_model_confirmed,
            &upstream_ids,
            &included,
            &excluded,
            &route_groups,
            &credential_ids,
        )
        .await?
        {
            let route_id = existing.id;
            tx.commit().await?;
            let routing = self
                .route_routing(route_id, &input.tenant_external_id)
                .await?;
            return Ok((existing, routing));
        }
        let route_id = Uuid::now_v7();
        // This compatibility column is no longer used for candidate selection.
        // A nil UUID explicitly represents a provider-group-only route.
        let legacy_account_id = upstream_ids.first().copied().unwrap_or_else(Uuid::nil);
        sqlx::query("INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9)")
            .bind(route_id.to_string()).bind(&tenant_id).bind(input.public_model.trim())
            .bind(legacy_account_id.to_string()).bind(input.upstream_model.trim())
            .bind(&input.protocol).bind(input.priority).bind(now).bind(now)
            .execute(&mut *tx).await?;
        let old_relations = route_relation_snapshot(&mut tx, &tenant_id, route_id).await?;
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            0,
            !credential_ids.is_empty(),
        )
        .await?;
        replace_route_associations(
            &mut tx,
            RouteAssociationReplacement {
                route_id,
                tenant_id: &tenant_id,
                upstream_model: input.upstream_model.trim(),
                upstream_ids: &upstream_ids,
                included_provider_group_ids: &included,
                excluded_provider_group_ids: &excluded,
                route_group_ids: &route_groups,
                credential_ids: &credential_ids,
                custom_model_confirmed: input.custom_model_confirmed,
            },
        )
        .await?;
        finish_route_relation_replace(
            &mut tx,
            &tenant_id,
            old_relations,
            &credential_ids,
            &route_groups,
            now,
        )
        .await?;
        ensure_route_has_eligible_candidate(&mut tx, &tenant_id, route_id).await?;
        tx.commit().await?;
        let route = ModelRouteView {
            id: route_id,
            tenant_id: parse_uuid(tenant_id.clone())?,
            tenant_external_id: Some(input.tenant_external_id),
            public_model: input.public_model.trim().to_owned(),
            upstream_account_id: legacy_account_id,
            upstream_model: input.upstream_model.trim().to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let routing = self.route_routing_view(route_id, &tenant_id, now).await?;
        Ok((route, routing))
    }

    pub async fn route_routing(
        &self,
        route_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<RouteRoutingView, AppError> {
        let row = sqlx::query(
            "SELECT r.tenant_id, r.updated_at FROM model_routes r JOIN tenants t ON t.id = r.tenant_id WHERE r.id = $1 AND t.external_id = $2",
        )
        .bind(route_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let tenant_id: String = row.try_get("tenant_id")?;
        let updated_at = row.try_get("updated_at")?;
        self.route_routing_view(route_id, &tenant_id, updated_at)
            .await
    }

    pub async fn replace_route_routing(
        &self,
        route_id: Uuid,
        input: ReplaceRouteRoutingInput,
    ) -> Result<RouteRoutingView, AppError> {
        let upstream_ids = bounded_unique_ids(input.upstream_account_ids, "upstream accounts")?;
        let included = bounded_unique_ids(
            input.included_provider_group_ids,
            "included provider groups",
        )?;
        let excluded = bounded_unique_ids(
            input.excluded_provider_group_ids,
            "excluded provider groups",
        )?;
        let mut route_groups = bounded_unique_ids(input.route_group_ids, "route groups")?;
        let credential_ids = bounded_unique_ids(input.granted_credential_ids, "credentials")?;
        if upstream_ids.is_empty() && included.is_empty() {
            return Err(AppError::BadRequest(
                "a route requires an explicit upstream account or included provider group".into(),
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = tenant_id(&mut tx, &input.tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let route = sqlx::query(
            "SELECT upstream_model, updated_at FROM model_routes WHERE id = $1 AND tenant_id = $2",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_updated_at: i64 = route.try_get("updated_at")?;
        if current_updated_at != input.expected_updated_at {
            return Err(AppError::Conflict(
                "reload the model route before saving routing rules".into(),
            ));
        }
        let upstream_model: String = route.try_get("upstream_model")?;
        require_members_tenant(&mut tx, "upstream_accounts", &upstream_ids, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &included, &tenant_id).await?;
        require_members_tenant(&mut tx, "provider_groups", &excluded, &tenant_id).await?;
        require_members_tenant(&mut tx, "route_groups", &route_groups, &tenant_id).await?;
        require_members_tenant(&mut tx, "key_records", &credential_ids, &tenant_id).await?;
        let route_group_names = bounded_group_names(input.route_group_names)?;
        create_or_find_route_groups(
            &mut tx,
            &tenant_id,
            route_group_names,
            &mut route_groups,
            unix_millis(),
        )
        .await?;
        route_groups = bounded_unique_ids(route_groups, "route groups")?;
        let old_relations = route_relation_snapshot(&mut tx, &tenant_id, route_id).await?;
        let direct_grants_changed = !same_ids(&old_relations.credential_ids, &credential_ids);
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            input.expected_grant_revision,
            direct_grants_changed,
        )
        .await?;
        replace_route_associations(
            &mut tx,
            RouteAssociationReplacement {
                route_id,
                tenant_id: &tenant_id,
                upstream_model: &upstream_model,
                upstream_ids: &upstream_ids,
                included_provider_group_ids: &included,
                excluded_provider_group_ids: &excluded,
                route_group_ids: &route_groups,
                credential_ids: &credential_ids,
                custom_model_confirmed: input.custom_model_confirmed,
            },
        )
        .await?;
        ensure_route_has_eligible_candidate(&mut tx, &tenant_id, route_id).await?;
        let now = unix_millis().max(current_updated_at.saturating_add(1));
        finish_route_relation_replace(
            &mut tx,
            &tenant_id,
            old_relations,
            &credential_ids,
            &route_groups,
            now,
        )
        .await?;
        let changed = sqlx::query("UPDATE model_routes SET updated_at = $1 WHERE id = $2 AND tenant_id = $3 AND updated_at = $4")
            .bind(now).bind(route_id.to_string()).bind(&tenant_id).bind(current_updated_at)
            .execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reload the model route before saving routing rules".into(),
            ));
        }
        tx.commit().await?;
        self.route_routing_view(route_id, &tenant_id, now).await
    }

    async fn route_routing_view(
        &self,
        route_id: Uuid,
        tenant_id: &str,
        updated_at: i64,
    ) -> Result<RouteRoutingView, AppError> {
        let upstream_account_ids = select_ids(&self.pool, "SELECT upstream_account_id AS id FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY upstream_account_id", tenant_id, route_id).await?;
        let included_provider_group_ids = select_ids(&self.pool, "SELECT provider_group_id AS id FROM model_route_included_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id", tenant_id, route_id).await?;
        let excluded_provider_group_ids = select_ids(&self.pool, "SELECT provider_group_id AS id FROM model_route_excluded_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id", tenant_id, route_id).await?;
        let route_group_ids = select_ids(&self.pool, "SELECT route_group_id AS id FROM model_route_group_memberships WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY route_group_id", tenant_id, route_id).await?;
        let granted_credential_ids = select_ids(&self.pool, "SELECT key_id AS id FROM routing_grants WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY key_id", tenant_id, route_id).await?;
        let candidate_upstream_account_ids =
            candidate_upstream_ids(&self.pool, tenant_id, route_id).await?;
        let custom_model_confirmed = sqlx::query("SELECT 1 FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 AND catalog_policy = 'explicit_custom' LIMIT 1")
            .bind(tenant_id).bind(route_id.to_string()).fetch_optional(&self.pool).await?.is_some();
        let grant_revision = route_grant_revision(&self.pool, tenant_id, route_id).await?;
        Ok(RouteRoutingView {
            route_id,
            upstream_account_ids,
            included_provider_group_ids,
            excluded_provider_group_ids,
            route_group_ids,
            granted_credential_ids,
            candidate_upstream_account_ids,
            updated_at,
            grant_revision,
            custom_model_confirmed,
        })
    }
}

fn same_ids(left: &[Uuid], right: &[Uuid]) -> bool {
    left.iter().copied().collect::<BTreeSet<_>>() == right.iter().copied().collect::<BTreeSet<_>>()
}

#[allow(clippy::too_many_arguments)]
async fn find_equivalent_route_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    tenant_external_id: &str,
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
    custom_model_confirmed: bool,
    upstream_ids: &[Uuid],
    included: &[Uuid],
    excluded: &[Uuid],
    route_groups: &[Uuid],
    credential_ids: &[Uuid],
) -> Result<Option<ModelRouteView>, AppError> {
    let rows = sqlx::query(
        "SELECT id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at \
         FROM model_routes WHERE tenant_id = $1 AND public_model = $2 AND upstream_model = $3 \
           AND protocol = $4 AND priority = $5 ORDER BY created_at, id LIMIT 101",
    )
    .bind(tenant_id)
    .bind(public_model.trim())
    .bind(upstream_model.trim())
    .bind(protocol)
    .bind(priority)
    .fetch_all(&mut **tx)
    .await?;
    for row in rows {
        let route_id = parse_uuid(row.try_get("id")?)?;
        if route_relations_match_in_transaction(
            tx,
            tenant_id,
            route_id,
            upstream_ids,
            included,
            excluded,
            route_groups,
            credential_ids,
            custom_model_confirmed,
        )
        .await?
        {
            return Ok(Some(ModelRouteView {
                id: route_id,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                tenant_external_id: Some(tenant_external_id.to_owned()),
                public_model: row.try_get("public_model")?,
                upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
                upstream_model: row.try_get("upstream_model")?,
                protocol: row.try_get("protocol")?,
                priority: row.try_get("priority")?,
                enabled: row.try_get::<i64, _>("enabled")? != 0,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            }));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn route_relations_match_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    upstream_ids: &[Uuid],
    included: &[Uuid],
    excluded: &[Uuid],
    route_groups: &[Uuid],
    credential_ids: &[Uuid],
    custom_model_confirmed: bool,
) -> Result<bool, AppError> {
    let actual_upstreams = select_route_ids_in_transaction(
        tx,
        "SELECT upstream_account_id AS id FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY upstream_account_id",
        tenant_id,
        route_id,
    )
    .await?;
    let actual_included = select_route_ids_in_transaction(
        tx,
        "SELECT provider_group_id AS id FROM model_route_included_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let actual_excluded = select_route_ids_in_transaction(
        tx,
        "SELECT provider_group_id AS id FROM model_route_excluded_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let actual_route_groups = select_route_ids_in_transaction(
        tx,
        "SELECT route_group_id AS id FROM model_route_group_memberships WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY route_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let actual_credentials = select_route_ids_in_transaction(
        tx,
        "SELECT key_id AS id FROM routing_grants WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY key_id",
        tenant_id,
        route_id,
    )
    .await?;
    let actual_custom = sqlx::query(
        "SELECT 1 FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 AND catalog_policy = 'explicit_custom' LIMIT 1",
    )
    .bind(tenant_id)
    .bind(route_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    Ok(same_ids(&actual_upstreams, upstream_ids)
        && same_ids(&actual_included, included)
        && same_ids(&actual_excluded, excluded)
        && same_ids(&actual_route_groups, route_groups)
        && same_ids(&actual_credentials, credential_ids)
        && actual_custom == custom_model_confirmed)
}

async fn select_route_ids_in_transaction(
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
        .map(|row| parse_uuid(row.try_get("id")?))
        .collect()
}

async fn resolve_existing_route_group_names(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    groups: &[(String, String)],
    route_group_ids: &mut Vec<Uuid>,
) -> Result<bool, AppError> {
    let mut all_exist = true;
    for (_, normalized_name) in groups {
        let existing = sqlx::query(
            "SELECT id FROM route_groups WHERE tenant_id = $1 AND normalized_name = $2",
        )
        .bind(tenant_id)
        .bind(normalized_name)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = existing {
            route_group_ids.push(parse_uuid(row.try_get("id")?)?);
        } else {
            all_exist = false;
        }
    }
    Ok(all_exist)
}

fn bounded_unique_ids(ids: Vec<Uuid>, label: &str) -> Result<Vec<Uuid>, AppError> {
    if ids.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "{label} cannot contain more than {MAX_GROUP_MEMBERS} entries"
        )));
    }
    Ok(ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn bounded_group_names(names: Vec<String>) -> Result<Vec<(String, String)>, AppError> {
    if names.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "route group names cannot contain more than {MAX_GROUP_MEMBERS} entries"
        )));
    }
    let mut normalized = BTreeMap::new();
    for raw in names {
        let (name, normalized_name) = normalize_group_name(&raw)?;
        normalized.entry(normalized_name).or_insert(name);
    }
    Ok(normalized
        .into_iter()
        .map(|(normalized_name, name)| (name, normalized_name))
        .collect())
}

fn normalize_group_name(raw: &str) -> Result<(String, String), AppError> {
    let name = raw.trim();
    if name.is_empty() || name.len() > MAX_GROUP_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "group name must contain 1 to 100 non-control bytes".into(),
        ));
    }
    Ok((name.to_owned(), name.to_lowercase()))
}

async fn create_or_find_route_groups(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    groups: Vec<(String, String)>,
    route_group_ids: &mut Vec<Uuid>,
    now: i64,
) -> Result<(), AppError> {
    for (name, normalized_name) in groups {
        let group_id = Uuid::now_v7();
        sqlx::query("INSERT INTO route_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, normalized_name) DO NOTHING")
            .bind(group_id.to_string()).bind(tenant_id).bind(name).bind(&normalized_name)
            .bind(now).bind(now).execute(&mut **tx).await?;
        let existing: String = sqlx::query(
            "SELECT id FROM route_groups WHERE tenant_id = $1 AND normalized_name = $2",
        )
        .bind(tenant_id)
        .bind(normalized_name)
        .fetch_one(&mut **tx)
        .await?
        .try_get("id")?;
        route_group_ids.push(parse_uuid(existing)?);
    }
    Ok(())
}

fn validate_route_fields(
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
) -> Result<(), AppError> {
    let public_model = public_model.trim();
    let upstream_model = upstream_model.trim();
    if public_model.is_empty() || upstream_model.is_empty() {
        return Err(AppError::BadRequest(
            "public_model and upstream_model are required".into(),
        ));
    }
    if public_model.len() > 200 || upstream_model.len() > 500 {
        return Err(AppError::BadRequest(
            "public_model and upstream_model exceed their length limit".into(),
        ));
    }
    if public_model.chars().any(char::is_control) || upstream_model.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "model names must not contain control characters".into(),
        ));
    }
    if !matches!(protocol, "openai" | "anthropic" | "generation") {
        return Err(AppError::BadRequest(
            "route protocol must be openai, anthropic, or generation".into(),
        ));
    }
    if !(-1_000_000..=1_000_000).contains(&priority) {
        return Err(AppError::BadRequest(
            "route priority must be between -1000000 and 1000000".into(),
        ));
    }
    Ok(())
}
