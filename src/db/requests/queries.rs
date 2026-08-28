use super::super::*;

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestRecordLocator {
    created_at: i64,
    tenant_id: String,
    key_id: String,
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
        let rows = sqlx::query(
            "SELECT e.event_id, e.request_id, e.event_at, e.event_kind, e.key_id, e.protocol, e.model, e.status_code, e.duration_ms, e.input_tokens, e.output_tokens, e.cost_micros, e.error_code FROM request_events e JOIN tenants t ON t.id = e.tenant_id WHERE t.external_id = $1 AND (e.event_at > $2 OR (e.event_at = $3 AND e.event_id > $4)) ORDER BY e.event_at ASC, e.event_id ASC LIMIT $5",
        )
        .bind(tenant_external_id)
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestEventView {
                    event_id: parse_uuid(row.try_get("event_id")?)?,
                    request_id: parse_uuid(row.try_get("request_id")?)?,
                    event_at: row.try_get("event_at")?,
                    event_kind: row.try_get("event_kind")?,
                    key_id: parse_uuid(row.try_get("key_id")?)?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
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
        let rows = sqlx::query(
            "SELECT event_id, request_id, event_at, event_kind, key_id, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_events WHERE (event_at > $1 OR (event_at = $2 AND event_id > $3)) ORDER BY event_at ASC, event_id ASC LIMIT $4",
        )
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
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = $1 AND created_at >= $2 AND created_at <= $3 AND (created_at < $4 OR (created_at = $4 AND id < $5)) AND ($6 = '' OR model = $6) AND ($7 = '' OR protocol = $7) AND ($8 = '' OR ($8 = 'success' AND status_code BETWEEN 200 AND 399) OR ($8 = 'error' AND status_code >= 400) OR ($8 = 'pending' AND status_code IS NULL)) AND ($9 = '' OR error_code = $9) AND ($10 = '' OR upstream_account_id = $10) AND ($11 = '' OR model_route_id = $11) AND ($12 < 0 OR duration_ms >= $12) AND ($13 < 0 OR duration_ms <= $13) AND ($14 < 0 OR cost_micros >= $14) AND ($15 < 0 OR cost_micros <= $15) ORDER BY created_at DESC, id DESC LIMIT $16",
        )
        .bind(key_id.to_string())
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.model.as_deref().unwrap_or_default())
        .bind(filter.protocol.as_deref().unwrap_or_default())
        .bind(filter.status.as_deref().unwrap_or_default())
        .bind(filter.error_code.as_deref().unwrap_or_default())
        .bind(
            filter
                .upstream_account_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .bind(filter.route_id.map(|id| id.to_string()).unwrap_or_default())
        .bind(filter.min_duration_ms.unwrap_or(-1))
        .bind(filter.max_duration_ms.unwrap_or(-1))
        .bind(filter.min_cost_micros.unwrap_or(-1))
        .bind(filter.max_cost_micros.unwrap_or(-1))
        .bind(filter.limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
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
        let query = build_operator_request_list_query(Some(tenant_external_id), &filter);
        let rows = self.fetch_operator_request_list(query).await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
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
        let query = build_operator_request_list_query(None, &filter);
        let rows = self.fetch_operator_request_list(query).await?;
        request_views(rows)
    }

    async fn fetch_operator_request_list(
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
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND key_id = $3",
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
                "SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code, r.request_object, r.response_object FROM request_records r JOIN tenants t ON t.id = $3 WHERE r.id = $1 AND r.created_at = $2 AND r.tenant_id = $3 AND t.external_id = $4",
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
            "SELECT g.id, g.created_at, g.completed_at, g.public_model, g.status, g.cost_micros, g.error_code, g.request_object, g.result_json FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id WHERE g.id = $1 AND t.external_id = $2",
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
                "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2",
            )
            .bind(&request_id)
            .bind(locator.created_at)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::Internal)?;
            return request_archive_refs_from_row(row);
        }
        let row = sqlx::query(
            "SELECT id, created_at, completed_at, public_model, status, cost_micros, error_code, request_object, result_json FROM generation_jobs WHERE id = $1",
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
            "SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.protocol, u.model, u.status_code, u.duration_ms, u.input_tokens, u.output_tokens, u.error_code, u.request_object, u.response_object, u.source, u.external_request_id, c.proof_digest FROM session_archive_unlinked_requests u JOIN session_archive_correlations c ON c.tenant_id = u.tenant_id AND c.source = u.source AND c.external_request_id = u.external_request_id AND c.disposition = 'unlinked' WHERE u.archive_request_id = $1 AND u.key_id = $2",
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
            "SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.protocol, u.model, u.status_code, u.duration_ms, u.input_tokens, u.output_tokens, u.error_code, u.request_object, u.response_object, u.source, u.external_request_id, c.proof_digest FROM session_archive_unlinked_requests u JOIN tenants t ON t.id = u.tenant_id JOIN session_archive_correlations c ON c.tenant_id = u.tenant_id AND c.source = u.source AND c.external_request_id = u.external_request_id AND c.disposition = 'unlinked' WHERE u.archive_request_id = $1 AND t.external_id = $2",
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
            "SELECT u.archive_request_id AS id, u.source_started_at AS created_at, u.protocol, u.model, u.status_code, u.duration_ms, u.input_tokens, u.output_tokens, u.error_code, u.request_object, u.response_object, u.source, u.external_request_id, c.proof_digest FROM session_archive_unlinked_requests u JOIN session_archive_correlations c ON c.tenant_id = u.tenant_id AND c.source = u.source AND c.external_request_id = u.external_request_id AND c.disposition = 'unlinked' WHERE u.archive_request_id = $1",
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
            "SELECT id, created_at, completed_at, public_model, status, cost_micros, error_code, request_object, result_json FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_archive_refs_from_row(row)
    }
}

