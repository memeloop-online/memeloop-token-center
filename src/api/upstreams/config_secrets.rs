use super::config_secret_graph::{has_secret, secret_cycle};
use super::config_secret_paths::paths;
use crate::{AppState, error::AppError, provider::UpstreamAccountView};
use serde_json::{Map, Value};

fn invalid() -> AppError {
    AppError::BadRequest("secret configuration requires an explicit non-empty replacement".into())
}

pub(super) fn validate_create(schema: &Value) -> Result<(), AppError> {
    if secret_cycle(schema)? {
        return Err(super::config_secret_graph::invalid());
    }
    // Complete analysis before any account row is inserted.
    if !has_secret(schema, schema, true)? {
        paths(schema)?;
    }
    Ok(())
}

/// Only schema-owned secret paths are inherited. Ordinary and unknown fields
/// retain the existing full-replacement semantics. The caller keeps its CAS.
pub(super) fn preserve(
    schema: &Value,
    current: &Value,
    incoming: &mut Value,
) -> Result<(), AppError> {
    if secret_cycle(schema)? || has_secret(schema, schema, true)? {
        return Err(AppError::BadRequest(
            "dynamic secret configuration cannot be edited through a partial account update".into(),
        ));
    }
    fn replace(
        current: Option<&Value>,
        incoming: &mut Value,
        path: &[String],
    ) -> Result<(), AppError> {
        let Some((key, tail)) = path.split_first() else {
            let empty = incoming.is_null()
                || incoming.as_str().is_some_and(|v| v.trim().is_empty())
                || incoming.as_object().is_some_and(Map::is_empty)
                || incoming.as_array().is_some_and(Vec::is_empty);
            return if empty { Err(invalid()) } else { Ok(()) };
        };
        let old = current.and_then(|value| value.get(key));
        let object = incoming.as_object_mut().ok_or_else(invalid)?;
        if !object.contains_key(key) {
            if tail.is_empty() {
                if let Some(old) = old {
                    object.insert(key.clone(), old.clone());
                }
                return Ok(());
            }
            if old.is_none() {
                return Ok(());
            }
            object.insert(key.clone(), Value::Object(Map::new()));
        }
        replace(old, object.get_mut(key).ok_or_else(invalid)?, tail)
    }
    for path in paths(schema)? {
        replace(Some(current), incoming, &path)?;
    }
    Ok(())
}

/// Standard OAuth credentials bind tenant-level editing to the existing config.
/// Global operators may change configuration, but must explicitly replace/clear
/// every existing hidden secret instead of inheriting it into changed authority.
pub(super) fn preserve_managed_oauth(
    schema: &Value,
    current: &Value,
    incoming: &mut Value,
    global_operator: bool,
) -> Result<(), AppError> {
    let supplied = incoming.clone();
    let mut previous = current.clone();
    fn at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
        path.iter().try_fold(value, |value, key| value.get(key))
    }
    fn remove(value: &mut Value, path: &[String]) {
        if let Some((first, tail)) = path.split_first() {
            if tail.is_empty() {
                if let Some(object) = value.as_object_mut() {
                    object.remove(first);
                }
            } else if let Some(child) = value.get_mut(first) {
                remove(child, tail);
            }
        }
    }
    let secret_paths = paths(schema)?;
    if global_operator {
        for path in &secret_paths {
            if at(&supplied, path).is_some_and(|value| {
                value.is_null() || value.as_object().is_some_and(Map::is_empty)
            }) {
                remove(&mut previous, path);
                remove(incoming, path);
            }
        }
    }
    preserve(schema, &previous, incoming)?;
    if *incoming != *current {
        if !global_operator {
            return Err(AppError::Forbidden);
        }
        if secret_paths
            .iter()
            .any(|path| at(current, path).is_some() && at(&supplied, path).is_none())
        {
            return Err(AppError::BadRequest("changing OAuth account configuration requires explicit secret replacement or clearing".into()));
        }
    }
    Ok(())
}

fn redact(schema: &Value, value: &mut Value) -> Result<(), AppError> {
    if secret_cycle(schema).unwrap_or(true) || has_secret(schema, schema, true).unwrap_or(true) {
        *value = Value::Object(Map::new());
        return Ok(());
    }
    fn remove(value: &mut Value, path: &[String]) {
        let Some((first, tail)) = path.split_first() else {
            *value = Value::Null;
            return;
        };
        if tail.is_empty() {
            if let Some(object) = value.as_object_mut() {
                object.remove(first);
            }
        } else if let Some(child) = value.get_mut(first) {
            remove(child, tail);
        }
    }
    let Ok(paths) = paths(schema) else {
        *value = Value::Object(Map::new());
        return Ok(());
    };
    for path in paths {
        remove(value, &path);
    }
    Ok(())
}

