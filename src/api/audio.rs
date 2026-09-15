use axum::extract::{Multipart, multipart::MultipartRejection};
use reqwest::multipart::{Form, Part};

use super::*;

const AUDIO_PROTOCOL: &str = "audio";
const AUDIO_AUDIT_PROTOCOL: &str = "audio-transcription";
const AUDIO_CLIENT_PROTOCOL: &str = "openai-audio-transcription";
const MAX_HOTWORDS: usize = 256;
const MAX_HOTWORD_BYTES: usize = 200;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum AudioResponseFormat {
    #[default]
    Json,
    VerboseJson,
}

impl AudioResponseFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::VerboseJson => "verbose_json",
        }
    }
}

#[derive(Debug)]
struct AudioFile {
    bytes: Bytes,
}

#[derive(Debug)]
struct AudioTranscriptionForm {
    file: AudioFile,
    model: String,
    prompt: Option<String>,
    hotwords: Option<Vec<String>>,
    response_format: AudioResponseFormat,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewrittenAudioMetadata {
    model: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    hotwords: Option<Vec<String>>,
    #[serde(default)]
    response_format: AudioResponseFormat,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pcm16WavDuration {
    channels: u16,
    sample_rate_hz: u32,
    frames: u64,
    duration_ms: i64,
    billed_seconds: i64,
}

impl Pcm16WavDuration {
    fn seconds(self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate_hz)
    }
}

pub(super) async fn create_audio_transcription(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let mut state = state.pin_application_plugins().await?;
    let form = parse_audio_transcription_form(
        multipart.map_err(|_| {
            AppError::BadRequest("request body must be multipart/form-data".to_owned())
        })?,
        state.config.audio_body_max_bytes as usize,
    )
    .await?;
    let audio = pcm16_wav_duration(&form.file.bytes)?;
    let applied = apply_traffic_policy(
        &state,
        &key,
        TrafficPolicyProtocols {
            client: AUDIO_CLIENT_PROTOCOL,
            routing: AUDIO_PROTOCOL,
        },
        json!({
            "model": form.model,
            "prompt": form.prompt,
            "hotwords": form.hotwords,
            "response_format": form.response_format.as_str(),
        }),
    )
    .await?;
    let metadata: RewrittenAudioMetadata = serde_json::from_value(applied.request_json)
        .map_err(|_| AppError::BadRequest("plugin-rewritten audio request is invalid".into()))?;
    validate_audio_metadata(&metadata)?;

    let request_id = Uuid::now_v7();
    let route = crate::generation::group_routing::prepare_route_for_protocol(
        &mut state,
        &key,
        &metadata.model,
        AUDIO_PROTOCOL,
        applied.upstream_account_hint,
        request_id,
        request_id,
    )
    .await?;
    let provider = state
        .providers
        .get(&route.driver)
        .ok_or(AppError::Internal)?;
    if !crate::provider::is_openai_compatible_http_driver(&route.driver)
        || !provider
            .protocols
            .iter()
            .any(|value| value == AUDIO_PROTOCOL)
        || !provider.modalities.iter().any(|value| value == "audio")
    {
        return Err(AppError::Upstream(
            "audio route does not implement the OpenAI Audio API".into(),
        ));
    }
    let generation_price = state
        .db
        .generation_price(&metadata.model, &key.currency)
        .await?;
    if generation_price.billing_unit != "second" {
        return Err(AppError::BadRequest(
            "audio transcription price must use second billing".into(),
        ));
    }
    let reservation_price = generation_price
        .reservation_price()
        .ok_or_else(|| AppError::BadRequest("generation price is too large".into()))?;
    let request_object = metadata_only_locator(json!({
        "kind": "audio_transcription",
        "media_archived": false,
        "media_bytes": form.file.bytes.len(),
        "duration_ms": audio.duration_ms,
        "sample_rate_hz": audio.sample_rate_hz,
        "channels": audio.channels,
        "response_format": metadata.response_format.as_str(),
    }))?;
    let upstream_form = upstream_form(form.file, &metadata, &route.upstream_model)?;
    let outbound_http = network::client_for_config_url_no_retry(
        &state.http,
        &route.base_url,
        &route.config,
        route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await?;
    let target_url = network::upstream_api_url(&route.base_url, "/v1/audio/transcriptions");
    let request = route.credential.apply(
        outbound_http.post(target_url).multipart(upstream_form),
        unix_millis(),
    )?;
    let reservation = state
        .db
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &reservation_price,
            input_token_ceiling: 0,
            output_token_ceiling: audio.billed_seconds,
            protocol: AUDIO_AUDIT_PROTOCOL,
            model: &metadata.model,
            request_object: &request_object,
            upstream_account_id: Some(route.account_id),
            model_route_id: Some(route.route_id),
        })
        .await?;
    let started = Instant::now();
    let mut attempt =
        match crate::generation::group_routing::admit(&state, key.tenant_id, request_id, &route)
            .await
        {
            Ok(attempt) => attempt,
            Err(error) => {
                finish_audio_request(
                    &state,
                    &reservation,
                    request_id,
                    key.tenant_id,
                    audio,
                    started,
                    StatusCode::BAD_GATEWAY,
                    0,
                    Some("audio_upstream_unavailable"),
                    json!({"kind":"audio_transcription","media_archived":false,"succeeded":false}),
                )
                .await?;
                return Err(error);
            }
        };
    let _activity = state.metrics.active_upstream(&route.driver, "audio");
    let upstream_result = tokio::time::timeout(SYNCHRONOUS_AUDIO_DEADLINE, request.send()).await;
    let upstream = match upstream_result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            let terminal = if error.is_connect() {
                MediaAttemptTerminal::Failed {
                    kind: crate::db::UpstreamFailureKind::Connection,
                    reason: crate::metrics::UpstreamHealthReason::Connection,
                }
            } else {
                MediaAttemptTerminal::Inconclusive
            };
            attempt.complete(terminal).await;
            finish_audio_request(
                &state,
                &reservation,
                request_id,
                key.tenant_id,
                audio,
                started,
                StatusCode::BAD_GATEWAY,
                0,
                Some("audio_upstream_transport"),
                json!({"kind":"audio_transcription","media_archived":false,"succeeded":false}),
            )
            .await?;
            return Err(AppError::Upstream("audio upstream transport failed".into()));
        }
        Err(_) => {
            attempt.complete(MediaAttemptTerminal::Inconclusive).await;
            finish_audio_request(
                &state,
                &reservation,
                request_id,
                key.tenant_id,
                audio,
                started,
                StatusCode::BAD_GATEWAY,
                0,
                Some("audio_upstream_timeout"),
                json!({"kind":"audio_transcription","media_archived":false,"succeeded":false}),
            )
            .await?;
            return Err(AppError::Upstream("audio upstream timed out".into()));
        }
    };
    state.metrics.observe_upstream(
        &route.driver,
        "audio",
        Some(upstream.status()),
        started.elapsed(),
    );
    let upstream_status = upstream.status();
    if upstream_status == StatusCode::TOO_MANY_REQUESTS {
        let kind = classify_media_rate_limit(upstream).await;
        attempt
            .complete(MediaAttemptTerminal::Failed {
                kind,
                reason: crate::metrics::UpstreamHealthReason::RateLimited,
            })
            .await;
        finish_audio_failure(
            &state,
            &reservation,
            request_id,
            key.tenant_id,
            audio,
            started,
            "audio_upstream_rate_limited",
        )
        .await?;
        return Err(AppError::Upstream("audio upstream rate limited".into()));
    }
    if !upstream_status.is_success() {
        let terminal = if matches!(
            upstream_status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            MediaAttemptTerminal::Failed {
                kind: crate::db::UpstreamFailureKind::Authentication,
                reason: crate::metrics::UpstreamHealthReason::Unavailable,
            }
        } else if upstream_status.is_server_error() {
            MediaAttemptTerminal::Failed {
                kind: crate::db::UpstreamFailureKind::Unavailable,
                reason: crate::metrics::UpstreamHealthReason::Unavailable,
            }
        } else {
            MediaAttemptTerminal::Inconclusive
        };
        attempt.complete(terminal).await;
        drop(upstream);
        finish_audio_failure(
            &state,
            &reservation,
            request_id,
            key.tenant_id,
            audio,
            started,
            "audio_upstream_rejected",
        )
        .await?;
        return Err(AppError::Upstream(
            "audio upstream rejected the request".into(),
        ));
    }
    let upstream_body = match read_audio_response_bounded(upstream).await {
        Ok(body) => body,
        Err(error) => {
            attempt
                .complete(MediaAttemptTerminal::invalid_response())
                .await;
            finish_audio_failure(
                &state,
                &reservation,
                request_id,
                key.tenant_id,
                audio,
                started,
                error,
            )
            .await?;
            return Err(AppError::Upstream(
                "audio upstream returned an invalid response".into(),
            ));
        }
    };
    let normalized =
        match normalize_audio_response(&upstream_body, metadata.response_format, audio.seconds()) {
            Ok(value) => value,
            Err(error) => {
                attempt
                    .complete(MediaAttemptTerminal::invalid_response())
                    .await;
                finish_audio_failure(
                    &state,
                    &reservation,
                    request_id,
                    key.tenant_id,
                    audio,
                    started,
                    "audio_upstream_invalid_response",
                )
                .await?;
                return Err(error);
            }
        };
    let segment_count = normalized
        .get("segments")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    finish_audio_request(
        &state,
        &reservation,
        request_id,
        key.tenant_id,
        audio,
        started,
        StatusCode::OK,
        audio.billed_seconds,
        None,
        json!({
            "kind":"audio_transcription",
            "media_archived":false,
            "succeeded":true,
            "duration_ms":audio.duration_ms,
            "segment_count":segment_count,
        }),
    )
    .await?;
    attempt
        .complete_committed(MediaAttemptTerminal::Succeeded)
        .await;
    let body = serde_json::to_vec(&normalized).map_err(|_| AppError::Internal)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .body(Body::from(body))
        .map_err(|_| AppError::Internal)
}

