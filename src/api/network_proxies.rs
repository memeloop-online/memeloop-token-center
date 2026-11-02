use super::*;
use crate::db::network_proxies::{CreateProxy, DeleteProxy, UpdateProxy};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProxyTenantQuery {
    tenant_external_id: String,
}

async fn authority(
    headers: &HeaderMap,
    state: &AppState,
    tenant: &str,
) -> Result<Option<Uuid>, AppError> {
    let service = require_service(headers, state, "providers:write").await?;
    require_global_service(&service)?;
    require_service_tenant(&service, tenant)?;
    Ok(service.service_id)
}

fn response(result: Result<(StatusCode, Value), AppError>) -> Response {
    let mut response = match result {
        Ok((StatusCode::NO_CONTENT, _)) => StatusCode::NO_CONTENT.into_response(),
        Ok((status, body)) => (status, Json(body)).into_response(),
        Err(AppError::Storage { .. }) => AppError::Overloaded.into_response(),
        Err(error) => error.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

pub(super) async fn list_network_proxies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProxyTenantQuery>,
) -> Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .list_network_proxies(
                        &query.tenant_external_id,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn get_network_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<ProxyTenantQuery>,
) -> Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .get_network_proxy(
                        id,
                        &query.tenant_external_id,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn create_network_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateProxy>,
) -> Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            Ok((
                StatusCode::CREATED,
                state
                    .db
                    .create_network_proxy(body, actor, state.config.key_pepper.as_bytes())
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn update_network_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateProxy>,
) -> Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .update_network_proxy(id, body, actor, state.config.key_pepper.as_bytes())
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn delete_network_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<DeleteProxy>,
) -> Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            state.db.delete_network_proxy(id, body, actor).await?;
            Ok((StatusCode::NO_CONTENT, Value::Null))
        }
        .await,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProxyObservationQuery {
    tenant_external_id: String,
    start: i64,
    end: i64,
}

pub(super) async fn network_proxy_observations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<ProxyObservationQuery>,
) -> Response {
    response(
        async {
            let service = require_service(&headers, &state, "requests:read").await?;
            require_service_tenant(&service, &query.tenant_external_id)?;
            Ok((
                StatusCode::OK,
                state
                    .db
                    .network_proxy_observations(
                        id,
                        &query.tenant_external_id,
                        query.start,
                        query.end,
                        state.config.key_pepper.as_bytes(),
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn probe_network_proxy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<DeleteProxy>,
) -> Response {
    response(
        async {
            authority(&headers, &state, &body.tenant_external_id).await?;
            Ok((
                StatusCode::ACCEPTED,
                state
                    .db
                    .request_network_proxy_probe(
                        id,
                        &body.tenant_external_id,
                        body.expected_version,
                    )
                    .await?,
            ))
        }
        .await,
    )
}

pub(super) async fn network_proxy_strategies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ProxyTenantQuery>,
) -> Response {
    response(async {
        authority(&headers, &state, &query.tenant_external_id).await?;
        let state = state.pin_application_plugins().await?;
        let mut items = vec![json!({"id":"manual","version":"1","schema":{"type":"object","additionalProperties":false},"default":{}}),
            json!({"id":"auto","version":"1","default":crate::db::transport_proxy_groups::strategy::AutoConfig::default(),
                "schema":{"type":"object","additionalProperties":false,"properties":{
                    "minimum_samples":{"type":"integer","minimum":1,"maximum":10000},
                    "minimum_dwell_ms":{"type":"integer","minimum":0,"maximum":3600000},
                    "health_max_age_ms":{"type":"integer","minimum":10000,"maximum":3600000},
                    "recovery_successes":{"type":"integer","minimum":1,"maximum":8},
                    "improvement_threshold":{"type":"number","minimum":0,"maximum":1},
                    "signal_window_ms":{"type":"integer","minimum":60000,"maximum":86400000}}}})];
        items.extend(state.plugins.network_proxy_strategies());
        Ok((StatusCode::OK, json!({"items": items})))
    }.await)
}

pub(super) async fn get_network_proxy_selection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account): Path<Uuid>,
    Query(query): Query<ProxyTenantQuery>,
) -> Response {
    response(
        async {
            authority(&headers, &state, &query.tenant_external_id).await?;
            let mut body = state
                .db
                .get_network_proxy_selection(account, &query.tenant_external_id)
                .await?;
            state
                .transport_proxy_groups
                .describe_network_proxy_selection(account, &mut body);
            Ok((StatusCode::OK, body))
        }
        .await,
    )
}

pub(super) async fn update_network_proxy_selection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account): Path<Uuid>,
    Json(body): Json<crate::db::transport_proxy_management::UpdateProxySelection>,
) -> Response {
    response(
        async {
            let actor = authority(&headers, &state, &body.tenant_external_id).await?;
            let state = state.pin_application_plugins().await?;
            transport_proxy_groups::validate_binding_provider(&state, account).await?;
            body.validate(&state.plugins)?;
            let mut body = state
                .db
                .update_network_proxy_selection(
                    account,
                    body,
                    actor,
                    state.config.key_pepper.as_bytes(),
                )
                .await?;
            state
                .transport_proxy_groups
                .describe_network_proxy_selection(account, &mut body);
            Ok((StatusCode::ACCEPTED, body))
        }
        .await,
    )
}
