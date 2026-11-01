//! Built-in asynchronous generation protocol selection and admission contract.
//!
//! Provider-specific HTTP execution still lives beside the worker lifecycle,
//! but route recognition, downstream modality, request normalization and
//! reservation units must agree before a job is persisted.  Keeping those
//! decisions behind this adapter boundary prevents a new image/video provider
//! from being wired into several unrelated match statements with subtly
//! different eligibility rules.
//!
//! This module deliberately contains only protocols whose wire contracts are
//! implemented and covered by the worker.  A future MiniMax adapter belongs
//! here after its submit, poll, cancellation and asset contracts are known; an
//! account URL or model name alone must never select an unimplemented dialect.

use serde_json::Value;

use crate::{error::AppError, provider::ResolvedUpstream};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenerationProtocolAdapter {
    SeedanceV3,
    ComfyUi,
    SiliconFlowVideoV1,
    WasuWanV1,
    TkmarketSeedanceV1,
}

impl GenerationProtocolAdapter {
    pub(crate) fn for_route(route: &ResolvedUpstream) -> Result<Self, AppError> {
        Self::detect(&route.driver, &route.config, &route.upstream_model)
            .ok_or_else(|| AppError::upstream("generation_driver_cannot_execute_asynchronous_jobs"))
    }

    pub(crate) fn detect(driver: &str, config: &Value, upstream_model: &str) -> Option<Self> {
        match driver {
            "volcengine-seedance" => Some(Self::SeedanceV3),
            "comfyui" => Some(Self::ComfyUi),
            "http-json"
                if config.get("video_api").and_then(Value::as_str) == Some("wasu-wan-v1")
                    && upstream_model == "wan3.0-video"
                    && config
                        .get("video_models")
                        .and_then(Value::as_array)
                        .is_some_and(|models| {
                            models
                                .iter()
                                .any(|model| model.as_str() == Some(upstream_model))
                        }) =>
            {
                Some(Self::WasuWanV1)
            }
            "http-json"
                if config.get("video_api").and_then(Value::as_str)
                    == Some(super::tkmarket_seedance::PROFILE)
                    && super::tkmarket_seedance::MODELS.contains(&upstream_model)
                    && config
                        .get("video_models")
                        .and_then(Value::as_array)
                        .is_some_and(|models| {
                            models
                                .iter()
                                .any(|model| model.as_str() == Some(upstream_model))
                        }) =>
            {
                Some(Self::TkmarketSeedanceV1)
            }
            "http-json" if is_siliconflow_video_profile(config, upstream_model) => {
                Some(Self::SiliconFlowVideoV1)
            }
            _ => None,
        }
    }

    pub(crate) fn supports_modality(self, modality: &str) -> bool {
        match self {
            Self::SeedanceV3
            | Self::SiliconFlowVideoV1
            | Self::WasuWanV1
            | Self::TkmarketSeedanceV1 => {
                matches!(modality, "video")
            }
            Self::ComfyUi => matches!(modality, "image" | "video"),
        }
    }

    /// Modalities advertised for a route in the public model catalog.  A
    /// SiliconFlow-profiled HTTP account retains the standard synchronous
    /// OpenAI Images contract alongside its asynchronous video adapter.
    pub(crate) const fn catalog_modalities(self) -> &'static [&'static str] {
        match self {
            Self::SeedanceV3 => &["video"],
            Self::WasuWanV1 | Self::TkmarketSeedanceV1 => &["video"],
            Self::ComfyUi => &["image", "video"],
            Self::SiliconFlowVideoV1 => &["image", "video"],
        }
    }

    pub(crate) fn normalize_input(self, input: &mut Value) -> Result<(), AppError> {
        match self {
            Self::WasuWanV1 => super::wasu_wan::validate_input(input),
            Self::TkmarketSeedanceV1 => super::tkmarket_seedance::validate_input(input),
            Self::SeedanceV3 => {
                normalize_seedance_duration(input)?;
                Ok(())
            }
            Self::SiliconFlowVideoV1 => {
                super::validate_siliconflow_video_parameters(input)?;
                Ok(())
            }
            // ComfyUI validates its versioned workflow parameters again at
            // dispatch, against the frozen account snapshot.
            Self::ComfyUi => Ok(()),
        }
    }

    pub(crate) fn estimated_units(
        self,
        billing_unit: &str,
        input: &Value,
    ) -> Result<i64, AppError> {
        match (self, billing_unit) {
            // Zero means no supplier-guaranteed token ceiling. Admission requires
            // the existing metered-unlimited policy, never a guessed hard cap.
            (Self::TkmarketSeedanceV1, "million_tokens") => {
                super::tkmarket_seedance::validate_input(input)?;
                Ok(0)
            }
            (Self::TkmarketSeedanceV1, _) => Err(AppError::BadRequest(
                "tkmarket video requires million_tokens customer pricing".into(),
            )),
            (Self::WasuWanV1, "second") => {
                super::wasu_wan::validate_input(input)?;
                Ok(5)
            }
            (Self::WasuWanV1, _) => Err(AppError::BadRequest(
                "Wasu Wan generation price must use second billing".into(),
            )),
            (Self::SeedanceV3, "second") => {
                let units = input
                    .get("duration")
                    .and_then(Value::as_i64)
                    .ok_or(AppError::internal("protocol_estimated_units_1"))?;
                if !(1..=60).contains(&units) {
                    return Err(AppError::internal("protocol_estimated_units_2"));
                }
                Ok(units)
            }
            (Self::ComfyUi, "job") | (Self::SiliconFlowVideoV1, "job") => Ok(1),
            (Self::ComfyUi, "megapixel") => super::comfyui_requested_pixels(input),
            (Self::SeedanceV3, _) => Err(AppError::BadRequest(
                "Seedance generation price must use second billing".into(),
            )),
            (Self::ComfyUi, _) => Err(AppError::BadRequest(
                "ComfyUI generation price must use job or megapixel billing".into(),
            )),
            (Self::SiliconFlowVideoV1, _) => Err(AppError::BadRequest(
                "SiliconFlow video generation price must use job billing".into(),
            )),
        }
    }

    pub(crate) fn parameter_schema(self, config: &Value) -> Option<Value> {
        match self {
            Self::WasuWanV1 => Some(super::wasu_wan::input_schema()),
            Self::TkmarketSeedanceV1 => Some(super::tkmarket_seedance::input_schema()),
            Self::ComfyUi => super::comfyui_parameter_schema(config).ok(),
            Self::SiliconFlowVideoV1 => Some(super::siliconflow_video_parameter_schema()),
            Self::SeedanceV3 => None,
        }
    }
}

