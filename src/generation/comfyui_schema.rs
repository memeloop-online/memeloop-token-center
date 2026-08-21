use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::error::AppError;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_PARAMETERS: usize = 100;
const MAX_WORKFLOW_DEPTH: usize = 64;
const MAX_WORKFLOW_NODES: usize = 20_000;

pub fn validate_config(config: &Value) -> Result<(), AppError> {
    let workflow = config
        .get("workflow_template")
        .ok_or_else(|| AppError::BadRequest("ComfyUI workflow_template is required".into()))?;
    let placeholders = workflow_placeholders(workflow)?;
    if let Some(schema) = config.get("parameter_schema") {
        let _ = sanitize_parameter_schema(schema, &placeholders)?;
    }
    Ok(())
}

pub fn effective_parameter_schema(config: &Value) -> Result<Value, AppError> {
    let workflow = config
        .get("workflow_template")
        .ok_or_else(|| AppError::BadRequest("ComfyUI workflow_template is required".into()))?;
    let placeholders = workflow_placeholders(workflow)?;
    match config.get("parameter_schema") {
        Some(schema) => sanitize_parameter_schema(schema, &placeholders),
        None => Ok(legacy_parameter_schema(&placeholders)),
    }
}

pub fn validate_parameters(config: &Value, parameters: &Value) -> Result<(), AppError> {
    crate::schema::validate_instance(&effective_parameter_schema(config)?, parameters)
}

fn workflow_placeholders(workflow: &Value) -> Result<BTreeSet<String>, AppError> {
    let mut names = BTreeSet::new();
    let mut nodes = 0usize;
    collect_placeholders(workflow, &mut names, 0, &mut nodes)?;
    if names.len() > MAX_PARAMETERS {
        return Err(AppError::BadRequest(
            "ComfyUI workflow exceeds the 100 parameter limit".into(),
        ));
    }
    Ok(names)
}

fn collect_placeholders(
    value: &Value,
    names: &mut BTreeSet<String>,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), AppError> {
    *nodes = nodes.saturating_add(1);
    if depth > MAX_WORKFLOW_DEPTH || *nodes > MAX_WORKFLOW_NODES {
        return Err(AppError::BadRequest(
            "ComfyUI workflow exceeds the structural limit".into(),
        ));
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect_placeholders(value, names, depth + 1, nodes)?;
            }
        }
        Value::Object(object) => {
            if object.contains_key("$mtc_param") {
                if object.len() != 1 {
                    return Err(AppError::BadRequest(
                        "$mtc_param must be the only field in its placeholder object".into(),
                    ));
                }
                let name = object["$mtc_param"].as_str().ok_or_else(|| {
                    AppError::BadRequest("$mtc_param must contain a parameter name".into())
                })?;
                if !safe_parameter_name(name) {
                    return Err(AppError::BadRequest(
                        "ComfyUI parameter names must be safe identifiers".into(),
                    ));
                }
                names.insert(name.to_owned());
            } else {
                for value in object.values() {
                    collect_placeholders(value, names, depth + 1, nodes)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn sanitize_parameter_schema(
    schema: &Value,
    placeholders: &BTreeSet<String>,
) -> Result<Value, AppError> {
    if serde_json::to_vec(schema)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_SCHEMA_BYTES
    {
        return Err(AppError::BadRequest(
            "ComfyUI parameter_schema exceeds 64 KiB".into(),
        ));
    }
    let root = schema.as_object().ok_or_else(|| {
        AppError::BadRequest("ComfyUI parameter_schema must be an object schema".into())
    })?;
    reject_unknown_keys(
        root,
        &[
            "$schema",
            "title",
            "description",
            "type",
            "properties",
            "required",
            "additionalProperties",
        ],
    )?;
    if root.get("type").and_then(Value::as_str) != Some("object")
        || root.get("additionalProperties").and_then(Value::as_bool) != Some(false)
    {
        return Err(AppError::BadRequest(
            "ComfyUI parameter_schema must be a closed object schema".into(),
        ));
    }
    let properties = root
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            AppError::BadRequest("ComfyUI parameter_schema properties are required".into())
        })?;
    let property_names = properties.keys().cloned().collect::<BTreeSet<_>>();
    let required = root
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError::BadRequest("ComfyUI parameter_schema required is required".into())
        })?;
    let required_names = required
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                AppError::BadRequest("ComfyUI required entries must be strings".into())
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if &property_names != placeholders || &required_names != placeholders {
        return Err(AppError::BadRequest(
            "ComfyUI parameter_schema properties and required must exactly match workflow placeholders"
                .into(),
        ));
    }

    let mut sanitized_properties = Map::new();
    for name in placeholders {
        sanitized_properties.insert(name.clone(), sanitize_scalar_schema(&properties[name])?);
    }
    let mut sanitized = Map::from_iter([
        (
            "$schema".into(),
            Value::String("https://json-schema.org/draft/2020-12/schema".into()),
        ),
        ("type".into(), Value::String("object".into())),
        ("additionalProperties".into(), Value::Bool(false)),
        (
            "required".into(),
            Value::Array(placeholders.iter().cloned().map(Value::String).collect()),
        ),
        ("properties".into(), Value::Object(sanitized_properties)),
    ]);
    for key in ["title", "description"] {
        if let Some(value) = root.get(key) {
            sanitized.insert(key.into(), bounded_text(value, key, 4_096)?);
        }
    }
    let sanitized = Value::Object(sanitized);
    crate::schema::validate_definition(&sanitized)?;
    Ok(sanitized)
}

