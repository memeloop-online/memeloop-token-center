use super::super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::api) enum ImageResponseReadError {
    Transport,
    TooLarge,
}

pub(in crate::api) fn scoped_upstream_image_idempotency(
    pepper: &[u8],
    tenant_id: Uuid,
    key_id: Uuid,
    route_id: Uuid,
    upstream_path: &str,
    downstream_idempotency_key: &str,
) -> String {
    let secret = blake3::hash(pepper);
    let mut hasher = blake3::Hasher::new_keyed(secret.as_bytes());
    for value in [
        tenant_id.as_bytes().as_slice(),
        key_id.as_bytes().as_slice(),
        route_id.as_bytes().as_slice(),
        upstream_path.as_bytes(),
        downstream_idempotency_key.as_bytes(),
    ] {
        hasher.update(&(value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    format!("mtc-img-{}", hasher.finalize().to_hex())
}

fn replayed_image_failure(request_id: Uuid, _error_code: &str) -> Response {
    let body = serde_json::to_vec(&json!({
        "error": {
            "code": "upstream_error",
            "message": "configured upstream is unavailable"
        }
    }))
    .expect("static image failure response is JSON");
    Response::builder()
        .status(StatusCode::BAD_GATEWAY)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(body))
        .expect("static image failure response headers are valid")
}

pub(super) async fn image_idempotency_replay_response(
    state: &AppState,
    replay: SynchronousImageIdempotencyClaim,
) -> Result<Response, AppError> {
    match replay {
        SynchronousImageIdempotencyClaim::Completed {
            request_id,
            response_status,
            response_object,
        } => {
            let response = state
                .archive
                .get_bounded(&response_object, MAX_IMAGE_RESPONSE)
                .await?;
            let status = u16::try_from(response_status)
                .ok()
                .and_then(|status| StatusCode::from_u16(status).ok())
                .ok_or(AppError::Internal)?;
            Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, response.len())
                .header(REQUEST_ID_HEADER, request_id.to_string())
                .body(Body::from(response))
                .map_err(|_| AppError::Internal)
        }
        SynchronousImageIdempotencyClaim::Failed {
            request_id,
            error_code,
        } => Ok(replayed_image_failure(request_id, &error_code)),
        SynchronousImageIdempotencyClaim::Pending { request_id } => Err(AppError::Conflict(
            format!("image request {request_id} with this Idempotency-Key is still in progress"),
        )),
        SynchronousImageIdempotencyClaim::Claimed => Err(AppError::Internal),
    }
}

pub(super) struct SyncImageRequest<'a> {
    pub(super) state: &'a AppState,
    pub(super) reservation: &'a crate::model::UsageReservation,
    pub(super) request_id: Uuid,
    pub(super) started: Instant,
    pub(super) billed_units: i64,
    pub(super) expected_image_count: i64,
    pub(super) key_id: Uuid,
    pub(super) idempotency_key: Option<&'a str>,
}

