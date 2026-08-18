use super::super::*;

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
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 AND r.created_at >= $2 AND r.created_at <= $3 AND (r.created_at < $4 OR (r.created_at = $4 AND r.id < $5)) AND ($6 = '' OR r.key_id = $6) AND ($7 = '' OR r.model = $7) AND ($8 = '' OR r.protocol = $8) AND ($9 = '' OR ($9 = 'success' AND r.status_code BETWEEN 200 AND 399) OR ($9 = 'error' AND r.status_code >= 400) OR ($9 = 'pending' AND r.status_code IS NULL)) AND ($10 = '' OR r.error_code = $10) AND ($11 = '' OR r.upstream_account_id = $11) AND ($12 = '' OR r.model_route_id = $12) AND ($13 < 0 OR r.duration_ms >= $13) AND ($14 < 0 OR r.duration_ms <= $14) AND ($15 < 0 OR r.cost_micros >= $15) AND ($16 < 0 OR r.cost_micros <= $16) AND ($17 = '' OR LOWER(k.alias) LIKE $17 ESCAPE '\\') AND ($18 = '' OR LOWER(p.external_id) LIKE $18 ESCAPE '\\') UNION ALL SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $1 AND g.created_at >= $2 AND g.created_at <= $3 AND (g.created_at < $4 OR (g.created_at = $4 AND g.id < $5)) AND ($6 = '' OR g.key_id = $6) AND ($7 = '' OR g.public_model = $7) AND ($8 = '' OR $8 = 'generation') AND ($9 = '' OR ($9 = 'success' AND g.status = 'succeeded') OR ($9 = 'error' AND g.status IN ('failed', 'cancelled')) OR ($9 = 'pending' AND g.status IN ('queued', 'running'))) AND ($10 = '' OR g.error_code = $10) AND ($11 = '' OR g.upstream_account_id = $11) AND $12 = '' AND ($13 < 0 OR (g.completed_at - g.created_at) >= $13) AND ($14 < 0 OR (g.completed_at - g.created_at) <= $14) AND ($15 < 0 OR g.cost_micros >= $15) AND ($16 < 0 OR g.cost_micros <= $16) AND ($17 = '' OR LOWER(k.alias) LIKE $17 ESCAPE '\\') AND ($18 = '' OR LOWER(p.external_id) LIKE $18 ESCAPE '\\')) AS all_requests ORDER BY created_at DESC, id DESC LIMIT $19",
        )
        .bind(tenant_external_id)
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
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
        .bind(search_prefix(filter.key_alias.as_deref()))
        .bind(search_prefix(filter.principal.as_deref()))
        .bind(filter.limit.clamp(1, 500))
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
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id WHERE r.created_at >= $1 AND r.created_at <= $2 AND (r.created_at < $3 OR (r.created_at = $3 AND r.id < $4)) AND ($5 = '' OR r.key_id = $5) AND ($6 = '' OR r.model = $6) AND ($7 = '' OR r.protocol = $7) AND ($8 = '' OR ($8 = 'success' AND r.status_code BETWEEN 200 AND 399) OR ($8 = 'error' AND r.status_code >= 400) OR ($8 = 'pending' AND r.status_code IS NULL)) AND ($9 = '' OR r.error_code = $9) AND ($10 = '' OR r.upstream_account_id = $10) AND ($11 = '' OR r.model_route_id = $11) AND ($12 < 0 OR r.duration_ms >= $12) AND ($13 < 0 OR r.duration_ms <= $13) AND ($14 < 0 OR r.cost_micros >= $14) AND ($15 < 0 OR r.cost_micros <= $15) AND ($16 = '' OR LOWER(k.alias) LIKE $16 ESCAPE '\\') AND ($17 = '' OR LOWER(p.external_id) LIKE $17 ESCAPE '\\') UNION ALL SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g JOIN key_records k ON k.id = g.key_id AND k.tenant_id = g.tenant_id JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id WHERE g.created_at >= $1 AND g.created_at <= $2 AND (g.created_at < $3 OR (g.created_at = $3 AND g.id < $4)) AND ($5 = '' OR g.key_id = $5) AND ($6 = '' OR g.public_model = $6) AND ($7 = '' OR $7 = 'generation') AND ($8 = '' OR ($8 = 'success' AND g.status = 'succeeded') OR ($8 = 'error' AND g.status IN ('failed', 'cancelled')) OR ($8 = 'pending' AND g.status IN ('queued', 'running'))) AND ($9 = '' OR g.error_code = $9) AND ($10 = '' OR g.upstream_account_id = $10) AND $11 = '' AND ($12 < 0 OR (g.completed_at - g.created_at) >= $12) AND ($13 < 0 OR (g.completed_at - g.created_at) <= $13) AND ($14 < 0 OR g.cost_micros >= $14) AND ($15 < 0 OR g.cost_micros <= $15) AND ($16 = '' OR LOWER(k.alias) LIKE $16 ESCAPE '\\') AND ($17 = '' OR LOWER(p.external_id) LIKE $17 ESCAPE '\\')) AS all_requests ORDER BY created_at DESC, id DESC LIMIT $18",
        )
        .bind(filter.from_created_at.unwrap_or(0))
        .bind(filter.to_created_at.unwrap_or(i64::MAX))
        .bind(filter.before_created_at.unwrap_or(i64::MAX))
        .bind(cursor_id(&filter))
        .bind(filter.key_id.map(|id| id.to_string()).unwrap_or_default())
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
        .bind(search_prefix(filter.key_alias.as_deref()))
        .bind(search_prefix(filter.principal.as_deref()))
        .bind(filter.limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        request_views(rows)
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
