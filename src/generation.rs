use futures_util::StreamExt;
use reqwest::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

mod attempt;
mod comfyui_schema;
pub(crate) mod group_routing;
mod protocol;
mod siliconflow_video_schema;
pub use comfyui_schema::{
    effective_parameter_schema as comfyui_parameter_schema,
    validate_config as validate_comfyui_config, validate_parameters as validate_comfyui_parameters,
};
pub(crate) use protocol::{GenerationProtocolAdapter, normalize_seedance_duration};
use sha2::{Digest, Sha256};
pub(crate) use siliconflow_video_schema::{
    parameter_schema as siliconflow_video_parameter_schema,
    validated_submit_parameters as validate_siliconflow_video_parameters,
};

use crate::{
    AppState,
    archive::StagedArchiveObject,
    archive_staging::{
        ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS, ArchiveStagingIntentDigest, ArchiveStagingKey,
        ArchiveStagingLeaseOwner, ArchiveStagingOwner, ArchiveStagingPurpose,
        ArchiveStagingWriteLease, BeginArchiveStagingInput, BeginArchiveStagingResult,
    },
    db::{FinishGenerationJobInput, unix_millis},
    error::AppError,
    model::{
        ArchivedGenerationAsset, GenerationJobWork, GenerationStagedAssets, ProviderGenerationAsset,
    },
    network,
    provider::{ResolvedUpstream, open_private_json, seal_private_json},
};

const MAX_CONTROL_BODY: usize = 4 * 1024 * 1024;
const MAX_ASSET_BODY: usize = 512 * 1024 * 1024;
const MAX_COMFY_ASSETS: usize = 16;
const MAX_SILICONFLOW_VIDEO_ASSETS: usize = 1;
const MAX_FAILURES: i64 = 20;
const MAX_JOB_AGE_MILLIS: i64 = 24 * 60 * 60 * 1_000;
pub(crate) const PROVIDER_REFERENCE_PREFIX: &str = "provider-reference-json:";
const PROVIDER_REFERENCE_AAD: &[u8] = b"memeloop-token-center/generation-provider-reference/v1";

#[derive(Deserialize, Serialize)]
struct ProviderAssetReference {
    assets: Vec<ProviderGenerationAsset>,
}

pub(crate) fn seal_provider_asset_reference(
    owner_kind: &str,
    owner_id: uuid::Uuid,
    assets: &[ProviderGenerationAsset],
    key_material: &[u8],
) -> Result<String, AppError> {
    let aad = provider_reference_aad(owner_kind, owner_id);
    let envelope = seal_private_json(
        &ProviderAssetReference {
            assets: assets.to_vec(),
        },
        key_material,
        &aad,
    )?;
    Ok(format!("{PROVIDER_REFERENCE_PREFIX}{envelope}"))
}

pub(crate) fn open_provider_asset_reference(
    owner_kind: &str,
    owner_id: uuid::Uuid,
    reference: &str,
    key_material: &[u8],
) -> Result<Vec<ProviderGenerationAsset>, AppError> {
    let envelope = reference
        .strip_prefix(PROVIDER_REFERENCE_PREFIX)
        .ok_or(AppError::Internal)?;
    let aad = provider_reference_aad(owner_kind, owner_id);
    let reference: ProviderAssetReference = open_private_json(envelope, key_material, &aad)?;
    Ok(reference.assets)
}

fn provider_reference_aad(owner_kind: &str, owner_id: uuid::Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        PROVIDER_REFERENCE_AAD.len() + owner_kind.len() + owner_id.as_bytes().len() + 2,
    );
    aad.extend_from_slice(PROVIDER_REFERENCE_AAD);
    aad.push(0);
    aad.extend_from_slice(owner_kind.as_bytes());
    aad.push(0);
    aad.extend_from_slice(owner_id.as_bytes());
    aad
}

/// Begins a non-secret, uniquely tracked staging attempt. The durable digest
/// deliberately covers only typed identities and random fencing material; it
/// never stores a hash of prompts, request bodies, source URLs, or assets.
pub(crate) async fn begin_generation_staging_attempt(
    state: &AppState,
    owner: ArchiveStagingOwner,
    purpose: ArchiveStagingPurpose,
    attempt_id: uuid::Uuid,
) -> Result<ArchiveStagingWriteLease, AppError> {
    let key = ArchiveStagingKey::new(owner, purpose, attempt_id)?;
    let lease_token = uuid::Uuid::now_v7();
    let lease_owner_id = uuid::Uuid::now_v7();
    let mut digest = Sha256::new();
    digest.update(b"memeloop-token-center/archive-staging-intent/v1\0");
    digest.update(owner.kind().as_bytes());
    digest.update(b"\0");
    digest.update(owner.id().as_bytes());
    digest.update(b"\0");
    digest.update(purpose.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(attempt_id.as_bytes());
    digest.update(b"\0");
    digest.update(lease_token.as_bytes());
    let intent_digest = ArchiveStagingIntentDigest::new(format!("{:x}", digest.finalize()))?;
    let result = state
        .db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest,
            lease_token,
            lease_owner: ArchiveStagingLeaseOwner::new(format!(
                "generation-staging-{lease_owner_id}"
            ))?,
        })
        .await?;
    match result {
        BeginArchiveStagingResult::Created(lease) | BeginArchiveStagingResult::Replayed(lease) => {
            Ok(lease)
        }
        BeginArchiveStagingResult::Existing(_) => Err(AppError::Conflict(
            "archive staging attempt is no longer writable".into(),
        )),
    }
}

async fn await_with_staging_heartbeat<F, T>(
    state: &AppState,
    lease: &mut ArchiveStagingWriteLease,
    future: F,
) -> Result<T, AppError>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(future);
    let period = std::time::Duration::from_millis(
        u64::try_from(ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS).map_err(|_| AppError::Internal)?,
    );
    let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = heartbeat.tick() => {
                if !state.db.heartbeat_archive_staging_write(lease).await? {
                    return Err(AppError::NotFound);
                }
            }
        }
    }
}

pub(crate) async fn write_generation_staging_bytes(
    state: &AppState,
    lease: &mut ArchiveStagingWriteLease,
    object_name: &str,
    bytes: bytes::Bytes,
) -> Result<StagedArchiveObject, AppError> {
    write_generation_staging_segments(state, lease, object_name, [bytes]).await
}

pub(crate) async fn write_generation_staging_segments(
    state: &AppState,
    lease: &mut ArchiveStagingWriteLease,
    object_name: &str,
    segments: impl IntoIterator<Item = bytes::Bytes>,
) -> Result<StagedArchiveObject, AppError> {
    if object_name.is_empty()
        || object_name.contains('/')
        || !object_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(AppError::BadRequest(
            "archive staging object name is invalid".into(),
        ));
    }
    let locator = format!("{}/{object_name}", lease.key.canonical_prefix());
    let mut writer = state.archive.start_writer(&locator).await?;
    for segment in segments {
        if let Err(error) =
            await_with_staging_heartbeat(state, lease, writer.write(segment)).await?
        {
            let _ = writer.abort().await;
            return Err(error);
        }
    }
    await_with_staging_heartbeat(state, lease, writer.finish_staged()).await?
}