async fn parse_audio_transcription_form(
    mut multipart: Multipart,
    maximum_body_bytes: usize,
) -> Result<AudioTranscriptionForm, AppError> {
    let mut file = None;
    let mut model = None;
    let mut prompt = None;
    let mut hotwords = None;
    let mut response_format = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| AppError::BadRequest("invalid multipart request".into()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "file" => {
                if file.is_some() {
                    return Err(AppError::BadRequest("file must be supplied once".into()));
                }
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|_| AppError::BadRequest("invalid audio file field".into()))?;
                if bytes.is_empty() || bytes.len() > maximum_body_bytes {
                    return Err(AppError::BadRequest(
                        "file must contain a bounded PCM16 WAV payload".into(),
                    ));
                }
                file = Some(AudioFile { bytes });
            }
            "model" => set_once_text(&mut model, field, "model").await?,
            "prompt" => set_once_text(&mut prompt, field, "prompt").await?,
            "hotwords" => {
                if hotwords.is_some() {
                    return Err(AppError::BadRequest(
                        "hotwords must be supplied once".into(),
                    ));
                }
                let mut raw = None;
                set_once_text(&mut raw, field, "hotwords").await?;
                hotwords = Some(parse_hotwords(raw.as_deref().unwrap_or_default())?);
            }
            "response_format" => {
                set_once_text(&mut response_format, field, "response_format").await?
            }
            "temperature" => {
                let _ = field
                    .text()
                    .await
                    .map_err(|_| AppError::BadRequest("invalid temperature field".into()))?;
            }
            _ => {
                return Err(AppError::BadRequest(format!(
                    "unsupported audio transcription field: {name}"
                )));
            }
        }
    }
    let model = model
        .filter(|value| !value.trim().is_empty() && value.len() <= 200)
        .ok_or_else(|| AppError::BadRequest("model is required".into()))?;
    let response_format = match response_format.as_deref().unwrap_or("json") {
        "json" => AudioResponseFormat::Json,
        "verbose_json" => AudioResponseFormat::VerboseJson,
        _ => {
            return Err(AppError::BadRequest(
                "response_format must be json or verbose_json".into(),
            ));
        }
    };
    let form = AudioTranscriptionForm {
        file: file.ok_or_else(|| AppError::BadRequest("file is required".into()))?,
        model,
        prompt,
        hotwords,
        response_format,
    };
    validate_optional_prompt(form.prompt.as_deref())?;
    validate_hotwords(form.hotwords.as_deref())?;
    Ok(form)
}