pub(super) fn redact_account(
    state: &AppState,
    account: &mut UpstreamAccountView,
) -> Result<(), AppError> {
    let Some(provider) = state.providers.get(&account.driver) else {
        // An unavailable plugin cannot supply trustworthy annotations.
        account.config = Value::Object(Map::new());
        return Ok(());
    };
    redact(&provider.config_schema, &mut account.config)
}

pub(super) fn public_account(
    state: &AppState,
    mut account: UpstreamAccountView,
) -> Result<UpstreamAccountView, AppError> {
    redact_account(state, &mut account)?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn excessive_distinct_output_paths_fail_closed_with_a_specific_error() {
        let mut definitions = serde_json::Map::new();
        definitions.insert("node0".into(), json!({"writeOnly": true}));
        for depth in 1..=12 {
            let target = format!("#/$defs/node{}", depth - 1);
            definitions.insert(
                format!("node{depth}"),
                json!({"properties": {"left": {"$ref": target}, "right": {"$ref": target}}}),
            );
        }
        let schema = json!({"$defs": definitions, "$ref": "#/$defs/node12"});
        assert!(matches!(
            validate_create(&schema),
            Err(AppError::SchemaSecretAnalysisTooComplex)
        ));
        let current = json!({"left": {"synthetic": "must-not-return"}});
        let mut public = current.clone();
        redact(&schema, &mut public).unwrap();
        assert_eq!(public, json!({}));
        let mut incoming = json!({"untouched": true});
        assert!(matches!(
            preserve(&schema, &current, &mut incoming),
            Err(AppError::SchemaSecretAnalysisTooComplex)
        ));
        assert_eq!(incoming, json!({"untouched": true}));
    }

    #[test]
    fn converging_secret_diamonds_expand_once_per_instance_path() {
        let mut definitions = serde_json::Map::new();
        definitions.insert("node0".into(), json!({"writeOnly": true}));
        for depth in 1..=32 {
            let target = format!("#/$defs/node{}", depth - 1);
            definitions.insert(
                format!("node{depth}"),
                json!({"allOf": [{"$ref": target}, {"$ref": target}]}),
            );
        }
        let schema = json!({"$defs": definitions, "properties": {
            "first": {"$ref": "#/$defs/node32"},
            "second": {"$ref": "#/$defs/node32"},
            "rows": {"items": {"$ref": "#/$defs/node32"}},
            "other_rows": {"items": {"$ref": "#/$defs/node32"}},
            "public": {"type": "string"}
        }});
        validate_create(&schema).unwrap();
        assert_eq!(
            paths(&schema).unwrap(),
            ["first", "other_rows", "rows", "second"].map(|key| vec![key.to_string()])
        );
        let current = json!({"first": "synthetic-one", "second": "synthetic-two", "rows": ["synthetic-three"], "other_rows": ["synthetic-four"], "public": "kept"});
        let mut public = current.clone();
        redact(&schema, &mut public).unwrap();
        assert_eq!(public, json!({"public": "kept"}));
        let mut incoming = json!({"public": "updated"});
        preserve(&schema, &current, &mut incoming).unwrap();
        for key in ["first", "second", "rows", "other_rows"] {
            assert_eq!(incoming[key], current[key]);
        }
        incoming["first"] = json!("synthetic-replacement");
        preserve(&schema, &current, &mut incoming).unwrap();
        assert_eq!(incoming["first"], "synthetic-replacement");
    }

    #[test]
    fn dense_public_recursive_schemas_do_not_exhaust_secret_path_budget() {
        for width in [8, 32, 64] {
            let mut definitions = serde_json::Map::new();
            for index in 0..width {
                definitions.insert(
                    format!("node{index}"),
                    json!({"allOf": (0..width).map(|target| json!({"$ref": format!("#/$defs/node{target}")})).collect::<Vec<_>>()}),
                );
            }
            let schema = json!({"$defs": definitions, "properties": {
                "public": {"$ref": "#/$defs/node0"},
                "secret": {"writeOnly": true}
            }});
            validate_create(&schema).unwrap();
            assert_eq!(paths(&schema).unwrap(), vec![vec!["secret".to_string()]]);
            let current = json!({"public": {"nested": "kept"}, "secret": "synthetic-old"});
            let mut public = current.clone();
            redact(&schema, &mut public).unwrap();
            assert_eq!(public, json!({"public": {"nested": "kept"}}));
            let mut incoming = json!({"public": {"nested": "updated"}});
            preserve(&schema, &current, &mut incoming).unwrap();
            assert_eq!(
                incoming,
                json!({"public": {"nested": "updated"}, "secret": "synthetic-old"})
            );
            incoming["secret"] = json!("synthetic-new");
            preserve(&schema, &current, &mut incoming).unwrap();
            assert_eq!(incoming["secret"], "synthetic-new");
        }
    }

    #[test]
    fn dynamic_and_conditional_secret_configs_are_fully_redacted_and_updates_rejected() {
        for schema in [
            json!({"type":"object","additionalProperties":{"properties":{"token":{"writeOnly":true}}}}),
            json!({"type":"object","if":{"properties":{"mode":{"const":"private"}}},"then":{"properties":{"token":{"writeOnly":true}}}}),
            json!({"$defs":{"s":{"writeOnly":true}},"additionalProperties":{"$ref":"#/$defs/s"}}),
        ] {
            let current = json!({"mode":"private","token":"synthetic-secret","dynamic":{"token":"synthetic-secret"}});
            let mut public = current.clone();
            redact(&schema, &mut public).unwrap();
            assert_eq!(public, json!({}));
            assert!(preserve(&schema, &current, &mut json!({"mode":"public"})).is_err());
        }
    }

    #[test]
    fn nested_secret_arrays_are_opaque_and_preserved_only_when_omitted() {
        let schema = json!({"properties":{"rows":{"type":"array","items":{"properties":{"token":{"writeOnly":true}}}}}});
        let current = json!({"rows":[{"token":"synthetic-secret"}]});
        let mut next = json!({});
        preserve(&schema, &current, &mut next).unwrap();
        assert_eq!(next, current);
        redact(&schema, &mut next).unwrap();
        assert_eq!(next, json!({}));
        assert!(preserve(&schema, &current, &mut json!({"rows":[]})).is_err());
    }

    #[test]
    fn nested_ref_allof_preservation_and_redaction_are_fail_closed() {
        let schema = json!({"$defs":{"secret":{"type":"string","writeOnly":true}},"properties":{
            "nested":{"properties":{"token":{"allOf":[{"$ref":"#/$defs/secret"}]}}},
            "plain":{"type":"string"}
        }});
        let current = json!({"nested":{"token":"synthetic-old","unknown":"must-not-inherit"},"plain":"old","unknown":"old"});
        let mut next = json!({"plain":"new"});
        preserve(&schema, &current, &mut next).unwrap();
        assert_eq!(
            next,
            json!({"plain":"new","nested":{"token":"synthetic-old"}})
        );
        let mut replacement = json!({"nested":{"token":"synthetic-new"}});
        preserve(&schema, &current, &mut replacement).unwrap();
        assert_eq!(replacement["nested"]["token"], "synthetic-new");
        for bad in [
            json!({"nested":null}),
            json!({"nested":{"token":null}}),
            json!({"nested":{"token":""}}),
        ] {
            assert!(preserve(&schema, &current, &mut bad.clone()).is_err());
        }
        redact(&schema, &mut next).unwrap();
        assert_eq!(next, json!({"plain":"new","nested":{}}));
    }

    #[test]
    fn generic_oauth_rebinding_requires_global_authority_and_explicit_secret_intent() {
        let schema = json!({"properties": {"base_url": {"type": "string"}, "headers": {"type": "object", "writeOnly": true}}});
        let current = json!({"base_url": "https://api.example.com", "headers": {"authorization": "fixture-secret"}});
        let mut rename_only = json!({"base_url": "https://api.example.com"});
        preserve_managed_oauth(&schema, &current, &mut rename_only, false).unwrap();
        assert_eq!(rename_only, current);
        for global in [false, true] {
            let mut moved = json!({"base_url": "https://other.example.com"});
            assert!(preserve_managed_oauth(&schema, &current, &mut moved, global).is_err());
        }
        let mut cleared = json!({"base_url": "https://other.example.com", "headers": {}});
        preserve_managed_oauth(&schema, &current, &mut cleared, true).unwrap();
        assert!(cleared.get("headers").is_none());
        let mut replaced = json!({"base_url": "https://other.example.com", "headers": {"authorization": "fixture-replacement"}});
        preserve_managed_oauth(&schema, &current, &mut replaced, true).unwrap();
        assert_eq!(replaced["headers"]["authorization"], "fixture-replacement");
    }
}
