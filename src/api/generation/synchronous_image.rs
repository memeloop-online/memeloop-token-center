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

fn unretained_image_replay(request_id: Uuid) -> Response {
    let body = serde_json::to_vec(&json!({
        "error": {
            "code": "image_result_not_retained",
            "message": "The original image result is no longer retained and cannot be replayed. The upstream request was not resubmitted.",
            "retryable": false
        }
    }))
    .expect("static image replay response is JSON");
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(body))
        .expect("static image replay response headers are valid")
}

pub(super) fn image_metadata_locator(metadata: Value) -> Result<String, AppError> {
    Ok(format!(
        "metadata-only-json:{}",
        serde_json::to_string(&metadata).map_err(|_| AppError::Internal)?
    ))
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
            if response_object.starts_with("metadata-only-json:")
                || response_object.starts_with("provider-reference-json:")
            {
                return Ok(unretained_image_replay(request_id));
            }
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
        SynchronousImageIdempotencyClaim::Uncertain { request_id } => {
            Ok(uncertain_image_response(request_id, None))
        }
        SynchronousImageIdempotencyClaim::Claimed => Err(AppError::Internal),
    }
}

#[derive(Clone, Copy)]
pub(super) enum ImageResponseFormat {
    OpenAi,
    ResponsesTool,
    Antigravity,
}

pub(super) enum SynchronousImageUpstreamRequest {
    Reqwest(reqwest::RequestBuilder),
    Codex(wreq::RequestBuilder),
}

enum SynchronousImageUpstreamResponse {
    Reqwest(reqwest::Response),
    Codex(wreq::Response),
}

impl SynchronousImageUpstreamResponse {
    fn status(&self) -> StatusCode {
        match self {
            Self::Reqwest(response) => response.status(),
            Self::Codex(response) => response.status(),
        }
    }

    fn bytes_stream(
        self,
    ) -> std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<Bytes, ImageResponseReadError>> + Send>,
    > {
        match self {
            Self::Reqwest(response) => Box::pin(response.bytes_stream().map(|chunk| {
                chunk.map_err(|error| {
                    tracing::warn!(
                        is_timeout = error.is_timeout(),
                        is_connect = error.is_connect(),
                        "synchronous image upstream response stream failed"
                    );
                    ImageResponseReadError::Transport
                })
            })),
            Self::Codex(response) => Box::pin(response.bytes_stream().map(|chunk| {
                chunk.map_err(|error| {
                    tracing::warn!(
                        is_timeout = error.is_timeout(),
                        is_connect = error.is_connect(),
                        "native Codex image response stream failed"
                    );
                    ImageResponseReadError::Transport
                })
            })),
        }
    }

    async fn classify_rate_limit(self) -> crate::db::UpstreamFailureKind {
        match self {
            Self::Reqwest(response) => crate::api::classify_media_rate_limit(response).await,
            Self::Codex(response) => {
                crate::api::proxy::classify_codex_media_rate_limit(response).await
            }
        }
    }
}

enum SynchronousImageSendError {
    Reqwest(reqwest::Error),
    Codex(wreq::Error),
}

impl SynchronousImageSendError {
    fn is_connect(&self) -> bool {
        match self {
            Self::Reqwest(error) => error.is_connect(),
            Self::Codex(error) => {
                error.is_connect() || error.is_proxy_connect() || error.is_dns() || error.is_tls()
            }
        }
    }

    fn is_timeout(&self) -> bool {
        match self {
            Self::Reqwest(error) => error.is_timeout(),
            Self::Codex(error) => error.is_timeout(),
        }
    }
}