async fn set_once_text(
    target: &mut Option<String>,
    field: axum::extract::multipart::Field<'_>,
    name: &str,
) -> Result<(), AppError> {
    if target.is_some() {
        return Err(AppError::BadRequest(format!(
            "{name} must be supplied once"
        )));
    }
    *target = Some(
        field
            .text()
            .await
            .map_err(|_| AppError::BadRequest(format!("invalid {name} field")))?,
    );
    Ok(())
}

fn parse_hotwords(value: &str) -> Result<Vec<String>, AppError> {
    let values: Vec<String> = serde_json::from_str(value)
        .map_err(|_| AppError::BadRequest("hotwords must be a JSON string array".into()))?;
    validate_hotwords(Some(&values))?;
    Ok(values)
}

fn validate_audio_metadata(metadata: &RewrittenAudioMetadata) -> Result<(), AppError> {
    if metadata.model.trim().is_empty() || metadata.model.len() > 200 {
        return Err(AppError::BadRequest("model is required".into()));
    }
    validate_optional_prompt(metadata.prompt.as_deref())?;
    validate_hotwords(metadata.hotwords.as_deref())
}

fn validate_optional_prompt(prompt: Option<&str>) -> Result<(), AppError> {
    if prompt.is_some_and(|value| value.len() > 32_000 || value.contains('\0')) {
        return Err(AppError::BadRequest("prompt contains invalid data".into()));
    }
    Ok(())
}

