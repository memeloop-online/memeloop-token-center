//! Generic operator inference using an explicitly authorized billing credential.
//! Service identities have no end-user principal. The caller therefore chooses
//! an existing key they may already copy, rather than minting a new identity.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InferenceRequest {
    key_id: Uuid,
    request: Value,
    conversation: Option<InferenceConversation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InferenceConversation {
    id: String,
    turn_id: Option<String>,
    parent_turn_id: Option<String>,
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

pub(super) async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ManagementTenantQuery>,
    Json(body): Json<InferenceRequest>,
) -> Result<Response, AppError> {
    execute(state, headers, query, body, Protocol::OpenAiResponses).await
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
    let inference_headers = inference_headers(body.conversation.as_ref())?;
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
    let bytes = Bytes::from(
        serde_json::to_vec(&body.request)
            .map_err(|_| AppError::internal("operator_inference_execute_1"))?,
    );
    let memory = state.proxy_memory_budget.reservation();
    if !memory.try_grow(
        bytes.len(),
        crate::gateway_body::memory::REQUEST_MEMORY_WEIGHT,
    ) {
        return Err(AppError::capacity_error("operator_inference_memory"));
    }
    // Operator authorization and caller-controlled forwarding headers must not
    // enter provider requests. Normal gateway routing, policy, usage and billing
    // all execute against the selected existing downstream identity.
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
            "request must be an inference object".into(),
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

fn inference_headers(conversation: Option<&InferenceConversation>) -> Result<HeaderMap, AppError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-mtc-client-name",
        HeaderValue::from_static("operator-inference"),
    );
    if let Some(conversation) = conversation {
        for (name, value) in [
            ("x-mtc-conversation-id", Some(conversation.id.as_str())),
            ("x-mtc-turn-id", conversation.turn_id.as_deref()),
            (
                "x-mtc-parent-turn-id",
                conversation.parent_turn_id.as_deref(),
            ),
        ] {
            if let Some(value) = value {
                if value.is_empty()
                    || value.len() > 128
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
                {
                    return Err(AppError::BadRequest(
                        "conversation identifiers must contain 1 to 128 ASCII letters, digits, dots, underscores, colons or hyphens".into(),
                    ));
                }
                headers.insert(
                    name,
                    HeaderValue::from_str(value).map_err(|_| {
                        AppError::BadRequest("invalid conversation identifier".into())
                    })?,
                );
            }
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests;
