use serde::Deserialize;
use serde_json::{Value, json};

use super::*;

#[cfg(test)]
mod retry_tests;

pub(super) use crate::network::client_for_config_url_no_retry as submit_http;

pub(super) fn is_mp4_response(headers: &reqwest::header::HeaderMap) -> bool {
    let mut values = headers.get_all(reqwest::header::CONTENT_TYPE).iter();
    let valid = values
        .next()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("video/mp4"))
        });
    valid && values.next().is_none()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    input: ReferenceInput,
    parameters: Parameters,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceInput {
    prompt: String,
    media: Vec<ReferenceImage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceImage {
    #[serde(rename = "type")]
    kind: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    resolution: String,
    ratio: String,
    duration: i64,
    prompt_extend: bool,
    watermark: bool,
}

pub(super) fn validate_input(value: &Value) -> Result<(), AppError> {
    let input: Input = serde_json::from_value(value.clone()).map_err(|_| {
        AppError::BadRequest("Wasu Wan input requires reference-image input and parameters".into())
    })?;
    if input.input.prompt.trim().is_empty()
        || input.input.prompt.chars().count() > 32_000
        || input.input.media.len() != 1
        || input.parameters.resolution != "720P"
        || input.parameters.ratio != "16:9"
        || input.parameters.duration != 5
        || input.parameters.prompt_extend
        || input.parameters.watermark
    {
        return Err(AppError::BadRequest(
            "Wasu Wan supports only one reference image, 5 seconds, 720P, 16:9, prompt_extend=false and watermark=false".into(),
        ));
    }
    let media = &input.input.media[0];
    let parsed = url::Url::parse(&media.url)
        .map_err(|_| AppError::BadRequest("Wasu Wan reference image URL is invalid".into()))?;
    if media.kind != "reference_image"
        || media.url.len() > 8192
        || parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AppError::BadRequest(
            "Wasu Wan reference image requires an HTTPS URL without user information or fragment"
                .into(),
        ));
    }
    Ok(())
}

pub(super) fn input_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["input", "parameters"],
        "properties": {
            "input": {
                "type": "object",
                "additionalProperties": false,
                "required": ["prompt", "media"],
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 32000},
                    "media": {
                        "type": "array", "minItems": 1, "maxItems": 1,
                        "items": {
                            "type": "object", "additionalProperties": false,
                            "required": ["type", "url"],
                            "properties": {
                                "type": {"const": "reference_image"},
                                "url": {"type": "string", "format": "uri", "pattern": "^https://", "maxLength": 8192}
                            }
                        }
                    }
                }
            },
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "required": ["resolution", "ratio", "duration", "prompt_extend", "watermark"],
                "properties": {
                    "resolution": {"const": "720P"},
                    "ratio": {"const": "16:9"},
                    "duration": {"const": 5},
                    "prompt_extend": {"const": false},
                    "watermark": {"const": false}
                }
            }
        }
    })
}

pub(super) fn url(base: &str, path: &str) -> Result<String, AppError> {
    let mut parsed = url::Url::parse(base).map_err(|_| AppError::Internal)?;
    if parsed.path() != "/v1" {
        return Err(AppError::Upstream(
            "Wasu Wan requires a /v1 base URL".into(),
        ));
    }
    parsed.set_path(path);
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.to_string())
}

pub(super) fn ack_output(body: &Value) -> Result<&Value, AppError> {
    let output = body
        .get("output")
        .ok_or_else(|| AppError::Upstream("Wasu Wan submit response has no output".into()))?;
    if !matches!(
        output.get("task_status").and_then(Value::as_str),
        Some("PENDING" | "RUNNING" | "SUCCEEDED")
    ) {
        return Err(AppError::Upstream(
            "Wasu Wan submit response has no accepted task status".into(),
        ));
    }
    let task_id = output
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Upstream("Wasu Wan submit response has no task id".into()))?;
    validated_upstream_job_id(task_id)?;
    Ok(output)
}

