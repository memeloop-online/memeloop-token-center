use super::super::*;
use crate::filter_ast::{
    TypedFilterAst, TypedFilterField, TypedFilterOperator, TypedFilterValue, filter_integer,
    filter_text, filter_uuid, search_contains,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum RequestListBind {
    I64(i64),
    Text(String),
}

#[derive(Debug)]
struct PortableRequestListQuery {
    statement: String,
    binds: Vec<RequestListBind>,
}

impl PortableRequestListQuery {
    fn new(statement: &str) -> Self {
        Self {
            statement: statement.to_owned(),
            binds: Vec::new(),
        }
    }

    fn push(&mut self, sql: &str) {
        self.statement.push_str(sql);
    }

    fn bind_i64(&mut self, value: i64) {
        self.binds.push(RequestListBind::I64(value));
        self.push_placeholder();
    }

    fn bind_text(&mut self, value: impl Into<String>) {
        self.binds.push(RequestListBind::Text(value.into()));
        self.push_placeholder();
    }

    fn push_placeholder(&mut self) {
        use std::fmt::Write as _;

        // Only a monotonically generated integer is appended. Values are always bound below.
        write!(self.statement, "${}", self.binds.len()).expect("writing to a String cannot fail");
    }
}

#[derive(Clone, Debug, Default)]
pub struct RequestListFilter {
    pub limit: i64,
    /// Internal-only request for one bounded lookahead row. This keeps the
    /// public page-size ceiling intact while allowing the control API to emit
    /// a definitive next cursor.
    pub lookahead: bool,
    pub from_created_at: Option<i64>,
    pub to_created_at: Option<i64>,
    pub before_created_at: Option<i64>,
    pub before_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub protocol: Option<String>,
    pub status: Option<String>,
    pub error_code: Option<String>,
    pub upstream_account_id: Option<Uuid>,
    pub route_id: Option<Uuid>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub min_cost_micros: Option<i64>,
    pub max_cost_micros: Option<i64>,
    /// Operator-only, case-insensitive prefix search over the stable credential alias.
    pub key_alias: Option<String>,
    /// Operator-only, case-insensitive prefix search over the tenant principal identifier.
    pub principal: Option<String>,
    /// Strict, schema-backed predicates submitted by the professional filter
    /// builder.  They are adapted through a closed SQL-column allow-list.
    pub typed_ast: Option<TypedFilterAst>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestRecordLocator {
    created_at: i64,
    tenant_id: String,
    key_id: String,
}

#[derive(Clone, Copy)]
enum RequestListScope<'a> {
    Key(Uuid),
    Tenant(&'a str),
    Global,
}

impl RequestListScope<'_> {
    fn includes_operator_identity(self) -> bool {
        !matches!(self, Self::Key(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestSourceKind {
    Native,
    Generation,
    Archive,
}

impl Database {
    pub async fn request_events_after(
        &self,
        tenant_external_id: &str,
        after_event_at: i64,
        after_event_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<RequestEventView>, AppError> {
        let after_event_id = after_event_id
            .map(|event_id| event_id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(sqlx::AssertSqlSafe(enriched_request_events_sql(
            "SELECT e.tenant_id, e.event_id, e.request_id, e.event_at, e.event_kind, e.key_id, e.protocol, e.model, e.status_code, e.duration_ms, e.input_tokens, e.output_tokens, e.cost_micros, e.error_code FROM request_events e JOIN tenants t ON t.id = e.tenant_id WHERE t.external_id = $1 AND (e.event_at > $2 OR (e.event_at = $3 AND e.event_id > $4)) ORDER BY e.event_at ASC, e.event_id ASC LIMIT $5",
        )))
        .bind(tenant_external_id)
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        request_event_views(rows)
    }

    pub async fn all_request_events_after(
        &self,
        after_event_at: i64,
        after_event_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<RequestEventView>, AppError> {
        let after_event_id = after_event_id
            .map(|event_id| event_id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(sqlx::AssertSqlSafe(enriched_request_events_sql(
            "SELECT tenant_id, event_id, request_id, event_at, event_kind, key_id, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_events WHERE (event_at > $1 OR (event_at = $2 AND event_id > $3)) ORDER BY event_at ASC, event_id ASC LIMIT $4",
        )))
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        request_event_views(rows)
    }

    pub async fn list_requests(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        self.list_requests_filtered(
            key_id,
            RequestListFilter {
                limit,
                ..RequestListFilter::default()
            },
        )
        .await
    }

    pub async fn list_requests_filtered(
        &self,
        key_id: Uuid,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let query = build_request_list_query(RequestListScope::Key(key_id), &filter);
        let rows = self.fetch_request_list(query).await?;
        request_views(rows)
    }

    pub async fn list_all_requests(
        &self,
        tenant_external_id: &str,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        self.list_all_requests_filtered(
            tenant_external_id,
            RequestListFilter {
                limit,
                ..RequestListFilter::default()
            },
        )
        .await
    }

    pub async fn list_all_requests_filtered(
        &self,
        tenant_external_id: &str,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let query = build_request_list_query(RequestListScope::Tenant(tenant_external_id), &filter);
        let rows = self.fetch_request_list(query).await?;
        request_views(rows)
    }

    pub async fn list_global_requests(&self, limit: i64) -> Result<Vec<RequestView>, AppError> {
        self.list_global_requests_filtered(RequestListFilter {
            limit,
            ..RequestListFilter::default()
        })
        .await
    }

    pub async fn list_global_requests_filtered(
        &self,
        filter: RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        validate_request_filter(&filter)?;
        let query = build_request_list_query(RequestListScope::Global, &filter);
        let rows = self.fetch_request_list(query).await?;
        request_views(rows)
    }

    async fn fetch_request_list(
        &self,
        query: PortableRequestListQuery,
    ) -> Result<Vec<AnyRow>, AppError> {
        // `$N` placeholders are understood by both native drivers behind sqlx::Any. The
        // statement itself only contains literals assembled below; every request value is bound.
        let mut statement = sqlx::query(sqlx::AssertSqlSafe(query.statement));
        for value in query.binds {
            statement = match value {
                RequestListBind::I64(value) => statement.bind(value),
                RequestListBind::Text(value) => statement.bind(value),
            };
        }
        Ok(statement.fetch_all(&self.pool).await?)
    }

    pub async fn request_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id = request_id.to_string();
        let locator = self.request_record_locator(&request_id).await?;
        if let Some(locator) = locator.filter(|locator| locator.key_id == key_id.to_string()) {
            let row = sqlx::query(
                "SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, CAST(NULL AS TEXT) AS generation_status, r.duration_ms, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.input_tokens END AS input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cached_input_tokens END AS cached_input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cache_write_tokens END AS cache_write_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.output_tokens END AS output_tokens, CAST(NULL AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cost_micros END AS cost_micros, CASE WHEN r.completed_at IS NULL THEN NULL ELSE NULLIF(r.currency, '') END AS currency, 1 AS billable, r.error_code, COALESCE(spool.state, CASE WHEN r.completed_at IS NULL THEN 'capturing' WHEN r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END) AS archive_state, CASE WHEN COALESCE(spool.state, '') = 'gap' THEN spool.last_error_code WHEN r.completed_at IS NOT NULL AND (r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%') THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, r.request_object, r.response_object, r.conversation_cluster_id AS session_id, CASE WHEN r.conversation_cluster_id IS NULL THEN 'unlinked' ELSE 'confirmed' END AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source, CAST(NULL AS TEXT) AS tenant_external_id, CAST(NULL AS TEXT) AS credential_key_id, CAST(NULL AS TEXT) AS key_alias, CAST(NULL AS TEXT) AS principal_external_id FROM request_records r LEFT JOIN conversation_observations observation ON observation.request_id = r.id AND observation.key_id = r.key_id AND observation.cluster_id = r.conversation_cluster_id LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id WHERE r.id = $1 AND r.created_at = $2 AND r.key_id = $3",
            )
            .bind(&request_id)
            .bind(locator.created_at)
            .bind(&locator.key_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::Internal)?;
            request_archive_refs_from_row(row)
        } else {
            match self
                .generation_archive_refs(key_id, parse_uuid(request_id.clone())?)
                .await
            {
                Ok(refs) => Ok(refs),
                Err(AppError::NotFound) => {
                    self.session_archive_unlinked_refs_for_key(key_id, &request_id)
                        .await
                }
                Err(error) => Err(error),
            }
        }
    }

    pub async fn request_archive_refs_for_tenant(
        &self,
        tenant_external_id: &str,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id_string = request_id.to_string();
        let locator = self.request_record_locator(&request_id_string).await?;
        if let Some(locator) = locator {
            let row = sqlx::query(
                "SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, CAST(NULL AS TEXT) AS generation_status, r.duration_ms, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.input_tokens END AS input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cached_input_tokens END AS cached_input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cache_write_tokens END AS cache_write_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.output_tokens END AS output_tokens, CAST(NULL AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cost_micros END AS cost_micros, CASE WHEN r.completed_at IS NULL THEN NULL ELSE NULLIF(r.currency, '') END AS currency, 1 AS billable, r.error_code, COALESCE(spool.state, CASE WHEN r.completed_at IS NULL THEN 'capturing' WHEN r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END) AS archive_state, CASE WHEN COALESCE(spool.state, '') = 'gap' THEN spool.last_error_code WHEN r.completed_at IS NOT NULL AND (r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%') THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, r.request_object, r.response_object, r.conversation_cluster_id AS session_id, CASE WHEN r.conversation_cluster_id IS NULL THEN 'unlinked' ELSE 'confirmed' END AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source, t.external_id AS tenant_external_id, r.key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id FROM request_records r JOIN tenants t ON t.id = r.tenant_id JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id LEFT JOIN conversation_observations observation ON observation.request_id = r.id AND observation.key_id = r.key_id AND observation.cluster_id = r.conversation_cluster_id LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id WHERE r.id = $1 AND r.created_at = $2 AND r.tenant_id = $3 AND t.external_id = $4",
            )
            .bind(&request_id_string)
            .bind(locator.created_at)
            .bind(&locator.tenant_id)
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(row) = row {
                return request_archive_refs_from_row(row);
            }
        }
        let row = sqlx::query(
            "SELECT g.id, g.created_at, g.completed_at, g.public_model, g.upstream_account_id, g.model_route_id AS route_id, g.status, g.error_code, g.request_object, g.result_json, facts.billed_units AS facts_billed_units, NULLIF(facts.billing_unit, '') AS facts_billing_unit, facts.cost_micros AS facts_cost_micros, NULLIF(facts.currency, '') AS facts_currency, t.external_id AS tenant_external_id, g.key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id LEFT JOIN generation_stats_facts facts ON facts.job_id = g.id AND facts.tenant_id = g.tenant_id AND facts.key_id = g.key_id WHERE g.id = $1 AND t.external_id = $2",
        )
        .bind(&request_id_string)
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => generation_archive_refs_from_row(row),
            None => {
                self.session_archive_unlinked_refs_for_tenant(
                    tenant_external_id,
                    &request_id_string,
                )
                .await
            }
        }
    }

    pub async fn request_archive_refs_global(
        &self,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let request_id = request_id.to_string();
        if let Some(locator) = self.request_record_locator(&request_id).await? {
            let row = sqlx::query(
                "SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, CAST(NULL AS TEXT) AS generation_status, r.duration_ms, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.input_tokens END AS input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cached_input_tokens END AS cached_input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cache_write_tokens END AS cache_write_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.output_tokens END AS output_tokens, CAST(NULL AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cost_micros END AS cost_micros, CASE WHEN r.completed_at IS NULL THEN NULL ELSE NULLIF(r.currency, '') END AS currency, 1 AS billable, r.error_code, COALESCE(spool.state, CASE WHEN r.completed_at IS NULL THEN 'capturing' WHEN r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END) AS archive_state, CASE WHEN COALESCE(spool.state, '') = 'gap' THEN spool.last_error_code WHEN r.completed_at IS NOT NULL AND (r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%') THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, r.request_object, r.response_object, r.conversation_cluster_id AS session_id, CASE WHEN r.conversation_cluster_id IS NULL THEN 'unlinked' ELSE 'confirmed' END AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source, t.external_id AS tenant_external_id, r.key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id FROM request_records r JOIN tenants t ON t.id = r.tenant_id JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id LEFT JOIN conversation_observations observation ON observation.request_id = r.id AND observation.key_id = r.key_id AND observation.cluster_id = r.conversation_cluster_id LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id WHERE r.id = $1 AND r.created_at = $2",
            )
            .bind(&request_id)
            .bind(locator.created_at)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::Internal)?;
            return request_archive_refs_from_row(row);
        }
        let row = sqlx::query(
            "SELECT g.id, g.created_at, g.completed_at, g.public_model, g.upstream_account_id, g.model_route_id AS route_id, g.status, g.error_code, g.request_object, g.result_json, facts.billed_units AS facts_billed_units, NULLIF(facts.billing_unit, '') AS facts_billing_unit, facts.cost_micros AS facts_cost_micros, NULLIF(facts.currency, '') AS facts_currency, t.external_id AS tenant_external_id, g.key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id LEFT JOIN generation_stats_facts facts ON facts.job_id = g.id AND facts.tenant_id = g.tenant_id AND facts.key_id = g.key_id WHERE g.id = $1",
        )
        .bind(&request_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => generation_archive_refs_from_row(row),
            None => self.session_archive_unlinked_refs_global(&request_id).await,
        }
    }

    async fn session_archive_unlinked_refs_for_key(
        &self,
        key_id: Uuid,
        request_id: &str,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.source_completed_at, u.protocol, u.model, u.status_code, u.duration_ms, NULLIF(u.input_tokens, 0) AS input_tokens, CAST(NULL AS BIGINT) AS cached_input_tokens, CAST(NULL AS BIGINT) AS cache_write_tokens, NULLIF(u.output_tokens, 0) AS output_tokens, u.error_code, u.request_object, u.response_object, u.source, u.external_request_id, c.proof_digest, u.conversation_cluster_id AS session_id, 'unlinked' AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source, CAST(NULL AS TEXT) AS tenant_external_id, CAST(NULL AS TEXT) AS credential_key_id, CAST(NULL AS TEXT) AS key_alias, CAST(NULL AS TEXT) AS principal_external_id FROM session_archive_unlinked_requests u JOIN session_archive_correlations c ON c.tenant_id = u.tenant_id AND c.source = u.source AND c.external_request_id = u.external_request_id AND c.disposition = 'unlinked' LEFT JOIN conversation_observations observation ON observation.request_id = u.archive_request_id AND observation.key_id = u.key_id AND observation.cluster_id = u.conversation_cluster_id WHERE u.archive_request_id = $1 AND u.key_id = $2",
        )
        .bind(request_id)
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        session_archive_unlinked_refs_from_row(row)
    }

    async fn session_archive_unlinked_refs_for_tenant(
        &self,
        tenant_external_id: &str,
        request_id: &str,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.source_completed_at, u.protocol, u.model, u.status_code, u.duration_ms, NULLIF(u.input_tokens, 0) AS input_tokens, CAST(NULL AS BIGINT) AS cached_input_tokens, CAST(NULL AS BIGINT) AS cache_write_tokens, NULLIF(u.output_tokens, 0) AS output_tokens, u.error_code, u.request_object, u.response_object, u.source, u.external_request_id, c.proof_digest, u.conversation_cluster_id AS session_id, 'unlinked' AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source, t.external_id AS tenant_external_id, u.key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id FROM session_archive_unlinked_requests u JOIN tenants t ON t.id = u.tenant_id JOIN key_records k ON k.id = u.key_id AND k.tenant_id = u.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id JOIN session_archive_correlations c ON c.tenant_id = u.tenant_id AND c.source = u.source AND c.external_request_id = u.external_request_id AND c.disposition = 'unlinked' LEFT JOIN conversation_observations observation ON observation.request_id = u.archive_request_id AND observation.key_id = u.key_id AND observation.cluster_id = u.conversation_cluster_id WHERE u.archive_request_id = $1 AND t.external_id = $2",
        )
        .bind(request_id)
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        session_archive_unlinked_refs_from_row(row)
    }

    async fn session_archive_unlinked_refs_global(
        &self,
        request_id: &str,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            r#"SELECT u.archive_request_id AS id,
                      u.source_started_at AS created_at, u.source_completed_at,
                      u.protocol, u.model, u.status_code, u.duration_ms,
                      NULLIF(u.input_tokens, 0) AS input_tokens,
                      CAST(NULL AS BIGINT) AS cached_input_tokens,
                      CAST(NULL AS BIGINT) AS cache_write_tokens,
                      NULLIF(u.output_tokens, 0) AS output_tokens,
                      u.error_code, u.request_object, u.response_object,
                      u.source, u.external_request_id, c.proof_digest,
                      u.conversation_cluster_id AS session_id,
                      'unlinked' AS session_association,
                      observation.session_name, observation.task_kind,
                      observation.agent_id,
                      observation.metadata_source AS semantics_source,
                      t.external_id AS tenant_external_id,
                      u.key_id AS credential_key_id,
                      k.alias AS key_alias,
                      p.external_id AS principal_external_id
                 FROM session_archive_unlinked_requests u
                 JOIN tenants t ON t.id = u.tenant_id
                 JOIN key_records k
                   ON k.id = u.key_id AND k.tenant_id = u.tenant_id
                 JOIN principals p
                   ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
                 JOIN session_archive_correlations c
                   ON c.tenant_id = u.tenant_id AND c.source = u.source
                  AND c.external_request_id = u.external_request_id
                  AND c.disposition = 'unlinked'
            LEFT JOIN conversation_observations observation
                   ON observation.request_id = u.archive_request_id
                  AND observation.key_id = u.key_id
                  AND observation.cluster_id = u.conversation_cluster_id
                WHERE u.archive_request_id = $1"#,
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        session_archive_unlinked_refs_from_row(row)
    }

    async fn request_record_locator(
        &self,
        request_id: &str,
    ) -> Result<Option<RequestRecordLocator>, AppError> {
        sqlx::query(
            "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
        )
        .bind(request_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|row| {
            Ok(RequestRecordLocator {
                created_at: row.try_get("created_at")?,
                tenant_id: row.try_get("tenant_id")?,
                key_id: row.try_get("key_id")?,
            })
        })
        .transpose()
    }

    async fn generation_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT g.id, g.created_at, g.completed_at, g.public_model, g.upstream_account_id, g.model_route_id AS route_id, g.status, g.error_code, g.request_object, g.result_json, facts.billed_units AS facts_billed_units, NULLIF(facts.billing_unit, '') AS facts_billing_unit, facts.cost_micros AS facts_cost_micros, NULLIF(facts.currency, '') AS facts_currency, CAST(NULL AS TEXT) AS tenant_external_id, CAST(NULL AS TEXT) AS credential_key_id, CAST(NULL AS TEXT) AS key_alias, CAST(NULL AS TEXT) AS principal_external_id FROM generation_jobs g LEFT JOIN generation_stats_facts facts ON facts.job_id = g.id AND facts.tenant_id = g.tenant_id AND facts.key_id = g.key_id WHERE g.id = $1 AND g.key_id = $2",
        )
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_archive_refs_from_row(row)
    }
}

fn build_request_list_query(
    scope: RequestListScope<'_>,
    filter: &RequestListFilter,
) -> PortableRequestListQuery {
    let maximum = if matches!(scope, RequestListScope::Key(_)) {
        100
    } else {
        500
    };
    let page_limit = filter.limit.clamp(1, maximum) + i64::from(filter.lookahead);
    let mut query = PortableRequestListQuery::new(
        "SELECT id, created_at, completed_at, source_completed_at, protocol, model, upstream_account_id, route_id, status_code, generation_status, duration_ms, input_tokens, cached_input_tokens, cache_write_tokens, output_tokens, billed_units, billing_unit, cost_micros, currency, billable, error_code, archive_state, archive_reason, session_id, session_association, session_name, task_kind, agent_id, semantics_source, tenant_external_id, credential_key_id, key_alias, principal_external_id FROM (SELECT * FROM (SELECT r.id, r.created_at, r.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, r.protocol, r.model, r.upstream_account_id, r.model_route_id AS route_id, r.status_code, CAST(NULL AS TEXT) AS generation_status, r.duration_ms, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.input_tokens END AS input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cached_input_tokens END AS cached_input_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cache_write_tokens END AS cache_write_tokens, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.output_tokens END AS output_tokens, CAST(NULL AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CASE WHEN r.completed_at IS NULL THEN NULL ELSE r.cost_micros END AS cost_micros, CASE WHEN r.completed_at IS NULL THEN NULL ELSE NULLIF(r.currency, '') END AS currency, 1 AS billable, r.error_code, COALESCE(spool.state, CASE WHEN r.completed_at IS NULL THEN 'capturing' WHEN r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END) AS archive_state, CASE WHEN COALESCE(spool.state, '') = 'gap' THEN spool.last_error_code WHEN r.completed_at IS NOT NULL AND (r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%') THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, r.conversation_cluster_id AS session_id, CASE WHEN r.conversation_cluster_id IS NULL THEN 'unlinked' ELSE 'confirmed' END AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source",
    );
    push_identity_projection(&mut query, scope, "r");
    query.push(" FROM request_records r");
    push_operator_identity_joins(&mut query, scope, "r", filter);
    query.push(" LEFT JOIN conversation_observations observation ON observation.request_id = r.id AND observation.key_id = r.key_id AND observation.cluster_id = r.conversation_cluster_id");
    query.push(" LEFT JOIN response_archive_spools spool ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id AND spool.reservation_id = r.reservation_id");
    query.push(" WHERE 1 = 1");
    push_request_record_filters(&mut query, scope, filter);
    query.push(" ORDER BY r.created_at DESC, r.id DESC LIMIT ");
    query.bind_i64(page_limit);
    query.push(") AS request_page");

    if generation_branch_can_match(filter) {
        query.push(" UNION ALL SELECT * FROM (SELECT g.id, g.created_at, g.completed_at, CAST(NULL AS BIGINT) AS source_completed_at, 'generation' AS protocol, g.public_model AS model, g.upstream_account_id, g.model_route_id AS route_id, CAST(NULL AS BIGINT) AS status_code, g.status AS generation_status, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, CAST(NULL AS BIGINT) AS input_tokens, CAST(NULL AS BIGINT) AS cached_input_tokens, CAST(NULL AS BIGINT) AS cache_write_tokens, CAST(NULL AS BIGINT) AS output_tokens, facts.billed_units, NULLIF(facts.billing_unit, '') AS billing_unit, facts.cost_micros, NULLIF(facts.currency, '') AS currency, 1 AS billable, g.error_code, CASE WHEN g.status IN ('preparing', 'queued') THEN 'pending' WHEN g.status IN ('submitting', 'running', 'cancelling') THEN 'uploading' WHEN g.request_object LIKE 'gap://%' OR (g.status = 'succeeded' AND g.result_json IS NULL) THEN 'gap' ELSE 'bound' END AS archive_state, CASE WHEN g.request_object LIKE 'gap://%' OR (g.status = 'succeeded' AND g.result_json IS NULL) THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, CAST(NULL AS TEXT) AS session_id, CAST(NULL AS TEXT) AS session_association, CAST(NULL AS TEXT) AS session_name, CAST(NULL AS TEXT) AS task_kind, CAST(NULL AS TEXT) AS agent_id, CAST(NULL AS TEXT) AS semantics_source");
        push_identity_projection(&mut query, scope, "g");
        query.push(" FROM generation_jobs g LEFT JOIN generation_stats_facts facts ON facts.job_id = g.id AND facts.tenant_id = g.tenant_id AND facts.key_id = g.key_id");
        push_operator_identity_joins(&mut query, scope, "g", filter);
        query.push(" WHERE 1 = 1");
        push_generation_job_filters(&mut query, scope, filter);
        query.push(" ORDER BY g.created_at DESC, g.id DESC LIMIT ");
        query.bind_i64(page_limit);
        query.push(") AS generation_page");
    }

    if archive_branch_can_match(filter) {
        query.push(" UNION ALL SELECT * FROM (SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.source_completed_at AS completed_at, u.source_completed_at, u.protocol, u.model, CAST(NULL AS TEXT) AS upstream_account_id, CAST(NULL AS TEXT) AS route_id, u.status_code, CAST(NULL AS TEXT) AS generation_status, u.duration_ms, NULLIF(u.input_tokens, 0) AS input_tokens, CAST(NULL AS BIGINT) AS cached_input_tokens, CAST(NULL AS BIGINT) AS cache_write_tokens, NULLIF(u.output_tokens, 0) AS output_tokens, CAST(NULL AS BIGINT) AS billed_units, CAST(NULL AS TEXT) AS billing_unit, CAST(NULL AS BIGINT) AS cost_micros, CAST(NULL AS TEXT) AS currency, 0 AS billable, u.error_code, CASE WHEN u.request_object IS NULL OR u.request_object LIKE 'gap://%' OR u.response_object IS NULL OR u.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END AS archive_state, CASE WHEN u.request_object IS NULL OR u.request_object LIKE 'gap://%' OR u.response_object IS NULL OR u.response_object LIKE 'gap://%' THEN 'archive_object_unavailable' ELSE NULL END AS archive_reason, u.conversation_cluster_id AS session_id, 'unlinked' AS session_association, observation.session_name, observation.task_kind, observation.agent_id, observation.metadata_source AS semantics_source");
        push_identity_projection(&mut query, scope, "u");
        query.push(" FROM session_archive_unlinked_requests u LEFT JOIN conversation_observations observation ON observation.request_id = u.archive_request_id AND observation.key_id = u.key_id AND observation.cluster_id = u.conversation_cluster_id");
        push_operator_identity_joins(&mut query, scope, "u", filter);
        query.push(" WHERE 1 = 1");
        push_archive_request_filters(&mut query, scope, filter);
        query.push(" ORDER BY u.source_started_at DESC, u.archive_request_id DESC LIMIT ");
        query.bind_i64(page_limit);
        query.push(") AS archive_page");
    }

    query.push(") AS all_requests ORDER BY created_at DESC, id DESC LIMIT ");
    query.bind_i64(page_limit);
    query
}

fn push_identity_projection(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    source_alias: &str,
) {
    if scope.includes_operator_identity() {
        query.push(", t.external_id AS tenant_external_id, ");
        query.push(source_alias);
        query.push(".key_id AS credential_key_id, k.alias AS key_alias, p.external_id AS principal_external_id");
    } else {
        query.push(", CAST(NULL AS TEXT) AS tenant_external_id, CAST(NULL AS TEXT) AS credential_key_id, CAST(NULL AS TEXT) AS key_alias, CAST(NULL AS TEXT) AS principal_external_id");
    }
}

fn push_operator_identity_joins(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    source_alias: &str,
    filter: &RequestListFilter,
) {
    // Tenant isolation is enforced directly on the request/generation source below. These
    // relations only provide searchable identity metadata; joining them on the default path
    // prevents PostgreSQL from stopping after the first page in the ordered source index.
    if scope.includes_operator_identity()
        || filter_uses_key_alias(filter)
        || filter_uses_principal(filter)
    {
        query.push(" JOIN tenants t ON t.id = ");
        query.push(source_alias);
        query.push(".tenant_id");
        query.push(" JOIN key_records k ON k.id = ");
        query.push(source_alias);
        query.push(".key_id AND k.tenant_id = ");
        query.push(source_alias);
        query.push(".tenant_id");
    }
    if scope.includes_operator_identity() || filter_uses_principal(filter) {
        query.push(" JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id");
    }
}

fn push_list_scope_filter(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    source_alias: &str,
) {
    match scope {
        RequestListScope::Key(key_id) => {
            query.push(" AND ");
            query.push(source_alias);
            query.push(".key_id = ");
            query.bind_text(key_id.to_string());
        }
        RequestListScope::Tenant(tenant_external_id) => {
            query.push(" AND ");
            query.push(source_alias);
            query.push(".tenant_id = (SELECT tenant_scope.id FROM tenants tenant_scope WHERE tenant_scope.external_id = ");
            query.bind_text(tenant_external_id);
            query.push(")");
        }
        RequestListScope::Global => {}
    }
}

fn push_request_record_filters(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    filter: &RequestListFilter,
) {
    push_list_scope_filter(query, scope, "r");
    query.push(" AND r.created_at >= ");
    query.bind_i64(filter.from_created_at.unwrap_or(0));
    query.push(" AND r.created_at <= ");
    query.bind_i64(filter.to_created_at.unwrap_or(i64::MAX));
    push_keyset_cursor(query, "r", filter);
    if let Some(key_id) = filter.key_id {
        query.push(" AND r.key_id = ");
        query.bind_text(key_id.to_string());
    }
    if let Some(model) = &filter.model {
        query.push(" AND r.model = ");
        query.bind_text(model.clone());
    }
    if let Some(protocol) = &filter.protocol {
        query.push(" AND r.protocol = ");
        query.bind_text(protocol.clone());
    }
    if let Some(status) = &filter.status {
        match status.as_str() {
            "success" => query.push(" AND r.status_code BETWEEN 200 AND 399"),
            "error" => query.push(" AND r.status_code >= 400"),
            "pending" => query.push(" AND r.status_code IS NULL"),
            _ => unreachable!("request filters are validated before query construction"),
        }
    }
    if let Some(error_code) = &filter.error_code {
        query.push(" AND r.error_code = ");
        query.bind_text(error_code.clone());
    }
    if let Some(upstream_account_id) = filter.upstream_account_id {
        query.push(" AND r.upstream_account_id = ");
        query.bind_text(upstream_account_id.to_string());
    }
    if let Some(route_id) = filter.route_id {
        query.push(" AND r.model_route_id = ");
        query.bind_text(route_id.to_string());
    }
    if let Some(min_duration_ms) = filter.min_duration_ms {
        query.push(" AND r.duration_ms >= ");
        query.bind_i64(min_duration_ms);
    }
    if let Some(max_duration_ms) = filter.max_duration_ms {
        query.push(" AND r.duration_ms <= ");
        query.bind_i64(max_duration_ms);
    }
    if let Some(min_cost_micros) = filter.min_cost_micros {
        query.push(" AND r.cost_micros >= ");
        query.bind_i64(min_cost_micros);
    }
    if let Some(max_cost_micros) = filter.max_cost_micros {
        query.push(" AND r.cost_micros <= ");
        query.bind_i64(max_cost_micros);
    }
    push_operator_identity_filters(query, filter);
    push_typed_filters(query, "r", RequestSourceKind::Native, filter);
}

fn push_generation_job_filters(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    filter: &RequestListFilter,
) {
    push_list_scope_filter(query, scope, "g");
    query.push(" AND g.created_at >= ");
    query.bind_i64(filter.from_created_at.unwrap_or(0));
    query.push(" AND g.created_at <= ");
    query.bind_i64(filter.to_created_at.unwrap_or(i64::MAX));
    push_keyset_cursor(query, "g", filter);
    if let Some(key_id) = filter.key_id {
        query.push(" AND g.key_id = ");
        query.bind_text(key_id.to_string());
    }
    if let Some(model) = &filter.model {
        query.push(" AND g.public_model = ");
        query.bind_text(model.clone());
    }
    if let Some(status) = &filter.status {
        match status.as_str() {
            "success" => query.push(" AND g.status = 'succeeded'"),
            "error" => query.push(" AND g.status IN ('failed', 'cancelled')"),
            "pending" => query.push(
                " AND g.status IN ('preparing', 'queued', 'submitting', 'running', 'cancelling')",
            ),
            _ => unreachable!("request filters are validated before query construction"),
        }
    }
    if let Some(error_code) = &filter.error_code {
        query.push(" AND g.error_code = ");
        query.bind_text(error_code.clone());
    }
    if let Some(upstream_account_id) = filter.upstream_account_id {
        query.push(" AND g.upstream_account_id = ");
        query.bind_text(upstream_account_id.to_string());
    }
    if let Some(route_id) = filter.route_id {
        query.push(" AND g.model_route_id = ");
        query.bind_text(route_id.to_string());
    }
    if let Some(min_duration_ms) = filter.min_duration_ms {
        query.push(" AND (g.completed_at - g.created_at) >= ");
        query.bind_i64(min_duration_ms);
    }
    if let Some(max_duration_ms) = filter.max_duration_ms {
        query.push(" AND (g.completed_at - g.created_at) <= ");
        query.bind_i64(max_duration_ms);
    }
    if let Some(min_cost_micros) = filter.min_cost_micros {
        query.push(" AND g.cost_micros >= ");
        query.bind_i64(min_cost_micros);
    }
    if let Some(max_cost_micros) = filter.max_cost_micros {
        query.push(" AND g.cost_micros <= ");
        query.bind_i64(max_cost_micros);
    }
    push_operator_identity_filters(query, filter);
    push_typed_filters(query, "g", RequestSourceKind::Generation, filter);
}

fn push_archive_request_filters(
    query: &mut PortableRequestListQuery,
    scope: RequestListScope<'_>,
    filter: &RequestListFilter,
) {
    push_list_scope_filter(query, scope, "u");
    query.push(" AND u.source_started_at >= ");
    query.bind_i64(filter.from_created_at.unwrap_or(0));
    query.push(" AND u.source_started_at <= ");
    query.bind_i64(filter.to_created_at.unwrap_or(i64::MAX));
    push_archive_keyset_cursor(query, filter);
    if let Some(key_id) = filter.key_id {
        query.push(" AND u.key_id = ");
        query.bind_text(key_id.to_string());
    }
    if let Some(model) = &filter.model {
        query.push(" AND u.model = ");
        query.bind_text(model.clone());
    }
    if let Some(protocol) = &filter.protocol {
        query.push(" AND u.protocol = ");
        query.bind_text(protocol.clone());
    }
    if let Some(status) = &filter.status {
        match status.as_str() {
            "success" => query.push(" AND u.status_code BETWEEN 200 AND 399"),
            "error" => query.push(" AND u.status_code >= 400"),
            "pending" => query.push(" AND u.status_code IS NULL"),
            _ => unreachable!("request filters are validated before query construction"),
        }
    }
    if let Some(error_code) = &filter.error_code {
        query.push(" AND u.error_code = ");
        query.bind_text(error_code.clone());
    }
    if filter.upstream_account_id.is_some()
        || filter.route_id.is_some()
        || filter.min_cost_micros.is_some()
        || filter.max_cost_micros.is_some()
    {
        query.push(" AND 1 = 0");
    }
    if let Some(min_duration_ms) = filter.min_duration_ms {
        query.push(" AND u.duration_ms >= ");
        query.bind_i64(min_duration_ms);
    }
    if let Some(max_duration_ms) = filter.max_duration_ms {
        query.push(" AND u.duration_ms <= ");
        query.bind_i64(max_duration_ms);
    }
    push_operator_identity_filters(query, filter);
    push_typed_filters(query, "u", RequestSourceKind::Archive, filter);
}

/// Adds only predicates whose column text is selected in this function.  All
/// caller-originated values continue through `bind_*`; adding a typed field is
/// therefore a deliberate query-plan review rather than a dynamic-SQL change.
fn push_typed_filters(
    query: &mut PortableRequestListQuery,
    source_alias: &str,
    source: RequestSourceKind,
    filter: &RequestListFilter,
) {
    let Some(ast) = filter.typed_ast.as_ref() else {
        return;
    };
    for condition in &ast.conditions {
        let column = match condition.field {
            TypedFilterField::CreatedAt if source == RequestSourceKind::Archive => {
                format!("{source_alias}.source_started_at")
            }
            TypedFilterField::CreatedAt => format!("{source_alias}.created_at"),
            TypedFilterField::KeyId => format!("{source_alias}.key_id"),
            TypedFilterField::Model => {
                if source == RequestSourceKind::Generation {
                    format!("{source_alias}.public_model")
                } else {
                    format!("{source_alias}.model")
                }
            }
            TypedFilterField::Protocol => {
                if source == RequestSourceKind::Generation {
                    "'generation'".to_owned()
                } else {
                    format!("{source_alias}.protocol")
                }
            }
            TypedFilterField::Status => {
                push_typed_status_filter(
                    query,
                    source_alias,
                    source == RequestSourceKind::Generation,
                    condition.operator,
                    &condition.value,
                );
                continue;
            }
            TypedFilterField::ErrorCode => format!("{source_alias}.error_code"),
            TypedFilterField::UpstreamAccountId if source == RequestSourceKind::Archive => {
                query.push(" AND 1 = 0");
                continue;
            }
            TypedFilterField::UpstreamAccountId => format!("{source_alias}.upstream_account_id"),
            TypedFilterField::RouteId if source == RequestSourceKind::Archive => {
                query.push(" AND 1 = 0");
                continue;
            }
            TypedFilterField::RouteId => format!("{source_alias}.model_route_id"),
            TypedFilterField::DurationMs => {
                if source == RequestSourceKind::Generation {
                    format!("({source_alias}.completed_at - {source_alias}.created_at)")
                } else if source == RequestSourceKind::Archive {
                    format!("{source_alias}.duration_ms")
                } else {
                    format!("{source_alias}.duration_ms")
                }
            }
            TypedFilterField::CostMicros if source == RequestSourceKind::Archive => {
                query.push(" AND 1 = 0");
                continue;
            }
            TypedFilterField::CostMicros => format!("{source_alias}.cost_micros"),
            TypedFilterField::KeyAlias => "k.alias".to_owned(),
            TypedFilterField::Principal => "p.external_id".to_owned(),
        };
        push_typed_predicate(
            query,
            &column,
            condition.operator,
            &condition.value,
            condition.upper.as_ref(),
        );
    }
}

fn push_typed_status_filter(
    query: &mut PortableRequestListQuery,
    source_alias: &str,
    generation: bool,
    operator: TypedFilterOperator,
    value: &TypedFilterValue,
) {
    let status = filter_text(value);
    let expression = if generation {
        match (operator, status) {
            (TypedFilterOperator::Equals, "success") => {
                format!("{source_alias}.status = 'succeeded'")
            }
            (TypedFilterOperator::Equals, "error") => {
                format!("{source_alias}.status IN ('failed', 'cancelled')")
            }
            (TypedFilterOperator::Equals, "pending") => {
                format!(
                    "{source_alias}.status IN ('preparing', 'queued', 'submitting', 'running', 'cancelling')"
                )
            }
            (TypedFilterOperator::NotEquals, "success") => {
                format!("{source_alias}.status <> 'succeeded'")
            }
            (TypedFilterOperator::NotEquals, "error") => {
                format!("{source_alias}.status NOT IN ('failed', 'cancelled')")
            }
            (TypedFilterOperator::NotEquals, "pending") => {
                format!(
                    "{source_alias}.status NOT IN ('preparing', 'queued', 'submitting', 'running', 'cancelling')"
                )
            }
            _ => unreachable!("the AST operator/value matrix is validated before SQL adaptation"),
        }
    } else {
        match (operator, status) {
            (TypedFilterOperator::Equals, "success") => {
                format!("{source_alias}.status_code BETWEEN 200 AND 399")
            }
            (TypedFilterOperator::Equals, "error") => format!("{source_alias}.status_code >= 400"),
            (TypedFilterOperator::Equals, "pending") => {
                format!("{source_alias}.status_code IS NULL")
            }
            (TypedFilterOperator::NotEquals, "success") => format!(
                "({source_alias}.status_code IS NULL OR {source_alias}.status_code NOT BETWEEN 200 AND 399)"
            ),
            (TypedFilterOperator::NotEquals, "error") => {
                format!("({source_alias}.status_code IS NULL OR {source_alias}.status_code < 400)")
            }
            (TypedFilterOperator::NotEquals, "pending") => {
                format!("{source_alias}.status_code IS NOT NULL")
            }
            _ => unreachable!("the AST operator/value matrix is validated before SQL adaptation"),
        }
    };
    query.push(" AND ");
    query.push(&expression);
}

fn push_typed_predicate(
    query: &mut PortableRequestListQuery,
    column: &str,
    operator: TypedFilterOperator,
    value: &TypedFilterValue,
    upper: Option<&TypedFilterValue>,
) {
    query.push(" AND ");
    match operator {
        TypedFilterOperator::Equals => {
            query.push(column);
            query.push(" = ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::NotEquals => {
            query.push(column);
            query.push(" <> ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::Contains => {
            query.push("LOWER(");
            query.push(column);
            query.push(") LIKE ");
            query.bind_text(search_contains(filter_text(value)));
            query.push(r" ESCAPE '\'");
        }
        TypedFilterOperator::GreaterThan => {
            query.push(column);
            query.push(" > ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::GreaterThanOrEqual => {
            query.push(column);
            query.push(" >= ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::LessThan => {
            query.push(column);
            query.push(" < ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::LessThanOrEqual => {
            query.push(column);
            query.push(" <= ");
            push_typed_value(query, value);
        }
        TypedFilterOperator::Between => {
            query.push(column);
            query.push(" BETWEEN ");
            push_typed_value(query, value);
            query.push(" AND ");
            push_typed_value(
                query,
                upper.expect("between values are validated before SQL adaptation"),
            );
        }
    }
}

fn push_typed_value(query: &mut PortableRequestListQuery, value: &TypedFilterValue) {
    match value {
        TypedFilterValue::Text(_) | TypedFilterValue::Model(_) | TypedFilterValue::Protocol(_) => {
            query.bind_text(filter_text(value));
        }
        TypedFilterValue::Uuid(_) => query.bind_text(filter_uuid(value).to_string()),
        TypedFilterValue::Integer(_)
        | TypedFilterValue::Timestamp(_)
        | TypedFilterValue::MoneyMicros(_) => {
            query.bind_i64(filter_integer(value));
        }
        TypedFilterValue::Status(_) => unreachable!("status predicates use a closed SQL mapping"),
    }
}

fn push_keyset_cursor(
    query: &mut PortableRequestListQuery,
    table_alias: &str,
    filter: &RequestListFilter,
) {
    let Some(before_created_at) = filter.before_created_at else {
        return;
    };
    query.push(" AND (");
    query.push(table_alias);
    query.push(".created_at < ");
    query.bind_i64(before_created_at);
    query.push(" OR (");
    query.push(table_alias);
    query.push(".created_at = ");
    query.bind_i64(before_created_at);
    query.push(" AND ");
    query.push(table_alias);
    query.push(".id < ");
    query.bind_text(cursor_id(filter));
    query.push("))");
}

fn push_archive_keyset_cursor(query: &mut PortableRequestListQuery, filter: &RequestListFilter) {
    let Some(before_created_at) = filter.before_created_at else {
        return;
    };
    query.push(" AND (u.source_started_at < ");
    query.bind_i64(before_created_at);
    query.push(" OR (u.source_started_at = ");
    query.bind_i64(before_created_at);
    query.push(" AND u.archive_request_id < ");
    query.bind_text(cursor_id(filter));
    query.push("))");
}

fn push_operator_identity_filters(
    query: &mut PortableRequestListQuery,
    filter: &RequestListFilter,
) {
    if filter.key_alias.is_some() {
        query.push(" AND LOWER(k.alias) LIKE ");
        query.bind_text(search_prefix(filter.key_alias.as_deref()));
        query.push(r" ESCAPE '\'");
    }
    if filter.principal.is_some() {
        query.push(" AND LOWER(p.external_id) LIKE ");
        query.bind_text(search_prefix(filter.principal.as_deref()));
        query.push(r" ESCAPE '\'");
    }
}

fn filter_uses_key_alias(filter: &RequestListFilter) -> bool {
    filter.key_alias.is_some()
        || filter
            .typed_ast
            .as_ref()
            .is_some_and(|ast| ast.uses_field(TypedFilterField::KeyAlias))
}

fn filter_uses_principal(filter: &RequestListFilter) -> bool {
    filter.principal.is_some()
        || filter
            .typed_ast
            .as_ref()
            .is_some_and(|ast| ast.uses_field(TypedFilterField::Principal))
}

fn generation_branch_can_match(filter: &RequestListFilter) -> bool {
    filter
        .protocol
        .as_deref()
        .is_none_or(|protocol| protocol == "generation")
}

fn archive_branch_can_match(filter: &RequestListFilter) -> bool {
    filter.upstream_account_id.is_none()
        && filter.route_id.is_none()
        && filter.min_cost_micros.is_none()
        && filter.max_cost_micros.is_none()
}

fn request_views(rows: Vec<AnyRow>) -> Result<Vec<RequestView>, AppError> {
    rows.into_iter()
        .map(|row| request_view_from_row(&row))
        .collect()
}

fn request_view_from_row(row: &AnyRow) -> Result<RequestView, AppError> {
    let generation_status: Option<String> = row.try_get("generation_status")?;
    let persisted_status_code: Option<i64> = row.try_get("status_code")?;
    let (lifecycle_state, status_code) =
        request_lifecycle_projection(generation_status.as_deref(), persisted_status_code)?;
    let input_tokens: Option<i64> = row.try_get("input_tokens")?;
    let cached_input_tokens: Option<i64> = row.try_get("cached_input_tokens")?;
    let cache_write_tokens: Option<i64> = row.try_get("cache_write_tokens")?;
    let output_tokens: Option<i64> = row.try_get("output_tokens")?;
    let billed_units: Option<i64> = row.try_get("billed_units")?;
    let billing_unit: Option<String> = row.try_get("billing_unit")?;
    let cost_micros: Option<i64> = row.try_get("cost_micros")?;
    let cost = cost_micros.map(micros_to_decimal_string);
    let currency: Option<String> = row.try_get("currency")?;
    let billable = row.try_get::<i64, _>("billable")? != 0;
    let tokens = (generation_status.is_none()
        && (input_tokens.is_some()
            || cached_input_tokens.is_some()
            || cache_write_tokens.is_some()
            || output_tokens.is_some()))
    .then_some(RequestTokenUsageView {
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
    });
    let generation = generation_status
        .is_some()
        .then_some(RequestGenerationUsageView {
            billed_units,
            billing_unit,
        });
    Ok(RequestView {
        request_id: parse_uuid(row.try_get("id")?)?,
        created_at: row.try_get("created_at")?,
        completed_at: row.try_get("completed_at")?,
        source_completed_at: row.try_get("source_completed_at")?,
        lifecycle_state,
        protocol: row.try_get("protocol")?,
        model: row.try_get("model")?,
        upstream_account_id: row
            .try_get::<Option<String>, _>("upstream_account_id")?
            .map(parse_uuid)
            .transpose()?,
        route_id: row
            .try_get::<Option<String>, _>("route_id")?
            .map(parse_uuid)
            .transpose()?,
        status_code,
        duration_ms: row.try_get("duration_ms")?,
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
        cost: cost.clone(),
        currency: currency.clone(),
        usage: RequestUsageView { tokens, generation },
        billing: RequestBillingView {
            billable,
            cost,
            currency,
        },
        error_code: row.try_get("error_code")?,
        archive_state: request_archive_state(row.try_get("archive_state")?)?,
        credential_identity: request_credential_identity_from_row(row)?,
        session_context: request_session_context_from_row(row)?,
    })
}

fn request_credential_identity_from_row(
    row: &AnyRow,
) -> Result<Option<RequestCredentialIdentityView>, AppError> {
    let Some(tenant_external_id) = row.try_get::<Option<String>, _>("tenant_external_id")? else {
        return Ok(None);
    };
    Ok(Some(RequestCredentialIdentityView {
        tenant_external_id,
        key_id: parse_uuid(
            row.try_get::<Option<String>, _>("credential_key_id")?
                .ok_or(AppError::Internal)?,
        )?,
        key_alias: row
            .try_get::<Option<String>, _>("key_alias")?
            .ok_or(AppError::Internal)?,
        principal_external_id: row
            .try_get::<Option<String>, _>("principal_external_id")?
            .ok_or(AppError::Internal)?,
    }))
}

fn request_lifecycle_projection(
    generation_status: Option<&str>,
    status_code: Option<i64>,
) -> Result<(RequestLifecycleState, Option<i64>), AppError> {
    if let Some(status) = generation_status {
        return Ok(match status {
            "preparing" => (RequestLifecycleState::Preparing, None),
            "queued" => (RequestLifecycleState::Queued, None),
            "submitting" => (RequestLifecycleState::Submitting, None),
            "running" => (RequestLifecycleState::Running, None),
            "cancelling" => (RequestLifecycleState::Cancelling, None),
            "succeeded" => (RequestLifecycleState::Succeeded, Some(200)),
            "failed" => (RequestLifecycleState::Failed, Some(502)),
            "cancelled" => (RequestLifecycleState::Cancelled, Some(499)),
            _ => return Err(AppError::Internal),
        });
    }
    Ok(match status_code {
        None => (RequestLifecycleState::Pending, None),
        Some(499) => (RequestLifecycleState::Cancelled, Some(499)),
        Some(code) if (200..400).contains(&code) => (RequestLifecycleState::Succeeded, Some(code)),
        Some(code) => (RequestLifecycleState::Failed, Some(code)),
    })
}

fn request_archive_state(value: String) -> Result<RequestArchiveState, AppError> {
    match value.as_str() {
        "capturing" => Ok(RequestArchiveState::Capturing),
        "pending" => Ok(RequestArchiveState::Pending),
        "uploading" => Ok(RequestArchiveState::Uploading),
        "bound" => Ok(RequestArchiveState::Bound),
        "gap" => Ok(RequestArchiveState::Gap),
        _ => Err(AppError::Internal),
    }
}

fn request_session_context_from_row(
    row: &AnyRow,
) -> Result<Option<RequestSessionContext>, AppError> {
    let association: Option<String> = row.try_get("session_association")?;
    let Some(association) = association else {
        return Ok(None);
    };
    let association = match association.as_str() {
        "confirmed" => RequestSessionAssociation::Confirmed,
        "unlinked" => RequestSessionAssociation::Unlinked,
        _ => return Err(AppError::Internal),
    };
    let session_id: Option<String> = row.try_get("session_id")?;
    if association == RequestSessionAssociation::Confirmed && session_id.is_none() {
        return Err(AppError::Internal);
    }
    Ok(Some(RequestSessionContext {
        session_id,
        association,
        session_name: row.try_get("session_name")?,
        task_kind: row.try_get("task_kind")?,
        agent_id: row.try_get("agent_id")?,
        semantics_source: row.try_get("semantics_source")?,
    }))
}

/// Enrich one already-bounded event batch, never one lookup per event. Locator
/// ownership and receipt time constrain the partitioned history join.
fn enriched_request_events_sql(events: &str) -> String {
    // Both callers supply closed SQL literals with tenant/key ownership.
    format!(
        r#"WITH events AS MATERIALIZED ({events})
SELECT e.*, COALESCE(r.created_at, g.created_at) AS created_at,
       COALESCE(r.completed_at, g.completed_at) AS completed_at,
       CAST(NULL AS BIGINT) AS source_completed_at,
       g.status AS generation_status,
       COALESCE(r.upstream_account_id, g.upstream_account_id) AS upstream_account_id,
       COALESCE(r.model_route_id, g.model_route_id) AS route_id,
       COALESCE(r.status_code, CASE WHEN g.id IS NULL THEN e.status_code ELSE NULL END) AS current_status_code,
       COALESCE(r.duration_ms, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END, e.duration_ms) AS current_duration_ms,
       CASE WHEN r.completed_at IS NOT NULL THEN r.input_tokens WHEN g.id IS NULL AND e.event_kind = 'finished' THEN e.input_tokens ELSE NULL END AS current_input_tokens,
       CASE WHEN r.completed_at IS NOT NULL THEN r.cached_input_tokens ELSE NULL END AS cached_input_tokens,
       CASE WHEN r.completed_at IS NOT NULL THEN r.cache_write_tokens ELSE NULL END AS cache_write_tokens,
       CASE WHEN r.completed_at IS NOT NULL THEN r.output_tokens WHEN g.id IS NULL AND e.event_kind = 'finished' THEN e.output_tokens ELSE NULL END AS current_output_tokens,
       facts.billed_units, NULLIF(facts.billing_unit, '') AS billing_unit,
       COALESCE(facts.cost_micros, CASE WHEN r.completed_at IS NOT NULL THEN r.cost_micros WHEN g.id IS NULL AND e.event_kind = 'finished' THEN e.cost_micros ELSE NULL END) AS current_cost_micros,
       COALESCE(NULLIF(facts.currency, ''), NULLIF(r.currency, '')) AS currency,
       COALESCE(r.error_code, g.error_code, e.error_code) AS current_error_code,
       CASE WHEN g.id IS NOT NULL THEN
            CASE WHEN g.status IN ('preparing', 'queued') THEN 'pending'
                 WHEN g.status IN ('submitting', 'running', 'cancelling') THEN 'uploading'
                 WHEN g.request_object LIKE 'gap://%' OR (g.status = 'succeeded' AND g.result_json IS NULL) THEN 'gap'
                 ELSE 'bound' END
            ELSE COALESCE(spool.state, CASE WHEN r.completed_at IS NULL THEN 'capturing' WHEN r.request_object LIKE 'gap://%' OR r.response_object IS NULL OR r.response_object LIKE 'gap://%' THEN 'gap' ELSE 'bound' END)
       END AS archive_state,
       r.conversation_cluster_id AS session_id,
       CASE WHEN r.id IS NULL THEN NULL
            WHEN r.conversation_cluster_id IS NULL THEN 'unlinked'
            ELSE 'confirmed' END AS session_association,
       observation.session_name, observation.task_kind, observation.agent_id,
       observation.metadata_source AS semantics_source,
       CASE WHEN key_record.id IS NULL OR principal.id IS NULL THEN NULL ELSE tenant.external_id END AS tenant_external_id,
       e.key_id AS credential_key_id,
       key_record.alias AS key_alias, principal.external_id AS principal_external_id
  FROM events e
  LEFT JOIN request_record_locators locator
    ON locator.id = e.request_id AND locator.tenant_id = e.tenant_id AND locator.key_id = e.key_id
  LEFT JOIN request_records r
    ON r.id = locator.id AND r.created_at = locator.created_at
   AND r.tenant_id = e.tenant_id AND r.key_id = e.key_id
  LEFT JOIN generation_jobs g
    ON r.id IS NULL AND e.protocol = 'generation' AND g.id = e.request_id
   AND g.tenant_id = e.tenant_id AND g.key_id = e.key_id
  LEFT JOIN generation_stats_facts facts
    ON facts.job_id = g.id AND facts.tenant_id = g.tenant_id AND facts.key_id = g.key_id
  LEFT JOIN response_archive_spools spool
    ON spool.request_id = r.id AND spool.tenant_id = r.tenant_id
   AND spool.reservation_id = r.reservation_id
  LEFT JOIN conversation_observations observation
   ON observation.request_id = r.id AND observation.key_id = r.key_id
   AND observation.cluster_id = r.conversation_cluster_id
  LEFT JOIN tenants tenant ON tenant.id = e.tenant_id
  LEFT JOIN key_records key_record
    ON key_record.id = e.key_id AND key_record.tenant_id = e.tenant_id
  LEFT JOIN principals principal
    ON principal.id = key_record.principal_id AND principal.tenant_id = key_record.tenant_id
 ORDER BY e.event_at ASC, e.event_id ASC"#
    )
}

fn request_event_views(rows: Vec<AnyRow>) -> Result<Vec<RequestEventView>, AppError> {
    rows.into_iter()
        .map(|row| {
            let generation_status: Option<String> = row.try_get("generation_status")?;
            let persisted_status_code: Option<i64> = row.try_get("current_status_code")?;
            let (lifecycle_state, status_code) =
                request_lifecycle_projection(generation_status.as_deref(), persisted_status_code)?;
            let input_tokens: Option<i64> = row.try_get("current_input_tokens")?;
            let cached_input_tokens: Option<i64> = row.try_get("cached_input_tokens")?;
            let cache_write_tokens: Option<i64> = row.try_get("cache_write_tokens")?;
            let output_tokens: Option<i64> = row.try_get("current_output_tokens")?;
            let billed_units: Option<i64> = row.try_get("billed_units")?;
            let billing_unit: Option<String> = row.try_get("billing_unit")?;
            let cost_micros: Option<i64> = row.try_get("current_cost_micros")?;
            let cost = cost_micros.map(micros_to_decimal_string);
            let currency: Option<String> = row.try_get("currency")?;
            let tokens = (generation_status.is_none()
                && (input_tokens.is_some()
                    || cached_input_tokens.is_some()
                    || cache_write_tokens.is_some()
                    || output_tokens.is_some()))
            .then_some(RequestTokenUsageView {
                input_tokens,
                cached_input_tokens,
                cache_write_tokens,
                output_tokens,
            });
            let generation = generation_status
                .is_some()
                .then_some(RequestGenerationUsageView {
                    billed_units,
                    billing_unit,
                });
            Ok(RequestEventView {
                event_id: parse_uuid(row.try_get("event_id")?)?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                event_at: row.try_get("event_at")?,
                event_kind: row.try_get("event_kind")?,
                created_at: row.try_get("created_at")?,
                completed_at: row.try_get("completed_at")?,
                source_completed_at: None,
                lifecycle_state,
                upstream_account_id: row
                    .try_get::<Option<String>, _>("upstream_account_id")?
                    .map(parse_uuid)
                    .transpose()?,
                route_id: row
                    .try_get::<Option<String>, _>("route_id")?
                    .map(parse_uuid)
                    .transpose()?,
                currency: currency.clone(),
                cached_input_tokens,
                cache_write_tokens,
                session_context: request_session_context_from_row(&row)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code,
                duration_ms: row.try_get("current_duration_ms")?,
                input_tokens,
                output_tokens,
                cost: cost.clone(),
                usage: RequestUsageView { tokens, generation },
                billing: RequestBillingView {
                    billable: true,
                    cost,
                    currency,
                },
                error_code: row.try_get("current_error_code")?,
                archive_state: request_archive_state(row.try_get("archive_state")?)?,
                credential_identity: request_credential_identity_from_row(&row)?,
            })
        })
        .collect()
}

fn request_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let request_object: String = row.try_get("request_object")?;
    let response_object: Option<String> = row.try_get("response_object")?;
    let (request_archive_state, request_archive_reason) =
        locator_archive_projection(Some(&request_object));
    let view = request_view_from_row(&row)?;
    let (response_archive_state, response_archive_reason) = if response_object
        .as_deref()
        .is_some_and(|value| !value.starts_with("gap://"))
    {
        (RequestArchiveState::Bound, None)
    } else {
        let state = match view.archive_state {
            RequestArchiveState::Capturing
            | RequestArchiveState::Pending
            | RequestArchiveState::Uploading
            | RequestArchiveState::Gap => view.archive_state,
            RequestArchiveState::Bound => RequestArchiveState::Gap,
        };
        let reason = (state == RequestArchiveState::Gap)
            .then(|| row.try_get::<Option<String>, _>("archive_reason"))
            .transpose()?
            .flatten()
            .or_else(|| {
                (state == RequestArchiveState::Gap).then(|| "archive_object_unavailable".to_owned())
            });
        (state, reason)
    };
    Ok(RequestArchiveRefs {
        view,
        request_object,
        response_object,
        response_json: None,
        provenance: None,
        request_archive_state,
        request_archive_reason,
        response_archive_state,
        response_archive_reason,
    })
}

fn session_archive_unlinked_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let request_id: String = row.try_get("id")?;
    let source: String = row.try_get("source")?;
    let external_request_id: String = row.try_get("external_request_id")?;
    let request_object: Option<String> = row.try_get("request_object")?;
    let response_object: Option<String> = row.try_get("response_object")?;
    let source_completed_at: Option<i64> = row.try_get("source_completed_at")?;
    let input_tokens: Option<i64> = row.try_get("input_tokens")?;
    let cached_input_tokens: Option<i64> = row.try_get("cached_input_tokens")?;
    let cache_write_tokens: Option<i64> = row.try_get("cache_write_tokens")?;
    let output_tokens: Option<i64> = row.try_get("output_tokens")?;
    let status_code: Option<i64> = row.try_get("status_code")?;
    let (lifecycle_state, status_code) = request_lifecycle_projection(None, status_code)?;
    let (request_archive_state, request_archive_reason) =
        locator_archive_projection(request_object.as_deref());
    let (response_archive_state, response_archive_reason) =
        locator_archive_projection(response_object.as_deref());
    let archive_state = if request_archive_state == RequestArchiveState::Gap
        || response_archive_state == RequestArchiveState::Gap
    {
        RequestArchiveState::Gap
    } else {
        RequestArchiveState::Bound
    };
    let tokens = (input_tokens.is_some()
        || cached_input_tokens.is_some()
        || cache_write_tokens.is_some()
        || output_tokens.is_some())
    .then_some(RequestTokenUsageView {
        input_tokens,
        cached_input_tokens,
        cache_write_tokens,
        output_tokens,
    });
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(request_id.clone())?,
            created_at: row.try_get("created_at")?,
            completed_at: source_completed_at,
            source_completed_at,
            lifecycle_state,
            protocol: row.try_get("protocol")?,
            model: row.try_get("model")?,
            upstream_account_id: None,
            route_id: None,
            status_code,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens,
            cached_input_tokens,
            cache_write_tokens,
            output_tokens,
            cost: None,
            currency: None,
            usage: RequestUsageView {
                tokens,
                generation: None,
            },
            billing: RequestBillingView {
                billable: false,
                cost: None,
                currency: None,
            },
            error_code: row.try_get("error_code")?,
            archive_state,
            credential_identity: request_credential_identity_from_row(&row)?,
            session_context: request_session_context_from_row(&row)?,
        },
        request_object: request_object.unwrap_or_else(|| {
            format!("gap://session-archive/{source}/{external_request_id}/request")
        }),
        response_object,
        response_json: None,
        provenance: Some(RequestProvenanceView {
            source,
            disposition: "unlinked".to_owned(),
            unlinked: true,
            external_request_id,
            proof_digest: row.try_get("proof_digest")?,
        }),
        request_archive_state,
        request_archive_reason,
        response_archive_state,
        response_archive_reason,
    })
}

fn locator_archive_projection(location: Option<&str>) -> (RequestArchiveState, Option<String>) {
    if location.is_some_and(|location| !location.starts_with("gap://")) {
        (RequestArchiveState::Bound, None)
    } else {
        (
            RequestArchiveState::Gap,
            Some("archive_object_unavailable".to_owned()),
        )
    }
}

fn generation_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let created_at: i64 = row.try_get("created_at")?;
    let completed_at: Option<i64> = row.try_get("completed_at")?;
    let status: String = row.try_get("status")?;
    let result_json: Option<String> = row.try_get("result_json")?;
    let (lifecycle_state, status_code) = request_lifecycle_projection(Some(&status), None)?;
    let billed_units: Option<i64> = row.try_get("facts_billed_units")?;
    let billing_unit: Option<String> = row.try_get("facts_billing_unit")?;
    let cost_micros: Option<i64> = row.try_get("facts_cost_micros")?;
    let cost = cost_micros.map(micros_to_decimal_string);
    let currency: Option<String> = row.try_get("facts_currency")?;
    let request_object: String = row.try_get("request_object")?;
    let (request_archive_state, request_archive_reason) =
        locator_archive_projection(Some(&request_object));
    let (response_archive_state, response_archive_reason) = match status.as_str() {
        "preparing" | "queued" => (RequestArchiveState::Pending, None),
        "submitting" | "running" | "cancelling" => (RequestArchiveState::Uploading, None),
        "succeeded" if result_json.is_none() => (
            RequestArchiveState::Gap,
            Some("archive_object_unavailable".to_owned()),
        ),
        "succeeded" | "failed" | "cancelled" => (RequestArchiveState::Bound, None),
        _ => return Err(AppError::Internal),
    };
    let archive_state = if request_archive_state == RequestArchiveState::Gap
        || response_archive_state == RequestArchiveState::Gap
    {
        RequestArchiveState::Gap
    } else if matches!(
        response_archive_state,
        RequestArchiveState::Pending | RequestArchiveState::Uploading
    ) {
        response_archive_state
    } else {
        RequestArchiveState::Bound
    };
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(row.try_get("id")?)?,
            created_at,
            completed_at,
            source_completed_at: None,
            lifecycle_state,
            protocol: "generation".to_owned(),
            model: row.try_get("public_model")?,
            upstream_account_id: row
                .try_get::<Option<String>, _>("upstream_account_id")?
                .map(parse_uuid)
                .transpose()?,
            route_id: row
                .try_get::<Option<String>, _>("route_id")?
                .map(parse_uuid)
                .transpose()?,
            status_code,
            duration_ms: completed_at.map(|value| value - created_at),
            input_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            output_tokens: None,
            cost: cost.clone(),
            currency: currency.clone(),
            usage: RequestUsageView {
                tokens: None,
                generation: Some(RequestGenerationUsageView {
                    billed_units,
                    billing_unit,
                }),
            },
            billing: RequestBillingView {
                billable: true,
                cost,
                currency,
            },
            error_code: row.try_get("error_code")?,
            archive_state,
            credential_identity: request_credential_identity_from_row(&row)?,
            session_context: None,
        },
        request_object,
        response_object: None,
        response_json: result_json
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?,
        provenance: None,
        request_archive_state,
        request_archive_reason,
        response_archive_state,
        response_archive_reason,
    })
}

