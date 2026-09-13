use std::collections::HashSet;

use serde_json::Value;

use crate::error::AppError;

pub(super) fn invalid() -> AppError {
    AppError::BadRequest("unsupported secret configuration schema".into())
}

fn edges<'a>(root: &'a Value, node: &'a Value) -> Result<Vec<(&'a Value, bool)>, AppError> {
    let mut result = Vec::new();
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        result.push((
            reference
                .strip_prefix('#')
                .and_then(|pointer| root.pointer(pointer))
                .ok_or_else(invalid)?,
            false,
        ));
    }
    // Definitions are traversed only when referenced. Literal instance data
    // (defaults/examples/const/enum) are not schema graph edges.
    for key in ["properties", "patternProperties", "dependentSchemas"] {
        if let Some(children) = node.get(key).and_then(Value::as_object) {
            result.extend(children.values().map(|child| (child, key != "properties")));
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
    ] {
        if let Some(child) = node.get(key) {
            result.push((child, key != "items"));
        }
    }
    for key in ["allOf", "oneOf", "anyOf", "prefixItems"] {
        if let Some(children) = node.get(key).and_then(Value::as_array) {
            result.extend(children.iter().map(|child| (child, key == "prefixItems")));
        }
    }
    Ok(result)
}

/// Iterative graph walk: each node is examined at most twice, once in each
/// search mode. Local reference cycles cannot exhaust a recursive depth limit.
pub(super) fn has_secret(
    root: &Value,
    start: &Value,
    dynamic_only: bool,
) -> Result<bool, AppError> {
    let mut pending = vec![(start, !dynamic_only)];
    let mut visited = HashSet::new();
    while let Some((node, searching)) = pending.pop() {
        if !visited.insert((node as *const Value as usize, searching)) {
            continue;
        }
        if searching
            && (node.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || node.get("format").and_then(Value::as_str) == Some("password"))
        {
            return Ok(true);
        }
        for (child, dynamic) in edges(root, node)? {
            pending.push((child, searching || dynamic));
        }
    }
    Ok(false)
}

/// A cycle with reachable secret annotations cannot be represented as a
/// finite omission patch. Non-secret cycles are valid and remain transparent.
pub(super) fn secret_cycle(root: &Value) -> Result<bool, AppError> {
    let mut pending = vec![(root, false)];
    let mut ancestors = HashSet::new();
    let mut finished = HashSet::new();
    while let Some((node, exiting)) = pending.pop() {
        let identity = node as *const Value as usize;
        if exiting {
            ancestors.remove(&identity);
            finished.insert(identity);
            continue;
        }
        if finished.contains(&identity) {
            continue;
        }
        if !ancestors.insert(identity) {
            if has_secret(root, node, false)? {
                return Ok(true);
            }
            continue;
        }
        pending.push((node, true));
        for (child, _) in edges(root, node)? {
            pending.push((child, false));
        }
    }
    Ok(false)
}