pub(super) async fn execute_synchronous_image_request(
    context: &SyncImageRequest<'_>,
    request_body: Bytes,
    staged_request_object: &str,
    route: &crate::provider::ResolvedUpstream,
    request: reqwest::RequestBuilder,
    responses_tool_mode: bool,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    let request_attempt = Uuid::now_v7();
    let mut request_lease = crate::generation::begin_generation_staging_attempt(
        state,
        crate::archive_staging::ArchiveStagingOwner::SynchronousRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        request_attempt,
    )
    .await?;
    let request_object = match crate::generation::write_generation_staging_bytes(
        state,
        &mut request_lease,
        "request.json",
        request_body,
    )
    .await
    {
        Ok(staged) => staged.object_locator,
        Err(_) => {
            state
                .db
                .abandon_archive_staging_attempt(&request_lease)
                .await?;
            return fail_image_request(context, "archive_write").await;
        }
    };
    if let Err(error) = state
        .db
        .attach_synchronous_image_request_object_staged(
            AttachSynchronousImageRequestObject {
                key_id: context.key_id,
                idempotency_key: context.idempotency_key,
                request_id,
                reservation_id: context.reservation.id,
                expected_staging_object: staged_request_object,
                request_object: &request_object,
            },
            &request_lease,
        )
        .await
    {
        tracing::warn!(
            request_id = %request_id,
            owner_lost = matches!(error, AppError::NotFound),
            "synchronous image request archive could not be attached"
        );
        return fail_image_request(context, "archive_metadata").await;
    }

    // The route lifecycle permit was acquired before the request body was read
    // and remains held by the authentication middleware through this handler.
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    let _upstream_activity = state.metrics.active_upstream(&route.driver, "image");
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "image",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        context.started.elapsed(),
    );
    let upstream = match upstream_result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                request_id = %request_id,
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "synchronous image upstream request failed"
            );
            return fail_image_request(context, "upstream_connection").await;
        }
    };
    let upstream_status = upstream.status();
    let response_bytes = match read_image_response_bounded(upstream).await {
        Ok(bytes) => bytes,
        Err(ImageResponseReadError::Transport) => {
            return fail_image_request(context, "upstream_stream").await;
        }
        Err(ImageResponseReadError::TooLarge) => {
            // No archive writer is created before the cumulative limit has
            // been checked, so an oversized body can never become a partial
            // or apparently successful archive.
            return fail_image_request(context, "upstream_image_too_large").await;
        }
    };
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    if responses_tool_mode {
        finish_responses_tool_image(context, upstream_status, response_bytes).await
    } else {
        finish_openai_image_response(context, route, upstream_status, response_bytes).await
    }
}