/// Dispatch durably arms a quarantine before the provider POST. Cancellation
/// is not an attempt failure: leave the persisted fencing and reservation
/// untouched for recovery, especially after provider dispatch.
pub async fn process_one_until_shutdown(
    state: &AppState,
    worker_id: &str,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, AppError> {
    finish_attempt_until_shutdown(process_one(state, worker_id), shutdown).await
}

pub(crate) async fn finish_attempt_until_shutdown(
    attempt: impl std::future::Future<Output = Result<bool, AppError>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, AppError> {
    tokio::select! {
        biased;
        _ = crate::worker::wait_for_shutdown(&mut shutdown) => Ok(false),
        outcome = attempt => outcome,
    }
}

pub async fn process_one(state: &AppState, worker_id: &str) -> Result<bool, AppError> {
    let Some(job) = state.db.claim_generation_job(worker_id).await? else {
        return Ok(false);
    };
    // An outer error means the lease could no longer be renewed. In that case
    // the in-flight upstream future is dropped and this worker must not settle,
    // reschedule, or otherwise mutate a job that another worker may now own.
    let outcome = process_claimed_with_lease(state, worker_id, &job).await?;
    if let Err(error) = outcome {
        // A send, response-body, response-parsing, or ACK-persistence error
        // cannot prove non-delivery. Inspect the durable state, not the claimed
        // snapshot (which was queued before submit armed its guard). Never let
        // generic retry/exhaustion turn an unknown provider side effect into a
        // refund. This also covers a guard commit whose acknowledgement failed.
        if state
            .db
            .retain_generation_delivery_unknown(job.job_id, worker_id)
            .await?
        {
            tracing::warn!(job_id = %job.job_id, "generation delivery remains quarantined pending provider confirmation");
            return Ok(true);
        }
        let next_failure = job.failure_count.saturating_add(1);
        tracing::warn!(job_id = %job.job_id, attempt = job.attempt_count, failure = next_failure, %error, "generation job attempt failed");
        if job.status == "cancelling" {
            // Never refund an upstream-submitted task merely because its
            // cancellation endpoint is unavailable. Keep the fenced job and
            // reservation retryable until the provider confirms cancellation.
            let exponent = u32::try_from(next_failure.clamp(0, 6)).unwrap_or(6);
            let delay = 1_000_i64.saturating_mul(2_i64.saturating_pow(exponent));
            state
                .db
                .reschedule_generation_job(
                    job.job_id,
                    worker_id,
                    delay.min(60_000),
                    Some("upstream_cancel_retry"),
                )
                .await?;
        } else if next_failure >= MAX_FAILURES {
            terminal_failure(state, worker_id, &job, "retry_exhausted").await?;
        } else {
            let exponent = u32::try_from(next_failure.clamp(0, 6)).unwrap_or(6);
            let delay = 1_000_i64.saturating_mul(2_i64.saturating_pow(exponent));
            state
                .db
                .reschedule_generation_job(
                    job.job_id,
                    worker_id,
                    delay.min(60_000),
                    Some("upstream_retry"),
                )
                .await?;
        }
    }
    Ok(true)
}

async fn process_claimed_with_lease(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
) -> Result<Result<(), AppError>, AppError> {
    let attempt = process_claimed(state, worker_id, job);
    tokio::pin!(attempt);
    let period = std::time::Duration::from_secs(20);
    let start = tokio::time::Instant::now() + period;
    let mut heartbeat = tokio::time::interval_at(start, period);
    loop {
        tokio::select! {
            outcome = &mut attempt => return Ok(outcome),
            _ = heartbeat.tick() => {
                state.db.renew_generation_lease(job.job_id, worker_id).await?;
            }
        }
    }
}

pub fn generation_request_hash(model: &str, input: &Value) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"memeloop-token-center/generation-request/v1\0");
    hasher.update(model.as_bytes());
    hasher.update(b"\0");
    hasher.update(&serde_json::to_vec(input).expect("serializing a JSON value cannot fail"));
    hasher.finalize().to_hex().to_string()
}

pub(crate) fn comfyui_requested_pixels(input: &Value) -> Result<i64, AppError> {
    let (pixels_per_output, outputs) = comfyui_pixel_contract(input)?;
    pixels_per_output
        .checked_mul(outputs)
        .ok_or_else(|| AppError::BadRequest("ComfyUI pixel count is too large".into()))
}

fn comfyui_pixel_contract(input: &Value) -> Result<(i64, i64), AppError> {
    let parameters = input
        .get("parameters")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::BadRequest("ComfyUI parameters are required".into()))?;
    let dimension = |name: &str| -> Result<i64, AppError> {
        let value = parameters
            .get(name)
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "ComfyUI megapixel billing requires an integer {name} parameter"
                ))
            })?;
        if !(1..=32_768).contains(&value) {
            return Err(AppError::BadRequest(format!(
                "ComfyUI {name} must be between 1 and 32768"
            )));
        }
        Ok(value)
    };
    let width = dimension("width")?;
    let height = dimension("height")?;
    let outputs = ["batch_size", "n", "images"]
        .into_iter()
        .find_map(|name| parameters.get(name))
        .map(|value| {
            value.as_i64().ok_or_else(|| {
                AppError::BadRequest(
                    "ComfyUI output count must be an integer for megapixel billing".into(),
                )
            })
        })
        .transpose()?
        .unwrap_or(1);
    if !(1..=MAX_COMFY_ASSETS as i64).contains(&outputs) {
        return Err(AppError::BadRequest(
            "ComfyUI output count must be between 1 and 16".into(),
        ));
    }
    let pixels_per_output = width
        .checked_mul(height)
        .ok_or_else(|| AppError::BadRequest("ComfyUI pixel count is too large".into()))?;
    Ok((pixels_per_output, outputs))
}

async fn comfyui_billed_pixels(
    state: &AppState,
    job: &GenerationJobWork,
    actual_outputs: usize,
) -> Result<Option<i64>, AppError> {
    if job.billing_unit == "job" {
        return Ok(Some(1));
    }
    if job.billing_unit != "megapixel" {
        return Err(AppError::Storage(
            "ComfyUI job has an unsupported billing snapshot".into(),
        ));
    }
    let archived = state
        .archive
        .get_bounded(&job.request_object, MAX_CONTROL_BODY)
        .await?;
    let outer: Value = serde_json::from_slice(&archived)
        .map_err(|_| AppError::Storage("generation request archive is invalid".into()))?;
    let input = outer
        .get("input")
        .ok_or_else(|| AppError::Storage("generation input archive is invalid".into()))?;
    let (pixels_per_output, reserved_outputs) = comfyui_pixel_contract(input)
        .map_err(|_| AppError::Storage("generation pixel contract is invalid".into()))?;
    let actual_outputs = i64::try_from(actual_outputs).map_err(|_| AppError::Internal)?;
    if actual_outputs > reserved_outputs {
        return Ok(None);
    }
    pixels_per_output
        .checked_mul(actual_outputs)
        .map(Some)
        .ok_or(AppError::Internal)
}