impl SynchronousImageUpstreamRequest {
    async fn send(self) -> Result<SynchronousImageUpstreamResponse, SynchronousImageSendError> {
        match self {
            Self::Reqwest(request) => request
                .send()
                .await
                .map(SynchronousImageUpstreamResponse::Reqwest)
                .map_err(SynchronousImageSendError::Reqwest),
            Self::Codex(request) => request
                .send()
                .await
                .map(SynchronousImageUpstreamResponse::Codex)
                .map_err(SynchronousImageSendError::Codex),
        }
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
    pub(super) tenant_id: Uuid,
    pub(super) arm_state: std::sync::atomic::AtomicU8,
    pub(super) invalid_response: std::sync::atomic::AtomicBool,
    pub(super) confirmed_rejection: std::sync::atomic::AtomicBool,
}

pub(super) const ARM_NOT_STARTED: u8 = 0;
pub(super) const ARM_PENDING: u8 = 1;
pub(super) const ARM_CONFIRMED: u8 = 2;

pub(super) async fn submission_may_have_started(
    context: &SyncImageRequest<'_>,
) -> Result<bool, AppError> {
    use std::sync::atomic::Ordering;
    match context.arm_state.load(Ordering::Acquire) {
        ARM_NOT_STARTED => Ok(false),
        ARM_CONFIRMED => Ok(true),
        _ => match context
            .state
            .db
            .confirm_synchronous_image_submission_started(
                context.key_id,
                context.request_id,
                context.reservation.id,
            )
            .await
        {
            Ok(started) => {
                context.arm_state.store(
                    if started {
                        ARM_CONFIRMED
                    } else {
                        ARM_NOT_STARTED
                    },
                    Ordering::Release,
                );
                Ok(started)
            }
            // Missing/changed ownership is not ours to refund or quarantine.
            // Preserve that explicit conflict instead of inventing uncertainty
            // for a request now owned or already settled by another worker.
            Err(error @ (AppError::NotFound | AppError::Conflict(_))) => Err(error),
            Err(error) => {
                tracing::warn!(request_id=%context.request_id, error_category=error.diagnostic_category(),
                    "image arm outcome could not be established; retaining uncertainty");
                Ok(true)
            }
        },
    }
}

fn uncertain_image_response(request_id: Uuid, reconciliation_available: Option<bool>) -> Response {
    let body = serde_json::to_vec(&json!({"error": {
        "code": "image_submission_uncertain",
        "message": if reconciliation_available == Some(true) {
            "Image submission may have executed; automatic retry is disabled. The request is recorded for reconciliation."
        } else {
            "Image submission may have executed; automatic retry is disabled. Check request status and reconcile once the request is listed."
        },
        "retryable": false,
        "reconciliation_available": reconciliation_available
    }})).expect("static uncertainty response");
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header(header::CONTENT_TYPE, "application/json")
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(body))
        .expect("static uncertainty headers")
}

fn image_submission_state_unavailable(request_id: Uuid) -> Response {
    let body = serde_json::to_vec(&json!({"error": {
        "code": "image_submission_state_unavailable",
        "message": "Submission state could not be confirmed or published. Do not resubmit. No refund or resubmission was initiated by this recovery attempt. Check this request after storage recovers; reconciliation availability is not confirmed.",
        "retryable": false,
        "reconciliation_available": false
    }})).expect("static submission-state response");
    Response::builder()
        .status(StatusCode::CONFLICT)
        .header(header::CONTENT_TYPE, "application/json")
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(body))
        .expect("static submission-state headers")
}

async fn quarantine_image_request(context: &SyncImageRequest<'_>) -> Response {
    if let Err(error) = context
        .state
        .db
        .quarantine_synchronous_image_submission(
            context.key_id,
            context.idempotency_key,
            context.request_id,
            context.reservation.id,
        )
        .await
    {
        // A pending arm is not proof that a quarantine row exists. Preserve
        // the reservation and prohibit resubmission, but do not advertise a
        // reconciliation action whose durable publication was not confirmed.
        tracing::warn!(request_id=%context.request_id, error_category=error.diagnostic_category(),
            "image uncertainty publication unavailable; this recovery attempt will not release or resubmit without authoritative verification");
        return image_submission_state_unavailable(context.request_id);
    }
    uncertain_image_response(context.request_id, Some(true))
}