async fn renew_image_request_claim(context: &SyncImageRequest<'_>) -> Result<bool, AppError> {
    let Some(idempotency_key) = context.idempotency_key else {
        return Ok(true);
    };
    match context
        .state
        .db
        .renew_synchronous_image_idempotency_claim(
            context.key_id,
            idempotency_key,
            context.request_id,
        )
        .await
    {
        Ok(()) => Ok(true),
        // The successful takeover transaction owns cleanup of the old
        // reservation and request. This worker must not independently settle
        // after losing its compare-and-swap ownership.
        Err(AppError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(in crate::api) async fn acquire_image_permit_with_heartbeat<'a, F, Fut>(
    semaphore: &'a tokio::sync::Semaphore,
    heartbeat_interval: Duration,
    mut heartbeat: F,
) -> Result<Option<tokio::sync::SemaphorePermit<'a>>, AppError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool, AppError>>,
{
    loop {
        match tokio::time::timeout(heartbeat_interval, semaphore.acquire()).await {
            Ok(Ok(permit)) => return Ok(Some(permit)),
            Ok(Err(_)) => return Err(AppError::Internal),
            Err(_) if !heartbeat().await? => return Ok(None),
            Err(_) => {}
        }
    }
}

pub(in crate::api) async fn read_image_response_bounded(
    response: reqwest::Response,
) -> Result<Bytes, ImageResponseReadError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "synchronous image upstream response stream failed"
            );
            ImageResponseReadError::Transport
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_IMAGE_RESPONSE {
            return Err(ImageResponseReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

#[allow(clippy::too_many_arguments)]
async fn commit_synchronous_image_terminal(
    context: &SyncImageRequest<'_>,
    status_code: i64,
    input_tokens: i64,
    output_tokens: i64,
    error_code: Option<&str>,
    response_object: &str,
    assets: &[crate::model::ArchivedGenerationAsset],
    result_lease: Option<&crate::archive_staging::ArchiveStagingWriteLease>,
) -> Result<FinishSynchronousImageResult, AppError> {
    for attempt in 0..3 {
        let result = context
            .state
            .db
            .finish_synchronous_image_request_staged(
                FinishSynchronousImageRequest {
                    key_id: context.key_id,
                    idempotency_key: context.idempotency_key,
                    request_id: context.request_id,
                    reservation: context.reservation,
                    status_code,
                    duration_ms: context.started.elapsed().as_millis() as i64,
                    input_tokens,
                    output_tokens,
                    error_code,
                    response_object,
                    assets,
                },
                result_lease,
            )
            .await;
        match result {
            Ok(result) => return Ok(result),
            Err(AppError::Internal) if attempt < 2 => {
                tokio::time::sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(AppError::Internal)
}

pub(super) async fn fail_image_request(
    context: &SyncImageRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    fail_image_request_with_staging(context, error_code, None).await
}

async fn fail_image_request_with_staging(
    context: &SyncImageRequest<'_>,
    error_code: &str,
    result_lease: Option<&crate::archive_staging::ArchiveStagingWriteLease>,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    let response_object = format!("gap://{request_id}/response");
    let (response, cleanup_staging) = match commit_synchronous_image_terminal(
        context,
        502,
        0,
        0,
        Some(error_code),
        &response_object,
        &[],
        result_lease,
    )
    .await?
    {
        FinishSynchronousImageResult::Finished { .. } => {
            (replayed_image_failure(request_id, error_code), false)
        }
        FinishSynchronousImageResult::Replay(replay) => {
            let cleanup = matches!(&replay, SynchronousImageIdempotencyClaim::Failed { .. });
            (
                image_idempotency_replay_response(state, replay).await?,
                cleanup,
            )
        }
    };
    let _ = cleanup_staging;
    Ok(response)
}

async fn finish_openai_image_response(
    context: &SyncImageRequest<'_>,
    route: &crate::provider::ResolvedUpstream,
    upstream_status: StatusCode,
    response_bytes: Bytes,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    let billed_units = context.billed_units;
    if !upstream_status.is_success() {
        let error_code = format!("upstream_http_{}", upstream_status.as_u16());
        return fail_image_request(context, &error_code).await;
    }
    let parsed = match super::openai_image_response::parse_openai_image_response(
        &response_bytes,
        context.expected_image_count,
    ) {
        Ok(parsed) => parsed,
        Err(super::openai_image_response::OpenAiImageParseError::InvalidJson) => {
            return fail_image_request(context, "upstream_image_invalid_json").await;
        }
        Err(super::openai_image_response::OpenAiImageParseError::InvalidPayload) => {
            return fail_image_request(context, "upstream_image_invalid_payload").await;
        }
    };
    let mut archived_assets = Vec::new();
    let mut result_lease = crate::generation::begin_generation_staging_attempt(
        state,
        crate::archive_staging::ArchiveStagingOwner::SynchronousRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Result,
        Uuid::now_v7(),
    )
    .await?;
    let archive_budget = crate::generation::AssetArchiveBudget::default();
    for (index, url) in parsed.url_assets() {
        if !renew_image_request_claim(context).await? {
            return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
        }
        let asset = match crate::generation::archive_asset_staged(
            state,
            route,
            &route.credential,
            &archive_budget,
            &mut result_lease,
            index,
            url,
            None,
        )
        .await
        {
            Ok(asset) => asset,
            Err(_) => {
                tracing::warn!(
                    request_id = %request_id,
                    asset_index = index,
                    "synchronous image URL asset archival failed"
                );
                return fail_image_request_with_staging(
                    context,
                    "upstream_image_asset",
                    Some(&result_lease),
                )
                .await;
            }
        };
        archived_assets.push(asset);
    }
    let (response_segments, response_len) =
        match super::openai_image_response::build_openai_image_segments(
            response_bytes,
            parsed,
            request_id,
            &archived_assets,
            unix_millis() / 1_000,
        ) {
            Ok(response) => response,
            Err(super::openai_image_response::OpenAiImageBuildError::TooLarge) => {
                return fail_image_request_with_staging(
                    context,
                    "upstream_image_response_too_large",
                    Some(&result_lease),
                )
                .await;
            }
            Err(super::openai_image_response::OpenAiImageBuildError::InvalidAssets) => {
                return fail_image_request_with_staging(
                    context,
                    "upstream_image_invalid_payload",
                    Some(&result_lease),
                )
                .await;
            }
            Err(super::openai_image_response::OpenAiImageBuildError::Internal) => {
                return Err(AppError::Internal);
            }
        };
    let response_object = match crate::generation::write_generation_staging_segments(
        state,
        &mut result_lease,
        "response.json",
        response_segments.clone(),
    )
    .await
    {
        Ok(staged) => staged.object_locator,
        Err(_) => {
            return fail_image_request_with_staging(context, "archive_write", Some(&result_lease))
                .await;
        }
    };
    match commit_synchronous_image_terminal(
        context,
        i64::from(upstream_status.as_u16()),
        0,
        billed_units,
        None,
        &response_object,
        &archived_assets,
        Some(&result_lease),
    )
    .await?
    {
        FinishSynchronousImageResult::Finished { .. } => {}
        FinishSynchronousImageResult::Replay(replay) => {
            return image_idempotency_replay_response(state, replay).await;
        }
    }
    Response::builder()
        .status(upstream_status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, response_len)
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from_stream(futures_util::stream::iter(
            response_segments
                .into_iter()
                .map(Ok::<_, std::convert::Infallible>),
        )))
        .map_err(|_| AppError::Internal)
}

#[cfg(test)]
pub(in crate::api) fn openai_image_urls(
    value: &Value,
    expected_count: i64,
) -> Result<Vec<&str>, AppError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Upstream("image upstream response has no data array".into()))?;
    if usize::try_from(expected_count).ok() != Some(data.len()) {
        return Err(AppError::Upstream(
            "image upstream response has an invalid result count".into(),
        ));
    }
    let mut urls = Vec::new();
    for item in data {
        let url = item
            .get("url")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        let b64 = item.get("b64_json").and_then(Value::as_str);
        match (url, b64) {
            (Some(url), None) => urls.push(url),
            (None, Some(encoded)) if is_valid_bounded_base64(encoded, MAX_IMAGE_RESPONSE) => {}
            _ => {
                return Err(AppError::Upstream(
                    "image upstream response result has invalid image data".into(),
                ));
            }
        }
    }
    Ok(urls)
}

#[cfg(test)]
pub(in crate::api) fn sanitize_openai_image_response(
    mut value: Value,
    request_id: Uuid,
    assets: &[crate::model::ArchivedGenerationAsset],
) -> Result<Value, AppError> {
    let created = value
        .get("created")
        .and_then(Value::as_i64)
        .filter(|created| *created >= 0)
        .unwrap_or_else(|| unix_millis() / 1_000);
    let usage = value.get("usage").and_then(sanitize_image_usage);
    let data = value
        .as_object_mut()
        .and_then(|object| object.remove("data"))
        .and_then(|data| match data {
            Value::Array(data) => Some(data),
            _ => None,
        })
        .ok_or_else(|| AppError::Upstream("image upstream response has no data array".into()))?;
    let mut assets = assets.iter();
    let mut sanitized_data = Vec::with_capacity(data.len());
    for item in data {
        let mut object = match item {
            Value::Object(object) => object,
            _ => {
                return Err(AppError::Upstream(
                    "image upstream response result is invalid".into(),
                ));
            }
        };
        let mut sanitized = serde_json::Map::new();
        if object.get("url").and_then(Value::as_str).is_some() {
            let asset = assets.next().ok_or_else(|| {
                AppError::Upstream("image upstream asset metadata is incomplete".into())
            })?;
            sanitized.insert(
                "url".to_owned(),
                Value::String(format!(
                    "/self/v1/requests/{request_id}/assets/{}",
                    asset.asset_id
                )),
            );
            sanitized.insert(
                "archived_asset".to_owned(),
                json!({
                    "asset_id": asset.asset_id,
                    "index": asset.index,
                    "mime_type": asset.mime_type,
                    "size_bytes": asset.size_bytes,
                    "filename": asset.filename
                }),
            );
        } else if let Some(Value::String(encoded)) = object.remove("b64_json") {
            sanitized.insert("b64_json".to_owned(), Value::String(encoded));
        } else {
            return Err(AppError::Upstream(
                "image upstream response result has invalid image data".into(),
            ));
        }
        if let Some(Value::String(revised_prompt)) =
            object.remove("revised_prompt").filter(|prompt| {
                prompt
                    .as_str()
                    .is_some_and(|prompt| prompt.len() <= 32_000 && !prompt.contains('\0'))
            })
        {
            sanitized.insert("revised_prompt".to_owned(), Value::String(revised_prompt));
        }
        sanitized_data.push(Value::Object(sanitized));
    }
    if assets.next().is_some() {
        return Err(AppError::Upstream(
            "image upstream asset metadata does not match results".into(),
        ));
    }
    let mut response = serde_json::Map::new();
    response.insert("created".to_owned(), json!(created));
    response.insert("data".to_owned(), Value::Array(sanitized_data));
    if let Some(usage) = usage {
        response.insert("usage".to_owned(), usage);
    }
    Ok(Value::Object(response))
}

#[cfg(test)]
fn sanitize_image_usage(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let mut sanitized = serde_json::Map::new();
    for field in ["total_tokens", "input_tokens", "output_tokens"] {
        if let Some(tokens) = object
            .get(field)
            .and_then(Value::as_i64)
            .filter(|tokens| (0..=MAX_REPORTED_TOKENS).contains(tokens))
        {
            sanitized.insert(field.to_owned(), json!(tokens));
        }
    }
    for details_field in ["input_tokens_details", "output_tokens_details"] {
        let Some(details) = object.get(details_field).and_then(Value::as_object) else {
            continue;
        };
        let mut sanitized_details = serde_json::Map::new();
        for field in ["image_tokens", "text_tokens"] {
            if let Some(tokens) = details
                .get(field)
                .and_then(Value::as_i64)
                .filter(|tokens| (0..=MAX_REPORTED_TOKENS).contains(tokens))
            {
                sanitized_details.insert(field.to_owned(), json!(tokens));
            }
        }
        if !sanitized_details.is_empty() {
            sanitized.insert(details_field.to_owned(), Value::Object(sanitized_details));
        }
    }
    (!sanitized.is_empty()).then_some(Value::Object(sanitized))
}

pub(super) fn responses_tool_image_request(
    config: &Value,
    image_model: &str,
    request: &Value,
) -> Result<Value, AppError> {
    let main_model = config
        .get("image_main_model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .ok_or_else(|| {
            AppError::BadRequest(
                "responses-tool image routes require config.image_main_model".into(),
            )
        })?;
    let prompt = request
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("prompt is required".into()))?;
    let mut tool = json!({
        "type": "image_generation",
        "model": image_model,
        "action": "generate"
    });
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "moderation",
    ] {
        if let Some(value) = request.get(field) {
            tool[field] = value.clone();
        }
    }
    for field in ["output_compression", "partial_images"] {
        if let Some(value) = request.get(field).filter(|value| value.is_number()) {
            tool[field] = value.clone();
        }
    }
    Ok(json!({
        "model": main_model,
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": prompt}]
        }],
        "tools": [tool],
        "tool_choice": {"type": "image_generation"},
        "stream": false,
        "store": false
    }))
}