async fn process_claimed(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
) -> Result<(), AppError> {
    let restored = group_routing::restore(state, job).await?;
    let state = &restored;
    // Manual proof of a previously unknown delivery starts one bounded polling
    // window. Preserve admission time and every ordinary job's timeout policy.
    let deadline = job
        .reconciliation_deadline_at
        .unwrap_or_else(|| job.created_at.saturating_add(MAX_JOB_AGE_MILLIS));
    if job.status != "cancelling" && unix_millis() > deadline {
        return terminal_failure(state, worker_id, job, "generation_timeout").await;
    }
    if job.status == "submitting"
        && !generation_driver_capabilities(state, &job.driver).provable_submit_idempotency
    {
        return terminal_failure(state, worker_id, job, "submission_outcome_unknown").await;
    }
    if let Some(staged) = job.staged_assets.as_ref() {
        if !generation_staged_manifest_is_well_formed(job, staged) {
            return terminal_failure(state, worker_id, job, "generation_staging_lost").await;
        }
        for asset in &staged.assets {
            let size = state.archive.head_size(&asset.object_locator).await?;
            if size != u64::try_from(asset.size_bytes).map_err(|_| AppError::Internal)? {
                return terminal_failure(state, worker_id, job, "generation_staging_lost").await;
            }
        }
        return terminal_success(state, worker_id, job, staged).await;
    }
    let route = state
        .db
        .load_generation_upstream_snapshot(job, state.config.key_pepper.as_bytes())
        .await?
        .ok_or_else(|| AppError::Upstream("generation upstream snapshot is unavailable".into()))?;
    if job.status == "cancelling" {
        let upstream_job_id = job.upstream_job_id.as_deref().ok_or(AppError::Internal)?;
        return cancel_upstream_generation(state, worker_id, job, &route, upstream_job_id).await;
    }
    match job.upstream_job_id.as_deref() {
        None => submit(state, worker_id, job, &route).await,
        Some(upstream_job_id) => poll(state, worker_id, job, &route, upstream_job_id).await,
    }
}

async fn cancel_upstream_generation(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
) -> Result<(), AppError> {
    let upstream_job_id = validated_upstream_job_id(upstream_job_id)?;
    let outbound_http = route_http(state, route, &route.base_url).await?;
    let adapter = GenerationProtocolAdapter::for_route(route)?;
    let (request, requires_delete_proof) = match adapter {
        GenerationProtocolAdapter::SeedanceV3 => {
            let url = generation_url(
                &route.base_url,
                &[
                    "api",
                    "v3",
                    "contents",
                    "generations",
                    "tasks",
                    upstream_job_id,
                ],
            )?;
            (outbound_http.delete(url), false)
        }
        GenerationProtocolAdapter::ComfyUi => {
            let prefix = comfy_prefix(route)?;
            if prefix == "/api" {
                let url =
                    generation_url(&route.base_url, &["api", "job", upstream_job_id, "cancel"])?;
                (outbound_http.post(url), false)
            } else {
                let url = generation_url(&route.base_url, &["queue"])?;
                // The classic ComfyUI API supports deleting one prompt from
                // its queue without the unsafe instance-wide /interrupt call.
                (
                    outbound_http
                        .post(url)
                        .json(&serde_json::json!({"delete": [upstream_job_id]})),
                    true,
                )
            }
        }
        GenerationProtocolAdapter::SiliconFlowVideoV1 => {
            return Err(AppError::Upstream("unsupported generation driver".into()));
        }
    };
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_cancel");
    let upstream_started = std::time::Instant::now();
    let response_result = route.credential.apply(request, unix_millis())?.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_cancel",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result
        .map_err(|error| sanitized_http_error(&error, "generation cancellation request"))?;
    let status = response.status();
    if !status.is_success() {
        drain_bounded_control_response(response).await?;
        return Err(AppError::Upstream(format!(
            "generation cancellation returned HTTP {}",
            status.as_u16()
        )));
    }
    if requires_delete_proof {
        let body = bounded_json(response).await?;
        if !comfyui_delete_confirmed(&body, upstream_job_id) {
            return Err(AppError::Upstream(
                "ComfyUI did not confirm the prompt deletion".into(),
            ));
        }
    } else {
        drain_bounded_control_response(response).await?;
    }
    // Only an explicit success proves cancellation. Treating a first 404 as
    // success could refund a task that actually completed and was later
    // purged by the provider, so absence remains retryable and keeps credit
    // reserved for operator reconciliation.
    terminal_cancelled(state, worker_id, job).await
}

fn comfyui_delete_confirmed(body: &Value, upstream_job_id: &str) -> bool {
    body.get("deleted").is_some_and(|value| match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64().is_some_and(|value| value > 0),
        Value::Array(values) => values
            .iter()
            .any(|value| value.as_str().is_some_and(|value| value == upstream_job_id)),
        _ => false,
    })
}

async fn drain_bounded_control_response(response: Response) -> Result<(), AppError> {
    let mut received = 0_usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| sanitized_http_error(&error, "generation cancellation response"))?;
        received = received.saturating_add(chunk.len());
        if received > MAX_CONTROL_BODY {
            return Err(AppError::Upstream(
                "generation cancellation response exceeds 4 MiB".into(),
            ));
        }
    }
    Ok(())
}

async fn submit(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
) -> Result<(), AppError> {
    let mut attempt = attempt::Attempt::new(state, worker_id, job, route);
    // Admission precedes even the durable submitting transition: an unsent
    // cooldown rejection must remain safely queued, never delivery-unknown.
    attempt.admit().await?;
    let result = submit_attempt(state, worker_id, job, route, &mut attempt).await;
    attempt.finish(result).await
}

