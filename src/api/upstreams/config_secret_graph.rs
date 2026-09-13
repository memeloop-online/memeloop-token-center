use std::collections::{HashMap, HashSet};

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
    secret_cycle_with_work(root).map(|(cycle, _)| cycle)
}

// Build reverse reachability once, rather than searching from every DFS
// back-edge. Each edge is visited at most three times, even for dense SCCs.
// The work count provides a deterministic complexity contract in tests.
pub(super) struct SecretReachability {
    graph: HashMap<usize, Vec<usize>>,
    reaches_secret: HashSet<usize>,
    work: usize,
}

impl SecretReachability {
    pub(super) fn contains(&self, node: &Value) -> bool {
        self.reaches_secret
            .contains(&(node as *const Value as usize))
    }
}

pub(super) fn secret_reachability(root: &Value) -> Result<SecretReachability, AppError> {
    let identity = |node: &Value| node as *const Value as usize;
    let mut pending = vec![root];
    let mut graph = HashMap::<usize, Vec<usize>>::new();
    let mut reverse = HashMap::<usize, Vec<usize>>::new();
    let mut reaches_secret = HashSet::new();
    let mut work = 0;
    while let Some(node) = pending.pop() {
        let id = identity(node);
        if graph.contains_key(&id) {
            continue;
        }
        if node.get("writeOnly").and_then(Value::as_bool) == Some(true)
            || node.get("format").and_then(Value::as_str) == Some("password")
        {
            reaches_secret.insert(id);
        }
        let children = edges(root, node)?;
        let mut outgoing = Vec::with_capacity(children.len());
        for (child, _) in children {
            work += 1;
            let child_id = identity(child);
            outgoing.push(child_id);
            reverse.entry(child_id).or_default().push(id);
            pending.push(child);
        }
        graph.insert(id, outgoing);
    }
    let mut propagation: Vec<_> = reaches_secret.iter().copied().collect();
    while let Some(id) = propagation.pop() {
        for predecessor in reverse.get(&id).into_iter().flatten() {
            work += 1;
            if reaches_secret.insert(*predecessor) {
                propagation.push(*predecessor);
            }
        }
    }
    Ok(SecretReachability {
        graph,
        reaches_secret,
        work,
    })
}

fn secret_cycle_with_work(root: &Value) -> Result<(bool, usize), AppError> {
    let SecretReachability {
        graph,
        reaches_secret,
        mut work,
    } = secret_reachability(root)?;
    let mut pending = vec![(root as *const Value as usize, false)];
    let mut ancestors = HashSet::new();
    let mut finished = HashSet::new();
    while let Some((identity, exiting)) = pending.pop() {
        if exiting {
            ancestors.remove(&identity);
            finished.insert(identity);
            continue;
        }
        if finished.contains(&identity) {
            continue;
        }
        if !ancestors.insert(identity) {
            if reaches_secret.contains(&identity) {
                return Ok((true, work));
            }
            continue;
        }
        pending.push((identity, true));
        for child in &graph[&identity] {
            work += 1;
            pending.push((*child, false));
        }
    }
    Ok((false, work))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dense_recursive_components_have_linear_edge_work() {
        for width in [8, 32, 64] {
            let mut definitions = serde_json::Map::new();
            let mut properties = serde_json::Map::new();
            properties.insert("secret".into(), json!({"writeOnly": true}));
            for index in 0..width {
                definitions.insert(
                    format!("node{index}"),
                    json!({"allOf": (0..width).map(|target| json!({"$ref": format!("#/$defs/node{target}")})).collect::<Vec<_>>()}),
                );
                properties.insert(
                    format!("node{index}"),
                    json!({"$ref": format!("#/$defs/node{index}")}),
                );
            }
            let mut schema = json!({"$defs": definitions, "properties": properties});
            // root -> properties, property refs -> definitions, definition
            // -> allOf members, and each allOf ref -> definition.
            let edge_count = 2 * width * width + 2 * width + 1;
            let (cycle, work) = secret_cycle_with_work(&schema).unwrap();
            assert!(!cycle, "an unrelated static secret does not taint an SCC");
            assert!(work <= 3 * edge_count, "width={width}, work={work}");
            schema["$defs"]["node0"]["writeOnly"] = json!(true);
            let (cycle, work) = secret_cycle_with_work(&schema).unwrap();
            assert!(cycle, "a secret reachable from the SCC remains fail-closed");
            assert!(work <= 3 * edge_count, "width={width}, work={work}");
        }
    }
}
