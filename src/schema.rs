use std::collections::HashSet;

use regex::Regex;
use serde_json::Value;

use crate::error::AppError;

const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_PATTERN_BYTES: usize = 1_024;

pub fn validate_definition(schema: &Value) -> Result<(), AppError> {
    if serde_json::to_vec(schema)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_SCHEMA_BYTES
    {
        return Err(AppError::BadRequest(
            "JSON Schema exceeds the 256 KiB limit".into(),
        ));
    }
    inspect_schema(schema, 0)
}

pub fn validate_instance(schema: &Value, instance: &Value) -> Result<(), AppError> {
    validate_definition(schema)?;
    validate(schema, instance, "$", 0)
}

fn inspect_schema(schema: &Value, depth: usize) -> Result<(), AppError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(AppError::BadRequest(
            "JSON Schema exceeds the maximum nesting depth".into(),
        ));
    }
    let Some(object) = schema.as_object() else {
        if schema.is_boolean() {
            return Ok(());
        }
        return Err(AppError::BadRequest(
            "JSON Schema nodes must be objects or booleans".into(),
        ));
    };
    const SUPPORTED: &[&str] = &[
        "$schema",
        "title",
        "description",
        "default",
        "examples",
        "readOnly",
        "writeOnly",
        "deprecated",
        "type",
        "const",
        "enum",
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "required",
        "properties",
        "additionalProperties",
        "items",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minimum",
        "maximum",
    ];
    if let Some(keyword) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        return Err(AppError::BadRequest(format!(
            "unsupported JSON Schema keyword: {keyword}"
        )));
    }
    if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
        && (pattern.len() > MAX_PATTERN_BYTES || Regex::new(pattern).is_err())
    {
        return Err(AppError::BadRequest(
            "JSON Schema contains an invalid or oversized pattern".into(),
        ));
    }
    for keyword in ["oneOf", "anyOf", "allOf"] {
        if let Some(children) = object.get(keyword) {
            let children = children.as_array().ok_or_else(|| {
                AppError::BadRequest(format!("JSON Schema {keyword} must be an array"))
            })?;
            if children.is_empty() || children.len() > 64 {
                return Err(AppError::BadRequest(format!(
                    "JSON Schema {keyword} must contain 1 to 64 choices"
                )));
            }
            for child in children {
                inspect_schema(child, depth + 1)?;
            }
        }
    }
    if let Some(child) = object.get("not") {
        inspect_schema(child, depth + 1)?;
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties.as_object().ok_or_else(|| {
            AppError::BadRequest("JSON Schema properties must be an object".into())
        })?;
        if properties.len() > 512 {
            return Err(AppError::BadRequest(
                "JSON Schema contains too many properties".into(),
            ));
        }
        for child in properties.values() {
            inspect_schema(child, depth + 1)?;
        }
    }
    for keyword in ["additionalProperties", "items"] {
        if let Some(child) = object.get(keyword).filter(|child| !child.is_boolean()) {
            inspect_schema(child, depth + 1)?;
        }
    }
    Ok(())
}