async fn submit_attempt(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let capabilities = generation_driver_capabilities(state, &route.driver);
    let submission_nonce = if job.status == "submitting" {
        if !capabilities.provable_submit_idempotency {
            return terminal_failure(state, worker_id, job, "submission_outcome_unknown").await;
        }
        job.submission_nonce.ok_or(AppError::Internal)?
    } else {
        let nonce = uuid::Uuid::now_v7();
        state
            .db
            .mark_generation_submitting(job.job_id, worker_id, nonce)
            .await?;
        nonce
    };
    let archived = state
        .archive
        .get_bounded(&job.request_object, MAX_CONTROL_BODY)
        .await?;
    let outer: Value = serde_json::from_slice(&archived)
        .map_err(|_| AppError::Storage("generation request archive is invalid".into()))?;
    let mut input = outer
        .get("input")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| AppError::Storage("generation input archive is invalid".into()))?;
    let adapter = GenerationProtocolAdapter::for_route(route)?;
    let (path, id_field) = match adapter {
        GenerationProtocolAdapter::SeedanceV3 => {
            input.insert(
                "model".to_owned(),
                Value::String(job.upstream_model.clone()),
            );
            ("/api/v3/contents/generations/tasks".to_owned(), "id")
        }
        GenerationProtocolAdapter::ComfyUi => {
            let prefix = comfy_prefix(route)?;
            let workflow_id = route
                .config
                .get("workflow_id")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::Storage("ComfyUI workflow id is missing".into()))?;
            if workflow_id != job.upstream_model {
                return Err(AppError::Upstream(
                    "ComfyUI route does not match its versioned workflow".into(),
                ));
            }
            let parameters = input
                .remove("parameters")
                .and_then(|value| value.as_object().cloned())
                .ok_or_else(|| AppError::BadRequest("ComfyUI parameters are required".into()))?;
            if !input.is_empty()
                || parameters.len() > 100
                || parameters
                    .values()
                    .any(|value| value.is_array() || value.is_object())
            {
                return Err(AppError::BadRequest(
                    "ComfyUI accepts at most 100 scalar workflow parameters".into(),
                ));
            }
            validate_comfyui_parameters(&route.config, &Value::Object(parameters.clone()))?;
            let mut workflow = route
                .config
                .get("workflow_template")
                .cloned()
                .ok_or_else(|| AppError::Storage("ComfyUI workflow template is missing".into()))?;
            apply_workflow_parameters(&mut workflow, &parameters)?;
            input = Map::from_iter([("prompt".to_owned(), workflow)]);
            (format!("{prefix}/prompt"), "prompt_id")
        }
        GenerationProtocolAdapter::SiliconFlowVideoV1 => {
            let mut parameters = validate_siliconflow_video_parameters(&Value::Object(input))?;
            parameters.insert(
                "model".to_owned(),
                Value::String(job.upstream_model.clone()),
            );
            input = parameters;
            ("/video/submit".to_owned(), "requestId")
        }
    };
    let outbound_http = route_http(state, route, &route.base_url).await?;
    let request = outbound_http
        .post(format!("{}{}", route.base_url, path))
        .header("idempotency-key", job.job_id.to_string())
        .json(&input);
    let request = route.credential.apply(request, unix_millis())?;
    // Commit before send, including before its first poll, for both supervised
    // and single-attempt workers. Only a durable provider ACK or an explicit
    // provider rejection may clear this guard; transport/response/ACK errors
    // and cancellation retain it for evidence-bearing reconciliation.
    state
        .db
        .arm_generation_shutdown_quarantine(job.job_id, worker_id, submission_nonce)
        .await?;
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_submit");
    let upstream_started = std::time::Instant::now();
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_submit",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result.map_err(|error| {
        attempt.transport_error(&error);
        sanitized_http_error(&error, "generation submit request")
    })?;
    let status = response.status();
    let body = attempt.json(response).await;
    if !status.is_success() {
        if status.is_client_error() && !matches!(status.as_u16(), 408 | 425 | 429) {
            // Consume the bounded response but never persist the provider envelope: it may
            // contain signed URLs, credentials, or internal paths.
            let _ = body;
            return terminal_failure(state, worker_id, job, "generation_rejected").await;
        }
        return Err(AppError::Upstream(format!(
            "generation submit returned HTTP {}",
            status.as_u16()
        )));
    }
    let body = body?;
    let upstream_job_id = body
        .get(id_field)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Upstream("generation submit response has no job id".into()))?;
    let upstream_job_id = validated_upstream_job_id(upstream_job_id)?;
    attempt.valid = true;
    state
        .db
        .mark_generation_submitted(job.job_id, worker_id, submission_nonce, upstream_job_id)
        .await
}

#[derive(Clone, Copy, Debug)]
struct GenerationDriverCapabilities {
    provable_submit_idempotency: bool,
}

fn generation_driver_capabilities(state: &AppState, driver: &str) -> GenerationDriverCapabilities {
    // Provider types, including plugin contributions, must explicitly declare
    // generation guarantees. Unknown versions and omitted capabilities fail closed.
    let provable_submit_idempotency = state
        .providers
        .get(driver)
        .and_then(|provider| provider.generation_adapter.as_ref())
        .filter(|adapter| adapter.api_version == "generation-adapter-v1")
        .is_some_and(|adapter| adapter.provable_submit_idempotency);
    GenerationDriverCapabilities {
        provable_submit_idempotency,
    }
}

fn provider_asset_reads_repeatable(state: &AppState, route: &ResolvedUpstream) -> bool {
    let Some(provider_default) = state
        .providers
        .get(&route.driver)
        .and_then(|provider| provider.generation_adapter.as_ref())
        .filter(|adapter| adapter.api_version == "generation-adapter-v1")
        .map(|adapter| adapter.provider_asset_reads_repeatable)
    else {
        return false;
    };
    route
        .config
        .get("provider_asset_reads_repeatable")
        .and_then(Value::as_bool)
        .unwrap_or(provider_default)
}

fn apply_workflow_parameters(
    value: &mut Value,
    parameters: &Map<String, Value>,
) -> Result<(), AppError> {
    match value {
        Value::Array(values) => {
            for value in values {
                apply_workflow_parameters(value, parameters)?;
            }
        }
        Value::Object(object) => {
            if object.len() == 1
                && let Some(name) = object.get("$mtc_param").and_then(Value::as_str)
            {
                *value = parameters.get(name).cloned().ok_or_else(|| {
                    AppError::BadRequest(format!("missing ComfyUI workflow parameter: {name}"))
                })?;
                return Ok(());
            }
            for value in object.values_mut() {
                apply_workflow_parameters(value, parameters)?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn poll(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
) -> Result<(), AppError> {
    let mut attempt = attempt::Attempt::new(state, worker_id, job, route);
    let result = match GenerationProtocolAdapter::for_route(route)? {
        GenerationProtocolAdapter::SeedanceV3 => {
            poll_seedance(state, worker_id, job, route, upstream_job_id, &mut attempt).await
        }
        GenerationProtocolAdapter::ComfyUi => {
            poll_comfy(state, worker_id, job, route, upstream_job_id, &mut attempt).await
        }
        GenerationProtocolAdapter::SiliconFlowVideoV1 => {
            poll_siliconflow_video(state, worker_id, job, route, upstream_job_id, &mut attempt)
                .await
        }
    };
    attempt.finish(result).await
}

async fn poll_siliconflow_video(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let poll_url = generation_url(&route.base_url, &["video", "status"])?;
    let outbound_http = route_http(state, route, &poll_url).await?;
    let request = outbound_http
        .post(poll_url)
        .json(&serde_json::json!({"requestId": upstream_job_id}));
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_poll");
    let upstream_started = std::time::Instant::now();
    let request = route.credential.apply(request, unix_millis())?;
    attempt.admit().await?;
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_poll",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result.map_err(|error| {
        attempt.transport_error(&error);
        sanitized_http_error(&error, "SiliconFlow video poll request")
    })?;
    let status = response.status();
    let body = attempt.json(response).await?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "SiliconFlow video poll returned HTTP {}",
            status.as_u16()
        )));
    }
    match body.get("status").and_then(Value::as_str) {
        Some("InQueue" | "InProgress") => {
            attempt.valid = true;
            state
                .db
                .reschedule_generation_job(
                    job.job_id,
                    worker_id,
                    siliconflow_poll_delay_millis(job.attempt_count),
                    None,
                )
                .await
        }
        Some("Failed") => {
            attempt.valid = true;
            terminal_failure(state, worker_id, job, "siliconflow_video_failed").await
        }
        Some("Succeed") => {
            let Some(videos) = body.pointer("/results/videos").and_then(Value::as_array) else {
                return terminal_failure(state, worker_id, job, "siliconflow_video_missing_asset")
                    .await;
            };
            if videos.len() > MAX_SILICONFLOW_VIDEO_ASSETS {
                return terminal_failure(
                    state,
                    worker_id,
                    job,
                    "siliconflow_video_asset_limit_exceeded",
                )
                .await;
            }
            let Some(video) = videos.first() else {
                return terminal_failure(state, worker_id, job, "siliconflow_video_missing_asset")
                    .await;
            };
            let Some(video_url) = video.get("url").and_then(Value::as_str) else {
                return terminal_failure(state, worker_id, job, "siliconflow_video_missing_asset")
                    .await;
            };
            attempt.envelope_valid();
            if ensure_asset_origin(route, video_url).is_err() {
                attempt.invalid_response();
                return terminal_failure(state, worker_id, job, "siliconflow_video_invalid_asset")
                    .await;
            }
            attempt.valid = true;
            terminal_provider_success(
                state,
                worker_id,
                job,
                route,
                1,
                vec![provider_generation_asset(
                    job.job_id,
                    0,
                    video_url,
                    provider_expiry_millis(video, video_url),
                    Some("video.mp4"),
                )?],
            )
            .await
        }
        Some(_) | None => Err(unknown_generation_status("siliconflow-video")),
    }
}