async fn finish_responses_tool_image(
    context: &SyncImageRequest<'_>,
    upstream_status: StatusCode,
    bytes: Bytes,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    let billed_units = context.billed_units;
    if !upstream_status.is_success() {
        let error_code = format!("upstream_http_{}", upstream_status.as_u16());
        return fail_image_request(context, &error_code).await;
    }
    let parsed = match super::responses_tool_image::parse_responses_tool_image(&bytes) {
        Ok(parsed) => parsed,
        Err(super::responses_tool_image::ResponsesToolImageParseError::InvalidJson) => {
            return fail_image_request(context, "upstream_image_invalid_json").await;
        }
        Err(super::responses_tool_image::ResponsesToolImageParseError::InvalidPayload) => {
            return fail_image_request(context, "upstream_image_invalid_payload").await;
        }
    };
    let (response_segments, response_len) =
        match build_responses_tool_image_segments(bytes, parsed, unix_millis() / 1_000) {
            Ok(response) => response,
            Err(_) => {
                return fail_image_request(context, "upstream_image_response_too_large").await;
            }
        };
    let mut result_lease = crate::generation::begin_generation_staging_attempt(
        state,
        crate::archive_staging::ArchiveStagingOwner::SynchronousRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Result,
        Uuid::now_v7(),
    )
    .await?;
    let response_object = match crate::generation::write_generation_staging_segments(
        state,
        &mut result_lease,
        "response.json",
        response_segments.clone(),
    )
    .await
    {
        Ok(staged) => staged.object_locator,
        Err(_) => {
            return fail_image_request_with_staging(context, "archive_write", Some(&result_lease))
                .await;
        }
    };
    match commit_synchronous_image_terminal(
        context,
        200,
        0,
        billed_units,
        None,
        &response_object,
        &[],
        Some(&result_lease),
    )
    .await?
    {
        FinishSynchronousImageResult::Finished { .. } => {}
        FinishSynchronousImageResult::Replay(replay) => {
            return image_idempotency_replay_response(state, replay).await;
        }
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, response_len)
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from_stream(futures_util::stream::iter(
            response_segments
                .into_iter()
                .map(Ok::<_, std::convert::Infallible>),
        )))
        .map_err(|_| AppError::Internal)
}