fn sanitize_scalar_schema(schema: &Value) -> Result<Value, AppError> {
    let object = schema.as_object().ok_or_else(|| {
        AppError::BadRequest("ComfyUI parameter properties must be schema objects".into())
    })?;
    const ALLOWED: &[&str] = &[
        "title",
        "description",
        "type",
        "enum",
        "default",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "pattern",
    ];
    reject_unknown_keys(object, ALLOWED)?;
    validate_scalar_types(object.get("type").ok_or_else(|| {
        AppError::BadRequest("ComfyUI parameter property type is required".into())
    })?)?;
    if let Some(values) = object.get("enum") {
        let values = values
            .as_array()
            .filter(|values| !values.is_empty() && values.len() <= 64)
            .ok_or_else(|| {
                AppError::BadRequest("ComfyUI enum must contain 1 to 64 values".into())
            })?;
        if values
            .iter()
            .any(|value| value.is_array() || value.is_object())
        {
            return Err(AppError::BadRequest(
                "ComfyUI enum values must be scalar".into(),
            ));
        }
    }
    for key in ["minimum", "maximum"] {
        if object.get(key).is_some_and(|value| !value.is_number()) {
            return Err(AppError::BadRequest(format!(
                "ComfyUI {key} must be numeric"
            )));
        }
    }
    for key in ["minLength", "maxLength"] {
        if object
            .get(key)
            .is_some_and(|value| value.as_u64().is_none_or(|value| value > 4_096))
        {
            return Err(AppError::BadRequest(format!("ComfyUI {key} exceeds 4096")));
        }
    }
    if object
        .get("pattern")
        .is_some_and(|value| value.as_str().is_none_or(|value| value.len() > 256))
    {
        return Err(AppError::BadRequest(
            "ComfyUI pattern exceeds 256 bytes".into(),
        ));
    }
    let mut sanitized = BTreeMap::new();
    for key in ALLOWED {
        if let Some(value) = object.get(*key) {
            let value = if matches!(*key, "title" | "description") {
                bounded_text(value, key, 4_096)?
            } else {
                value.clone()
            };
            sanitized.insert((*key).to_owned(), value);
        }
    }
    let sanitized = serde_json::to_value(sanitized).map_err(|_| AppError::Internal)?;
    if let Some(default) = sanitized.get("default") {
        crate::schema::validate_instance(&sanitized, default).map_err(|_| {
            AppError::BadRequest("ComfyUI parameter default does not match its type".into())
        })?;
    }
    if let Some(values) = sanitized.get("enum").and_then(Value::as_array) {
        for value in values {
            crate::schema::validate_instance(&sanitized, value).map_err(|_| {
                AppError::BadRequest("ComfyUI enum value does not match its type".into())
            })?;
        }
    }
    Ok(sanitized)
}

