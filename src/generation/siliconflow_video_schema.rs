use serde_json::{Map, Value, json};

use crate::error::AppError;

const MAX_PROMPT_CHARS: usize = 32_000;

pub(crate) fn parameter_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": ["prompt", "image_size"],
        "properties": {
            "prompt": {
                "title": "Prompt",
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_PROMPT_CHARS
            },
            "image_size": {
                "title": "Image size",
                "type": "string",
                "enum": ["1280x720", "720x1280", "960x960"],
                "default": "1280x720"
            },
            "negative_prompt": {
                "title": "Negative prompt",
                "type": "string",
                "maxLength": MAX_PROMPT_CHARS
            },
            "seed": {
                "title": "Seed",
                "type": "integer"
            }
        }
    })
}

pub(crate) fn validated_submit_parameters(input: &Value) -> Result<Map<String, Value>, AppError> {
    let input = input.as_object().ok_or_else(|| {
        AppError::BadRequest("SiliconFlow video input must be a JSON object".into())
    })?;
    if input.len() != 1 {
        return Err(AppError::BadRequest(
            "SiliconFlow video input accepts only parameters".into(),
        ));
    }
    let parameters = input
        .get("parameters")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::BadRequest("SiliconFlow video parameters are required".into()))?;
    if parameters.len() > 4
        || parameters.keys().any(|key| {
            !matches!(
                key.as_str(),
                "prompt" | "image_size" | "negative_prompt" | "seed"
            )
        })
    {
        return Err(AppError::BadRequest(
            "SiliconFlow video parameters contain an unsupported field".into(),
        ));
    }
    let prompt = parameters
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("SiliconFlow video prompt is required".into()))?;
    if prompt.trim().is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(AppError::BadRequest(
            "SiliconFlow video prompt must contain 1 to 32000 characters".into(),
        ));
    }
    let image_size = parameters
        .get("image_size")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("SiliconFlow video image_size is required".into()))?;
    if !matches!(image_size, "1280x720" | "720x1280" | "960x960") {
        return Err(AppError::BadRequest(
            "SiliconFlow video image_size is unsupported".into(),
        ));
    }
    if parameters.get("negative_prompt").is_some_and(|value| {
        value
            .as_str()
            .is_none_or(|value| value.chars().count() > MAX_PROMPT_CHARS)
    }) {
        return Err(AppError::BadRequest(
            "SiliconFlow video negative_prompt must be a string of at most 32000 characters".into(),
        ));
    }
    if parameters
        .get("seed")
        .is_some_and(|value| value.as_i64().is_none())
    {
        return Err(AppError::BadRequest(
            "SiliconFlow video seed must be a JSON integer".into(),
        ));
    }
    Ok(parameters.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_closed_and_t2v_only() {
        let valid = json!({"parameters": {
            "prompt": "a fox in the wind",
            "image_size": "1280x720",
            "negative_prompt": "blur",
            "seed": 42
        }});
        assert_eq!(validated_submit_parameters(&valid).unwrap().len(), 4);

        for invalid in [
            json!({"parameters": {"prompt": "cat", "image_size": "1024x1024"}}),
            json!({"parameters": {"prompt": "cat", "image_size": "1280x720", "image": "https://example.test/private.png"}}),
            json!({"parameters": {"prompt": "", "image_size": "1280x720"}}),
            json!({"parameters": {"prompt": "cat", "image_size": "1280x720", "seed": 1.5}}),
            json!({"parameters": {"prompt": "cat", "image_size": "1280x720"}, "duration": 5}),
        ] {
            assert!(validated_submit_parameters(&invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn published_schema_matches_the_validated_parameter_surface() {
        let schema = parameter_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["prompt", "image_size"]));
        assert_eq!(
            schema["properties"]["image_size"]["enum"],
            json!(["1280x720", "720x1280", "960x960"])
        );
        assert!(schema["properties"].get("image").is_none());
    }
}