fn build_responses_tool_image_segments(
    bytes: Bytes,
    parsed: super::responses_tool_image::ParsedResponsesToolImage,
    created: i64,
) -> Result<([Bytes; 3], usize), AppError> {
    let prefix = Bytes::from(format!(
        "{{\"created\":{created},\"data\":[{{\"b64_json\":\""
    ));
    let image = bytes.slice(parsed.image_range);
    let mut suffix = Vec::with_capacity(256);
    suffix.extend_from_slice(b"\"}]");
    if let Some(usage) = parsed.usage {
        suffix.extend_from_slice(b",\"usage\":");
        serde_json::to_writer(&mut suffix, &usage).map_err(|_| AppError::Internal)?;
    }
    suffix.push(b'}');
    let suffix = Bytes::from(suffix);
    let response_len = prefix
        .len()
        .checked_add(image.len())
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or(AppError::Internal)?;
    if response_len > MAX_IMAGE_RESPONSE {
        return Err(AppError::Upstream(
            "image response exceeds the bounded response size".into(),
        ));
    }
    Ok(([prefix, image, suffix], response_len))
}

#[cfg(test)]
fn take_image_results(value: &mut Value, images: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                take_image_results(value, images);
            }
        }
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|value| value == "image_generation_call")
                && object.get("result").is_some_and(Value::is_string)
                && let Some(Value::String(result)) = object.remove("result")
            {
                images.push(result);
            }
            for value in object.values_mut() {
                take_image_results(value, images);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
pub(in crate::api) fn extract_responses_tool_image(
    mut response: Value,
) -> Result<(String, Option<Value>), AppError> {
    let usage = response.get("usage").and_then(sanitize_image_usage);
    let mut images = Vec::with_capacity(1);
    take_image_results(&mut response, &mut images);
    if !has_one_valid_bounded_image(&images) {
        return Err(AppError::Upstream(
            "image upstream response has invalid image results".into(),
        ));
    }
    Ok((images.pop().expect("one image was validated"), usage))
}

#[cfg(test)]
pub(in crate::api) fn has_one_valid_bounded_image<T: AsRef<str>>(images: &[T]) -> bool {
    let Some(image) = images.first().filter(|_| images.len() == 1) else {
        return false;
    };
    is_valid_bounded_base64(image.as_ref(), MAX_IMAGE_RESPONSE)
}

pub(in crate::api) fn is_valid_bounded_base64(encoded: &str, max_decoded_len: usize) -> bool {
    let encoded = encoded.as_bytes();
    if encoded.is_empty() || !encoded.len().is_multiple_of(4) {
        return false;
    }

    // This bound rejects impossible inputs up front without allocating a
    // decoded image. The per-quantum decoder below performs strict alphabet
    // and padding validation using only three bytes of scratch space.
    let max_encoded_len = max_decoded_len.div_ceil(3).saturating_mul(4);
    if encoded.len() > max_encoded_len {
        return false;
    }

    let mut decoded_len = 0_usize;
    let mut decoded_quantum = [0_u8; 3];
    let quantum_count = encoded.len() / 4;
    for (index, quantum) in encoded.chunks_exact(4).enumerate() {
        if index + 1 != quantum_count && quantum.contains(&b'=') {
            return false;
        }
        let Ok(written) = STANDARD.decode_slice(quantum, &mut decoded_quantum) else {
            return false;
        };
        let Some(next_len) = decoded_len.checked_add(written) else {
            return false;
        };
        decoded_len = next_len;
        if decoded_len > max_decoded_len {
            return false;
        }
    }
    decoded_len != 0
}

#[cfg(test)]
mod segmented_response_tests {
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn benchmark_sized_response_is_exact_and_keeps_the_image_segment_shared() {
        let raw = vec![b'x'; 11 * 1024 * 1024];
        assert_eq!(
            format!("{:x}", Sha256::digest(&raw)),
            "d3cc623cd0df8c815806104a74383e616f7fc26c4c8710ae8d787809de886bea"
        );
        let encoded = STANDARD.encode(&raw);
        assert_eq!(encoded.len(), 15_379_116);
        let upstream = Bytes::from(
            serde_json::to_vec(&json!({
                "id": "resp_memory_image",
                "output": [{
                    "type": "image_generation_call",
                    "id": "ig_memory_image",
                    "result": encoded
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }))
            .expect("mock upstream response is JSON"),
        );
        let parsed = super::super::responses_tool_image::parse_responses_tool_image(&upstream)
            .expect("benchmark image is valid");
        let image_pointer = upstream.as_ptr() as usize + parsed.image_range.start;
        let (segments, content_length) =
            build_responses_tool_image_segments(upstream, parsed, 1_700_000_000)
                .expect("bounded response builds");
        assert_eq!(content_length, 15_379_225);
        assert_eq!(segments[1].as_ptr() as usize, image_pointer);
        assert_eq!(
            segments.iter().map(Bytes::len).sum::<usize>(),
            content_length
        );
        let archived_response = segments.clone().concat();
        let response = segments.concat();
        assert_eq!(archived_response, response);
        let value: Value = serde_json::from_slice(&response).expect("response is valid JSON");
        let decoded = STANDARD
            .decode(value["data"][0]["b64_json"].as_str().expect("image string"))
            .expect("image is strict base64");
        assert_eq!(decoded.len(), 11 * 1024 * 1024);
        assert_eq!(
            format!("{:x}", Sha256::digest(decoded)),
            "d3cc623cd0df8c815806104a74383e616f7fc26c4c8710ae8d787809de886bea"
        );
        assert_eq!(
            value["usage"],
            json!({"input_tokens": 1, "output_tokens": 1, "total_tokens": 2})
        );
    }
}
