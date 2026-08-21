use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::error::AppError;

const MAX_SCHEMA_BYTES: usize = 256 * 1024;
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 4_096;
const MAX_SCHEMA_CHOICES: usize = 64;
const MAX_SCHEMA_PROPERTIES: usize = 512;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_INSTANCE_BYTES: usize = 1024 * 1024;

/// A compiled, reusable schema from the declarative subset supported by both
/// the service and the CSP-safe browser renderer.
#[derive(Clone)]
pub struct CompiledSchema(jsonschema::Validator);

impl CompiledSchema {
    pub fn validate(&self, instance: &Value) -> Result<(), AppError> {
        let encoded = serde_json::to_vec(instance).map_err(|_| AppError::Internal)?;
        if encoded.len() > MAX_INSTANCE_BYTES {
            return Err(AppError::BadRequest(
                "JSON Schema instance exceeds the 1 MiB limit".into(),
            ));
        }
        self.0.validate(instance).map_err(|error| {
            // Never render `error` itself: some validator errors contain the
            // rejected instance, which may be an API key or OAuth token.
            let path = error.instance_path().to_string();
            let path = if path.is_empty() { "/" } else { path.as_str() };
            AppError::BadRequest(format!(
                "JSON Schema validation failed at {path}: value is not allowed"
            ))
        })
    }
}

pub fn compile(schema: &Value) -> Result<CompiledSchema, AppError> {
    inspect_definition(schema)?;
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        // The linear-time `regex` engine is also the dialect accepted by the
        // browser validator. Do not enable fancy-regex/backtracking here.
        .with_pattern_options(jsonschema::PatternOptions::regex())
        .build(schema)
        .map_err(|_| AppError::BadRequest("invalid JSON Schema definition".into()))?;
    Ok(CompiledSchema(validator))
}

pub fn validate_definition(schema: &Value) -> Result<(), AppError> {
    compile(schema).map(|_| ())
}

fn inspect_definition(schema: &Value) -> Result<(), AppError> {
    if serde_json::to_vec(schema)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_SCHEMA_BYTES
    {
        return Err(AppError::BadRequest(
            "JSON Schema exceeds the 256 KiB limit".into(),
        ));
    }
    let mut nodes = 0;
    inspect_schema(schema, 0, &mut nodes)
}

pub fn validate_instance(schema: &Value, instance: &Value) -> Result<(), AppError> {
    compile(schema)?.validate(instance)
}

fn inspect_schema(schema: &Value, depth: usize, nodes: &mut usize) -> Result<(), AppError> {
    *nodes = nodes.saturating_add(1);
    if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
        return Err(AppError::BadRequest(
            "JSON Schema exceeds the structural complexity limit".into(),
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
        "$id",
        "$ref",
        "$defs",
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
        "if",
        "then",
        "else",
        "required",
        "properties",
        "propertyNames",
        "additionalProperties",
        "minProperties",
        "maxProperties",
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
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ];
    if let Some(keyword) = object.keys().find(|key| !SUPPORTED.contains(&key.as_str())) {
        return Err(AppError::BadRequest(format!(
            "unsupported JSON Schema keyword: {keyword}"
        )));
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str)
        && !reference.starts_with("#/")
    {
        return Err(AppError::BadRequest(
            "JSON Schema references must be local JSON pointers".into(),
        ));
    }
    if let Some(dialect) = object.get("$schema").and_then(Value::as_str)
        && dialect != "https://json-schema.org/draft/2020-12/schema"
    {
        return Err(AppError::BadRequest(
            "only JSON Schema draft 2020-12 is supported".into(),
        ));
    }
    if let Some(format) = object.get("format").and_then(Value::as_str)
        && !matches!(format, "uri" | "uri-reference" | "uuid")
    {
        return Err(AppError::BadRequest(format!(
            "unsupported JSON Schema format: {format}"
        )));
    }
    if let Some(pattern) = object.get("pattern").and_then(Value::as_str)
        && (pattern.len() > MAX_PATTERN_BYTES
            || Regex::new(pattern).is_err()
            || !browser_safe_pattern(pattern))
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
            if children.is_empty() || children.len() > MAX_SCHEMA_CHOICES {
                return Err(AppError::BadRequest(format!(
                    "JSON Schema {keyword} must contain 1 to {MAX_SCHEMA_CHOICES} choices"
                )));
            }
            for child in children {
                inspect_schema(child, depth + 1, nodes)?;
            }
        }
    }
    for keyword in ["not", "if", "then", "else", "propertyNames"] {
        if let Some(child) = object.get(keyword) {
            inspect_schema(child, depth + 1, nodes)?;
        }
    }
    for keyword in ["properties", "$defs"] {
        if let Some(children) = object.get(keyword) {
            let children = children.as_object().ok_or_else(|| {
                AppError::BadRequest(format!("JSON Schema {keyword} must be an object"))
            })?;
            if children.len() > MAX_SCHEMA_PROPERTIES {
                return Err(AppError::BadRequest(format!(
                    "JSON Schema {keyword} contains too many entries"
                )));
            }
            for child in children.values() {
                inspect_schema(child, depth + 1, nodes)?;
            }
        }
    }
    for keyword in ["additionalProperties", "items"] {
        if let Some(child) = object.get(keyword) {
            inspect_schema(child, depth + 1, nodes)?;
        }
    }
    Ok(())
}

