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
const MAX_DISPLAY_LOCALES: usize = 32;
const MAX_DISPLAY_TEXT_BYTES: usize = 4_096;
const MAX_DISPLAY_KEY_BYTES: usize = 64;
const MAX_INSTANCE_BYTES: usize = 1024 * 1024;

/// A compiled, reusable schema from the declarative subset supported by both
/// the service and the CSP-safe browser renderer.
#[derive(Clone)]
pub struct CompiledSchema(jsonschema::Validator);

impl CompiledSchema {
    pub fn validate(&self, instance: &Value) -> Result<(), AppError> {
        let encoded =
            serde_json::to_vec(instance).map_err(|_| AppError::internal("schema_validate_1"))?;
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
        .map_err(|_| AppError::internal("schema_inspect_definition_1"))?
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
        "x-i18n",
        "x-resource-selector",
        "x-resource-choice",
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
    inspect_display_annotations(object)?;
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

// Presentation annotations describe labels and resource bindings for configuration editors.
fn inspect_display_annotations(object: &serde_json::Map<String, Value>) -> Result<(), AppError> {
    let invalid = || AppError::BadRequest("invalid JSON Schema display annotation".into());
    if let Some(translations) = object.get("x-i18n") {
        let translations = translations.as_object().ok_or_else(invalid)?;
        if translations.is_empty() || translations.len() > MAX_DISPLAY_LOCALES {
            return Err(invalid());
        }
        for (locale, labels) in translations {
            if locale.is_empty()
                || locale.len() > MAX_DISPLAY_KEY_BYTES
                || !locale
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(invalid());
            }
            let labels = labels.as_object().ok_or_else(invalid)?;
            if labels.is_empty()
                || labels.iter().any(|(key, value)| {
                    !matches!(key.as_str(), "title" | "description")
                        || value
                            .as_str()
                            .is_none_or(|text| text.len() > MAX_DISPLAY_TEXT_BYTES)
                })
            {
                return Err(invalid());
            }
        }
    }
    if let Some(choice) = object.get("x-resource-choice") {
        let choice = choice.as_object().ok_or_else(invalid)?;
        let bindings = choice
            .get("bindings")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(invalid)?;
        let valid_key = |value: &str| {
            !value.is_empty()
                && value.len() <= MAX_DISPLAY_KEY_BYTES
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        };
        if object.get("type").and_then(Value::as_str) != Some("object")
            || choice.len() != 3
            || choice
                .get("source")
                .and_then(Value::as_str)
                .is_none_or(|value| !valid_key(value))
            || !choice
                .get("autoSelectSingle")
                .is_some_and(Value::is_boolean)
            || bindings.is_empty()
            || bindings.len() > MAX_DISPLAY_KEY_BYTES
            || bindings.iter().any(|(field, attribute)| {
                properties
                    .get(field)
                    .and_then(Value::as_object)
                    .and_then(|property| property.get("type"))
                    .and_then(Value::as_str)
                    != Some("string")
                    || attribute.as_str().is_none_or(|value| !valid_key(value))
            })
        {
            return Err(invalid());
        }
    }
    if let Some(selector) = object.get("x-resource-selector") {
        let selector = selector.as_object().ok_or_else(invalid)?;
        if object.get("type").and_then(Value::as_str) != Some("array")
            || object
                .get("items")
                .and_then(Value::as_object)
                .and_then(|items| items.get("type"))
                .and_then(Value::as_str)
                != Some("string")
            || selector.len() != 2
            || selector.get("ordered").and_then(Value::as_bool) != Some(true)
            || selector
                .get("source")
                .and_then(Value::as_str)
                .is_none_or(|source| {
                    source.is_empty()
                        || source.len() > MAX_DISPLAY_KEY_BYTES
                        || !source
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                })
        {
            return Err(invalid());
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
    fn published_policy_schemas_allow_unset_rates_but_not_zero_or_fake_infinity() {
        let standalone: Value =
            serde_json::from_str(include_str!("../schemas/key-policy.schema.json")).unwrap();
        let create: Value =
            serde_json::from_str(include_str!("../schemas/key-create.schema.json")).unwrap();
        for schema in [&standalone, &create["properties"]["policy"]] {
            for field in [
                "requests_per_minute",
                "tokens_per_minute",
                "max_concurrency",
            ] {
                assert!(schema["properties"][field].get("default").is_none());
            }
        }
        let policy_validator = compile(&standalone).unwrap();
        let create_validator = compile(&create).unwrap();
        for policy in [
            json!({}),
            json!({"requests_per_minute": null, "tokens_per_minute": null, "max_concurrency": null}),
            json!({"requests_per_minute": 60, "tokens_per_minute": 100000, "max_concurrency": 4}),
        ] {
            policy_validator.validate(&policy).unwrap();
            create_validator
                .validate(&json!({"principal_external_id": "member", "alias": "optional", "policy": policy}))
                .unwrap();
        }
        for field in [
            "requests_per_minute",
            "tokens_per_minute",
            "max_concurrency",
        ] {
            for invalid in [json!(0), json!(-1), json!("unlimited"), json!(1.5)] {
                let mut policy = json!({});
                policy[field] = invalid;
                assert!(policy_validator.validate(&policy).is_err());
                assert!(create_validator.validate(&json!({"principal_external_id": "member", "alias": "optional", "policy": policy})).is_err());
            }
        }
        for (field, overflow) in [
            ("requests_per_minute", 4294967296_u64),
            ("tokens_per_minute", 9007199254740992_u64),
            ("max_concurrency", 4294967296_u64),
        ] {
            let mut policy = json!({});
            policy[field] = json!(overflow);
            assert!(policy_validator.validate(&policy).is_err());
        }
    }

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

    #[test]
    fn display_annotations_preserve_instance_validation() {
        let schema = json!({"type": "object", "additionalProperties": false,
            "x-i18n": {"zh-CN": {"title": "策略", "description": "展示"}},
            "properties": {"accounts": {"type": "array", "maxItems": 2, "uniqueItems": true,
                "items": {"type": "string", "minLength": 1},
                "x-i18n": {"en": {"title": "Accounts"}},
                "x-resource-selector": {"source": "group-accounts", "ordered": true}}}});
        let validator = compile(&schema).unwrap();
        // An unknown resource is still a valid string; resource access remains
        // the caller's responsibility, independent of presentation metadata.
        validator
            .validate(&json!({"accounts": ["unknown-resource"]}))
            .unwrap();
        for value in [
            json!({"accounts": [""]}),
            json!({"accounts": [1]}),
            json!({"accounts": ["a", "a"]}),
            json!({"accounts": ["a", "b", "c"]}),
            json!({"secret": "secret-token"}),
        ] {
            let error = validator.validate(&value).unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
        }
    }

    #[test]
    fn resource_choices_bind_existing_string_fields() {
        let schema = json!({"type":"object","required":["provider","window"],"additionalProperties":false,
            "properties":{"provider":{"type":"string"},"window":{"type":"string"}},
            "x-resource-choice":{"source":"group-quota-windows","bindings":{"provider":"provider","window":"window_id"},"autoSelectSingle":true}});
        let validator = compile(&schema).unwrap();
        validator
            .validate(&json!({"provider":"openai-codex","window":"code:primary_window"}))
            .unwrap();
        assert!(
            validator
                .validate(&json!({"provider":42,"window":"x"}))
                .is_err()
        );
        for choice in [
            json!(null),
            json!({"source":"group-quota-windows","bindings":{},"autoSelectSingle":true}),
            json!({"source":"group-quota-windows","bindings":{"missing":"provider"},"autoSelectSingle":true}),
            json!({"source":"group-quota-windows","bindings":{"provider":42},"autoSelectSingle":true}),
            json!({"source":"group-quota-windows","bindings":{"provider":"provider"},"autoSelectSingle":"true"}),
            json!({"source":"group-quota-windows","bindings":{"provider":"provider"},"autoSelectSingle":true,"extra":true}),
        ] {
            let mut invalid = schema.clone();
            invalid["x-resource-choice"] = choice;
            assert!(validate_definition(&invalid).is_err());
        }
    }

    #[test]
    fn rejects_malformed_or_oversized_display_annotations() {
        for hint in [
            json!(null),
            json!([]),
            json!({}),
            json!({"en": "secret-token"}),
            json!({"en": {}}),
            json!({"en": {"title": 1}}),
            json!({"en": {"default": "secret-token"}}),
            json!({"bad_locale": {"title": "label"}}),
            json!({"en": {"description": "x".repeat(MAX_DISPLAY_TEXT_BYTES + 1)}}),
        ] {
            let error =
                validate_definition(&json!({"type": "string", "x-i18n": hint})).unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
        }
        let locales: serde_json::Map<String, Value> = (0..=MAX_DISPLAY_LOCALES)
            .map(|index| (format!("locale-{index}"), json!({"title": "label"})))
            .collect();
        assert!(validate_definition(&json!({"x-i18n": locales})).is_err());
        for hint in [
            json!(null),
            json!([]),
            json!({}),
            json!({"source": "group-accounts", "ordered": "true"}),
            json!({"source": "group-accounts", "ordered": false}),
            json!({"source": "", "ordered": true}),
            json!({"source": "x".repeat(MAX_DISPLAY_KEY_BYTES + 1), "ordered": true}),
            json!({"source": "https://secret-token", "ordered": true}),
            json!({"source": "group-accounts", "ordered": true, "permissions": []}),
        ] {
            let error = validate_definition(&json!({"type": "array", "items": {"type": "string"},
                "x-resource-selector": hint}))
            .unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
        }
        for shape in [
            json!({}),
            json!({"type": "string"}),
            json!({"type": "array"}),
            json!({"type": "array", "items": true}),
            json!({"type": "array", "items": {"type": "integer"}}),
        ] {
            let mut schema = shape;
            schema["x-resource-selector"] = json!({"source": "group-accounts", "ordered": true});
            assert!(validate_definition(&schema).is_err());
        }
        assert!(validate_definition(&json!({"x-other": {"title": "unknown"}})).is_err());
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
