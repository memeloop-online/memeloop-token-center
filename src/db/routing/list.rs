use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid};
use crate::provider::ModelRouteView;

const MAX_ROUTE_PAGE: usize = 100;

// Keep this query static and use PostgreSQL-style placeholders: sqlx::Any does
// not rewrite SQLite `?` placeholders for PostgreSQL. A fixed-size CTE keeps
// association loading to one query without constructing backend-specific SQL.
const ROUTE_ASSOCIATIONS_SQL: &str = r#"
WITH selected_routes(route_id) AS (
    VALUES
        ($1), ($2), ($3), ($4), ($5), ($6), ($7), ($8), ($9), ($10),
        ($11), ($12), ($13), ($14), ($15), ($16), ($17), ($18), ($19), ($20),
        ($21), ($22), ($23), ($24), ($25), ($26), ($27), ($28), ($29), ($30),
        ($31), ($32), ($33), ($34), ($35), ($36), ($37), ($38), ($39), ($40),
        ($41), ($42), ($43), ($44), ($45), ($46), ($47), ($48), ($49), ($50),
        ($51), ($52), ($53), ($54), ($55), ($56), ($57), ($58), ($59), ($60),
        ($61), ($62), ($63), ($64), ($65), ($66), ($67), ($68), ($69), ($70),
        ($71), ($72), ($73), ($74), ($75), ($76), ($77), ($78), ($79), ($80),
        ($81), ($82), ($83), ($84), ($85), ($86), ($87), ($88), ($89), ($90),
        ($91), ($92), ($93), ($94), ($95), ($96), ($97), ($98), ($99), ($100)
)
SELECT 'upstream_account' AS relation_kind,
       association.model_route_id AS route_id,
       association.upstream_account_id AS related_id,
       CAST(NULL AS BIGINT) AS numeric_value
FROM model_route_upstream_accounts association
JOIN selected_routes selected ON selected.route_id = association.model_route_id
UNION ALL
SELECT 'included_provider_group', association.model_route_id,
       association.provider_group_id, CAST(NULL AS BIGINT)
FROM model_route_included_provider_groups association
JOIN selected_routes selected ON selected.route_id = association.model_route_id
UNION ALL
SELECT 'excluded_provider_group', association.model_route_id,
       association.provider_group_id, CAST(NULL AS BIGINT)
FROM model_route_excluded_provider_groups association
JOIN selected_routes selected ON selected.route_id = association.model_route_id
UNION ALL
SELECT 'route_group', association.model_route_id,
       association.route_group_id, CAST(NULL AS BIGINT)
FROM model_route_group_memberships association
JOIN selected_routes selected ON selected.route_id = association.model_route_id
UNION ALL
SELECT 'granted_credential', grant_edge.model_route_id,
       grant_edge.key_id, CAST(NULL AS BIGINT)
FROM routing_grants grant_edge
JOIN selected_routes selected ON selected.route_id = grant_edge.model_route_id
WHERE grant_edge.model_route_id IS NOT NULL
UNION ALL
SELECT 'candidate_upstream_account', candidate.model_route_id,
       candidate.upstream_account_id, CAST(NULL AS BIGINT)
FROM model_route_eligible_upstream_accounts candidate
JOIN selected_routes selected ON selected.route_id = candidate.model_route_id
JOIN upstream_accounts account
  ON account.tenant_id = candidate.tenant_id
 AND account.id = candidate.upstream_account_id
 AND account.status = 'active'
UNION ALL
SELECT 'custom_model_confirmed', association.model_route_id,
       association.upstream_account_id, CAST(NULL AS BIGINT)
FROM model_route_upstream_accounts association
JOIN selected_routes selected ON selected.route_id = association.model_route_id
WHERE association.catalog_policy = 'explicit_custom'
UNION ALL
SELECT 'grant_revision', revision.model_route_id,
       CAST(NULL AS TEXT), revision.revision
FROM routing_grant_relation_revisions revision
JOIN selected_routes selected ON selected.route_id = revision.model_route_id
WHERE revision.subject_kind = 'route'
ORDER BY route_id, relation_kind, related_id
"#;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct EnrichedModelRouteView {
    #[serde(flatten)]
    pub route: ModelRouteView,
    pub route_id: Uuid,
    pub upstream_account_ids: Vec<Uuid>,
    pub included_provider_group_ids: Vec<Uuid>,
    pub excluded_provider_group_ids: Vec<Uuid>,
    pub route_group_ids: Vec<Uuid>,
    pub granted_credential_ids: Vec<Uuid>,
    pub candidate_upstream_account_ids: Vec<Uuid>,
    pub grant_revision: i64,
    pub custom_model_confirmed: bool,
}