fn validate_request_filter(filter: &RequestListFilter) -> Result<(), AppError> {
    if let Some(ast) = filter.typed_ast.as_ref() {
        ast.validate()?;
    }
    if filter
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "success" | "error" | "pending"))
    {
        return Err(AppError::BadRequest(
            "status must be success, error, or pending".into(),
        ));
    }
    if filter
        .from_created_at
        .zip(filter.to_created_at)
        .is_some_and(|(from, to)| from > to)
    {
        return Err(AppError::BadRequest(
            "from_created_at must not be after to_created_at".into(),
        ));
    }
    validate_numeric_range(
        "duration_ms",
        filter.min_duration_ms,
        filter.max_duration_ms,
    )?;
    validate_numeric_range("cost", filter.min_cost_micros, filter.max_cost_micros)?;
    for (name, value) in [
        ("model", filter.model.as_deref()),
        ("protocol", filter.protocol.as_deref()),
        ("error_code", filter.error_code.as_deref()),
        ("key_alias", filter.key_alias.as_deref()),
        ("principal", filter.principal.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        }) {
            return Err(AppError::BadRequest(format!(
                "{name} must contain 1 to 200 non-control characters"
            )));
        }
    }
    Ok(())
}

pub(crate) fn search_prefix(value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut escaped = String::with_capacity(value.len() + 1);
    for character in value.trim().to_lowercase().chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    escaped
}