fn validate_scalar_types(value: &Value) -> Result<(), AppError> {
    let types = match value {
        Value::String(value) => vec![value.as_str()],
        Value::Array(values) if !values.is_empty() && values.len() <= 5 => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| AppError::BadRequest("ComfyUI types must be strings".into()))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(AppError::BadRequest(
                "ComfyUI parameter type is invalid".into(),
            ));
        }
    };
    let unique = types.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != types.len()
        || unique
            .iter()
            .any(|value| !matches!(*value, "string" | "number" | "integer" | "boolean" | "null"))
    {
        return Err(AppError::BadRequest(
            "ComfyUI parameters support only scalar JSON types".into(),
        ));
    }
    Ok(())
}

fn legacy_parameter_schema(placeholders: &BTreeSet<String>) -> Value {
    let properties = placeholders
        .iter()
        .map(|name| {
            (
                name.clone(),
                json!({"type": ["string", "number", "integer", "boolean", "null"]}),
            )
        })
        .collect::<Map<_, _>>();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "required": placeholders,
        "properties": properties
    })
}

fn reject_unknown_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), AppError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(AppError::BadRequest(
            "ComfyUI parameter_schema contains an unsupported keyword".into(),
        ));
    }
    Ok(())
}

fn bounded_text(value: &Value, name: &str, max: usize) -> Result<Value, AppError> {
    value
        .as_str()
        .filter(|value| value.len() <= max && !value.chars().any(char::is_control))
        .map(|value| Value::String(value.to_owned()))
        .ok_or_else(|| AppError::BadRequest(format!("ComfyUI {name} is invalid")))
}

fn safe_parameter_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && name.len() <= 64
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(schema: Option<Value>) -> Value {
        let mut value = json!({
            "workflow_template": {"1": {"inputs": {
                "prompt": {"$mtc_param": "prompt"},
                "seed": {"$mtc_param": "seed"}
            }}}
        });
        if let Some(schema) = schema {
            value["parameter_schema"] = schema;
        }
        value
    }

    #[test]
    fn accepts_and_sanitizes_exact_scalar_schema() {
        let schema = json!({
            "type": "object", "additionalProperties": false,
            "required": ["prompt", "seed"],
            "properties": {
                "prompt": {"title": "Prompt", "type": "string", "enum": ["cat", "dog"], "maxLength": 1000},
                "seed": {"type": "integer", "minimum": 0, "maximum": 100}
            }
        });
        let effective = effective_parameter_schema(&config(Some(schema))).unwrap();
        assert_eq!(effective["properties"]["seed"]["type"], "integer");
        assert_eq!(effective["properties"]["prompt"]["title"], "Prompt");
        assert_eq!(effective["properties"]["prompt"]["enum"][0], "cat");
        assert_eq!(effective["properties"]["seed"]["maximum"], 100);
        assert!(effective.get("workflow_template").is_none());
        validate_parameters(&config(None), &json!({"prompt": "hi", "seed": 1})).unwrap();
    }

    #[test]
    fn rejects_refs_mismatch_and_complex_types() {
        for schema in [
            json!({"type":"object","additionalProperties":false,"required":["prompt","seed"],"properties":{"prompt":{"$ref":"https://example.invalid/x"},"seed":{"type":"integer"}}}),
            json!({"type":"object","additionalProperties":false,"required":["prompt"],"properties":{"prompt":{"type":"string"}}}),
            json!({"type":"object","additionalProperties":false,"required":["prompt","seed"],"properties":{"prompt":{"type":"object"},"seed":{"type":"integer"}}}),
        ] {
            assert!(effective_parameter_schema(&config(Some(schema))).is_err());
        }
    }
}