fn validate_hotwords(hotwords: Option<&[String]>) -> Result<(), AppError> {
    if hotwords.is_some_and(|values| {
        values.len() > MAX_HOTWORDS
            || values.iter().any(|value| {
                value.is_empty() || value.len() > MAX_HOTWORD_BYTES || value.contains('\0')
            })
    }) {
        return Err(AppError::BadRequest(
            "hotwords contains invalid data".into(),
        ));
    }
    Ok(())
}

fn pcm16_wav_duration(bytes: &[u8]) -> Result<Pcm16WavDuration, AppError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(AppError::BadRequest(
            "file must be a PCM16 WAV file in ASR phase one".into(),
        ));
    }
    let riff_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if riff_size.checked_add(8) != Some(bytes.len()) {
        return Err(AppError::BadRequest("WAV RIFF length is invalid".into()));
    }
    let mut offset = 12_usize;
    let mut format = None;
    let mut data_bytes = None;
    while offset.saturating_add(8) <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| AppError::BadRequest("invalid WAV chunk".into()))?,
        ) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| AppError::BadRequest("invalid WAV chunk length".into()))?;
        if id == b"fmt " {
            if size < 16 || format.is_some() {
                return Err(AppError::BadRequest("invalid WAV format chunk".into()));
            }
            let audio_format = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap());
            let channels = u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap());
            let sample_rate_hz =
                u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap());
            let byte_rate = u32::from_le_bytes(bytes[start + 8..start + 12].try_into().unwrap());
            let block_align = u16::from_le_bytes(bytes[start + 12..start + 14].try_into().unwrap());
            let bits_per_sample =
                u16::from_le_bytes(bytes[start + 14..start + 16].try_into().unwrap());
            let expected_align = channels.checked_mul(2).unwrap_or_default();
            let expected_rate = sample_rate_hz.checked_mul(u32::from(expected_align));
            if audio_format != 1
                || !(1..=8).contains(&channels)
                || sample_rate_hz == 0
                || sample_rate_hz > 384_000
                || bits_per_sample != 16
                || block_align != expected_align
                || expected_rate != Some(byte_rate)
            {
                return Err(AppError::BadRequest(
                    "file must use uncompressed PCM16 WAV audio".into(),
                ));
            }
            format = Some((channels, sample_rate_hz, block_align));
        } else if id == b"data" && data_bytes.replace(size as u64).is_some() {
            return Err(AppError::BadRequest(
                "WAV must contain exactly one audio data chunk".into(),
            ));
        }
        offset = end
            .checked_add(size & 1)
            .filter(|next| *next <= bytes.len())
            .ok_or_else(|| AppError::BadRequest("invalid WAV chunk padding".into()))?;
    }
    if offset != bytes.len() {
        return Err(AppError::BadRequest("invalid WAV trailing data".into()));
    }
    let (channels, sample_rate_hz, block_align) =
        format.ok_or_else(|| AppError::BadRequest("WAV format chunk is required".into()))?;
    let data_bytes = data_bytes
        .ok_or_else(|| AppError::BadRequest("WAV audio data chunk is required".into()))?;
    if data_bytes == 0 || data_bytes % u64::from(block_align) != 0 {
        return Err(AppError::BadRequest("WAV audio data is invalid".into()));
    }
    let frames = data_bytes / u64::from(block_align);
    let rate = u64::from(sample_rate_hz);
    let duration_ms = frames
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(rate / 2))
        .map(|value| value / rate)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| AppError::BadRequest("WAV duration is too large".into()))?;
    let billed_seconds = frames
        .checked_add(rate - 1)
        .map(|value| value / rate)
        .and_then(|value| i64::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| AppError::BadRequest("WAV duration is invalid".into()))?;
    Ok(Pcm16WavDuration {
        channels,
        sample_rate_hz,
        frames,
        duration_ms,
        billed_seconds,
    })
}

