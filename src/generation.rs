use futures_util::StreamExt;
use reqwest::Response;
use serde_json::{Map, Value, json};

use crate::{
    AppState,
    db::{FinishGenerationJobInput, unix_millis},
    error::AppError,
    model::GenerationJobWork,
    network,
    provider::{ResolvedUpstream, UpstreamCredential},
};

const MAX_CONTROL_BODY: usize = 4 * 1024 * 1024;
const MAX_ASSET_BODY: usize = 512 * 1024 * 1024;
const MAX_COMFY_ASSETS: usize = 16;
const MAX_FAILURES: i64 = 20;
const MAX_JOB_AGE_MILLIS: i64 = 24 * 60 * 60 * 1_000;

pub async fn process_one(state: &AppState, worker_id: &str) -> Result<bool, AppError> {
    let Some(job) = state.db.claim_generation_job(worker_id).await? else {
        return Ok(false);
    };
    // An outer error means the lease could no longer be renewed. In that case
    // the in-flight upstream future is dropped and this worker must not settle,
    // reschedule, or otherwise mutate a job that another worker may now own.
    let outcome = process_claimed_with_lease(state, worker_id, &job).await?;
    if let Err(error) = outcome {
        let next_failure = job.failure_count.saturating_add(1);
        tracing::warn!(job_id = %job.job_id, attempt = job.attempt_count, failure = next_failure, %error, "generation job attempt failed");
        if next_failure >= MAX_FAILURES {
            let cost = state.db.settle_usage(&job.reservation, 0, 0).await?;
            state
                .db
                .finish_generation_job(FinishGenerationJobInput {
                    job_id: job.job_id,
                    worker_id,
                    status: "failed",
                    billed_units: 0,
                    cost_micros: cost,
                    result: None,
                    error_code: Some("retry_exhausted"),
                })
                .await?;
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

async fn process_claimed(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
) -> Result<(), AppError> {
    if unix_millis().saturating_sub(job.created_at) > MAX_JOB_AGE_MILLIS {
        return terminal_failure(state, worker_id, job, "generation_timeout", None).await;
    }
    let route = state
        .db
        .resolve_upstream_with_hint(
            job.tenant_id,
            &job.public_model,
            "generation",
            Some(job.upstream_account_id),
            state.config.key_pepper.as_bytes(),
        )
        .await?
        .ok_or_else(|| AppError::Upstream("generation route was removed".into()))?;
    if route.driver != job.driver {
        return Err(AppError::Upstream(
            "generation route driver changed while job was queued".into(),
        ));
    }
    match job.upstream_job_id.as_deref() {
        None => submit(state, worker_id, job, &route).await,
        Some(upstream_job_id) => poll(state, worker_id, job, &route, upstream_job_id).await,
    }
}

async fn submit(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
) -> Result<(), AppError> {
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
    let (path, id_field) = match route.driver.as_str() {
        "volcengine-seedance" => {
            input.insert(
                "model".to_owned(),
                Value::String(job.upstream_model.clone()),
            );
            ("/api/v3/contents/generations/tasks".to_owned(), "id")
        }
        "comfyui" => {
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
            let mut workflow = route
                .config
                .get("workflow_template")
                .cloned()
                .ok_or_else(|| AppError::Storage("ComfyUI workflow template is missing".into()))?;
            apply_workflow_parameters(&mut workflow, &parameters)?;
            input = Map::from_iter([("prompt".to_owned(), workflow)]);
            (format!("{prefix}/prompt"), "prompt_id")
        }
        _ => return Err(AppError::Upstream("unsupported generation driver".into())),
    };
    let outbound_http = route_http(state, route, &route.base_url).await?;
    let request = outbound_http
        .post(format!("{}{}", route.base_url, path))
        .header("idempotency-key", job.job_id.to_string())
        .json(&input);
    let upstream_started = std::time::Instant::now();
    let response_result = route.credential.apply(request, unix_millis())?.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_submit",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result
        .map_err(|error| sanitized_http_error(&error, "generation submit request"))?;
    let status = response.status();
    let body = bounded_json(response).await;
    if !status.is_success() {
        if status.is_client_error() && !matches!(status.as_u16(), 408 | 425 | 429) {
            let result = body.unwrap_or_else(|_| json!({"http_status": status.as_u16()}));
            return terminal_failure(state, worker_id, job, "generation_rejected", Some(&result))
                .await;
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
    state
        .db
        .mark_generation_submitted(job.job_id, worker_id, upstream_job_id)
        .await
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
    match route.driver.as_str() {
        "volcengine-seedance" => poll_seedance(state, worker_id, job, route, upstream_job_id).await,
        "comfyui" => poll_comfy(state, worker_id, job, route, upstream_job_id).await,
        _ => Err(AppError::Upstream("unsupported generation driver".into())),
    }
}

async fn poll_seedance(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
) -> Result<(), AppError> {
    let poll_url = format!(
        "{}/api/v3/contents/generations/tasks/{upstream_job_id}",
        route.base_url
    );
    let outbound_http = route_http(state, route, &poll_url).await?;
    let request = outbound_http.get(poll_url);
    let upstream_started = std::time::Instant::now();
    let response_result = route.credential.apply(request, unix_millis())?.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_poll",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response =
        response_result.map_err(|error| sanitized_http_error(&error, "Seedance poll request"))?;
    let status = response.status();
    let mut body = bounded_json(response).await?;
    if !status.is_success() {
        return Err(AppError::Upstream(format!(
            "Seedance poll returned HTTP {}",
            status.as_u16()
        )));
    }
    match body.get("status").and_then(Value::as_str) {
        Some("queued" | "running") | None => {
            state
                .db
                .reschedule_generation_job(job.job_id, worker_id, 2_000, None)
                .await
        }
        Some("failed" | "cancelled") => {
            let error_code = body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("generation_failed");
            terminal_failure(state, worker_id, job, error_code, Some(&body)).await
        }
        Some("succeeded") => {
            if let Some(video_url) = body.pointer("/content/video_url").and_then(Value::as_str) {
                let archive_object =
                    archive_asset(state, route, &route.credential, job.job_id, 0, video_url)
                        .await?;
                body["archive_objects"] = json!([archive_object]);
            }
            let billed_units = body
                .get("duration")
                .and_then(json_i64)
                .unwrap_or(job.estimated_units)
                .clamp(1, job.estimated_units);
            terminal_success(state, worker_id, job, billed_units, &body).await
        }
        Some(other) => Err(AppError::Upstream(format!(
            "unknown Seedance status: {other}"
        ))),
    }
}

async fn poll_comfy(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    upstream_job_id: &str,
) -> Result<(), AppError> {
    let prefix = comfy_prefix(route)?;
    if prefix == "/api" {
        let status_body = authenticated_json(
            state,
            route,
            format!("{}/api/job/{upstream_job_id}/status", route.base_url),
        )
        .await?;
        match status_body.get("status").and_then(Value::as_str) {
            Some("pending" | "in_progress") | None => {
                return state
                    .db
                    .reschedule_generation_job(job.job_id, worker_id, 2_000, None)
                    .await;
            }
            Some("failed" | "cancelled") => {
                return terminal_failure(
                    state,
                    worker_id,
                    job,
                    "comfyui_failed",
                    Some(&status_body),
                )
                .await;
            }
            Some("completed") => {}
            Some(other) => {
                return Err(AppError::Upstream(format!(
                    "unknown ComfyUI status: {other}"
                )));
            }
        }
    }
    let history_path = format!("{}{}/history/{upstream_job_id}", route.base_url, prefix);
    let history = authenticated_json(state, route, history_path).await?;
    let entry = if let Some(entry) = history.get(upstream_job_id) {
        entry.clone()
    } else if prefix == "/api" {
        history.clone()
    } else {
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
        return terminal_failure(
            state,
            worker_id,
            job,
            "comfyui_execution_error",
            Some(&entry),
        )
        .await;
    }
    let mut assets = Vec::new();
    find_comfy_assets(&entry, &mut assets);
    let mut archive_objects = Vec::new();
    for (index, asset) in assets.into_iter().take(MAX_COMFY_ASSETS).enumerate() {
        let url = comfy_asset_url(route, &prefix, &asset)?;
        archive_objects.push(
            archive_asset(
                state,
                route,
                &route.credential,
                job.job_id,
                index,
                url.as_str(),
            )
            .await?,
        );
    }
    let result = json!({
        "history": entry,
        "archive_objects": archive_objects
    });
    terminal_success(state, worker_id, job, 1, &result).await
}

async fn authenticated_json(
    state: &AppState,
    route: &ResolvedUpstream,
    url: String,
) -> Result<Value, AppError> {
    let outbound_http = route_http(state, route, &url).await?;
    let request = route
        .credential
        .apply(outbound_http.get(url), unix_millis())?;
    let upstream_started = std::time::Instant::now();
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_poll",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response =
        response_result.map_err(|error| sanitized_http_error(&error, "generation poll request"))?;
    let status = response.status();
    let body = bounded_json(response).await?;
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

async fn archive_asset(
    state: &AppState,
    route: &ResolvedUpstream,
    credential: &UpstreamCredential,
    job_id: uuid::Uuid,
    index: usize,
    url: &str,
) -> Result<String, AppError> {
    ensure_asset_origin(route, url)?;
    let asset = url::Url::parse(url)
        .map_err(|_| AppError::Upstream("generation asset URL is invalid".into()))?;
    let base = url::Url::parse(&route.base_url).map_err(|_| AppError::Internal)?;
    let outbound_http = route_http(state, route, url).await?;
    let request = outbound_http.get(url);
    // A provider credential may be needed for same-origin ComfyUI assets. Signed
    // Seedance result URLs are often hosted on a configured CDN origin, where
    // forwarding that credential would disclose it to another service.
    let request = if asset.origin() == base.origin() {
        credential.apply(request, unix_millis())?
    } else {
        request
    };
    let upstream_started = std::time::Instant::now();
    let response_result = request.send().await;
    state.metrics.observe_upstream(
        &route.driver,
        "generation_asset",
        response_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let response = response_result
        .map_err(|error| sanitized_http_error(&error, "generation asset request"))?;
    if !response.status().is_success() {
        return Err(AppError::Upstream(format!(
            "generation asset returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let mut writer = state
        .archive
        .start_writer(&format!("staging/{job_id}/asset-{index}"))
        .await?;
    let mut total = 0_usize;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| sanitized_http_error(&error, "generation asset response"))?;
        total = total.saturating_add(chunk.len());
        if total > MAX_ASSET_BODY {
            writer.abort().await?;
            return Err(AppError::Upstream(
                "generation asset exceeds 512 MiB".into(),
            ));
        }
        writer.write(chunk).await?;
    }
    writer.finish().await
}

async fn route_http(
    state: &AppState,
    route: &ResolvedUpstream,
    url: &str,
) -> Result<reqwest::Client, AppError> {
    network::client_for_config_url(
        &state.http,
        url,
        &route.config,
        state.config.allow_oauth_loopback,
    )
    .await
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

fn ensure_asset_origin(route: &ResolvedUpstream, asset: &str) -> Result<(), AppError> {
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
    billed_units: i64,
    result: &Value,
) -> Result<(), AppError> {
    let cost = state
        .db
        .settle_usage(&job.reservation, 0, billed_units)
        .await?;
    state
        .db
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id,
            status: "succeeded",
            billed_units,
            cost_micros: cost,
            result: Some(result),
            error_code: None,
        })
        .await
}

async fn terminal_failure(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    error_code: &str,
    result: Option<&Value>,
) -> Result<(), AppError> {
    let cost = state.db.settle_usage(&job.reservation, 0, 0).await?;
    state
        .db
        .finish_generation_job(FinishGenerationJobInput {
            job_id: job.job_id,
            worker_id,
            status: "failed",
            billed_units: 0,
            cost_micros: cost,
            result,
            error_code: Some(error_code),
        })
        .await
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComfyAsset {
    filename: String,
    subfolder: String,
    kind: String,
}

fn find_comfy_assets(value: &Value, output: &mut Vec<ComfyAsset>) {
    if output.len() >= MAX_COMFY_ASSETS {
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
