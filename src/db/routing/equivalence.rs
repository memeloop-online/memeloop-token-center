use std::collections::BTreeSet;

use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, parse_uuid};
use crate::provider::ModelRouteView;

pub(super) fn same_ids(left: &[Uuid], right: &[Uuid]) -> bool {
    left.iter().copied().collect::<BTreeSet<_>>() == right.iter().copied().collect::<BTreeSet<_>>()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn find_equivalent_route_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    tenant_external_id: &str,
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
    enabled: bool,
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
           AND protocol = $4 AND priority = $5 AND enabled = $6 ORDER BY created_at, id LIMIT 101",
    )
    .bind(tenant_id)
    .bind(public_model.trim())
    .bind(upstream_model.trim())
    .bind(protocol)
    .bind(priority)
    .bind(i64::from(enabled))
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
pub(super) async fn route_relations_match_in_transaction(
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