fn upstream_form(
    file: AudioFile,
    metadata: &RewrittenAudioMetadata,
    upstream_model: &str,
) -> Result<Form, AppError> {
    let part = Part::bytes(file.bytes.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|_| AppError::Internal)?;
    let mut form = Form::new()
        .part("file", part)
        .text("model", upstream_model.to_owned())
        .text(
            "response_format",
            metadata.response_format.as_str().to_owned(),
        );
    if let Some(prompt) = metadata.prompt.as_ref() {
        form = form.text("prompt", prompt.clone());
    }
    if let Some(hotwords) = metadata.hotwords.as_ref() {
        form = form.text(
            "hotwords",
            serde_json::to_string(hotwords).map_err(|_| AppError::Internal)?,
        );
    }
    Ok(form)
}

async fn read_audio_response_bounded(response: reqwest::Response) -> Result<Bytes, &'static str> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUDIO_RESPONSE_BODY as u64)
    {
        return Err("audio_upstream_response_too_large");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "audio_upstream_response_stream")?;
        if body.len().saturating_add(chunk.len()) > MAX_AUDIO_RESPONSE_BODY {
            return Err("audio_upstream_response_too_large");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

fn normalize_audio_response(
    bytes: &[u8],
    response_format: AudioResponseFormat,
    measured_duration_seconds: f64,
) -> Result<Value, AppError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| AppError::Upstream("audio upstream response must be JSON".into()))?;
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Upstream("audio upstream response must be an object".into()))?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= MAX_AUDIO_RESPONSE_BODY)
        .ok_or_else(|| AppError::Upstream("audio upstream response text is missing".into()))?;
    if response_format == AudioResponseFormat::Json {
        return Ok(json!({"text": text}));
    }
    let mut segments = Vec::new();
    if let Some(upstream_segments) = object.get("segments") {
        let upstream_segments = upstream_segments
            .as_array()
            .ok_or_else(|| AppError::Upstream("audio upstream segments must be an array".into()))?;
        if upstream_segments.len() > 100_000 {
            return Err(AppError::Upstream(
                "audio upstream returned too many segments".into(),
            ));
        }
        for segment in upstream_segments {
            let start = finite_non_negative(segment.get("start"), "start")?;
            let end = finite_non_negative(segment.get("end"), "end")?;
            let text = segment
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| AppError::Upstream("audio segment text is missing".into()))?;
            if end < start {
                return Err(AppError::Upstream(
                    "audio upstream segment timestamps are invalid".into(),
                ));
            }
            segments.push(json!({"start":start,"end":end,"text":text}));
        }
    }
    Ok(json!({
        "text": text,
        "duration": measured_duration_seconds,
        "segments": segments,
    }))
}