fn build_operator_request_list_query(
    tenant_external_id: Option<&str>,
    filter: &RequestListFilter,
) -> PortableRequestListQuery {
    let page_limit = filter.limit.clamp(1, 500);
    let mut query = PortableRequestListQuery::new(
        "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT * FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r",
    );
    push_operator_identity_joins(&mut query, "r", filter);
    query.push(" WHERE 1 = 1");
    push_request_record_filters(&mut query, tenant_external_id, filter);
    query.push(" ORDER BY r.created_at DESC, r.id DESC LIMIT ");
    query.bind_i64(page_limit);
    query.push(") AS request_page");

    if generation_branch_can_match(filter) {
        query.push(" UNION ALL SELECT * FROM (SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g");
        push_operator_identity_joins(&mut query, "g", filter);
        query.push(" WHERE 1 = 1");
        push_generation_job_filters(&mut query, tenant_external_id, filter);
        query.push(" ORDER BY g.created_at DESC, g.id DESC LIMIT ");
        query.bind_i64(page_limit);
        query.push(") AS generation_page");
    }

    query.push(") AS all_requests ORDER BY created_at DESC, id DESC LIMIT ");
    query.bind_i64(page_limit);
    query
}

fn push_operator_identity_joins(
    query: &mut PortableRequestListQuery,
    source_alias: &str,
    filter: &RequestListFilter,
) {
    // Tenant isolation is enforced directly on the request/generation source below. These
    // relations only provide searchable identity metadata; joining them on the default path
    // prevents PostgreSQL from stopping after the first page in the ordered source index.
    if filter.key_alias.is_some() || filter.principal.is_some() {
        query.push(" JOIN key_records k ON k.id = ");
        query.push(source_alias);
        query.push(".key_id AND k.tenant_id = ");
        query.push(source_alias);
        query.push(".tenant_id");
    }
    if filter.principal.is_some() {
        query.push(" JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id");
    }
}

fn push_request_record_filters(
    query: &mut PortableRequestListQuery,
    tenant_external_id: Option<&str>,
    filter: &RequestListFilter,
) {
    if let Some(tenant_external_id) = tenant_external_id {
        query.push(
            " AND r.tenant_id = (SELECT tenant_scope.id FROM tenants tenant_scope WHERE tenant_scope.external_id = ",
        );
        query.bind_text(tenant_external_id);
        query.push(")");
    }
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
}

