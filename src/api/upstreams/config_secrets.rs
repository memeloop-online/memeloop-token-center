use serde_json::{Map, Value};
use std::collections::HashSet;

use super::config_secret_graph::{
    SecretReachability, has_secret, secret_cycle, secret_reachability,
};
use crate::{AppState, error::AppError, provider::UpstreamAccountView};

fn invalid() -> AppError {
    AppError::BadRequest("secret configuration requires an explicit non-empty replacement".into())
}

fn paths(schema: &Value) -> Result<Vec<Vec<String>>, AppError> {
    let reachability = secret_reachability(schema)?;
    if !reachability.contains(schema) {
        return Ok(Vec::new());
    }
    fn visit(
        root: &Value,
        node: &Value,
        path: &mut Vec<String>,
        output: &mut Vec<Vec<String>>,
        ancestors: &mut HashSet<usize>,
        budget: &mut usize,
        reachability: &SecretReachability,
    ) -> Result<(), AppError> {
        // Non-secret SCCs may have exponentially many simple paths. Their
        // contents cannot contribute an omission path, so prune them once
        // using the same linear reachability analysis as cycle detection.
        if !reachability.contains(node) {
            return Ok(());
        }
        *budget = budget.checked_sub(1).ok_or_else(invalid)?;
        if ancestors.len() >= 256 {
            return Err(super::config_secret_graph::invalid());
        }
        if node.get("writeOnly").and_then(Value::as_bool) == Some(true)
            || node.get("format").and_then(Value::as_str) == Some("password")
        {
            output.push(path.clone());
            return Ok(());
        }
        let identity = node as *const Value as usize;
        if !ancestors.insert(identity) {
            return Ok(());
        }
        if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
            let target = reference
                .strip_prefix('#')
                .and_then(|pointer| root.pointer(pointer))
                .ok_or_else(invalid)?;
            visit(root, target, path, output, ancestors, budget, reachability)?;
        }
        for keyword in ["allOf", "oneOf", "anyOf"] {
            if let Some(parts) = node.get(keyword).and_then(Value::as_array) {
                for part in parts {
                    visit(root, part, path, output, ancestors, budget, reachability)?;
                }
            }
        }
        if let Some(properties) = node.get("properties").and_then(Value::as_object) {
            for (key, child) in properties {
                path.push(key.clone());
                visit(root, child, path, output, ancestors, budget, reachability)?;
                path.pop();
            }
        }
        // An array containing secrets is opaque as a whole: never expose or
        // reconstruct indices from a partially redacted array.
        if let Some(items) = node.get("items") {
            let mut nested = Vec::new();
            visit(
                root,
                items,
                &mut Vec::new(),
                &mut nested,
                ancestors,
                budget,
                reachability,
            )?;
            if !nested.is_empty() {
                output.push(path.clone());
            }
        }
        ancestors.remove(&identity);
        Ok(())
    }
    let mut output = Vec::new();
    visit(
        schema,
        schema,
        &mut Vec::new(),
        &mut output,
        &mut HashSet::new(),
        &mut 20_480,
        &reachability,
    )?;
    output.sort();
    output.dedup();
    Ok(output)
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
}