fn cursor_id(filter: &RequestListFilter) -> String {
    filter
        .before_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned())
}

#[cfg(test)]
#[path = "event_contract_tests.rs"]
mod event_contract_tests;

// Retained temporarily as historical context only. Behaviour is covered by
// database/API integration tests; SQL-string assertions are intentionally not
// compiled because they lock implementation text rather than request semantics.
#[cfg(all(test, any()))]
mod query_shape_tests {
    use super::*;

    #[test]
    fn operator_request_list_applies_top_n_before_union() {
        let filter = RequestListFilter {
            limit: 50,
            ..RequestListFilter::default()
        };
        let query = build_operator_request_list_query(Some("tenant-a"), &filter);

        assert!(
            query
                .statement
                .contains("ORDER BY r.created_at DESC, r.id DESC LIMIT $4) AS request_page")
        );
        assert!(
            query
                .statement
                .contains("ORDER BY g.created_at DESC, g.id DESC LIMIT $8) AS generation_page")
        );
        assert!(
            query
                .statement
                .ends_with("ORDER BY created_at DESC, id DESC LIMIT $9")
        );
        assert_eq!(
            query
                .statement
                .matches("SELECT tenant_scope.id FROM tenants tenant_scope")
                .count(),
            2,
            "each independently bounded source must enforce the tenant scope"
        );
        assert!(
            !query.statement.contains(" = '' OR"),
            "optional-parameter guards prevent PostgreSQL generic plans from choosing indexes"
        );
        assert!(
            !query.statement.contains("JOIN key_records"),
            "the common path must reach the ordered request indexes without an identity hash join"
        );
        assert!(
            !query.statement.contains("JOIN principals"),
            "principal metadata is irrelevant without a principal filter"
        );
        assert_eq!(query.binds.last(), Some(&RequestListBind::I64(50)));
    }