fn push_generation_job_filters(
    query: &mut PortableRequestListQuery,
    tenant_external_id: Option<&str>,
    filter: &RequestListFilter,
) {
    if let Some(tenant_external_id) = tenant_external_id {
        query.push(
            " AND g.tenant_id = (SELECT tenant_scope.id FROM tenants tenant_scope WHERE tenant_scope.external_id = ",
        );
        query.bind_text(tenant_external_id);
        query.push(")");
    }
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
            "pending" => query.push(" AND g.status IN ('queued', 'running')"),
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

fn generation_branch_can_match(filter: &RequestListFilter) -> bool {
    filter.route_id.is_none()
        && filter
            .protocol
            .as_deref()
            .is_none_or(|protocol| protocol == "generation")
}

fn request_views(rows: Vec<AnyRow>) -> Result<Vec<RequestView>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(RequestView {
                request_id: parse_uuid(row.try_get("id")?)?,
                created_at: row.try_get("created_at")?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code: row.try_get("status_code")?,
                duration_ms: row.try_get("duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                error_code: row.try_get("error_code")?,
            })
        })
        .collect()
}

fn request_event_views(rows: Vec<AnyRow>) -> Result<Vec<RequestEventView>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(RequestEventView {
                event_id: parse_uuid(row.try_get("event_id")?)?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                event_at: row.try_get("event_at")?,
                event_kind: row.try_get("event_kind")?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code: row.try_get("status_code")?,
                duration_ms: row.try_get("duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                error_code: row.try_get("error_code")?,
            })
        })
        .collect()
}

fn request_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(row.try_get("id")?)?,
            created_at: row.try_get("created_at")?,
            protocol: row.try_get("protocol")?,
            model: row.try_get("model")?,
            status_code: row.try_get("status_code")?,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            cost: micros_to_decimal_string(row.try_get("cost_micros")?),
            error_code: row.try_get("error_code")?,
        },
        request_object: row.try_get("request_object")?,
        response_object: row.try_get("response_object")?,
        response_json: None,
        provenance: None,
    })
}

fn session_archive_unlinked_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let request_id: String = row.try_get("id")?;
    let source: String = row.try_get("source")?;
    let external_request_id: String = row.try_get("external_request_id")?;
    let request_object: Option<String> = row.try_get("request_object")?;
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(request_id.clone())?,
            created_at: row.try_get("created_at")?,
            protocol: row.try_get("protocol")?,
            model: row.try_get("model")?,
            status_code: row.try_get("status_code")?,
            duration_ms: row.try_get("duration_ms")?,
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            cost: "0".to_owned(),
            error_code: row.try_get("error_code")?,
        },
        request_object: request_object.unwrap_or_else(|| {
            format!("gap://session-archive/{source}/{external_request_id}/request")
        }),
        response_object: row.try_get("response_object")?,
        response_json: None,
        provenance: Some(RequestProvenanceView {
            source,
            disposition: "unlinked".to_owned(),
            unlinked: true,
            external_request_id,
            proof_digest: row.try_get("proof_digest")?,
        }),
    })
}

fn generation_archive_refs_from_row(row: AnyRow) -> Result<RequestArchiveRefs, AppError> {
    let created_at: i64 = row.try_get("created_at")?;
    let completed_at: Option<i64> = row.try_get("completed_at")?;
    let status: String = row.try_get("status")?;
    let result_json: Option<String> = row.try_get("result_json")?;
    Ok(RequestArchiveRefs {
        view: RequestView {
            request_id: parse_uuid(row.try_get("id")?)?,
            created_at,
            protocol: "generation".to_owned(),
            model: row.try_get("public_model")?,
            status_code: match status.as_str() {
                "succeeded" => Some(200),
                "failed" | "cancelled" => Some(502),
                _ => None,
            },
            duration_ms: completed_at.map(|value| value - created_at),
            input_tokens: 0,
            output_tokens: 0,
            cost: micros_to_decimal_string(row.try_get("cost_micros")?),
            error_code: row.try_get("error_code")?,
        },
        request_object: row.try_get("request_object")?,
        response_object: None,
        response_json: result_json
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?,
        provenance: None,
    })
}

fn validate_request_filter(filter: &RequestListFilter) -> Result<(), AppError> {
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
    fn operator_request_list_emits_only_concrete_active_filters() {
        let key_id = Uuid::now_v7();
        let upstream_account_id = Uuid::now_v7();
        let before_id = Uuid::now_v7();
        let filter = RequestListFilter {
            limit: 17,
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
