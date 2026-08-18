use super::super::*;

#[cfg(test)]
mod tests;

#[derive(Debug, Deserialize)]
pub(in crate::api) struct RequestsQuery {
    #[serde(default = "default_limit")]
    pub(in crate::api) limit: i64,
    pub(in crate::api) tenant_external_id: Option<String>,
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
    before_created_at: Option<i64>,
    before_id: Option<Uuid>,
    key_id: Option<Uuid>,
    model: Option<String>,
    protocol: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    upstream_account_id: Option<Uuid>,
    route_id: Option<Uuid>,
    min_duration_ms: Option<i64>,
    max_duration_ms: Option<i64>,
    min_cost: Option<String>,
    max_cost: Option<String>,
    key_alias: Option<String>,
    principal: Option<String>,
}

impl RequestsQuery {
    pub(in crate::api) fn to_filter(&self, operator: bool) -> Result<RequestListFilter, AppError> {
        Ok(RequestListFilter {
            limit: self.limit,
            from_created_at: self.from_created_at,
            to_created_at: self.to_created_at,
            before_created_at: self.before_created_at,
            before_id: self.before_id,
            key_id: self.key_id,
            model: self.model.clone(),
            protocol: self.protocol.clone(),
            status: self.status.clone(),
            error_code: self.error_code.clone(),
            upstream_account_id: self.upstream_account_id,
            route_id: self.route_id,
            min_duration_ms: self.min_duration_ms,
            max_duration_ms: self.max_duration_ms,
            min_cost_micros: self
                .min_cost
                .as_deref()
                .map(|value| parse_money_micros(value, "min_cost"))
                .transpose()?,
            max_cost_micros: self
                .max_cost
                .as_deref()
                .map(|value| parse_money_micros(value, "max_cost"))
                .transpose()?,
            key_alias: operator.then(|| self.key_alias.clone()).flatten(),
            principal: operator.then(|| self.principal.clone()).flatten(),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
pub(in crate::api) struct StatsQuery {
    pub(in crate::api) tenant_external_id: Option<String>,
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
    key_id: Option<Uuid>,
    model: Option<String>,
    protocol: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    upstream_account_id: Option<Uuid>,
    route_id: Option<Uuid>,
    min_duration_ms: Option<i64>,
    max_duration_ms: Option<i64>,
    min_cost: Option<String>,
    max_cost: Option<String>,
    key_alias: Option<String>,
    principal: Option<String>,
}

impl StatsQuery {
    pub(in crate::api) fn to_filter(
        &self,
        operator: bool,
        authenticated_key: Option<Uuid>,
    ) -> Result<StatsFilter, AppError> {
        let to_created_at = self.to_created_at.unwrap_or_else(unix_millis);
        let from_created_at = self
            .from_created_at
            .unwrap_or_else(|| to_created_at.saturating_sub(30 * 86_400_000));
        Ok(StatsFilter {
            from_created_at: Some(from_created_at),
            to_created_at: Some(to_created_at),
            key_id: authenticated_key.or(operator.then_some(self.key_id).flatten()),
            model: self.model.clone(),
            protocol: self.protocol.clone(),
            status: self.status.clone(),
            error_code: self.error_code.clone(),
            upstream_account_id: self.upstream_account_id,
            route_id: self.route_id,
            min_duration_ms: self.min_duration_ms,
            max_duration_ms: self.max_duration_ms,
            min_cost_micros: self
                .min_cost
                .as_deref()
                .map(|value| parse_money_micros(value, "min_cost"))
                .transpose()?,
            max_cost_micros: self
                .max_cost
                .as_deref()
                .map(|value| parse_money_micros(value, "max_cost"))
                .transpose()?,
            key_alias: operator.then(|| self.key_alias.clone()).flatten(),
            principal: operator.then(|| self.principal.clone()).flatten(),
        })
    }
}

pub(in crate::api) fn default_limit() -> i64 {
    50
}

pub(in crate::api) async fn self_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RequestsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .list_requests_filtered(key.key_id, query.to_filter(false)?)
            .await?,
    ))
}

pub(in crate::api) async fn self_request_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(request_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let refs = state
        .db
        .request_archive_refs(key.key_id, request_id)
        .await?;
    request_detail_response(&state, refs).await
}

pub(in crate::api) async fn request_detail_response(
    state: &AppState,
    refs: crate::model::RequestArchiveRefs,
) -> Result<Response, AppError> {
    let mut detail = request_detail(state, refs).await;
    let mut body = serde_json::to_vec(&detail).map_err(|_| AppError::Internal)?;
    if body.len() > MAX_ARCHIVE_DETAIL_RESPONSE {
        detail.request_body = Value::Null;
        detail.response_body = Value::Null;
        detail.archive_complete = false;
        body = serde_json::to_vec(&detail).map_err(|_| AppError::Internal)?;
    }
    let body_len = body.len();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body_len)
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from(body))
        .map_err(|_| AppError::Internal)
}

pub(in crate::api) async fn self_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .stats_filtered(key.key_id, query.to_filter(false, Some(key.key_id))?)
            .await?,
    ))
}
