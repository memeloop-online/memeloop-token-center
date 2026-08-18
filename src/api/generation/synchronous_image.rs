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

    // Both OpenAI Images and Responses-tool providers share the same hard
    // memory/concurrency envelope. Keeping this permit through URL asset
    // archival also prevents a signed-URL provider from bypassing the bound.
    let _image_response_permit = match acquire_image_permit_with_heartbeat(
        &IMAGE_RESPONSE_PERMITS,
        Duration::from_secs(5 * 60),
        || renew_image_request_claim(context),
    )
    .await
    {
        Ok(Some(permit)) => permit,
        Ok(None) => {
            return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
        }
        Err(_) => return fail_image_request(context, "image_concurrency_unavailable").await,
    };
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
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

async fn archive_synchronous_image_response(
    context: &SyncImageRequest<'_>,
    result_lease: &mut crate::archive_staging::ArchiveStagingWriteLease,
    response: Bytes,
) -> Result<String, AppError> {
    Ok(crate::generation::write_generation_staging_bytes(
        context.state,
        result_lease,
        "response.json",
        response,
    )
    .await?
    .object_locator)
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
    let mut archived_assets = Vec::new();
    let parsed: Value = match serde_json::from_slice(&response_bytes) {
        Ok(value) => value,
        Err(_) => {
            return fail_image_request(context, "upstream_image_invalid_json").await;
        }
    };
    let urls = match openai_image_urls(&parsed, context.expected_image_count) {
        Ok(urls) => urls,
        Err(_) => {
            return fail_image_request(context, "upstream_image_invalid_payload").await;
        }
    };
    let mut result_lease = crate::generation::begin_generation_staging_attempt(
        state,
        crate::archive_staging::ArchiveStagingOwner::SynchronousRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Result,
        Uuid::now_v7(),
    )
    .await?;
    let archive_budget = crate::generation::AssetArchiveBudget::default();
    for (index, url) in urls.into_iter().enumerate() {
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
    let sanitized = match sanitize_openai_image_response(&parsed, request_id, &archived_assets) {
        Ok(response) => response,
        Err(_) => {
            return fail_image_request_with_staging(
                context,
                "upstream_image_invalid_payload",
                Some(&result_lease),
            )
            .await;
        }
    };
    let archive_bytes =
        Bytes::from(serde_json::to_vec(&sanitized).map_err(|_| AppError::Internal)?);
    let response_object =
        match archive_synchronous_image_response(context, &mut result_lease, archive_bytes.clone())
            .await
        {
            Ok(location) => location,
            Err(_) => {
                return fail_image_request_with_staging(
                    context,
                    "archive_write",
                    Some(&result_lease),
                )
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
        .header(header::CONTENT_LENGTH, archive_bytes.len())
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(archive_bytes))
        .map_err(|_| AppError::Internal)
}

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
            (None, Some(encoded))
                if STANDARD
                    .decode(encoded)
                    .is_ok_and(|decoded| !decoded.is_empty()) => {}
            _ => {
                return Err(AppError::Upstream(
                    "image upstream response result has invalid image data".into(),
                ));
            }
        }
    }
    Ok(urls)
}

pub(in crate::api) fn sanitize_openai_image_response(
    value: &Value,
    request_id: Uuid,
    assets: &[crate::model::ArchivedGenerationAsset],
) -> Result<Value, AppError> {
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Upstream("image upstream response has no data array".into()))?;
    let mut assets = assets.iter();
    let mut sanitized_data = Vec::with_capacity(data.len());
    for item in data {
        let object = item.as_object().ok_or_else(|| {
            AppError::Upstream("image upstream response result is invalid".into())
        })?;
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
        } else if let Some(encoded) = object.get("b64_json").and_then(Value::as_str) {
            sanitized.insert("b64_json".to_owned(), Value::String(encoded.to_owned()));
        } else {
            return Err(AppError::Upstream(
                "image upstream response result has invalid image data".into(),
            ));
        }
        if let Some(revised_prompt) = object
            .get("revised_prompt")
            .and_then(Value::as_str)
            .filter(|prompt| prompt.len() <= 32_000 && !prompt.contains('\0'))
        {
            sanitized.insert(
                "revised_prompt".to_owned(),
                Value::String(revised_prompt.to_owned()),
            );
        }
        sanitized_data.push(Value::Object(sanitized));
    }
    if assets.next().is_some() {
        return Err(AppError::Upstream(
            "image upstream asset metadata does not match results".into(),
        ));
    }
    let mut response = json!({
        "created": value
            .get("created")
            .and_then(Value::as_i64)
            .filter(|created| *created >= 0)
            .unwrap_or_else(|| unix_millis() / 1_000),
        "data": sanitized_data
    });
    if let Some(usage) = value.get("usage").and_then(sanitize_image_usage) {
        response["usage"] = usage;
    }
    Ok(response)
}

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
    let response = match serde_json::from_slice::<Value>(&bytes) {
        Ok(response) => response,
        Err(_) => {
            return fail_image_request(context, "upstream_image_invalid_json").await;
        }
    };
    let mut images = Vec::new();
    collect_image_results(&response, &mut images);
    if !has_one_valid_bounded_image(&images) {
        return fail_image_request(context, "upstream_image_invalid_payload").await;
    }
    let usage = response.get("usage").and_then(sanitize_image_usage);
    let mut transformed = json!({
        "created": unix_millis() / 1_000,
        "data": images.into_iter().map(|image| json!({"b64_json": image})).collect::<Vec<_>>()
    });
    if let Some(usage) = usage {
        transformed["usage"] = usage;
    }
    let response_bytes =
        Bytes::from(serde_json::to_vec(&transformed).expect("image response is JSON serializable"));
    let mut result_lease = crate::generation::begin_generation_staging_attempt(
        state,
        crate::archive_staging::ArchiveStagingOwner::SynchronousRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Result,
        Uuid::now_v7(),
    )
    .await?;
    let response_object = match archive_synchronous_image_response(
        context,
        &mut result_lease,
        response_bytes.clone(),
    )
    .await
    {
        Ok(location) => location,
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
        .header(header::CONTENT_LENGTH, response_bytes.len())
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(response_bytes))
        .map_err(|_| AppError::Internal)
}

fn collect_image_results(value: &Value, images: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_image_results(value, images);
            }
        }
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|value| value == "image_generation_call")
                && let Some(result) = object.get("result").and_then(Value::as_str)
            {
                images.push(result.to_owned());
            }
            for value in object.values() {
                collect_image_results(value, images);
            }
        }
        _ => {}
    }
}

pub(in crate::api) fn has_one_valid_bounded_image(images: &[String]) -> bool {
    images.len() == 1
        && STANDARD
            .decode(images.first().map(String::as_str).unwrap_or_default())
            .is_ok_and(|decoded| !decoded.is_empty() && decoded.len() <= MAX_IMAGE_RESPONSE)
}