pub(super) async fn execute_synchronous_image_request(
    context: &SyncImageRequest<'_>,
    _request_body: Bytes,
    _staged_request_object: &str,
    route: &crate::provider::ResolvedUpstream,
    request: SynchronousImageUpstreamRequest,
    response_format: ImageResponseFormat,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    // The route lifecycle permit was acquired before the request body was read
    // and remains held by the authentication middleware through this handler.
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    let mut attempt =
        match crate::generation::group_routing::admit(state, context.tenant_id, request_id, route)
            .await
        {
            Ok(attempt) => attempt,
            Err(error) => {
                // No request has been sent: ordinary terminal cleanup is safe.
                let _ = fail_image_request(context, "upstream_unavailable").await?;
                return Err(error);
            }
        };
    // Archival/admission may have waited since preparation. Recheck expiry
    // before arming, while a local credential failure is still non-dispatched.
    // Preparation already attached the credential. Revalidate its lifetime,
    // without appending a second Authorization (or custom API-key) header.
    if route.credential.validate(unix_millis()).is_err() {
        return fail_image_request(context, "upstream_credential_invalid").await;
    }
    // Pending is not proof of dispatch. After an error/cancelled wait, resolve
    // this state through the serialized authoritative DB query before deciding
    // whether zero-cost cleanup is safe. Only an acknowledged arm permits send.
    context
        .arm_state
        .store(ARM_PENDING, std::sync::atomic::Ordering::Release);
    state
        .db
        .arm_synchronous_image_submission(
            context.key_id,
            context.idempotency_key,
            request_id,
            context.reservation.id,
        )
        .await?;
    context
        .arm_state
        .store(ARM_CONFIRMED, std::sync::atomic::Ordering::Release);
    let _upstream_activity = state.metrics.active_upstream(&route.driver, "image");
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "image",
        upstream_result
            .as_ref()
            .ok()
            .map(|response| response.status()),
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
            let terminal = if error.is_connect() {
                crate::api::MediaAttemptTerminal::Failed {
                    kind: crate::db::UpstreamFailureKind::Connection,
                    reason: crate::metrics::UpstreamHealthReason::Connection,
                }
            } else {
                crate::api::MediaAttemptTerminal::Inconclusive
            };
            attempt.complete(terminal).await;
            return fail_image_request(context, "upstream_connection").await;
        }
    };
    let upstream_status = upstream.status();
    // A complete explicit client rejection is non-execution evidence, except
    // timeout/too-early/rate-limit responses whose semantics remain uncertain.
    // Record it immediately from headers, before awaiting hooks/body/settlement.
    // This only permits zero-cost settlement; it never permits another POST.
    if upstream_status.is_client_error() && !matches!(upstream_status.as_u16(), 408 | 425 | 429) {
        context
            .confirmed_rejection
            .store(true, std::sync::atomic::Ordering::Release);
    }
    if upstream_status == StatusCode::TOO_MANY_REQUESTS {
        let kind = upstream.classify_rate_limit().await;
        attempt
            .complete(crate::api::MediaAttemptTerminal::Failed {
                kind,
                reason: crate::metrics::UpstreamHealthReason::RateLimited,
            })
            .await;
        return fail_image_request(context, "upstream_http_429").await;
    }
    let failure = if matches!(
        upstream_status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        Some((
            crate::db::UpstreamFailureKind::Authentication,
            crate::metrics::UpstreamHealthReason::Unavailable,
        ))
    } else if upstream_status.is_server_error() {
        Some((
            crate::db::UpstreamFailureKind::Unavailable,
            crate::metrics::UpstreamHealthReason::Unavailable,
        ))
    } else {
        None
    };
    if let Some((kind, reason)) = failure {
        attempt
            .complete(crate::api::MediaAttemptTerminal::Failed { kind, reason })
            .await;
    }
    if !upstream_status.is_success() {
        drop(upstream);
        return fail_image_request(
            context,
            &format!("upstream_http_{}", upstream_status.as_u16()),
        )
        .await;
    }
    let response_bytes = match read_synchronous_image_response_bounded(upstream).await {
        Ok(bytes) => bytes,
        Err(ImageResponseReadError::Transport) => {
            attempt
                .complete(crate::api::MediaAttemptTerminal::invalid_response())
                .await;
            return fail_image_request(context, "upstream_stream").await;
        }
        Err(ImageResponseReadError::TooLarge) => {
            attempt
                .complete(crate::api::MediaAttemptTerminal::Inconclusive)
                .await;
            // No archive writer is created before the cumulative limit has
            // been checked, so an oversized body can never become a partial
            // or apparently successful archive.
            return fail_image_request(context, "upstream_image_too_large").await;
        }
    };
    if !renew_image_request_claim(context).await? {
        return Ok(replayed_image_failure(request_id, "idempotency_claim_lost"));
    }
    let result = match response_format {
        ImageResponseFormat::ResponsesTool | ImageResponseFormat::Antigravity => {
            finish_responses_tool_image(context, upstream_status, response_bytes, response_format)
                .await
        }
        ImageResponseFormat::OpenAi => {
            finish_openai_image_response(context, route, upstream_status, response_bytes).await
        }
    };
    if result
        .as_ref()
        .is_ok_and(|response| response.status().is_success())
    {
        attempt
            .complete_committed(crate::api::MediaAttemptTerminal::Succeeded)
            .await;
    } else if context
        .invalid_response
        .load(std::sync::atomic::Ordering::Acquire)
    {
        attempt
            .complete(crate::api::MediaAttemptTerminal::invalid_response())
            .await;
    }
    result
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
    read_synchronous_image_response_bounded(SynchronousImageUpstreamResponse::Reqwest(response))
        .await
}