fn siliconflow_poll_delay_millis(attempt_count: i64) -> i64 {
    let exponent =
        u32::try_from(attempt_count.saturating_sub(1).div_euclid(5).clamp(0, 4)).unwrap_or(4);
    2_000_i64.saturating_mul(2_i64.saturating_pow(exponent))
}

async fn poll_seedance(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let poll_url = generation_url(
        &route.base_url,
        &[
            "api",
            "v3",
            "contents",
            "generations",
            "tasks",
            upstream_job_id,
        ],
    )?;
    let outbound_http = route_http(state, route, &poll_url).await?;
    let request = outbound_http.get(poll_url);
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_poll");
    let upstream_started = std::time::Instant::now();
    let request = route.credential.apply(request, unix_millis())?;
    attempt.admit().await?;
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_poll",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result.map_err(|error| {
        attempt.transport_error(&error);
        sanitized_http_error(&error, "Seedance poll request")
    })?;
    let status = response.status();
    let body = attempt.json(response).await?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "Seedance poll returned HTTP {}",
            status.as_u16()
        )));
    }
    match body.get("status").and_then(Value::as_str) {
        Some("queued" | "running") | None => {
            attempt.valid = body.get("status").and_then(Value::as_str).is_some();
            state
                .db
                .reschedule_generation_job(job.job_id, worker_id, 2_000, None)
                .await
        }
        Some("failed" | "cancelled") => {
            attempt.valid = true;
            terminal_failure(state, worker_id, job, "seedance_generation_failed").await
        }
        Some("succeeded") => {
            let billed_units = match body.get("duration") {
                None => job.estimated_units,
                Some(duration) => match json_i64(duration) {
                    Some(duration) if (1..=job.estimated_units).contains(&duration) => duration,
                    _ => {
                        return terminal_failure_billed(
                            state,
                            worker_id,
                            job,
                            "upstream_usage_exceeds_contract",
                            job.estimated_units,
                        )
                        .await;
                    }
                },
            };
            let Some(content) = body.get("content") else {
                return terminal_failure(state, worker_id, job, "seedance_missing_asset").await;
            };
            let Some(video_url) = content.get("video_url").and_then(Value::as_str) else {
                return terminal_failure(state, worker_id, job, "seedance_missing_asset").await;
            };
            attempt.envelope_valid();
            if ensure_asset_origin(route, video_url).is_err() {
                attempt.invalid_response();
                return terminal_failure(state, worker_id, job, "seedance_invalid_asset").await;
            }
            attempt.valid = true;
            terminal_provider_success(
                state,
                worker_id,
                job,
                route,
                billed_units,
                vec![provider_generation_asset(
                    job.job_id,
                    0,
                    video_url,
                    provider_expiry_millis(content, video_url),
                    Some("video.mp4"),
                )?],
            )
            .await
        }
        Some(_) => Err(unknown_generation_status("volcengine-seedance")),
    }
}

async fn poll_comfy(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let prefix = comfy_prefix(route)?;
    if prefix == "/api" {
        let status_body = authenticated_json(
            state,
            route,
            generation_url(&route.base_url, &["api", "job", upstream_job_id, "status"])?,
            attempt,
        )
        .await?;
        match status_body.get("status").and_then(Value::as_str) {
            Some("pending" | "in_progress") | None => {
                attempt.valid = status_body.get("status").and_then(Value::as_str).is_some();
                return state
                    .db
                    .reschedule_generation_job(job.job_id, worker_id, 2_000, None)
                    .await;
            }
            Some("failed" | "cancelled") => {
                attempt.valid = true;
                return terminal_failure(state, worker_id, job, "comfyui_failed").await;
            }
            Some("completed") => {
                attempt.next_http().await?;
            }
            Some(_) => {
                return Err(unknown_generation_status("comfyui"));
            }
        }
    }
    let history_path = if prefix == "/api" {
        generation_url(&route.base_url, &["api", "history", upstream_job_id])?
    } else {
        generation_url(&route.base_url, &["history", upstream_job_id])?
    };
    let history = authenticated_json(state, route, history_path, attempt).await?;
    let entry = if let Some(entry) = history.get(upstream_job_id) {
        entry.clone()
    } else if prefix == "/api" {
        history.clone()
    } else {
        attempt.valid = history.is_object();
        return state
            .db
            .reschedule_generation_job(job.job_id, worker_id, 2_000, None)
            .await;
    };
    if entry
        .pointer("/status/status_str")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "error")
    {
        attempt.valid = true;
        return terminal_failure(state, worker_id, job, "comfyui_execution_error").await;
    }
    let mut assets = Vec::new();
    find_comfy_assets(&entry, &mut assets);
    if assets.is_empty() {
        return terminal_failure(state, worker_id, job, "comfyui_missing_assets").await;
    }
    if assets.len() > MAX_COMFY_ASSETS {
        return terminal_failure(state, worker_id, job, "comfyui_asset_limit_exceeded").await;
    }
    attempt.envelope_valid();
    let billed_units = match comfyui_billed_pixels(state, job, assets.len()).await? {
        Some(units) if units > 0 && units <= job.estimated_units => units,
        _ => {
            attempt.invalid_response();
            return terminal_failure_billed(
                state,
                worker_id,
                job,
                "upstream_usage_exceeds_contract",
                job.estimated_units,
            )
            .await;
        }
    };
    let mut provider_assets = Vec::new();
    for (index, asset) in assets.into_iter().enumerate() {
        let url = comfy_asset_url(route, &prefix, &asset)?;
        ensure_asset_origin(route, url.as_str())?;
        provider_assets.push(provider_generation_asset(
            job.job_id,
            index,
            url.as_str(),
            None,
            Some(&asset.filename),
        )?);
    }
    attempt.valid = true;
    terminal_provider_success(state, worker_id, job, route, billed_units, provider_assets).await
}

async fn authenticated_json(
    state: &AppState,
    route: &ResolvedUpstream,
    url: String,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<Value, AppError> {
    let outbound_http = route_http(state, route, &url).await?;
    let request = route
        .credential
        .apply(outbound_http.get(url), unix_millis())?;
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_poll");
    let upstream_started = std::time::Instant::now();
    attempt.admit().await?;
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_poll",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result.map_err(|error| {
        attempt.transport_error(&error);
        sanitized_http_error(&error, "generation poll request")
    })?;
    let status = response.status();
    let body = attempt.json(response).await?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(AppError::Upstream(format!(
            "generation poll returned HTTP {}",
            status.as_u16()
        )))
    }
}