fn browser_safe_pattern(pattern: &str) -> bool {
    // JavaScript's native RegExp is the only CSP-compatible browser engine
    // available without shipping another runtime. Keep its accepted dialect
    // to a conservative, non-backtracking-heavy subset of Rust `regex`.
    static NESTED_REPETITION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\([^)]*(?:[+*]|\{\d)[^)]*\)(?:[+*]|\{\d)")
            .expect("static nested-repetition expression")
    });
    static REPEATED_ALTERNATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\([^)]*\|[^)]*\)(?:[+*]|\{\d)")
            .expect("static repeated-alternation expression")
    });
    !NESTED_REPETITION.is_match(pattern) && !REPEATED_ALTERNATION.is_match(pattern)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[test]
    fn published_routing_schemas_match_normalized_grant_contracts() {
        let route: Value =
            serde_json::from_str(include_str!("../schemas/model-route.schema.json")).unwrap();
        let key_create: Value =
            serde_json::from_str(include_str!("../schemas/key-create.schema.json")).unwrap();
        let key_policy: Value =
            serde_json::from_str(include_str!("../schemas/key-policy.schema.json")).unwrap();
        compile(&route).unwrap();
        compile(&key_create).unwrap();
        compile(&key_policy).unwrap();

        assert!(key_policy["properties"].get("allowed_models").is_none());
        assert!(
            key_create["properties"]["policy"]["properties"]
                .get("allowed_models")
                .is_none()
        );
        assert!(key_create["properties"].get("route_ids").is_some());
        assert!(key_create["properties"].get("route_group_ids").is_some());

        let direct = json!({
            "public_model": "codex-public",
            "upstream_model": "codex-upstream",
            "protocol": "openai",
            "upstream_account_ids": [uuid::Uuid::now_v7()]
        });
        validate_instance(&route, &direct).unwrap();
        let grouped = json!({
            "public_model": "claude-public",
            "upstream_model": "claude-upstream",
            "protocol": "anthropic",
            "included_provider_group_ids": [uuid::Uuid::now_v7()],
            "excluded_provider_group_ids": [uuid::Uuid::now_v7()],
            "route_group_names": ["production"],
            "granted_credential_ids": [uuid::Uuid::now_v7()]
        });
        validate_instance(&route, &grouped).unwrap();
        assert!(
            validate_instance(
                &route,
                &json!({
                    "public_model": "orphan",
                    "upstream_model": "orphan",
                    "protocol": "openai"
                })
            )
            .is_err()
        );
    }

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
    fn validates_property_names_conditionals_and_numeric_constraints() {
        let schema = json!({
            "type": "object",
            "propertyNames": {"pattern": "^[a-z]+$"},
            "additionalProperties": {
                "type": "number", "exclusiveMinimum": 0, "multipleOf": 0.5
            }
        });
        assert!(validate_instance(&schema, &json!({"model": 1.5})).is_ok());
        assert!(validate_instance(&schema, &json!({"Bad": 1.5})).is_err());
        assert!(validate_instance(&schema, &json!({"model": 1.25})).is_err());
    }

    #[test]
    fn required_follows_json_schema_and_does_not_treat_empty_as_missing() {
        let schema = json!({"type": "object", "required": ["name"], "properties": {
            "name": {"type": "string"}
        }});
        assert!(validate_instance(&schema, &json!({"name": ""})).is_ok());
        assert!(validate_instance(&schema, &json!({})).is_err());
    }

    #[test]
    fn rejects_remote_references_and_unknown_or_unsafe_keywords() {
        assert!(validate_definition(&json!({"$ref": "https://untrusted.invalid/schema"})).is_err());
        assert!(validate_definition(&json!({"type": "object", "patternProperties": {}})).is_err());
        assert!(validate_definition(&json!({"type": "string", "pattern": "(?=unsafe)"})).is_err());
    }

    #[test]
    fn validation_errors_do_not_echo_secret_instances() {
        let error = validate_instance(&json!({"type": "integer"}), &json!("secret-token"))
            .expect_err("invalid value");
        assert!(!error.to_string().contains("secret-token"));
    }

    #[derive(Deserialize)]
    struct ParityFixture {
        name: String,
        schema: Value,
        cases: Vec<ParityCase>,
    }

    #[derive(Deserialize)]
    struct ParityCase {
        valid: bool,
        value: Value,
    }

    #[test]
    fn service_matches_the_shared_browser_validation_contract() {
        let fixtures: Vec<ParityFixture> =
            serde_json::from_str(include_str!("../tests/fixtures/schema-parity.json"))
                .expect("schema parity fixtures");
        for fixture in fixtures {
            let validator = compile(&fixture.schema)
                .unwrap_or_else(|error| panic!("{} schema: {error}", fixture.name));
            for (index, case) in fixture.cases.into_iter().enumerate() {
                assert_eq!(
                    validator.validate(&case.value).is_ok(),
                    case.valid,
                    "{} case {index}",
                    fixture.name
                );
            }
        }
    }
}
