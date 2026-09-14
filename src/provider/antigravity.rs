//! Native Antigravity wire protocol. No account import, broker or generated credentials.
//! All network operations share the account's explicit outbound proxy policy.
use std::{collections::BTreeMap, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{error::AppError, network, provider::UpstreamCredential};

pub const DRIVER: &str = "google-antigravity";
pub const BASE_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CONTROL_URL: &str = "https://cloudcode-pa.googleapis.com";

const IMAGE_LIMIT: usize = 32 * 1024 * 1024;
const CONTROL_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub base_url: String,
    pub control_url: String,
    pub project_id: String,
    #[serde(default)]
    pub request_headers: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: BASE_URL.into(),
            control_url: CONTROL_URL.into(),
            project_id: String::new(),
            request_headers: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn apply_headers(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, AppError> {
        if self.request_headers.len() > 64 {
            return Err(AppError::BadRequest("too many request headers".into()));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &self.request_headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AppError::BadRequest("invalid request header".into()))?;
            if matches!(
                name.as_str(),
                "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "upgrade"
                    | "trailer"
                    | "te"
            ) || value.len() > 8192
            {
                return Err(AppError::BadRequest(
                    "request header overrides HTTP framing".into(),
                ));
            }
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| AppError::BadRequest("invalid request header".into()))?;
            headers.insert(name, value);
        }
        Ok(request.headers(headers))
    }
    pub fn from_account(value: &Value) -> Result<Self, AppError> {
        let mut config = Self::default();
        if let Some(base) = value.get("base_url").and_then(Value::as_str) {
            config.base_url = base.to_owned();
        }
        if let Some(base) = value.get("control_url").and_then(Value::as_str) {
            config.control_url = base.to_owned();
        }
        if let Some(project) = value.get("project_id").and_then(Value::as_str) {
            config.project_id = project.to_owned();
        }
        if let Some(headers) = value.get("request_headers") {
            config.request_headers = serde_json::from_value(headers.clone())
                .map_err(|_| AppError::BadRequest("invalid request headers".into()))?;
        }
        Ok(config)
    }
}

/// Catalog names are labels, never a promise of entitlement or availability.
/// Only exact IDs reported by the account's live catalog may be routed.
pub fn model_display_name(id: &str) -> &str {
    match id {
        "gemini-2.5-flash-image" => "Nano Banana",
        "gemini-3-pro-image" | "gemini-3-pro-image-preview" => "Nano Banana Pro",
        "gemini-3.1-flash-image" | "gemini-3.1-flash-image-preview" => "Nano Banana 2",
        _ => id,
    }
}

pub struct NativeClient<'a> {
    pub http: &'a reqwest::Client,
    pub credential: &'a UpstreamCredential,
    pub config: &'a Config,
    pub allow_test_loopback: bool,
}