async fn bounded_json(response: Response) -> Result<Value, AppError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| sanitized_http_error(&error, "generation control response"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_CONTROL_BODY {
            return Err(AppError::Upstream(
                "generation control response exceeds 4 MiB".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| AppError::Upstream("generation control response is not JSON".into()))
}

pub(crate) async fn route_http(
    state: &AppState,
    route: &ResolvedUpstream,
    url: &str,
) -> Result<reqwest::Client, AppError> {
    network::client_for_config_url(
        &state.http,
        url,
        &route.config,
        route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
}

pub(crate) async fn provider_asset_get(
    state: &AppState,
    route: &ResolvedUpstream,
    url: &str,
) -> Result<reqwest::RequestBuilder, AppError> {
    ensure_asset_origin(route, url)?;
    let client = route_http(state, route, url).await?;
    let asset_url = url::Url::parse(url)
        .map_err(|_| AppError::Upstream("generation asset URL is invalid".into()))?;
    let base_url = url::Url::parse(&route.base_url).map_err(|_| AppError::Internal)?;
    let request = client.get(url);
    if asset_url.origin() == base_url.origin() {
        route.credential.apply(request, unix_millis())
    } else {
        Ok(request)
    }
}

pub(crate) async fn provider_asset_is_obtainable(
    state: &AppState,
    route: &ResolvedUpstream,
    asset: &ProviderGenerationAsset,
) -> Result<bool, AppError> {
    if asset
        .expires_at
        .is_some_and(|expires_at| expires_at <= unix_millis())
    {
        return Ok(false);
    }
    // A validation GET must never consume the only deliverable copy. Providers
    // without an explicit repeatable-read guarantee fail settlement uncharged.
    if !provider_asset_reads_repeatable(state, route) {
        return Ok(false);
    }
    let request = provider_asset_get(state, route, &asset.url)
        .await?
        .header(reqwest::header::RANGE, "bytes=0-0");
    let _upstream_activity = state
        .metrics
        .active_upstream(&route.driver, "generation_asset_probe");
    let started = std::time::Instant::now();
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_asset_probe",
        response_result.as_ref().ok().map(reqwest::Response::status),
        started.elapsed(),
    );
    let response = response_result
        .map_err(|error| sanitized_http_error(&error, "generation asset validation request"))?;
    if matches!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::GONE
            | reqwest::StatusCode::RANGE_NOT_SATISFIABLE
    ) {
        return Ok(false);
    }
    if response.status() == reqwest::StatusCode::OK {
        return Ok(false);
    }
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(AppError::Upstream(
            "generation asset validation did not honor the bounded byte range".into(),
        ));
    }
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_satisfied_content_range);
    if !content_range.is_some_and(|range| range.start == 0 && range.end == 0 && range.total > 0)
        || response.content_length() != Some(1)
    {
        return Ok(false);
    }
    let mut body = response.bytes_stream();
    let mut bytes = 0_u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            sanitized_http_error(&error, "generation asset validation response")
        })?;
        bytes = bytes
            .checked_add(u64::try_from(chunk.len()).map_err(|_| AppError::Internal)?)
            .ok_or(AppError::Internal)?;
        if bytes > 1 {
            return Ok(false);
        }
    }
    Ok(bytes == 1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SatisfiedContentRange {
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) total: u64,
}

pub(crate) fn parse_satisfied_content_range(value: &str) -> Option<SatisfiedContentRange> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    (total > 0 && start <= end && end < total).then_some(SatisfiedContentRange {
        start,
        end,
        total,
    })
}

pub(crate) fn parse_unsatisfied_content_range(value: &str) -> Option<u64> {
    value.strip_prefix("bytes */")?.parse::<u64>().ok()
}

fn sanitized_http_error(error: &reqwest::Error, operation: &'static str) -> AppError {
    // reqwest error displays may contain the complete URL. Generation asset
    // URLs are commonly signed, so log only non-secret classifications and
    // return an operation label that is safe for the worker retry log.
    tracing::warn!(
        operation,
        is_timeout = error.is_timeout(),
        is_connect = error.is_connect(),
        "generation upstream HTTP operation failed"
    );
    AppError::Upstream(format!("{operation} failed"))
}

pub(crate) fn ensure_asset_origin(route: &ResolvedUpstream, asset: &str) -> Result<(), AppError> {
    let asset = url::Url::parse(asset)
        .map_err(|_| AppError::Upstream("generation asset URL is invalid".into()))?;
    let base = url::Url::parse(&route.base_url).map_err(|_| AppError::Internal)?;
    let asset_origin = asset.origin().ascii_serialization();
    let base_origin = base.origin().ascii_serialization();
    let configured = route
        .config
        .get("result_origins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    if asset_origin == base_origin || configured.into_iter().any(|origin| origin == asset_origin) {
        Ok(())
    } else {
        Err(AppError::Upstream(format!(
            "generation asset origin is not allowed: {asset_origin}"
        )))
    }
}

async fn terminal_success(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    staged: &GenerationStagedAssets,
) -> Result<(), AppError> {
    state
        .db
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id,
            status: "succeeded",
            billed_units: staged.billed_units,
            error_code: None,
            assets: &staged.assets,
            staged_assets: Some(staged),
        })
        .await
        .map(|_| ())
}

async fn terminal_provider_success(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    billed_units: i64,
    assets: Vec<ProviderGenerationAsset>,
) -> Result<(), AppError> {
    for asset in &assets {
        if !provider_asset_is_obtainable(state, route, asset).await? {
            return terminal_failure(state, worker_id, job, "generation_asset_unavailable").await;
        }
    }
    state
        .db
        .finish_generation_job_with_provider_assets(
            FinishGenerationJobInput {
                job_id: job.job_id,
                worker_id,
                status: "succeeded",
                billed_units,
                error_code: None,
                assets: &[],
                staged_assets: None,
            },
            &assets,
            state.config.key_pepper.as_bytes(),
        )
        .await
        .map(|_| ())
}

pub(crate) fn provider_generation_asset(
    job_id: uuid::Uuid,
    index: usize,
    url: &str,
    expires_at: Option<i64>,
    filename: Option<&str>,
) -> Result<ProviderGenerationAsset, AppError> {
    let inferred_filename = url::Url::parse(url)
        .ok()
        .and_then(|url| url.path_segments()?.next_back().map(str::to_owned));
    let mime_type = provider_asset_mime(filename.or(inferred_filename.as_deref()));
    let mut digest = blake3::Hasher::new();
    digest.update(job_id.as_bytes());
    digest.update(&index.to_be_bytes());
    digest.update(url.as_bytes());
    let mut asset_id = [0_u8; 16];
    asset_id.copy_from_slice(&digest.finalize().as_bytes()[..16]);
    asset_id[6] = (asset_id[6] & 0x0f) | 0x80;
    asset_id[8] = (asset_id[8] & 0x3f) | 0x80;
    Ok(ProviderGenerationAsset {
        asset_id: uuid::Uuid::from_bytes(asset_id),
        index: i64::try_from(index).map_err(|_| AppError::Internal)?,
        url: url.to_owned(),
        expires_at,
        filename: safe_asset_filename(filename, index, &mime_type),
        mime_type,
    })
}

fn provider_asset_mime(filename: Option<&str>) -> String {
    let filename = filename.unwrap_or_default().to_ascii_lowercase();
    if filename.ends_with(".png") {
        "image/png"
    } else if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
        "image/jpeg"
    } else if filename.ends_with(".webp") {
        "image/webp"
    } else if filename.ends_with(".gif") {
        "image/gif"
    } else if filename.ends_with(".webm") {
        "video/webm"
    } else if filename.ends_with(".mov") {
        "video/quicktime"
    } else if filename.ends_with(".mp4") {
        "video/mp4"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}