#[derive(Default)]
struct RouteAssociations {
    upstream_account_ids: BTreeSet<Uuid>,
    included_provider_group_ids: BTreeSet<Uuid>,
    excluded_provider_group_ids: BTreeSet<Uuid>,
    route_group_ids: BTreeSet<Uuid>,
    granted_credential_ids: BTreeSet<Uuid>,
    candidate_upstream_account_ids: BTreeSet<Uuid>,
    grant_revision: i64,
    custom_model_confirmed: bool,
}

impl Database {
    pub(crate) async fn list_enriched_model_routes_page(
        &self,
        tenant_external_id: Option<&str>,
        before_created_at: Option<i64>,
        before_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<EnrichedModelRouteView>, AppError> {
        let routes = self
            .list_model_routes_page(
                tenant_external_id,
                before_created_at,
                before_id,
                limit.clamp(1, MAX_ROUTE_PAGE as i64),
            )
            .await?;
        if routes.is_empty() {
            return Ok(Vec::new());
        }
        if routes.len() > MAX_ROUTE_PAGE {
            return Err(AppError::Internal);
        }

        let mut associations = routes
            .iter()
            .map(|route| (route.id, RouteAssociations::default()))
            .collect::<BTreeMap<_, _>>();
        let mut query = sqlx::query(ROUTE_ASSOCIATIONS_SQL);
        for route_id in routes
            .iter()
            .map(|route| route.id)
            .chain(std::iter::repeat(Uuid::nil()))
            .take(MAX_ROUTE_PAGE)
        {
            query = query.bind(route_id.to_string());
        }

        for row in query.fetch_all(&self.pool).await? {
            let route_id = parse_uuid(row.try_get("route_id")?)?;
            let Some(route) = associations.get_mut(&route_id) else {
                // The base page is the tenant authorization boundary. Ignore
                // anything that was not selected by that scoped query.
                continue;
            };
            let kind: String = row.try_get("relation_kind")?;
            match kind.as_str() {
                "upstream_account" => {
                    route.upstream_account_ids.insert(parse_related_id(&row)?);
                }
                "included_provider_group" => {
                    route
                        .included_provider_group_ids
                        .insert(parse_related_id(&row)?);
                }
                "excluded_provider_group" => {
                    route
                        .excluded_provider_group_ids
                        .insert(parse_related_id(&row)?);
                }
                "route_group" => {
                    route.route_group_ids.insert(parse_related_id(&row)?);
                }
                "granted_credential" => {
                    route.granted_credential_ids.insert(parse_related_id(&row)?);
                }
                "candidate_upstream_account" => {
                    route
                        .candidate_upstream_account_ids
                        .insert(parse_related_id(&row)?);
                }
                "custom_model_confirmed" => route.custom_model_confirmed = true,
                "grant_revision" => {
                    route.grant_revision = row.try_get("numeric_value")?;
                }
                _ => return Err(AppError::Internal),
            }
        }

        routes
            .into_iter()
            .map(|route| {
                let route_id = route.id;
                let association = associations.remove(&route_id).ok_or(AppError::Internal)?;
                Ok(EnrichedModelRouteView {
                    route,
                    route_id,
                    upstream_account_ids: association.upstream_account_ids.into_iter().collect(),
                    included_provider_group_ids: association
                        .included_provider_group_ids
                        .into_iter()
                        .collect(),
                    excluded_provider_group_ids: association
                        .excluded_provider_group_ids
                        .into_iter()
                        .collect(),
                    route_group_ids: association.route_group_ids.into_iter().collect(),
                    granted_credential_ids: association
                        .granted_credential_ids
                        .into_iter()
                        .collect(),
                    candidate_upstream_account_ids: association
                        .candidate_upstream_account_ids
                        .into_iter()
                        .collect(),
                    grant_revision: association.grant_revision,
                    custom_model_confirmed: association.custom_model_confirmed,
                })
            })
            .collect()
    }
}

fn parse_related_id(row: &sqlx::any::AnyRow) -> Result<Uuid, AppError> {
    parse_uuid(row.try_get("related_id")?)
}