fn poll_output<'body>(body: &'body Value, task_id: &str) -> Result<&'body Value, AppError> {
    let output = body
        .get("output")
        .ok_or_else(|| AppError::Upstream("Wasu Wan query response has no output".into()))?;
    if output.get("task_id").and_then(Value::as_str) != Some(task_id) {
        return Err(AppError::Upstream(
            "Wasu Wan query task identity mismatch".into(),
        ));
    }
    Ok(output)
}

fn billed_seconds(body: &Value, reserved: i64) -> Result<i64, AppError> {
    let usage = body
        .get("usage")
        .or_else(|| body.pointer("/output/usage"))
        .ok_or_else(|| AppError::Upstream("Wasu Wan query response has no usage".into()))?;
    let duration = usage.get("duration").and_then(Value::as_i64);
    let output_duration = usage.get("output_video_duration").and_then(Value::as_i64);
    if usage.get("video_count").and_then(Value::as_i64) != Some(1)
        || duration != output_duration
        || duration.is_none_or(|value| value <= 0 || value > reserved)
    {
        return Err(AppError::Upstream(
            "Wasu Wan usage violates the reserved seconds contract".into(),
        ));
    }
    duration.ok_or(AppError::Internal)
}

pub(super) async fn poll(
    state: &AppState,
    worker_id: &str,
    job: &GenerationJobWork,
    route: &ResolvedUpstream,
    task_id: &str,
    attempt: &mut attempt::Attempt<'_>,
) -> Result<(), AppError> {
    let task_id = validated_upstream_job_id(task_id)?;
    let poll_url = url(&route.base_url, &format!("/api/v1/tasks/{task_id}"))?;
    let body = authenticated_json(state, route, poll_url, attempt).await?;
    let output = poll_output(&body, task_id)?;
    match output.get("task_status").and_then(Value::as_str) {
        Some("PENDING" | "RUNNING") => {
            attempt.valid = true;
            state
                .db
                .reschedule_generation_job(job.job_id, worker_id, 10_000, None)
                .await
        }
        Some("FAILED" | "CANCELED" | "CANCELLED") => {
            attempt.valid = true;
            terminal_failure(state, worker_id, job, "wasu_wan_generation_failed").await
        }
        Some("SUCCEEDED") => {
            let billed_units = match billed_seconds(&body, job.estimated_units) {
                Ok(units) => units,
                Err(_) => {
                    attempt.invalid_response();
                    return terminal_failure(state, worker_id, job, "wasu_wan_invalid_usage").await;
                }
            };
            let Some(video_url) = output.get("video_url").and_then(Value::as_str) else {
                attempt.invalid_response();
                return terminal_failure(state, worker_id, job, "wasu_wan_missing_asset").await;
            };
            attempt.envelope_valid();
            if ensure_asset_origin(route, video_url).is_err() {
                attempt.invalid_response();
                return terminal_failure(state, worker_id, job, "wasu_wan_invalid_asset").await;
            }
            let asset = provider_generation_asset(
                job.job_id,
                0,
                video_url,
                provider_expiry_millis(output, video_url),
                Some("video.mp4"),
            )?;
            attempt.valid = true;
            terminal_provider_success(state, worker_id, job, route, billed_units, vec![asset]).await
        }
        _ => Err(AppError::Upstream(
            "unknown Wasu Wan generation status".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp4_response_requires_an_unambiguous_provider_content_type() {
        use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
        assert!(!is_mp4_response(&HeaderMap::new()));
        for (mime, expected) in [
            ("video/mp4", true),
            ("VIDEO/MP4; codecs=avc1", true),
            ("image/png", false),
            ("application/xml", false),
            ("text/html", false),
            ("application/octet-stream", false),
            ("audio/mp4", false),
            ("video/mp4, text/html", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
            assert_eq!(is_mp4_response(&headers), expected);
            headers.append(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            assert!(!is_mp4_response(&headers));
        }
    }

    fn verified_input() -> Value {
        json!({
            "input": {"prompt": "a cat running on grass", "media": [
                {"type": "reference_image", "url": "https://example.test/reference.png"}
            ]},
            "parameters": {
                "resolution": "720P", "ratio": "16:9", "duration": 5,
                "prompt_extend": false, "watermark": false
            }
        })
    }

    #[test]
    fn reference_profile_rejects_unverified_variants_before_reservation() {
        let valid = verified_input();
        validate_input(&valid).unwrap();
        crate::schema::validate_instance(&input_schema(), &valid).unwrap();
        for (pointer, replacement) in [
            ("/parameters/duration", json!(1)),
            ("/parameters/duration", json!(5.0)),
            ("/parameters/resolution", json!("480P")),
            ("/parameters/prompt_extend", json!(true)),
            ("/parameters/watermark", json!(true)),
            ("/input/media/0/type", json!("image")),
            (
                "/input/media/0/url",
                json!("https://user:private@example.test/ref.png"),
            ),
            ("/input/media/0/url", json!("http://example.test/ref.png")),
            ("/input/prompt", json!(" ")),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(validate_input(&invalid).is_err());
        }
        let mut invalid = valid.clone();
        invalid["model"] = json!("minimax-h3");
        assert!(validate_input(&invalid).is_err());
        invalid = valid;
        invalid["input"]["media"] = json!([]);
        assert!(validate_input(&invalid).is_err());
    }

    #[test]
    fn explicit_profile_preserves_shared_text_accounts_and_seconds_billing() {
        let config = json!({"video_api": "wasu-wan-v1", "video_models": ["wan3.0-video"]});
        let adapter =
            GenerationProtocolAdapter::detect("http-json", &config, "wan3.0-video").unwrap();
        assert_eq!(adapter, GenerationProtocolAdapter::WasuWanV1);
        assert_eq!(adapter.catalog_modalities(), &["video"]);
        assert!(!adapter.supports_modality("image"));
        assert!(
            GenerationProtocolAdapter::detect("http-json", &config, "deepseek-v4.1-flash")
                .is_none()
        );
        assert!(GenerationProtocolAdapter::detect("http-json", &config, "minimax-h3").is_none());
        assert!(
            GenerationProtocolAdapter::detect("http-json", &json!({}), "wan3.0-video").is_none()
        );
        assert_eq!(
            adapter
                .estimated_units("second", &verified_input())
                .unwrap(),
            5
        );
        assert!(adapter.estimated_units("job", &verified_input()).is_err());
        assert!(adapter.estimated_units("image", &verified_input()).is_err());
    }

    #[test]
    fn submit_ack_and_query_identity_fail_closed_without_raw_envelope_errors() {
        let ack = json!({"output": {"task_id": "wan-task-1", "task_status": "PENDING"}});
        assert_eq!(ack_output(&ack).unwrap()["task_id"], "wan-task-1");
        assert!(poll_output(&ack, "wan-task-1").is_ok());
        for body in [
            json!({"task_id": "wan-task-1", "task_status": "PENDING"}),
            json!({"output": {"task_id": "wan-task-1", "task_status": "UNKNOWN_PRIVATE"}}),
            json!({"output": {"task_id": "../private", "task_status": "PENDING"}}),
            json!({"output": {"task_status": "PENDING"}}),
        ] {
            assert!(ack_output(&body).is_err());
        }
        assert!(poll_output(&ack, "another-task").is_err());
    }

    #[test]
    fn succeeded_usage_requires_one_video_and_matching_bounded_seconds() {
        let mut body =
            json!({"usage": {"video_count": 1, "duration": 5, "output_video_duration": 5}});
        assert_eq!(billed_seconds(&body, 5).unwrap(), 5);
        assert!(billed_seconds(&body, 4).is_err());
        body["usage"]["duration"] = json!(6);
        assert!(billed_seconds(&body, 5).is_err());
        body["usage"]["duration"] = json!(5);
        body["usage"]["video_count"] = json!(2);
        assert!(billed_seconds(&body, 5).is_err());
        assert!(billed_seconds(&json!({}), 5).is_err());
    }

    #[test]
    fn native_paths_replace_chat_prefix_without_accepting_arbitrary_base_paths() {
        assert_eq!(
            url("https://example.test/v1", "/api/v1/tasks/wan-task-1").unwrap(),
            "https://example.test/api/v1/tasks/wan-task-1",
        );
        assert!(url("https://example.test/arbitrary", "/api/v1/tasks/wan-task-1").is_err());
    }
}