pub(crate) fn provider_expiry_millis(value: &Value, url: &str) -> Option<i64> {
    for key in [
        "expires_at",
        "expiresAt",
        "expire_time",
        "expireTime",
        "expiration",
    ] {
        let raw = value
            .get(key)
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()));
        if let Some(raw) = raw.filter(|raw| *raw > 0) {
            return Some(if raw < 10_000_000_000 {
                raw.saturating_mul(1_000)
            } else {
                raw
            });
        }
    }
    let parsed = url::Url::parse(url).ok()?;
    parsed.query_pairs().find_map(|(key, value)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "expires" | "expires_at" | "x-oss-expires"
        )
        .then(|| value.parse::<i64>().ok())
        .flatten()
        .filter(|value| *value > 0)
        .map(|value| {
            if value < 10_000_000_000 {
                value.saturating_mul(1_000)
            } else {
                value
            }
        })
    })
}

async fn terminal_failure(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    error_code: &str,
) -> Result<(), AppError> {
    terminal_failure_billed(state, worker_id, job, error_code, 0).await
}

async fn terminal_cancelled(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
) -> Result<(), AppError> {
    state
        .db
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id,
            status: "cancelled",
            billed_units: 0,
            error_code: Some("cancelled_by_user"),
            assets: &[],
            staged_assets: None,
        })
        .await
        .map(|_| ())
}

async fn terminal_failure_billed(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    error_code: &str,
    billed_units: i64,
) -> Result<(), AppError> {
    state
        .db
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id,
            status: "failed",
            billed_units,
            error_code: Some(error_code),
            assets: &[],
            staged_assets: job.staged_assets.as_ref(),
        })
        .await
        .map(|_| ())
}

fn generation_staging_prefix(job_id: uuid::Uuid, attempt_nonce: uuid::Uuid) -> String {
    format!("staging/generation/{job_id}/assets/{attempt_nonce}")
}

fn generation_staged_manifest_is_well_formed(
    job: &GenerationJobWork,
    staged: &crate::model::GenerationStagedAssets,
) -> bool {
    if staged.billed_units <= 0
        || staged.billed_units > job.estimated_units
        || staged.assets.is_empty()
        || match job.driver.as_str() {
            "volcengine-seedance" => {
                staged.assets.len() != 1 || !staged.assets[0].mime_type.starts_with("video/")
            }
            "comfyui" => !(1..=MAX_COMFY_ASSETS).contains(&staged.assets.len()),
            "http-json" => {
                staged.assets.len() != 1 || !staged.assets[0].mime_type.starts_with("video/")
            }
            _ => true,
        }
    {
        return false;
    }
    if !generation_assets_fit_archive_budget(&staged.assets) {
        return false;
    }
    let prefix = generation_staging_prefix(job.job_id, staged.attempt_nonce);
    staged.assets.iter().enumerate().all(|(position, asset)| {
        asset.index == i64::try_from(position).unwrap_or(-1)
            && asset.size_bytes > 0
            && asset.object_locator == format!("{prefix}/asset-{position}")
    })
}

fn generation_assets_fit_archive_budget(assets: &[ArchivedGenerationAsset]) -> bool {
    assets
        .iter()
        .try_fold(0_u64, |total, asset| {
            let size = u64::try_from(asset.size_bytes).ok()?;
            total.checked_add(size)
        })
        .is_some_and(|total| total <= MAX_ASSET_BODY as u64)
}

#[cfg(test)]
fn valid_generation_staging_prefix(value: &str) -> bool {
    value.starts_with("staging/")
        && !value.ends_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
}

fn safe_asset_filename(value: Option<&str>, index: usize, mime_type: &str) -> String {
    let extension = match mime_type {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        _ => ".bin",
    };
    let candidate = value
        .and_then(|value| value.rsplit(['/', '\\']).next())
        .unwrap_or("");
    let mut safe = candidate
        .chars()
        .take(120)
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '.' | '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    if safe.is_empty() || safe == "." || safe == ".." {
        safe = format!("asset-{index}{extension}");
    }
    safe
}

fn comfy_prefix(route: &ResolvedUpstream) -> Result<String, AppError> {
    let prefix = route
        .config
        .get("api_prefix")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(prefix, "" | "/api") {
        Ok(prefix.to_owned())
    } else {
        Err(AppError::Upstream(
            "ComfyUI api_prefix must be empty or /api".into(),
        ))
    }
}

fn validated_upstream_job_id(value: &str) -> Result<&str, AppError> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(AppError::Upstream(
            "generation submit response has invalid job id".into(),
        ));
    }
    Ok(value)
}

fn unknown_generation_status(driver: &str) -> AppError {
    let provider = match driver {
        "volcengine-seedance" => "Seedance",
        "comfyui" => "ComfyUI",
        "siliconflow-video" => "SiliconFlow video",
        _ => "upstream",
    };
    AppError::Upstream(format!("unknown {provider} generation status"))
}