fn finite_non_negative(value: Option<&Value>, field: &str) -> Result<f64, AppError> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| AppError::Upstream(format!("audio segment {field} is invalid")))
}

fn metadata_only_locator(metadata: Value) -> Result<String, AppError> {
    Ok(format!(
        "metadata-only-json:{}",
        serde_json::to_string(&metadata).map_err(|_| AppError::Internal)?
    ))
}

async fn finish_audio_failure(
    state: &AppState,
    reservation: &crate::model::UsageReservation,
    request_id: Uuid,
    tenant_id: Uuid,
    audio: Pcm16WavDuration,
    started: Instant,
    error_code: &'static str,
) -> Result<(), AppError> {
    finish_audio_request(
        state,
        reservation,
        request_id,
        tenant_id,
        audio,
        started,
        StatusCode::BAD_GATEWAY,
        0,
        Some(error_code),
        json!({"kind":"audio_transcription","media_archived":false,"succeeded":false}),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_audio_request(
    state: &AppState,
    reservation: &crate::model::UsageReservation,
    request_id: Uuid,
    tenant_id: Uuid,
    audio: Pcm16WavDuration,
    started: Instant,
    status: StatusCode,
    billed_seconds: i64,
    error_code: Option<&str>,
    response_metadata: Value,
) -> Result<(), AppError> {
    let response_object = metadata_only_locator(response_metadata)?;
    state
        .db
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: None,
            first_output_ms: None,
            generation_duration_ms: Some(audio.duration_ms),
            request_id,
            tenant_id,
            reservation,
            input_token_ceiling: 0,
            output_token_ceiling: audio.billed_seconds,
            requested_service_tier: None,
            status_code: i64::from(status.as_u16()),
            duration_ms: started.elapsed().as_millis().min(i64::MAX as u128) as i64,
            usage: TokenUsage {
                output_tokens: billed_seconds,
                ..TokenUsage::default()
            },
            charge_contract_ceiling: false,
            error_code,
            response_object: &response_object,
            conversation: None,
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use rust_decimal::Decimal;
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn wav(sample_rate: u32, channels: u16, frames: u32) -> Vec<u8> {
        let data_size = frames * u32::from(channels) * 2;
        let mut value = Vec::with_capacity(44 + data_size as usize);
        value.extend_from_slice(b"RIFF");
        value.extend_from_slice(&(36 + data_size).to_le_bytes());
        value.extend_from_slice(b"WAVEfmt ");
        value.extend_from_slice(&16_u32.to_le_bytes());
        value.extend_from_slice(&1_u16.to_le_bytes());
        value.extend_from_slice(&channels.to_le_bytes());
        value.extend_from_slice(&sample_rate.to_le_bytes());
        value.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        value.extend_from_slice(&(channels * 2).to_le_bytes());
        value.extend_from_slice(&16_u16.to_le_bytes());
        value.extend_from_slice(b"data");
        value.extend_from_slice(&data_size.to_le_bytes());
        value.resize(44 + data_size as usize, 0);
        value
    }

    fn multipart_body(model: &str, wav: &[u8]) -> (String, Vec<u8>) {
        let boundary = "mtc-audio-test-boundary";
        let mut body = Vec::new();
        for value in [
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n"
            ),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\nverbose_json\r\n"
            ),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nsensitive prompt\r\n"
            ),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"hotwords\"\r\n\r\n[\"private-name\"]\r\n"
            ),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"temperature\"\r\n\r\n0.3\r\n"
            ),
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"sample.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
            ),
        ] {
            body.extend_from_slice(value.as_bytes());
        }
        body.extend_from_slice(wav);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        (boundary.to_owned(), body)
    }

    #[test]
    fn pcm16_wav_duration_preserves_milliseconds_and_bills_whole_seconds() {
        let duration = pcm16_wav_duration(&wav(16_000, 1, 24_001)).unwrap();
        assert_eq!(duration.duration_ms, 1_500);
        assert_eq!(duration.billed_seconds, 2);
        assert_eq!(duration.sample_rate_hz, 16_000);
    }

    #[test]
    fn response_formats_are_normalized_without_requiring_upstream_duration() {
        assert_eq!(
            normalize_audio_response(br#"{"text":"hello"}"#, AudioResponseFormat::Json, 1.5,)
                .unwrap(),
            json!({"text":"hello"})
        );
        assert_eq!(
            normalize_audio_response(
                br#"{"text":"hello","segments":[{"id":1,"start":0,"end":1.4,"text":"hello"}]}"#,
                AudioResponseFormat::VerboseJson,
                1.5,
            )
            .unwrap(),
            json!({"text":"hello","duration":1.5,"segments":[{"start":0.0,"end":1.4,"text":"hello"}]})
        );
    }

    #[tokio::test]
    async fn transcription_routes_bills_measured_wav_seconds_and_archives_only_metadata() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "text":"sensitive transcript",
                "duration":999,
                "segments":[{"id":9,"start":0,"end":1.4,"text":"sensitive transcript"}]
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, _directory) = crate::api::tests::test_state().await;
        let tenant = "audio-transcription-test";
        let model = "local-asr-test";
        let account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "local-asr".to_owned(),
                    driver: "http-json".to_owned(),
                    config: json!({"base_url":upstream.uri(),"network_scope":"private"}),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        let route = state
            .db
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.to_owned(),
                public_model: model.to_owned(),
                upstream_account_id: account.id,
                upstream_model: "asr-upstream".to_owned(),
                protocol: AUDIO_PROTOCOL.to_owned(),
                priority: 0,
            })
            .await
            .unwrap();
        let issued = state
            .db
            .create_key_with_routing(
                CreateKeyInput {
                    tenant_external_id: tenant.to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "audio".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                &[route.id],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        state
            .db
            .upsert_generation_price(model, "USD", "second", Decimal::new(25, 2))
            .await
            .unwrap();
        let (boundary, multipart) = multipart_body(model, &wav(16_000, 1, 24_001));
        let response = router_for_role(state.clone(), RuntimeRole::Gateway)
            .oneshot(
                Request::post("/v1/audio/transcriptions")
                    .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(multipart))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id: Uuid = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let response_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), MAX_AUDIO_RESPONSE_BODY)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response_body["text"], "sensitive transcript");
        assert_eq!(response_body["duration"], 1.5000625);
        assert_eq!(
            response_body["segments"][0],
            json!({
                "start":0.0,"end":1.4,"text":"sensitive transcript"
            })
        );
        let requests = upstream.received_requests().await.unwrap();
        assert!(
            requests[0]
                .body
                .windows(b"asr-upstream".len())
                .any(|window| window == b"asr-upstream")
        );

        let refs = state
            .db
            .request_archive_refs(issued.key_id, request_id)
            .await
            .unwrap();
        assert_eq!(refs.view.protocol, AUDIO_AUDIT_PROTOCOL);
        assert_eq!(refs.view.generation_duration_ms, Some(1_500));
        assert_eq!(refs.view.input_tokens, 0);
        assert_eq!(refs.view.output_tokens, 0);
        assert!(refs.view.usage.tokens.is_none());
        assert_eq!(
            refs.view.usage.generation.as_ref().unwrap().billed_units,
            Some(2)
        );
        assert_eq!(
            refs.view.archive_state,
            crate::model::RequestArchiveState::MetadataOnly
        );
        assert!(refs.request_object.starts_with("metadata-only-json:"));
        assert!(!refs.request_object.contains("sensitive prompt"));
        assert!(!refs.request_object.contains("private-name"));
        assert!(
            !refs
                .response_object
                .unwrap()
                .contains("sensitive transcript")
        );
        let authenticated = state
            .db
            .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
            .await
            .unwrap();
        assert_eq!(
            state
                .db
                .key_view(&authenticated)
                .await
                .unwrap()
                .available_balance,
            "9.5"
        );
    }
}