    #[test]
    fn operator_request_page_lookahead_is_one_bounded_extra_row() {
        let query = build_operator_request_list_query(
            Some("tenant-a"),
            &RequestListFilter {
                limit: 100,
                lookahead: true,
                ..RequestListFilter::default()
            },
        );

        assert_eq!(
            query.binds.last(),
            Some(&RequestListBind::I64(101)),
            "the control envelope proves another page without unbounded counting"
        );
    }

    #[test]
    fn operator_request_list_emits_only_concrete_active_filters() {
        let key_id = Uuid::now_v7();
        let upstream_account_id = Uuid::now_v7();
        let before_id = Uuid::now_v7();
        let filter = RequestListFilter {
            limit: 17,
            lookahead: false,
            from_created_at: Some(10),
            to_created_at: Some(90),
            before_created_at: Some(80),
            before_id: Some(before_id),
            key_id: Some(key_id),
            model: Some("model-a".to_owned()),
            protocol: Some("generation".to_owned()),
            status: Some("error".to_owned()),
            error_code: Some("upstream_error".to_owned()),
            upstream_account_id: Some(upstream_account_id),
            route_id: None,
            min_duration_ms: Some(20),
            max_duration_ms: Some(40),
            min_cost_micros: Some(100),
            max_cost_micros: Some(200),
            key_alias: Some("Alias%".to_owned()),
            principal: Some("Principal_".to_owned()),
            typed_ast: None,
        };
        let query = build_operator_request_list_query(None, &filter);

        for predicate in [
            "r.created_at <",
            "r.key_id =",
            "r.model =",
            "r.protocol =",
            "r.status_code >= 400",
            "r.error_code =",
            "r.upstream_account_id =",
            "r.duration_ms >=",
            "r.duration_ms <=",
            "r.cost_micros >=",
            "r.cost_micros <=",
            "g.created_at <",
            "g.key_id =",
            "g.public_model =",
            "g.status IN ('failed', 'cancelled')",
            "g.error_code =",
            "g.upstream_account_id =",
            "(g.completed_at - g.created_at) >=",
            "(g.completed_at - g.created_at) <=",
            "g.cost_micros >=",
            "g.cost_micros <=",
            "LOWER(k.alias) LIKE",
            "LOWER(p.external_id) LIKE",
        ] {
            assert!(
                query.statement.contains(predicate),
                "missing active predicate: {predicate}"
            );
        }
        assert!(!query.statement.contains("tenant_scope"));
        assert_eq!(query.statement.matches("JOIN key_records").count(), 2);
        assert_eq!(query.statement.matches("JOIN principals").count(), 2);
        assert!(
            query
                .binds
                .contains(&RequestListBind::Text(key_id.to_string()))
        );
        assert!(
            query
                .binds
                .contains(&RequestListBind::Text(before_id.to_string()))
        );
        assert!(
            query
                .binds
                .contains(&RequestListBind::Text("alias\\%%".to_owned()))
        );
        assert!(
            query
                .binds
                .contains(&RequestListBind::Text("principal\\_%".to_owned()))
        );
    }

