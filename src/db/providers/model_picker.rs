use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use super::super::*;

pub const MODEL_PICKER_ITEM_LIMIT: i64 = 100;
pub const MODEL_PICKER_SOURCE_LIMIT: i64 = 100;
pub const MODEL_PICKER_GROUP_LIMIT: i64 = 20;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPickerSelectionKind {
    Route,
    Model,
}

impl ModelPickerSelectionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Route => "route",
            Self::Model => "model",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelPickerSelectionIdentity {
    Route { route_id: Uuid },
    Model { model: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerNamedIdentity {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerProviderIdentity {
    pub id: String,
    pub label: String,
    pub protocols: Vec<String>,
    pub modalities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerConfigurationAvailability {
    pub status: String,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerCatalogEvidence {
    pub status: String,
    pub observed_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub error_code: Option<String>,
    pub model_listed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerHealthEvidence {
    pub status: String,
    pub observed_at: Option<i64>,
    pub cooldown_until: Option<i64>,
    pub failure_kind: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerSourceCapabilities {
    pub route_protocol: String,
    pub upstream_model: String,
    pub catalog_model_listed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerSource {
    pub route_id: Uuid,
    pub provider: ModelPickerProviderIdentity,
    pub provider_groups: Vec<ModelPickerNamedIdentity>,
    pub provider_groups_truncated: bool,
    pub account: ModelPickerNamedIdentity,
    pub configuration_availability: ModelPickerConfigurationAvailability,
    pub catalog: ModelPickerCatalogEvidence,
    pub capabilities: ModelPickerSourceCapabilities,
    pub passive_health: ModelPickerHealthEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelPickerItem {
    pub selection: ModelPickerSelectionIdentity,
    pub value: String,
    pub label: String,
    pub sources: Vec<ModelPickerSource>,
    pub sources_truncated: bool,
    #[serde(skip)]
    pub(crate) sort_label: String,
    #[serde(skip)]
    pub(crate) sort_identity: String,
}

#[derive(Clone, Debug)]
pub struct ModelPickerProjectionFilter<'a> {
    pub tenant_external_id: &'a str,
    pub selection_kind: ModelPickerSelectionKind,
    pub query: &'a str,
    pub after_sort_label: &'a str,
    pub after_identity: &'a str,
    pub explicit_account_ids: &'a [Uuid],
    pub included_provider_group_ids: &'a [Uuid],
    pub excluded_provider_group_ids: &'a [Uuid],
    pub public_provider_ids: &'a [String],
    pub limit: i64,
}

impl Database {
    /// One bounded read-only projection backs all operator route/model pickers.
    /// The method deliberately issues exactly one SQL statement. Provider
    /// labels and declared capabilities are enriched from the in-memory public
    /// ProviderCatalog by the API layer, never by provider network requests.
    pub async fn model_picker_projection(
        &self,
        filter: ModelPickerProjectionFilter<'_>,
    ) -> Result<Vec<ModelPickerItem>, AppError> {
        let explicit_accounts = uuid_list_json(filter.explicit_account_ids)?;
        let included_groups = uuid_list_json(filter.included_provider_group_ids)?;
        let excluded_groups = uuid_list_json(filter.excluded_provider_group_ids)?;
        let public_providers =
            serde_json::to_string(filter.public_provider_ids).map_err(|_| AppError::Internal)?;
        let has_positive_filter = if filter.explicit_account_ids.is_empty()
            && filter.included_provider_group_ids.is_empty()
        {
            0_i64
        } else {
            1_i64
        };
        let query = format!("%{}%", escape_like(&filter.query.to_ascii_lowercase()));
        let rows = sqlx::query(sqlx::AssertSqlSafe(model_picker_sql(self.backend)))
            .bind(filter.tenant_external_id)
            .bind(filter.selection_kind.as_str())
            .bind(query)
            .bind(filter.after_sort_label)
            .bind(filter.after_identity)
            .bind(filter.limit.clamp(1, MODEL_PICKER_ITEM_LIMIT) + 1)
            .bind(explicit_accounts)
            .bind(included_groups)
            .bind(excluded_groups)
            .bind(public_providers)
            .bind(has_positive_filter)
            .bind(unix_millis())
            .bind(MODEL_PICKER_SOURCE_LIMIT)
            .bind(MODEL_PICKER_GROUP_LIMIT)
            .fetch_all(&self.pool)
            .await?;

        let now = unix_millis();
        let mut items = Vec::<ModelPickerItem>::new();
        for row in rows {
            let identity: String = row.try_get("item_identity")?;
            if items
                .last()
                .is_none_or(|item| item.sort_identity != identity)
            {
                let value: String = row.try_get("item_value")?;
                let selection = match filter.selection_kind {
                    ModelPickerSelectionKind::Route => ModelPickerSelectionIdentity::Route {
                        route_id: parse_uuid(identity.clone())?,
                    },
                    ModelPickerSelectionKind::Model => ModelPickerSelectionIdentity::Model {
                        model: identity.clone(),
                    },
                };
                items.push(ModelPickerItem {
                    selection,
                    value,
                    label: row.try_get("item_label")?,
                    sources: Vec::new(),
                    sources_truncated: row.try_get::<i64, _>("source_count")?
                        > MODEL_PICKER_SOURCE_LIMIT,
                    sort_label: row.try_get("sort_label")?,
                    sort_identity: identity.clone(),
                });
            }

            let Some(account_id) = row.try_get::<Option<String>, _>("account_id")? else {
                continue;
            };
            let route_id = parse_uuid(row.try_get::<String, _>("route_id")?)?;
            let account_id = parse_uuid(account_id)?;
            let item = items.last_mut().ok_or(AppError::Internal)?;
            let is_new_source = item.sources.last().is_none_or(|source| {
                source.route_id != route_id || source.account.id != account_id.to_string()
            });
            if is_new_source {
                let account_status: String = row.try_get("account_status")?;
                let credential_available = row.try_get::<i64, _>("credential_available")? != 0;
                let route_candidate_eligible =
                    row.try_get::<i64, _>("route_candidate_eligible")? != 0;
                let mut reasons = Vec::new();
                if account_status != "active" {
                    reasons.push("account_inactive".to_owned());
                }
                if !credential_available {
                    reasons.push("credential_unavailable".to_owned());
                }
                if !route_candidate_eligible {
                    reasons.push("route_candidate_ineligible".to_owned());
                }
                let catalog_snapshot_current =
                    row.try_get::<i64, _>("catalog_snapshot_current")? != 0;
                let catalog_model_listed = row.try_get::<i64, _>("catalog_model_listed")? != 0;
                let catalog_state: Option<String> = row.try_get("catalog_state")?;
                let catalog_error: Option<String> = row.try_get("catalog_error_code")?;
                let catalog_expires_at: Option<i64> = row.try_get("catalog_expires_at")?;
                let catalog_status = catalog_evidence_status(
                    catalog_state.as_deref(),
                    catalog_snapshot_current,
                    catalog_error.as_deref(),
                    catalog_expires_at,
                    now,
                );
                let health_generation_matches =
                    row.try_get::<i64, _>("health_generation_matches")? != 0;
                let health_failures: Option<i64> = row.try_get("health_failures")?;
                let health_cooldown: Option<i64> = row.try_get("health_cooldown_until")?;
                let health_status = if !health_generation_matches || health_failures.is_none() {
                    "unknown"
                } else if health_cooldown.is_some_and(|until| until > now) {
                    "unhealthy"
                } else if health_failures.is_some_and(|count| count > 0) {
                    "degraded"
                } else {
                    "healthy"
                };
                let route_protocol: String = row.try_get("route_protocol")?;
                let upstream_model: String = row.try_get("upstream_model")?;
                item.sources.push(ModelPickerSource {
                    route_id,
                    provider: ModelPickerProviderIdentity {
                        id: row.try_get("provider_id")?,
                        label: row.try_get("provider_id")?,
                        protocols: Vec::new(),
                        modalities: Vec::new(),
                    },
                    provider_groups: Vec::new(),
                    provider_groups_truncated: row.try_get::<i64, _>("provider_group_count")?
                        > MODEL_PICKER_GROUP_LIMIT,
                    account: ModelPickerNamedIdentity {
                        id: account_id.to_string(),
                        label: row.try_get("account_label")?,
                    },
                    configuration_availability: ModelPickerConfigurationAvailability {
                        status: if reasons.is_empty() {
                            "available".to_owned()
                        } else {
                            "unavailable".to_owned()
                        },
                        reasons,
                    },
                    catalog: ModelPickerCatalogEvidence {
                        status: catalog_status.to_owned(),
                        observed_at: row.try_get("catalog_observed_at")?,
                        last_success_at: row.try_get("catalog_last_success_at")?,
                        expires_at: catalog_expires_at,
                        error_code: catalog_error,
                        model_listed: catalog_model_listed,
                    },
                    capabilities: ModelPickerSourceCapabilities {
                        route_protocol,
                        upstream_model,
                        catalog_model_listed,
                    },
                    passive_health: ModelPickerHealthEvidence {
                        status: health_status.to_owned(),
                        observed_at: if health_generation_matches {
                            row.try_get("health_observed_at")?
                        } else {
                            None
                        },
                        cooldown_until: if health_generation_matches {
                            health_cooldown
                        } else {
                            None
                        },
                        failure_kind: if health_generation_matches {
                            row.try_get::<Option<String>, _>("health_failure_kind")?
                                .filter(|value| !value.is_empty())
                        } else {
                            None
                        },
                    },
                });
            }
            let Some(group_id) = row.try_get::<Option<String>, _>("provider_group_id")? else {
                continue;
            };
            let group = ModelPickerNamedIdentity {
                id: parse_uuid(group_id)?.to_string(),
                label: row.try_get("provider_group_label")?,
            };
            let source = item.sources.last_mut().ok_or(AppError::Internal)?;
            if source.provider_groups.last() != Some(&group) {
                source.provider_groups.push(group);
            }
        }
        Ok(items)
    }
}

fn catalog_evidence_status(
    state: Option<&str>,
    snapshot_current: bool,
    error_code: Option<&str>,
    expires_at: Option<i64>,
    now: i64,
) -> &'static str {
    match (state, snapshot_current) {
        (None | Some("unknown"), _) => "never_observed",
        (Some("syncing"), false) => "loading",
        (Some("syncing"), true) => "partial",
        (Some("ready"), true) if expires_at.is_some_and(|expiry| expiry <= now) => "stale",
        (Some("ready"), true) => "ready",
        (Some("stale" | "error"), true) if error_code.is_some() => "partial",
        (Some("stale"), true) => "stale",
        (Some("error"), false) => "error",
        (Some(_), false) => "stale",
        _ => "error",
    }
}

fn model_picker_sql(backend: DatabaseBackend) -> &'static str {
    match backend {
        DatabaseBackend::PostgreSql => MODEL_PICKER_POSTGRES_SQL,
        DatabaseBackend::Sqlite => MODEL_PICKER_SQLITE_SQL,
    }
}

fn uuid_list_json(values: &[Uuid]) -> Result<String, AppError> {
    serde_json::to_string(&values.iter().map(Uuid::to_string).collect::<Vec<_>>())
        .map_err(|_| AppError::Internal)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

const MODEL_PICKER_POSTGRES_SQL: &str = r#"
WITH direct_sources AS (
    SELECT direct.tenant_id, direct.model_route_id, direct.upstream_account_id, direct.upstream_model
      FROM model_route_upstream_accounts direct
), raw_sources AS (
    SELECT direct.tenant_id, direct.model_route_id, direct.upstream_account_id, direct.upstream_model
      FROM direct_sources direct
    UNION
    SELECT included.tenant_id, included.model_route_id, member.upstream_account_id, route.upstream_model
      FROM model_route_included_provider_groups included
      JOIN upstream_account_provider_groups member
        ON member.tenant_id = included.tenant_id
       AND member.provider_group_id = included.provider_group_id
      JOIN model_routes route
        ON route.tenant_id = included.tenant_id AND route.id = included.model_route_id
     WHERE NOT EXISTS (
         -- A direct route/account assignment is authoritative over group expansion.
         SELECT 1 FROM direct_sources direct
          WHERE direct.tenant_id = included.tenant_id
            AND direct.model_route_id = included.model_route_id
            AND direct.upstream_account_id = member.upstream_account_id
     )
), configured_sources AS (
    SELECT raw.tenant_id, raw.model_route_id, raw.upstream_account_id, raw.upstream_model
      FROM raw_sources raw
     WHERE NOT EXISTS (
         SELECT 1 FROM model_route_excluded_provider_groups excluded
         JOIN upstream_account_provider_groups blocked
           ON blocked.tenant_id = excluded.tenant_id
          AND blocked.provider_group_id = excluded.provider_group_id
        WHERE excluded.tenant_id = raw.tenant_id
          AND excluded.model_route_id = raw.model_route_id
          AND blocked.upstream_account_id = raw.upstream_account_id
     )
), selected_sources AS MATERIALIZED (
    SELECT source.tenant_id, source.model_route_id, source.upstream_account_id, source.upstream_model
      FROM configured_sources source
      JOIN tenants tenant ON tenant.id = source.tenant_id AND tenant.external_id = $1 AND tenant.status = 'active'
      JOIN upstream_accounts account ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
     WHERE account.driver IN (
         SELECT public_driver.value
           FROM jsonb_array_elements_text(CAST($10 AS jsonb)) AS public_driver(value)
     )
       AND ($11 = 0
            OR account.id IN (SELECT selected.value FROM jsonb_array_elements_text(CAST($7 AS jsonb)) AS selected(value))
            OR EXISTS (
                SELECT 1 FROM upstream_account_provider_groups membership
                 WHERE membership.tenant_id = source.tenant_id
                   AND membership.upstream_account_id = account.id
                   AND membership.provider_group_id IN (
                       SELECT selected.value FROM jsonb_array_elements_text(CAST($8 AS jsonb)) AS selected(value)
                   )
            ))
       AND NOT EXISTS (
           SELECT 1 FROM upstream_account_provider_groups membership
            WHERE membership.tenant_id = source.tenant_id
              AND membership.upstream_account_id = account.id
              AND membership.provider_group_id IN (
                  SELECT selected.value FROM jsonb_array_elements_text(CAST($9 AS jsonb)) AS selected(value)
              )
       )
), logical_items AS (
    SELECT DISTINCT
           (CASE WHEN $2 = 'route' THEN route.id ELSE route.public_model END) COLLATE "C" AS item_identity,
           CASE WHEN $2 = 'route' THEN route.id ELSE route.public_model END AS item_value,
           route.public_model AS item_label,
           LOWER(route.public_model COLLATE "C") AS sort_label
      FROM model_routes route
      JOIN selected_sources source
        ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
     WHERE route.enabled = 1
), matching_items AS (
    SELECT item.*
      FROM logical_items item
     WHERE EXISTS (
         SELECT 1
           FROM model_routes route
           JOIN selected_sources source
             ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
           JOIN upstream_accounts account
             ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
          WHERE route.enabled = 1
            AND (($2 = 'route' AND route.id = item.item_identity)
                 OR ($2 = 'model' AND route.public_model = item.item_identity))
            AND (LOWER(route.public_model COLLATE "C") LIKE CAST($3 AS TEXT) COLLATE "C" ESCAPE '\'
                 OR LOWER(source.upstream_model COLLATE "C") LIKE CAST($3 AS TEXT) COLLATE "C" ESCAPE '\'
                 OR LOWER(account.name COLLATE "C") LIKE CAST($3 AS TEXT) COLLATE "C" ESCAPE '\'
                 OR LOWER(account.driver COLLATE "C") LIKE CAST($3 AS TEXT) COLLATE "C" ESCAPE '\'
                 OR EXISTS (
                     SELECT 1 FROM upstream_account_provider_groups membership
                     JOIN provider_groups provider_group
                       ON provider_group.tenant_id = membership.tenant_id
                      AND provider_group.id = membership.provider_group_id
                    WHERE membership.tenant_id = source.tenant_id
                      AND membership.upstream_account_id = account.id
                      AND LOWER(provider_group.name COLLATE "C") LIKE CAST($3 AS TEXT) COLLATE "C" ESCAPE '\'
                 ))
     )
), page AS MATERIALIZED (
    SELECT * FROM matching_items
     WHERE ($4 = '' OR sort_label > $4 OR (sort_label = $4 AND item_identity > $5))
     ORDER BY sort_label, item_identity
     LIMIT $6
), ranked_sources AS MATERIALIZED (
    SELECT page.item_identity, page.item_value, page.item_label, page.sort_label,
           route.id AS route_id, route.protocol AS route_protocol, route.priority,
           source.upstream_model, account.id AS account_id, account.name AS account_label,
           account.driver AS provider_id, account.status AS account_status,
           COUNT(*) OVER (PARTITION BY page.item_identity) AS source_count,
           ROW_NUMBER() OVER (PARTITION BY page.item_identity ORDER BY route.priority, route.id, account.id) AS source_rank,
           CASE WHEN EXISTS (
               SELECT 1 FROM upstream_credentials credential
                WHERE credential.upstream_account_id = account.id
                  AND credential.generation = account.credential_generation
                  AND credential.revoked_at IS NULL
                  AND (credential.expires_at IS NULL OR credential.expires_at > $12)
           ) THEN 1 ELSE 0 END AS credential_available,
           CASE WHEN EXISTS (
               SELECT 1 FROM model_route_eligible_upstream_accounts eligible
                WHERE eligible.tenant_id = route.tenant_id
                  AND eligible.model_route_id = route.id
                  AND eligible.upstream_account_id = account.id
                  AND eligible.upstream_model = source.upstream_model
           ) THEN 1 ELSE 0 END AS route_candidate_eligible,
           catalog.status AS catalog_state, catalog.last_attempt_at AS catalog_observed_at,
           catalog.last_success_at AS catalog_last_success_at, catalog.expires_at AS catalog_expires_at,
           catalog.last_error_code AS catalog_error_code,
           CASE WHEN snapshot.id IS NOT NULL
                     AND catalog.credential_generation = account.credential_generation
                     AND snapshot.credential_generation = account.credential_generation
                THEN 1 ELSE 0 END AS catalog_snapshot_current,
           CASE WHEN snapshot.id IS NOT NULL
                     AND catalog.credential_generation = account.credential_generation
                     AND snapshot.credential_generation = account.credential_generation
                     AND EXISTS (
                         SELECT 1 FROM upstream_models model
                          WHERE model.tenant_id = account.tenant_id
                            AND model.upstream_account_id = account.id
                            AND model.snapshot_id = snapshot.id
                            AND model.model_id = source.upstream_model
                            AND (model.protocol = 'any' OR model.protocol = route.protocol)
                     ) THEN 1 ELSE 0 END AS catalog_model_listed,
           health.credential_generation AS health_generation,
           CASE WHEN health.credential_generation = account.credential_generation THEN 1 ELSE 0 END AS health_generation_matches,
           health.consecutive_failures AS health_failures,
           health.cooldown_until AS health_cooldown_until,
           health.last_failure_kind AS health_failure_kind,
           health.updated_at AS health_observed_at
      FROM page
      JOIN model_routes route
        ON route.enabled = 1
       AND (($2 = 'route' AND route.id = page.item_identity)
            OR ($2 = 'model' AND route.public_model = page.item_identity))
      JOIN selected_sources source
        ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
      JOIN upstream_accounts account
        ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
      LEFT JOIN upstream_model_catalog_state catalog
        ON catalog.tenant_id = account.tenant_id AND catalog.upstream_account_id = account.id
      LEFT JOIN upstream_model_catalog_snapshots snapshot
        ON snapshot.tenant_id = account.tenant_id
       AND snapshot.upstream_account_id = account.id
       AND snapshot.id = catalog.current_snapshot_id
      LEFT JOIN upstream_account_health health ON health.upstream_account_id = account.id
), ranked_groups AS MATERIALIZED (
    SELECT membership.upstream_account_id, provider_group.id AS provider_group_id,
           provider_group.name AS provider_group_label,
           COUNT(*) OVER (PARTITION BY membership.upstream_account_id) AS provider_group_count,
           ROW_NUMBER() OVER (PARTITION BY membership.upstream_account_id ORDER BY provider_group.id) AS provider_group_rank
      FROM upstream_account_provider_groups membership
      JOIN provider_groups provider_group
        ON provider_group.tenant_id = membership.tenant_id AND provider_group.id = membership.provider_group_id
      JOIN tenants tenant ON tenant.id = membership.tenant_id AND tenant.external_id = $1
)
SELECT source.*, provider_group.provider_group_id, provider_group.provider_group_label,
       COALESCE(provider_group.provider_group_count, 0) AS provider_group_count
  FROM ranked_sources source
  LEFT JOIN ranked_groups provider_group
    ON provider_group.upstream_account_id = source.account_id
   AND provider_group.provider_group_rank <= $14
 WHERE source.source_rank <= $13
 ORDER BY source.sort_label, source.item_identity, source.source_rank, provider_group.provider_group_rank
"#;

const MODEL_PICKER_SQLITE_SQL: &str = r#"
WITH direct_sources AS (
    SELECT direct.tenant_id, direct.model_route_id, direct.upstream_account_id, direct.upstream_model
      FROM model_route_upstream_accounts direct
), raw_sources AS (
    SELECT direct.tenant_id, direct.model_route_id, direct.upstream_account_id, direct.upstream_model
      FROM direct_sources direct
    UNION
    SELECT included.tenant_id, included.model_route_id, member.upstream_account_id, route.upstream_model
      FROM model_route_included_provider_groups included
      JOIN upstream_account_provider_groups member
        ON member.tenant_id = included.tenant_id
       AND member.provider_group_id = included.provider_group_id
      JOIN model_routes route
        ON route.tenant_id = included.tenant_id AND route.id = included.model_route_id
     WHERE NOT EXISTS (
         -- Keep SQLite and PostgreSQL precedence identical for historical rows.
         SELECT 1 FROM direct_sources direct
          WHERE direct.tenant_id = included.tenant_id
            AND direct.model_route_id = included.model_route_id
            AND direct.upstream_account_id = member.upstream_account_id
     )
), configured_sources AS (
    SELECT raw.tenant_id, raw.model_route_id, raw.upstream_account_id, raw.upstream_model
      FROM raw_sources raw
     WHERE NOT EXISTS (
         SELECT 1 FROM model_route_excluded_provider_groups excluded
         JOIN upstream_account_provider_groups blocked
           ON blocked.tenant_id = excluded.tenant_id
          AND blocked.provider_group_id = excluded.provider_group_id
        WHERE excluded.tenant_id = raw.tenant_id
          AND excluded.model_route_id = raw.model_route_id
          AND blocked.upstream_account_id = raw.upstream_account_id
     )
), selected_sources AS MATERIALIZED (
    SELECT source.tenant_id, source.model_route_id, source.upstream_account_id, source.upstream_model
      FROM configured_sources source
      JOIN tenants tenant ON tenant.id = source.tenant_id AND tenant.external_id = $1 AND tenant.status = 'active'
      JOIN upstream_accounts account ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
     WHERE account.driver IN (SELECT value FROM json_each($10))
       AND ($11 = 0
            OR account.id IN (SELECT value FROM json_each($7))
            OR EXISTS (
                SELECT 1 FROM upstream_account_provider_groups membership
                 WHERE membership.tenant_id = source.tenant_id
                   AND membership.upstream_account_id = account.id
                   AND membership.provider_group_id IN (SELECT value FROM json_each($8))
            ))
       AND NOT EXISTS (
           SELECT 1 FROM upstream_account_provider_groups membership
            WHERE membership.tenant_id = source.tenant_id
              AND membership.upstream_account_id = account.id
              AND membership.provider_group_id IN (SELECT value FROM json_each($9))
       )
), logical_items AS (
    SELECT DISTINCT
           CASE WHEN $2 = 'route' THEN route.id ELSE route.public_model END AS item_identity,
           CASE WHEN $2 = 'route' THEN route.id ELSE route.public_model END AS item_value,
           route.public_model AS item_label,
           LOWER(route.public_model) AS sort_label
      FROM model_routes route
      JOIN selected_sources source
        ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
     WHERE route.enabled = 1
), matching_items AS (
    SELECT item.*
      FROM logical_items item
     WHERE EXISTS (
         SELECT 1
           FROM model_routes route
           JOIN selected_sources source
             ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
           JOIN upstream_accounts account
             ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
          WHERE route.enabled = 1
            AND (($2 = 'route' AND route.id = item.item_identity)
                 OR ($2 = 'model' AND route.public_model = item.item_identity))
            AND (LOWER(route.public_model) LIKE LOWER($3) ESCAPE '\'
                 OR LOWER(source.upstream_model) LIKE LOWER($3) ESCAPE '\'
                 OR LOWER(account.name) LIKE LOWER($3) ESCAPE '\'
                 OR LOWER(account.driver) LIKE LOWER($3) ESCAPE '\'
                 OR EXISTS (
                     SELECT 1 FROM upstream_account_provider_groups membership
                     JOIN provider_groups provider_group
                       ON provider_group.tenant_id = membership.tenant_id
                      AND provider_group.id = membership.provider_group_id
                    WHERE membership.tenant_id = source.tenant_id
                      AND membership.upstream_account_id = account.id
                      AND LOWER(provider_group.name) LIKE LOWER($3) ESCAPE '\'
                 ))
     )
), page AS MATERIALIZED (
    SELECT * FROM matching_items
     WHERE ($4 = '' OR sort_label > $4 OR (sort_label = $4 AND item_identity > $5))
     ORDER BY sort_label, item_identity
     LIMIT $6
), ranked_sources AS MATERIALIZED (
    SELECT page.item_identity, page.item_value, page.item_label, page.sort_label,
           route.id AS route_id, route.protocol AS route_protocol, route.priority,
           source.upstream_model, account.id AS account_id, account.name AS account_label,
           account.driver AS provider_id, account.status AS account_status,
           COUNT(*) OVER (PARTITION BY page.item_identity) AS source_count,
           ROW_NUMBER() OVER (PARTITION BY page.item_identity ORDER BY route.priority, route.id, account.id) AS source_rank,
           CASE WHEN EXISTS (
               SELECT 1 FROM upstream_credentials credential
                WHERE credential.upstream_account_id = account.id
                  AND credential.generation = account.credential_generation
                  AND credential.revoked_at IS NULL
                  AND (credential.expires_at IS NULL OR credential.expires_at > $12)
           ) THEN 1 ELSE 0 END AS credential_available,
           CASE WHEN EXISTS (
               SELECT 1 FROM model_route_eligible_upstream_accounts eligible
                WHERE eligible.tenant_id = route.tenant_id
                  AND eligible.model_route_id = route.id
                  AND eligible.upstream_account_id = account.id
                  AND eligible.upstream_model = source.upstream_model
           ) THEN 1 ELSE 0 END AS route_candidate_eligible,
           catalog.status AS catalog_state, catalog.last_attempt_at AS catalog_observed_at,
           catalog.last_success_at AS catalog_last_success_at, catalog.expires_at AS catalog_expires_at,
           catalog.last_error_code AS catalog_error_code,
           CASE WHEN snapshot.id IS NOT NULL
                     AND catalog.credential_generation = account.credential_generation
                     AND snapshot.credential_generation = account.credential_generation
                THEN 1 ELSE 0 END AS catalog_snapshot_current,
           CASE WHEN snapshot.id IS NOT NULL
                     AND catalog.credential_generation = account.credential_generation
                     AND snapshot.credential_generation = account.credential_generation
                     AND EXISTS (
                         SELECT 1 FROM upstream_models model
                          WHERE model.tenant_id = account.tenant_id
                            AND model.upstream_account_id = account.id
                            AND model.snapshot_id = snapshot.id
                            AND model.model_id = source.upstream_model
                            AND (model.protocol = 'any' OR model.protocol = route.protocol)
                     ) THEN 1 ELSE 0 END AS catalog_model_listed,
           health.credential_generation AS health_generation,
           CASE WHEN health.credential_generation = account.credential_generation THEN 1 ELSE 0 END AS health_generation_matches,
           health.consecutive_failures AS health_failures,
           health.cooldown_until AS health_cooldown_until,
           health.last_failure_kind AS health_failure_kind,
           health.updated_at AS health_observed_at
      FROM page
      JOIN model_routes route
        ON route.enabled = 1
       AND (($2 = 'route' AND route.id = page.item_identity)
            OR ($2 = 'model' AND route.public_model = page.item_identity))
      JOIN selected_sources source
        ON source.tenant_id = route.tenant_id AND source.model_route_id = route.id
      JOIN upstream_accounts account
        ON account.tenant_id = source.tenant_id AND account.id = source.upstream_account_id
      LEFT JOIN upstream_model_catalog_state catalog
        ON catalog.tenant_id = account.tenant_id AND catalog.upstream_account_id = account.id
      LEFT JOIN upstream_model_catalog_snapshots snapshot
        ON snapshot.tenant_id = account.tenant_id
       AND snapshot.upstream_account_id = account.id
       AND snapshot.id = catalog.current_snapshot_id
      LEFT JOIN upstream_account_health health ON health.upstream_account_id = account.id
), ranked_groups AS MATERIALIZED (
    SELECT membership.upstream_account_id, provider_group.id AS provider_group_id,
           provider_group.name AS provider_group_label,
           COUNT(*) OVER (PARTITION BY membership.upstream_account_id) AS provider_group_count,
           ROW_NUMBER() OVER (PARTITION BY membership.upstream_account_id ORDER BY provider_group.id) AS provider_group_rank
      FROM upstream_account_provider_groups membership
      JOIN provider_groups provider_group
        ON provider_group.tenant_id = membership.tenant_id AND provider_group.id = membership.provider_group_id
      JOIN tenants tenant ON tenant.id = membership.tenant_id AND tenant.external_id = $1
)
SELECT source.*, provider_group.provider_group_id, provider_group.provider_group_label,
       COALESCE(provider_group.provider_group_count, 0) AS provider_group_count
  FROM ranked_sources source
  LEFT JOIN ranked_groups provider_group
    ON provider_group.upstream_account_id = source.account_id
   AND provider_group.provider_group_rank <= $14
 WHERE source.source_rank <= $13
 ORDER BY source.sort_label, source.item_identity, source.source_rank, provider_group.provider_group_rank
"#;

#[cfg(test)]
mod tests {
    use super::catalog_evidence_status;

    #[test]
    fn catalog_evidence_distinguishes_absence_loading_stale_and_retained_snapshots() {
        assert_eq!(
            catalog_evidence_status(None, false, None, None, 10),
            "never_observed"
        );
        assert_eq!(
            catalog_evidence_status(Some("unknown"), false, None, None, 10),
            "never_observed"
        );
        assert_eq!(
            catalog_evidence_status(Some("syncing"), false, None, None, 10),
            "loading"
        );
        assert_eq!(
            catalog_evidence_status(Some("syncing"), true, None, Some(20), 10),
            "partial"
        );
        assert_eq!(
            catalog_evidence_status(Some("ready"), true, None, Some(9), 10),
            "stale"
        );
        assert_eq!(
            catalog_evidence_status(Some("ready"), true, None, Some(20), 10),
            "ready"
        );
        assert_eq!(
            catalog_evidence_status(Some("stale"), true, Some("rate_limited"), Some(20), 10),
            "partial"
        );
        assert_eq!(
            catalog_evidence_status(Some("error"), false, Some("invalid_response"), None, 10),
            "error"
        );
    }
}