impl NativeClient<'_> {
    async fn post(
        &self,
        base: &str,
        path: &str,
        body: &Value,
        limit: usize,
    ) -> Result<Value, AppError> {
        let endpoint = format!("{}{path}", base.trim_end_matches('/'));
        let http = network::client_for_config_url_no_retry(
            self.http,
            &endpoint,
            &json!({"network_scope": "public"}),
            self.credential.proxy(),
            self.allow_test_loopback,
        )
        .await?;
        let request = http
            .post(endpoint)
            .timeout(Duration::from_secs(180))
            .json(body);
        let request = self.credential.apply(request, crate::db::unix_millis())?;
        let response = self
            .config
            .apply_headers(request)?
            .send()
            .await
            .map_err(|_| AppError::Upstream("Antigravity request failed".into()))?;
        let status = response.status();
        let body = bounded_body(response, limit).await?;
        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "Antigravity returned HTTP {}",
                status.as_u16()
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|_| AppError::Upstream("invalid Antigravity response".into()))
    }

    /// Read the existing project only. Never enroll, accept terms, reset quota,
    /// synthesize a project ID or cross-fallback between consumer/enterprise tiers.
    pub async fn discover_project(&self) -> Result<String, AppError> {
        let response = self
            .post(
                &self.config.control_url,
                "/v1internal:loadCodeAssist",
                &json!({"metadata": {"ideType": "ANTIGRAVITY"}}),
                CONTROL_LIMIT,
            )
            .await?;
        response
            .get("cloudaicompanionProject")
            .and_then(|project| {
                project
                    .as_str()
                    .or_else(|| project.get("id").and_then(Value::as_str))
            })
            .filter(|id| !id.trim().is_empty() && id.len() <= 256)
            .map(str::to_owned)
            .ok_or_else(|| {
                AppError::Conflict(
                    "Antigravity account requires project onboarding before it can be connected"
                        .into(),
                )
            })
    }

    pub async fn list_models(&self) -> Result<Vec<String>, AppError> {
        self.validate_project()?;
        let response = self
            .post(
                &self.config.base_url,
                "/v1internal:fetchAvailableModels",
                &json!({"project": self.config.project_id}),
                CONTROL_LIMIT,
            )
            .await?;
        let models = response
            .get("models")
            .and_then(Value::as_object)
            .ok_or_else(|| AppError::Upstream("Antigravity model catalog is invalid".into()))?;
        if models.len() > 4096 {
            return Err(AppError::Upstream(
                "Antigravity model catalog is too large".into(),
            ));
        }
        Ok(models
            .keys()
            .filter(|id| !id.is_empty() && id.len() <= 200)
            .cloned()
            .collect())
    }

    /// Exactly one paid operation; callers must establish their durable spend
    /// fence before invoking this method and must not replay an ambiguous result.
    pub async fn generate_image(
        &self,
        model: &str,
        request: Value,
    ) -> Result<ImageResponse, AppError> {
        self.validate_project()?;
        let payload = image_request(model, &self.config.project_id, request)?;
        let response = self
            .post(
                &self.config.base_url,
                "/v1internal:generateContent",
                &payload,
                IMAGE_LIMIT,
            )
            .await?;
        decode_images(&response)
    }

    fn validate_project(&self) -> Result<(), AppError> {
        if self.config.project_id.trim().is_empty() || self.config.project_id.len() > 256 {
            return Err(AppError::BadRequest(
                "Antigravity project is required".into(),
            ));
        }
        Ok(())
    }
}

pub fn openai_image_request(
    model: &str,
    project: &str,
    request: &Value,
) -> Result<Value, AppError> {
    let prompt = request
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| AppError::BadRequest("image prompt is required".into()))?;
    if request
        .get("n")
        .and_then(Value::as_i64)
        .is_some_and(|n| n != 1)
    {
        return Err(AppError::BadRequest(
            "Antigravity image generation requires n=1".into(),
        ));
    }
    let mut generation_config = json!({});
    if let Some(size) = request.get("size").and_then(Value::as_str) {
        let ratio = match size {
            "auto" => None,
            "1024x1024" => Some("1:1"),
            "1536x1024" => Some("3:2"),
            "1024x1536" => Some("2:3"),
            _ => {
                return Err(AppError::BadRequest(
                    "unsupported Antigravity image size".into(),
                ));
            }
        };
        if let Some(ratio) = ratio {
            generation_config["imageConfig"] = json!({"aspectRatio": ratio});
        }
    }
    image_request(
        model,
        project,
        json!({"contents": [{"role": "user", "parts": [{"text": prompt}]}], "generationConfig": generation_config}),
    )
}

pub fn image_request(model: &str, project: &str, mut request: Value) -> Result<Value, AppError> {
    if !model.contains("image") || model.len() > 200 || project.trim().is_empty() {
        return Err(AppError::BadRequest(
            "an Antigravity image model and project are required".into(),
        ));
    }
    let object = request
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Gemini image request must be an object".into()))?;
    if object
        .get("contents")
        .and_then(Value::as_array)
        .is_none_or(|contents| contents.is_empty())
    {
        return Err(AppError::BadRequest(
            "Gemini image contents are required".into(),
        ));
    }
    // Do not delete or weaken caller safety settings. Reject caller attempts
    // to smuggle routing/credential authority inside the native request.
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "contents" | "generationConfig" | "safetySettings" | "systemInstruction"
        )
    }) {
        return Err(AppError::BadRequest(
            "unsupported Gemini image request field".into(),
        ));
    }
    let config = object
        .entry("generationConfig")
        .or_insert_with(|| json!({}));
    let config = config
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("generationConfig must be an object".into()))?;
    config.insert("responseModalities".into(), json!(["TEXT", "IMAGE"]));
    Ok(json!({
        "model": model,
        "project": project,
        "userAgent": "antigravity",
        "requestType": "image_gen",
        "requestId": format!("image_gen/{}/{}/12", crate::db::unix_millis(), Uuid::now_v7()),
        "request": request,
    }))
}