fn generation_url(base_url: &str, segments: &[&str]) -> Result<String, AppError> {
    let mut url = url::Url::parse(base_url).map_err(|_| AppError::Internal)?;
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url
        .path_segments_mut()
        .map_err(|_| AppError::Upstream("generation upstream URL cannot be a base URL".into()))?;
    path.pop_if_empty();
    path.extend(segments);
    drop(path);
    Ok(url.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComfyAsset {
    filename: String,
    subfolder: String,
    kind: String,
}

fn find_comfy_assets(value: &Value, output: &mut Vec<ComfyAsset>) {
    // Retain one overflow sentinel so a terminal response cannot silently truncate assets and
    // still be billed as a complete success.
    if output.len() > MAX_COMFY_ASSETS {
        return;
    }
    match value {
        Value::Object(object) => {
            if let Some(filename) = object.get("filename").and_then(Value::as_str) {
                output.push(ComfyAsset {
                    filename: filename.to_owned(),
                    subfolder: object
                        .get("subfolder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    kind: object
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("output")
                        .to_owned(),
                });
                return;
            }
            for child in object.values() {
                find_comfy_assets(child, output);
            }
        }
        Value::Array(array) => {
            for child in array {
                find_comfy_assets(child, output);
            }
        }
        _ => {}
    }
}

fn comfy_asset_url(
    route: &ResolvedUpstream,
    prefix: &str,
    asset: &ComfyAsset,
) -> Result<url::Url, AppError> {
    let mut url = url::Url::parse(&format!("{}{prefix}/view", route.base_url))
        .map_err(|_| AppError::Internal)?;
    url.query_pairs_mut()
        .append_pair("filename", &asset.filename)
        .append_pair("subfolder", &asset.subfolder)
        .append_pair("type", &asset.kind);
    Ok(url)
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_comfy_assets_without_treating_arbitrary_strings_as_paths() {
        let input = json!({
            "outputs": {
                "9": {
                    "images": [
                        {"filename": "result.png", "subfolder": "daily", "type": "output"}
                    ],
                    "text": ["do not archive me"]
                }
            }
        });
        let mut assets = Vec::new();
        find_comfy_assets(&input, &mut assets);
        assert_eq!(
            assets,
            vec![ComfyAsset {
                filename: "result.png".to_owned(),
                subfolder: "daily".to_owned(),
                kind: "output".to_owned()
            }]
        );
    }

    #[test]
    fn generation_request_hash_is_stable_for_json_object_key_order() {
        assert_eq!(
            generation_request_hash("image-test", &json!({"prompt": "cat", "count": 1})),
            generation_request_hash("image-test", &json!({"count": 1, "prompt": "cat"})),
        );
        assert_ne!(
            generation_request_hash("image-test", &json!({"prompt": "cat"})),
            generation_request_hash("image-test", &json!({"prompt": "dog"})),
        );
    }

    #[test]
    fn siliconflow_normal_polling_uses_bounded_exponential_backoff() {
        assert_eq!(siliconflow_poll_delay_millis(1), 2_000);
        assert_eq!(siliconflow_poll_delay_millis(5), 2_000);
        assert_eq!(siliconflow_poll_delay_millis(6), 4_000);
        assert_eq!(siliconflow_poll_delay_millis(11), 8_000);
        assert_eq!(siliconflow_poll_delay_millis(21), 32_000);
        assert_eq!(siliconflow_poll_delay_millis(i64::MAX), 32_000);
    }

    #[test]
    fn comfyui_megapixel_contract_reserves_exact_pixels_and_bounds_outputs() {
        assert_eq!(
            comfyui_requested_pixels(&json!({
                "parameters": {"width": 1024, "height": 768, "batch_size": 3}
            }))
            .unwrap(),
            2_359_296
        );
        assert_eq!(
            comfyui_requested_pixels(&json!({
                "parameters": {"width": 512, "height": 512}
            }))
            .unwrap(),
            262_144
        );
        for invalid in [
            json!({"parameters": {"height": 512}}),
            json!({"parameters": {"width": 512, "height": 0}}),
            json!({"parameters": {"width": 512, "height": 512, "n": 17}}),
            json!({"parameters": {"width": 512.5, "height": 512}}),
        ] {
            assert!(comfyui_requested_pixels(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn classic_comfyui_cancellation_requires_explicit_prompt_deletion_proof() {
        assert!(comfyui_delete_confirmed(
            &json!({"deleted": ["prompt-1"]}),
            "prompt-1"
        ));
        assert!(comfyui_delete_confirmed(&json!({"deleted": 1}), "prompt-1"));
        assert!(!comfyui_delete_confirmed(&json!({}), "prompt-1"));
        assert!(!comfyui_delete_confirmed(
            &json!({"deleted": ["another-prompt"]}),
            "prompt-1"
        ));
    }

    #[test]
    fn generation_asset_filename_does_not_trust_provider_input() {
        assert_eq!(
            safe_asset_filename(Some("../../evil\r\n\".png"), 4, "image/png"),
            "evil___.png"
        );
        assert_eq!(
            safe_asset_filename(Some(".."), 2, "video/mp4"),
            "asset-2.mp4"
        );
        assert_eq!(safe_asset_filename(None, 0, "image/png"), "asset-0.png");
        assert!(!safe_asset_filename(None, 0, "image/png").contains("SECRET_TOKEN"));
    }

    #[test]
    fn provider_content_ranges_are_strict_and_numeric() {
        assert_eq!(
            parse_satisfied_content_range("bytes 2-5/13"),
            Some(SatisfiedContentRange {
                start: 2,
                end: 5,
                total: 13,
            })
        );
        for invalid in [
            "bytes 5-2/13",
            "bytes 2-13/13",
            "bytes 2-5/*",
            "bytes */13",
            "items 2-5/13",
        ] {
            assert_eq!(parse_satisfied_content_range(invalid), None, "{invalid}");
        }
        assert_eq!(parse_unsatisfied_content_range("bytes */13"), Some(13));
        assert_eq!(parse_unsatisfied_content_range("bytes 0-0/13"), None);
    }

    #[test]
    fn generation_staging_prefix_rejects_traversal_and_ambiguous_segments() {
        assert!(valid_generation_staging_prefix(
            "staging/synchronous/019fffff-ffff-7fff-bfff-ffffffffffff"
        ));
        for invalid in [
            "objects/blake3/aa/digest",
            "staging/",
            "staging//asset",
            "staging/./asset",
            "staging/../asset",
            "staging\\asset",
            "staging/request/",
        ] {
            assert!(!valid_generation_staging_prefix(invalid), "{invalid}");
        }
    }

    #[test]
    fn historical_staged_assets_remain_bounded_during_recovery() {
        let request_id = uuid::Uuid::now_v7();
        let assets = (0..16)
            .map(|index| ArchivedGenerationAsset {
                asset_id: uuid::Uuid::now_v7(),
                index,
                object_locator: format!("staging/synchronous/{request_id}/asset-{index}"),
                mime_type: "image/png".to_owned(),
                size_bytes: if index == 15 {
                    i64::try_from(MAX_ASSET_BODY).unwrap()
                } else {
                    1
                },
                filename: format!("asset-{index}.png"),
            })
            .collect::<Vec<_>>();
        assert!(!generation_assets_fit_archive_budget(&assets));
    }

    #[test]
    fn upstream_job_ids_are_bounded_opaque_segments_and_never_echoed_on_rejection() {
        assert_eq!(
            validated_upstream_job_id("job_01H-test:2").unwrap(),
            "job_01H-test:2"
        );
        for invalid in [
            String::new(),
            ".".to_owned(),
            "..".to_owned(),
            "../private?token=must-not-leak".to_owned(),
            "job/child".to_owned(),
            "x".repeat(257),
        ] {
            let error = validated_upstream_job_id(&invalid).unwrap_err().to_string();
            assert_eq!(
                error,
                "configured upstream is unavailable: generation submit response has invalid job id"
            );
            assert!(!error.contains("must-not-leak"));
        }

        let url = generation_url(
            "https://provider.example/base",
            &["history", "job?token=must-not-leak"],
        )
        .unwrap();
        assert_eq!(
            url,
            "https://provider.example/base/history/job%3Ftoken=must-not-leak"
        );
        assert!(url::Url::parse(&url).unwrap().query().is_none());
        assert_eq!(
            generation_url(
                "https://provider.example/base",
                &["api", "v3", "contents", "generations", "tasks", "opaque.1"],
            )
            .unwrap(),
            "https://provider.example/base/api/v3/contents/generations/tasks/opaque.1"
        );
        assert_eq!(
            generation_url("https://provider.example/base", &["history", "opaque.1"],).unwrap(),
            "https://provider.example/base/history/opaque.1"
        );

        let unknown = unknown_generation_status("volcengine-seedance").to_string();
        assert_eq!(
            unknown,
            "configured upstream is unavailable: unknown Seedance generation status"
        );
        let unknown_provider =
            unknown_generation_status("provider-status-with-sensitive-token").to_string();
        assert!(!unknown_provider.contains("provider-status-with-sensitive-token"));
    }

    #[tokio::test]
    async fn signed_asset_url_is_not_copied_into_retry_error() {
        let error = reqwest::Client::new()
            .get("http://127.0.0.1:0/asset?token=must-not-leak")
            .send()
            .await
            .expect_err("port zero must fail");
        let message = sanitized_http_error(&error, "generation asset request").to_string();
        assert!(message.contains("generation asset request failed"));
        assert!(!message.contains("must-not-leak"));
    }
}