fn is_siliconflow_video_profile(config: &Value, upstream_model: &str) -> bool {
    config.get("video_api").and_then(Value::as_str) == Some("siliconflow-v1")
        && config
            .get("video_models")
            .and_then(Value::as_array)
            .is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model.as_str() == Some(upstream_model))
            })
}

pub(crate) fn normalize_seedance_duration(input: &mut Value) -> Result<i64, AppError> {
    let object = input
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("generation input must be a JSON object".into()))?;
    let explicit = match object.get("duration") {
        None => None,
        Some(Value::Number(value)) => Some(value.as_i64().ok_or_else(|| {
            AppError::BadRequest("Seedance duration must be a JSON integer".into())
        })?),
        Some(_) => {
            return Err(AppError::BadRequest(
                "Seedance duration must be a JSON integer".into(),
            ));
        }
    };
    if explicit.is_some_and(|duration| !(1..=60).contains(&duration)) {
        return Err(AppError::BadRequest(
            "Seedance duration must be between 1 and 60 seconds".into(),
        ));
    }

    let mut content_duration = None;
    if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
        for item in content {
            let Some(text) = item.get_mut("text") else {
                continue;
            };
            let Some(original) = text.as_str() else {
                continue;
            };
            let tokens = original.split_whitespace().collect::<Vec<_>>();
            let mut normalized = Vec::with_capacity(tokens.len());
            let mut index = 0;
            let mut removed_duration = false;
            while index < tokens.len() {
                let token = tokens[index];
                if token == "--dur" {
                    if content_duration.is_some() {
                        return Err(AppError::BadRequest(
                            "Seedance content must contain at most one --dur option".into(),
                        ));
                    }
                    let raw = tokens.get(index + 1).ok_or_else(|| {
                        AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        )
                    })?;
                    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        ));
                    }
                    let duration = raw.parse::<i64>().map_err(|_| {
                        AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        )
                    })?;
                    if !(1..=60).contains(&duration) {
                        return Err(AppError::BadRequest(
                            "Seedance duration must be between 1 and 60 seconds".into(),
                        ));
                    }
                    content_duration = Some(duration);
                    removed_duration = true;
                    index += 2;
                    continue;
                }
                if token.starts_with("--dur") {
                    return Err(AppError::BadRequest(
                        "Seedance content contains a malformed --dur option".into(),
                    ));
                }
                normalized.push(token);
                index += 1;
            }
            if removed_duration {
                *text = Value::String(normalized.join(" "));
            }
        }
    }
    if explicit.is_some() && content_duration.is_some() && explicit != content_duration {
        return Err(AppError::BadRequest(
            "Seedance duration conflicts with content --dur".into(),
        ));
    }
    let duration = explicit.or(content_duration).unwrap_or(5);
    object.insert("duration".to_owned(), Value::from(duration));
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_explicitly_implemented_route_contracts_select_an_adapter() {
        assert_eq!(
            GenerationProtocolAdapter::detect("volcengine-seedance", &json!({}), "model"),
            Some(GenerationProtocolAdapter::SeedanceV3)
        );
        assert_eq!(
            GenerationProtocolAdapter::detect("comfyui", &json!({}), "workflow"),
            Some(GenerationProtocolAdapter::ComfyUi)
        );
        assert_eq!(
            GenerationProtocolAdapter::detect(
                "http-json",
                &json!({
                    "video_api": "siliconflow-v1",
                    "video_models": ["video-model"]
                }),
                "video-model"
            ),
            Some(GenerationProtocolAdapter::SiliconFlowVideoV1)
        );
        assert_eq!(
            GenerationProtocolAdapter::detect(
                "http-json",
                &json!({
                    "video_api": "siliconflow-v1",
                    "video_models": ["another-model"]
                }),
                "video-model"
            ),
            None
        );
        assert_eq!(
            GenerationProtocolAdapter::detect(
                "http-json",
                &json!({"video_api": "minimax-h3"}),
                "video-model"
            ),
            None
        );
    }

    #[test]
    fn adapter_owns_modality_normalization_and_billing_contracts() {
        let mut seedance = json!({"content": [{"type": "text", "text": "fox --dur 7"}]});
        GenerationProtocolAdapter::SeedanceV3
            .normalize_input(&mut seedance)
            .unwrap();
        assert_eq!(seedance["duration"], 7);
        assert!(GenerationProtocolAdapter::SeedanceV3.supports_modality("video"));
        assert!(!GenerationProtocolAdapter::SeedanceV3.supports_modality("image"));
        assert_eq!(
            GenerationProtocolAdapter::SeedanceV3
                .estimated_units("second", &seedance)
                .unwrap(),
            7
        );
        assert!(
            GenerationProtocolAdapter::SeedanceV3
                .estimated_units("job", &seedance)
                .is_err()
        );
    }
}