pub struct GeneratedImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

pub struct ImageResponse {
    pub images: Vec<GeneratedImage>,
    pub text: Vec<String>,
    pub usage: Value,
}

pub fn decode_images(envelope: &Value) -> Result<ImageResponse, AppError> {
    let response = envelope.get("response").unwrap_or(envelope);
    let candidates = response
        .get("candidates")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Upstream("Antigravity image response has no candidates".into()))?;
    let mut result = ImageResponse {
        images: vec![],
        text: vec![],
        usage: response
            .get("usageMetadata")
            .cloned()
            .unwrap_or(Value::Null),
    };
    let mut total = 0usize;
    for candidate in candidates {
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    result.text.push(text.to_owned());
                }
                let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data"))
                else {
                    continue;
                };
                let mime = inline
                    .get("mimeType")
                    .or_else(|| inline.get("mime_type"))
                    .and_then(Value::as_str)
                    .filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/webp"))
                    .ok_or_else(|| {
                        AppError::Upstream("Antigravity image MIME type is unsupported".into())
                    })?;
                let data = inline
                    .get("data")
                    .and_then(Value::as_str)
                    .filter(|data| data.len() <= IMAGE_LIMIT)
                    .ok_or_else(|| {
                        AppError::Upstream("Antigravity image data is invalid".into())
                    })?;
                let bytes = STANDARD.decode(data).map_err(|_| {
                    AppError::Upstream("Antigravity image encoding is invalid".into())
                })?;
                total = total.saturating_add(bytes.len());
                if bytes.is_empty() || total > IMAGE_LIMIT || result.images.len() >= 16 {
                    return Err(AppError::Upstream(
                        "Antigravity image output exceeds limits".into(),
                    ));
                }
                result.images.push(GeneratedImage {
                    mime_type: mime.into(),
                    bytes,
                });
            }
        }
    }
    if result.images.is_empty() {
        return Err(AppError::Upstream("Antigravity returned no image".into()));
    }
    Ok(result)
}

async fn bounded_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(AppError::Upstream(
            "Antigravity response exceeds limit".into(),
        ));
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|_| AppError::Upstream("Antigravity response interrupted".into()))?;
        if output.len().saturating_add(chunk.len()) > limit {
            return Err(AppError::Upstream(
                "Antigravity response exceeds limit".into(),
            ));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    #[test]
    fn image_wire_envelope_preserves_safety_and_rejects_routing_override() {
        let request = image_request("gemini-3-pro-image", "project-fixture", json!({"contents": [{"parts": [{"text": "a tree"}]}], "safetySettings": [{"category": "example"}]})).unwrap();
        assert_eq!(request["requestType"], "image_gen");
        assert_eq!(
            request["request"]["generationConfig"]["responseModalities"],
            json!(["TEXT", "IMAGE"])
        );
        assert!(request["request"].get("safetySettings").is_some());
        assert!(
            image_request(
                "gemini-3-pro-image",
                "project-fixture",
                json!({"contents": [{}], "project": "other"})
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn native_catalog_and_image_use_google_wire_protocol() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:fetchAvailableModels"))
            .and(header("authorization", "Bearer fixture-access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"models": {"gemini-3-pro-image": {}}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST")).and(path("/v1internal:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"response": {"candidates": [{"content": {"parts": [{"inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}}]}}], "usageMetadata": {"totalTokenCount": 7}}})))
            .expect(1).mount(&server).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let config = Config {
            base_url: server.uri(),
            control_url: server.uri(),
            project_id: "project-fixture".into(),
            ..Config::default()
        };
        let client = NativeClient {
            http: &http,
            credential: &credential,
            config: &config,
            allow_test_loopback: true,
        };
        assert_eq!(
            client.list_models().await.unwrap(),
            vec!["gemini-3-pro-image"]
        );
        let response = client
            .generate_image(
                "gemini-3-pro-image",
                json!({"contents": [{"parts": [{"text": "a tree"}]}]}),
            )
            .await
            .unwrap();
        assert_eq!(response.images[0].bytes, b"image");
    }
}