async fn read_synchronous_image_response_bounded(
    response: SynchronousImageUpstreamResponse,
) -> Result<Bytes, ImageResponseReadError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
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
    if submission_may_have_started(context).await?
        && !context
            .confirmed_rejection
            .load(std::sync::atomic::Ordering::Acquire)
    {
        if matches!(
            error_code,
            "upstream_image_invalid_json"
                | "upstream_image_invalid_payload"
                | "upstream_image_response_too_large"
                | "upstream_image_too_large"
        ) {
            context
                .invalid_response
                .store(true, std::sync::atomic::Ordering::Release);
        }
        // An unusable result never becomes a published asset. Hand its owned
        // staging attempt to durable cleanup without deleting bytes here or
        // interpreting this local cleanup as permission to refund/resubmit.
        if let Some(lease) = result_lease
            && let Err(error) = context
                .state
                .db
                .abandon_archive_staging_attempt(lease)
                .await
        {
            tracing::warn!(request_id=%context.request_id, error_category=error.diagnostic_category(),
                "uncertain image result cleanup publication failed; lease expiry retains recovery");
        }
        return Ok(quarantine_image_request(context).await);
    }
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
    let mut provider_assets = Vec::new();
    for (index, url) in parsed.url_assets() {
        if crate::generation::ensure_asset_origin(route, url).is_err() {
            return fail_image_request(context, "upstream_image_asset").await;
        }
        provider_assets.push(crate::generation::provider_generation_asset(
            request_id,
            index,
            url,
            crate::generation::provider_expiry_millis(&Value::Null, url),
            None,
        )?);
    }
    for asset in &provider_assets {
        match crate::generation::provider_asset_is_obtainable(state, route, asset).await {
            Ok(true) => {}
            Ok(false) => {
                context
                    .invalid_response
                    .store(true, std::sync::atomic::Ordering::Release);
                return fail_image_request(context, "upstream_image_asset_unavailable").await;
            }
            Err(error) => {
                tracing::warn!(
                    request_id = %request_id,
                    error_category = error.diagnostic_category(),
                    "synchronous image provider asset could not be validated"
                );
                context
                    .invalid_response
                    .store(true, std::sync::atomic::Ordering::Release);
                return fail_image_request(context, "upstream_image_asset_unavailable").await;
            }
        }
    }
    let (response_segments, response_len) =
        match super::openai_image_response::build_provider_referenced_openai_image_segments(
            response_bytes,
            parsed,
            request_id,
            &provider_assets,
            unix_millis() / 1_000,
        ) {
            Ok(response) => response,
            Err(super::openai_image_response::OpenAiImageBuildError::TooLarge) => {
                return fail_image_request(context, "upstream_image_response_too_large").await;
            }
            Err(super::openai_image_response::OpenAiImageBuildError::InvalidAssets) => {
                return fail_image_request(context, "upstream_image_invalid_payload").await;
            }
            Err(super::openai_image_response::OpenAiImageBuildError::Internal) => {
                return Err(AppError::Internal);
            }
        };
    let response_object = if provider_assets.is_empty() {
        image_metadata_locator(json!({
            "kind": "synchronous_image",
            "media_archived": false,
            "replay_available": false,
            "result_count": context.expected_image_count,
            "billed_units": billed_units
        }))?
    } else {
        crate::generation::seal_provider_asset_reference(
            "request",
            request_id,
            &provider_assets,
            state.config.key_pepper.as_bytes(),
        )?
    };
    match commit_synchronous_image_terminal(
        context,
        i64::from(upstream_status.as_u16()),
        0,
        billed_units,
        None,
        &response_object,
        &[],
        None,
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
    response_format: ImageResponseFormat,
) -> Result<Response, AppError> {
    let state = context.state;
    let request_id = context.request_id;
    let billed_units = context.billed_units;
    if !upstream_status.is_success() {
        let error_code = format!("upstream_http_{}", upstream_status.as_u16());
        return fail_image_request(context, &error_code).await;
    }
    let parsed = match match response_format {
        ImageResponseFormat::Antigravity => super::antigravity_image::parse(&bytes),
        _ => super::responses_tool_image::parse_responses_tool_image(&bytes),
    } {
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
    let response_object = image_metadata_locator(json!({
        "kind": "synchronous_image",
        "media_archived": false,
        "replay_available": false,
        "result_count": 1,
        "billed_units": billed_units
    }))?;
    match commit_synchronous_image_terminal(
        context,
        200,
        0,
        billed_units,
        None,
        &response_object,
        &[],
        None,
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
