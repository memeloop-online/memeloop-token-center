//! Generic operator inference using an explicitly authorized billing credential.
//! Service identities have no end-user principal. The caller therefore chooses
//! an existing key they may already copy, rather than minting a new identity.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InferenceRequest {
    key_id: Uuid,
    request: Value,
}

pub(super) async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ManagementTenantQuery>,
    Json(body): Json<InferenceRequest>,
) -> Result<Response, AppError> {
    execute(state, headers, query, body, Protocol::OpenAiChat).await
}

pub(super) async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ManagementTenantQuery>,
    Json(body): Json<InferenceRequest>,
) -> Result<Response, AppError> {
    execute(state, headers, query, body, Protocol::AnthropicMessages).await
}

async fn execute(
    state: AppState,
    headers: HeaderMap,
    query: ManagementTenantQuery,
    body: InferenceRequest,
    protocol: Protocol,
) -> Result<Response, AppError> {
    let service = require_service(&headers, &state, "keys:write").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    validate_request(&body.request)?;
    // The database atomically fences tenant, active status and credential
    // generation using the same authorization as the explicit copy operation.
    let copied = state
        .db
        .copy_key_credential(
            body.key_id,
            state.config.key_pepper.as_bytes(),
            tenant.as_deref(),
            service.allows("keys:write"),
        )
        .await?;
    let key = state
        .db
        .authenticate_key(&copied.key, state.config.key_pepper.as_bytes())
        .await?;
    drop(copied);
    let bytes = Bytes::from(serde_json::to_vec(&body.request).map_err(|_| AppError::Internal)?);
    let memory = state.proxy_memory_budget.reservation();
    if !memory.try_grow(
        bytes.len(),
        crate::gateway_body::memory::REQUEST_MEMORY_WEIGHT,
    ) {
        return Err(AppError::Overloaded);
    }
    // Operator authorization and caller-controlled forwarding headers must not
    // enter provider requests. Normal gateway routing, policy, usage and billing
    // all execute against the selected existing downstream identity.
    let mut inference_headers = HeaderMap::new();
    inference_headers.insert(
        "x-mtc-client-name",
        HeaderValue::from_static("operator-inference"),
    );
    let mut response = proxy::proxy_with_identity(
        state,
        inference_headers,
        bytes,
        protocol,
        key,
        None,
        memory,
        None,
    )
    .await?;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn validate_request(request: &Value) -> Result<(), AppError> {
    if !request.is_object() {
        return Err(AppError::BadRequest(
            "request must be a text inference object".into(),
        ));
    }
    if request
        .get("stream")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(AppError::BadRequest(
            "operator inference requires stream=false".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
