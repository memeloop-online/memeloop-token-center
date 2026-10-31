//! tkmarket's video-task dialect, deliberately independent of the Ark dialect.
//! The provider receipt contains billing tokens AND quota; neither is seconds
//! or an MTC customer price. Admission stays closed until native token
//! reservation and receipt settlement are supplied by the billing lifecycle.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::*;

#[cfg(test)]
mod tests;

pub(super) const PROFILE: &str = "tkmarket-seedance-v1";
pub(super) const MODELS: &[&str] = &["seedance-2.0-idle-ts", "seedance-2.5-idle-ts"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    prompt: String,
    duration: i64,
    resolution: String,
    aspect_ratio: String,
}

pub(super) fn validate_input(value: &Value) -> Result<(), AppError> {
    let input: Input = serde_json::from_value(value.clone()).map_err(|_| {
        AppError::BadRequest(
            "tkmarket video requires prompt, duration, resolution and aspect_ratio".into(),
        )
    })?;
    if input.prompt.trim().is_empty()
        || input.prompt.chars().count() > 32_000
        || input.duration != 5
        || input.resolution != "720p"
        || input.aspect_ratio != "16:9"
    {
        return Err(AppError::BadRequest(
            "tkmarket Seedance profile requires a prompt, 720p, 5 seconds and 16:9".into(),
        ));
    }
    Ok(())
}

pub(super) fn input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object", "additionalProperties": false,
        "required": ["prompt", "duration", "resolution", "aspect_ratio"],
        "properties": {
            "prompt": {"type": "string", "minLength": 1, "maxLength": 32000},
            "duration": {"const": 5}, "resolution": {"const": "720p"},
            "aspect_ratio": {"const": "16:9"}
        }
    })
}

pub(super) fn submit_body(model: &str, input: &Value) -> Result<Value, AppError> {
    if !MODELS.contains(&model) {
        return Err(AppError::upstream("tkmarket_video_model_not_supported"));
    }
    validate_input(input)?;
    let mut body = input
        .as_object()
        .cloned()
        .ok_or(AppError::internal("tkmarket_input"))?;
    // The documented gateway accepts integer duration, as well as seconds.
    body.insert("model".into(), json!(model));
    Ok(Value::Object(body))
}

pub(super) fn url(base: &str, task_id: Option<&str>) -> Result<String, AppError> {
    let mut parsed =
        url::Url::parse(base).map_err(|_| AppError::upstream("tkmarket_video_base_url_invalid"))?;
    if parsed.path() != "/v1" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(AppError::upstream("tkmarket_video_requires_v1_base_url"));
    }
    let path = if let Some(id) = task_id {
        let id = validated_upstream_job_id(id)?;
        format!("/v1/video/generations/{id}")
    } else {
        "/v1/video/generations".to_owned()
    };
    parsed.set_path(&path);
    Ok(parsed.to_string())
}

pub(super) fn ack<'a>(body: &'a Value, model: &str) -> Result<&'a Value, AppError> {
    if body.get("model").and_then(Value::as_str) != Some(model)
        || body.get("status").and_then(Value::as_str) != Some("queued")
    {
        return Err(AppError::upstream("tkmarket_video_invalid_ack"));
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::upstream("tkmarket_video_missing_task_id"))?;
    validated_upstream_job_id(id)?;
    Ok(body)
}

/// Exact supplier receipt. `quota` is final only on a terminal response.
/// 500,000 quota = USD 1; integer quota therefore corresponds to two USD
/// micros each. This is supplier cost evidence, never a customer price.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProviderUsage {
    pub(crate) total_tokens: i64,
    pub(crate) quota: i64,
    pub(crate) supplier_usd_micros: i64,
}

pub(super) struct CompletedVideo {
    pub(super) usage: ProviderUsage,
    pub(super) result_url: String,
    // The upstream implementation name is evidence, not an API model alias.
    pub(super) provider_model_name: String,
    pub(super) expires_at: Option<i64>,
}

pub(super) enum QueryResult {
    Pending,
    Failed { quota: i64 },
    Succeeded(CompletedVideo),
}

fn nonnegative_integer(body: &Value, name: &str) -> Result<i64, AppError> {
    body.get(name)
        .and_then(Value::as_i64)
        .filter(|v| *v >= 0)
        .ok_or_else(|| AppError::upstream("tkmarket_video_invalid_usage"))
}