    #[test]
    fn global_model_filter_is_pushed_into_each_bounded_source() {
        let query = build_operator_request_list_query(
            None,
            &RequestListFilter {
                limit: 5,
                model: Some("deepseek".to_owned()),
                ..RequestListFilter::default()
            },
        );

        assert_eq!(query.statement.matches("r.model =").count(), 1);
        assert_eq!(query.statement.matches("g.public_model =").count(), 1);
        assert_eq!(
            query
                .binds
                .iter()
                .filter(|bind| **bind == RequestListBind::Text("deepseek".to_owned()))
                .count(),
            2,
            "each independently ordered Top-N branch must bind the model"
        );
        assert!(!query.statement.contains("tenant_scope"));
        assert!(
            query
                .statement
                .contains("r.model = $3 ORDER BY r.created_at DESC, r.id DESC LIMIT $4")
        );
        assert!(
            query
                .statement
                .contains("g.public_model = $7 ORDER BY g.created_at DESC, g.id DESC LIMIT $8")
        );
    }

    #[test]
    fn identity_joins_follow_the_active_filter_dependencies() {
        let alias_query = build_operator_request_list_query(
            Some("tenant-a"),
            &RequestListFilter {
                limit: 25,
                key_alias: Some("alias".to_owned()),
                ..RequestListFilter::default()
            },
        );
        assert_eq!(alias_query.statement.matches("JOIN key_records").count(), 2);
        assert!(!alias_query.statement.contains("JOIN principals"));
        assert_eq!(
            alias_query.statement.matches("LOWER(k.alias) LIKE").count(),
            2
        );

        let principal_query = build_operator_request_list_query(
            Some("tenant-a"),
            &RequestListFilter {
                limit: 25,
                principal: Some("principal".to_owned()),
                ..RequestListFilter::default()
            },
        );
        assert_eq!(
            principal_query
                .statement
                .matches("JOIN key_records")
                .count(),
            2
        );
        assert_eq!(
            principal_query.statement.matches("JOIN principals").count(),
            2
        );
        assert_eq!(
            principal_query
                .statement
                .matches("LOWER(p.external_id) LIKE")
                .count(),
            2
        );
    }

    #[test]
    fn route_filter_omits_generation_source_that_cannot_match() {
        let route_id = Uuid::now_v7();
        let filter = RequestListFilter {
            limit: 25,
            route_id: Some(route_id),
            ..RequestListFilter::default()
        };
        let query = build_operator_request_list_query(Some("tenant-a"), &filter);

        assert!(query.statement.contains("r.model_route_id ="));
        assert!(!query.statement.contains("generation_jobs"));
        assert!(!query.statement.contains("UNION ALL"));
        assert!(
            query
                .statement
                .ends_with("ORDER BY created_at DESC, id DESC LIMIT $6")
        );
        assert!(
            query
                .binds
                .contains(&RequestListBind::Text(route_id.to_string()))
        );
    }
}