fn validate(schema: &Value, instance: &Value, path: &str, depth: usize) -> Result<(), AppError> {
    if depth > MAX_SCHEMA_DEPTH {
        return schema_error(path, "value exceeds maximum nesting depth");
    }
    if let Some(allowed) = schema.as_bool() {
        return if allowed {
            Ok(())
        } else {
            schema_error(path, "value is not allowed")
        };
    }
    let object = schema.as_object().ok_or(AppError::Internal)?;
    if let Some(expected) = object.get("const")
        && instance != expected
    {
        return schema_error(path, "value does not match the required constant");
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array)
        && !values.contains(instance)
    {
        return schema_error(path, "value is not in the allowed set");
    }
    if let Some(children) = object.get("allOf").and_then(Value::as_array) {
        for child in children {
            validate(child, instance, path, depth + 1)?;
        }
    }
    if let Some(children) = object.get("anyOf").and_then(Value::as_array)
        && !children
            .iter()
            .any(|child| validate(child, instance, path, depth + 1).is_ok())
    {
        return schema_error(path, "value does not match any allowed schema");
    }
    if let Some(children) = object.get("oneOf").and_then(Value::as_array) {
        let matches = children
            .iter()
            .filter(|child| validate(child, instance, path, depth + 1).is_ok())
            .count();
        if matches != 1 {
            return schema_error(path, "value must match exactly one allowed schema");
        }
    }
    if object
        .get("not")
        .is_some_and(|child| validate(child, instance, path, depth + 1).is_ok())
    {
        return schema_error(path, "value matches a forbidden schema");
    }
    if let Some(expected) = object.get("type") {
        let matches = match expected {
            Value::String(expected) => matches_type(expected, instance),
            Value::Array(expected) => expected
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| matches_type(expected, instance)),
            _ => false,
        };
        if !matches {
            return schema_error(path, "value has the wrong type");
        }
    }
    if let Some(value) = instance.as_object() {
        let properties = object.get("properties").and_then(Value::as_object);
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !value.contains_key(field) {
                    return schema_error(path, &format!("required property {field} is missing"));
                }
            }
        }
        for (field, child) in value {
            if let Some(child_schema) = properties.and_then(|properties| properties.get(field)) {
                validate(child_schema, child, &format!("{path}.{field}"), depth + 1)?;
            } else if object.get("additionalProperties") == Some(&Value::Bool(false)) {
                return schema_error(path, &format!("property {field} is not allowed"));
            } else if let Some(additional) = object
                .get("additionalProperties")
                .filter(|additional| additional.is_object())
            {
                validate(additional, child, &format!("{path}.{field}"), depth + 1)?;
            }
        }
    }
    if let Some(values) = instance.as_array() {
        if object
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| values.len() < minimum as usize)
        {
            return schema_error(path, "array has too few items");
        }
        if object
            .get("maxItems")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| values.len() > maximum as usize)
        {
            return schema_error(path, "array has too many items");
        }
        if object.get("uniqueItems") == Some(&Value::Bool(true)) {
            let mut unique = HashSet::new();
            if values.iter().any(|value| !unique.insert(value.to_string())) {
                return schema_error(path, "array items must be unique");
            }
        }
        if let Some(items) = object.get("items") {
            for (index, value) in values.iter().enumerate() {
                validate(items, value, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
    }
    if let Some(value) = instance.as_str() {
        let length = value.chars().count();
        if object
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|minimum| length < minimum as usize)
        {
            return schema_error(path, "string is too short");
        }
        if object
            .get("maxLength")
            .and_then(Value::as_u64)
            .is_some_and(|maximum| length > maximum as usize)
        {
            return schema_error(path, "string is too long");
        }
        if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
            && !Regex::new(pattern)
                .map_err(|_| AppError::Internal)?
                .is_match(value)
        {
            return schema_error(path, "string does not match the required pattern");
        }
        if object.get("format").and_then(Value::as_str) == Some("uri")
            && url::Url::parse(value).is_err()
        {
            return schema_error(path, "string is not a valid URI");
        }
    }
    if let Some(value) = instance.as_f64() {
        if object
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|minimum| value < minimum)
        {
            return schema_error(path, "number is below the minimum");
        }
        if object
            .get("maximum")
            .and_then(Value::as_f64)
            .is_some_and(|maximum| value > maximum)
        {
            return schema_error(path, "number is above the maximum");
        }
    }
    Ok(())
}

fn matches_type(expected: &str, value: &Value) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "string" => value.is_string(),
        _ => false,
    }
}

fn schema_error<T>(path: &str, message: &str) -> Result<T, AppError> {
    Err(AppError::BadRequest(format!(
        "JSON Schema validation failed at {path}: {message}"
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn enforces_required_properties_and_rejects_unknown_properties() {
        let schema = json!({
            "type": "object",
            "required": ["provider"],
            "additionalProperties": false,
            "properties": {"provider": {"type": "string", "enum": ["copilot", "cursor"]}}
        });
        assert!(validate_instance(&schema, &json!({"provider": "cursor"})).is_ok());
        assert!(validate_instance(&schema, &json!({"provider": "other"})).is_err());
        assert!(
            validate_instance(&schema, &json!({"provider": "cursor", "secret": "no"})).is_err()
        );
    }

    #[test]
    fn rejects_unsupported_schema_keywords_instead_of_silently_ignoring_them() {
        assert!(validate_definition(&json!({"type": "object", "$ref": "https://bad"})).is_err());
    }
}