pub(super) fn query_result(body: &Value, task_id: &str) -> Result<QueryResult, AppError> {
    validated_upstream_job_id(task_id)?;
    let data = body
        .get("data")
        .filter(|data| {
            body.get("code").and_then(Value::as_str) == Some("success")
                && data.get("task_id").and_then(Value::as_str) == Some(task_id)
        })
        .ok_or_else(|| AppError::upstream("tkmarket_video_query_identity_mismatch"))?;
    match data.get("status").and_then(Value::as_str) {
        Some("IN_PROGRESS") => Ok(QueryResult::Pending),
        // Failed tasks can still report supplier cost. The billing owner must
        // preserve that evidence separately from any MTC customer refund.
        Some("FAILURE") => Ok(QueryResult::Failed {
            quota: nonnegative_integer(data, "quota")?,
        }),
        Some("SUCCESS") => {
            let total_tokens = nonnegative_integer(data, "total_tokens")?;
            let quota = nonnegative_integer(data, "quota")?;
            let supplier_usd_micros = quota
                .checked_mul(2)
                .ok_or_else(|| AppError::upstream("tkmarket_video_quota_overflow"))?;
            if total_tokens == 0
                || data.get("duration").and_then(Value::as_i64) != Some(5)
                || data.get("ratio").and_then(Value::as_str) != Some("16:9")
            {
                return Err(AppError::upstream("tkmarket_video_invalid_usage"));
            }
            let result_url = data
                .get("result_url")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::upstream("tkmarket_video_missing_asset"))?;
            let parsed = url::Url::parse(result_url)
                .map_err(|_| AppError::upstream("tkmarket_video_invalid_asset"))?;
            if result_url.len() > 16 * 1024
                || parsed.scheme() != "https"
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(AppError::upstream("tkmarket_video_invalid_asset"));
            }
            let provider_model_name = data
                .get("model_name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty() && name.len() <= 500)
                .ok_or_else(|| AppError::upstream("tkmarket_video_missing_provider_model"))?;
            let expires_at = match (data.get("finish_time"), data.get("execution_expires_after")) {
                (None, None) => None,
                (Some(finish), Some(lifetime)) => {
                    let finish = finish.as_i64().filter(|v| *v > 0);
                    let lifetime = lifetime.as_i64().filter(|v| *v > 0);
                    Some(
                        finish
                            .zip(lifetime)
                            .and_then(|(f, t)| f.checked_add(t))
                            .and_then(|seconds| seconds.checked_mul(1000))
                            .ok_or_else(|| AppError::upstream("tkmarket_video_invalid_expiry"))?,
                    )
                }
                _ => return Err(AppError::upstream("tkmarket_video_invalid_expiry")),
            };
            Ok(QueryResult::Succeeded(CompletedVideo {
                usage: ProviderUsage {
                    total_tokens,
                    quota,
                    supplier_usd_micros,
                },
                result_url: result_url.to_owned(),
                provider_model_name: provider_model_name.to_owned(),
                expires_at,
            }))
        }
        _ => Err(AppError::upstream("tkmarket_video_unknown_status")),
    }
}

pub(super) fn settlement_unavailable() -> AppError {
    AppError::BadRequest(
        "tkmarket video requires native token reservation and separate supplier quota settlement; this billing interface is not available".into(),
    )
}

pub(super) async fn poll(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    task_id: &str,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let body =
        authenticated_json(state, route, url(&route.base_url, Some(task_id))?, attempt).await?;
    let result = query_result(&body, task_id)?;
    attempt.envelope_valid();
    match result {
        QueryResult::Pending => {
            attempt.valid = true;
            state
                .db
                .reschedule_generation_job(job.job_id, worker_id, 10_000, None)
                .await
        }
        QueryResult::Failed { quota } => {
            // Do not discard a nonzero supplier charge by projecting a generic
            // uncharged failure. No jobs may be admitted before this is wired.
            let _supplier_quota = quota;
            Err(settlement_unavailable())
        }
        QueryResult::Succeeded(video) => {
            ensure_asset_origin(route, &video.result_url)?;
            let _verified_receipt = (&video.usage, &video.provider_model_name, video.expires_at);
            Err(settlement_unavailable())
        }
    }
}
