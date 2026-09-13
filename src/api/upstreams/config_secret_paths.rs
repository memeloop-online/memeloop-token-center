//! Shared relative-path tries avoid expanding schema routes into instance
//! paths until the final output. Expected work is O(V + E + U + P): U counts
//! hash-consing/union branch and label work (including ordered-map factors),
//! P counts emitted path/label work.
//! U and P have independent hard caps; this is not an unconditional linear
//! bound in the input schema size. No schema/value text enters diagnostics.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use super::config_secret_graph::{SecretReachability, secret_reachability};
use crate::error::AppError;

const WORK_LIMIT: usize = 20_480;
const EMPTY: usize = 0;
const TERMINAL: usize = 1;

#[derive(Clone, Hash, PartialEq, Eq)]
enum Trie {
    Empty,
    Terminal,
    Branch(BTreeMap<String, usize>),
}

#[derive(Default, Debug)]
struct Work {
    schema_nodes: usize,
    union_work: usize,
    output_work: usize,
}

fn charge(counter: &mut usize, units: usize, limit: usize) -> Result<(), AppError> {
    *counter = counter.saturating_add(units);
    if *counter > limit {
        return Err(AppError::SchemaSecretAnalysisTooComplex);
    }
    Ok(())
}

fn map_work(children: &BTreeMap<String, usize>) -> usize {
    let ordered_factor = 1 + children.len().max(1).ilog2() as usize;
    1 + children.keys().map(|key| key.len() + 1).sum::<usize>() * ordered_factor
}

struct Analysis<'a> {
    root: &'a Value,
    reachable: SecretReachability,
    nodes: Vec<Trie>,
    interned: HashMap<Trie, usize>,
    unions: HashMap<(usize, usize), usize>,
    schemas: HashMap<usize, usize>,
    active: HashSet<usize>,
    work: Work,
    limit: usize,
}

impl<'a> Analysis<'a> {
    fn new(root: &'a Value, limit: usize) -> Result<Self, AppError> {
        Ok(Self {
            root,
            reachable: secret_reachability(root)?,
            nodes: vec![Trie::Empty, Trie::Terminal],
            interned: HashMap::new(),
            unions: HashMap::new(),
            schemas: HashMap::new(),
            active: HashSet::new(),
            work: Work::default(),
            limit,
        })
    }

    fn branch(&mut self, children: BTreeMap<String, usize>) -> Result<usize, AppError> {
        if children.is_empty() {
            return Ok(EMPTY);
        }
        charge(&mut self.work.union_work, map_work(&children), self.limit)?;
        let node = Trie::Branch(children);
        if let Some(id) = self.interned.get(&node) {
            return Ok(*id);
        }
        let id = self.nodes.len();
        self.nodes.push(node.clone());
        self.interned.insert(node, id);
        Ok(id)
    }

    fn union(&mut self, left: usize, right: usize, depth: usize) -> Result<usize, AppError> {
        charge(&mut self.work.union_work, 1, self.limit)?;
        if depth > 256 {
            return Err(AppError::SchemaSecretAnalysisTooComplex);
        }
        if left == right || right == EMPTY {
            return Ok(left);
        }
        if left == EMPTY {
            return Ok(right);
        }
        // A secret ancestor already covers all of its descendants.
        if left == TERMINAL || right == TERMINAL {
            return Ok(TERMINAL);
        }
        let pair = (left.min(right), left.max(right));
        if let Some(id) = self.unions.get(&pair) {
            return Ok(*id);
        }
        let (Trie::Branch(a), Trie::Branch(b)) = (&self.nodes[left], &self.nodes[right]) else {
            unreachable!("nonempty nonterminal trie nodes are branches");
        };
        charge(
            &mut self.work.union_work,
            map_work(a) + map_work(b),
            self.limit,
        )?;
        let mut merged = a.clone();
        let additional = b.clone();
        for (key, right_child) in additional {
            let child = if let Some(left_child) = merged.get(&key) {
                self.union(*left_child, right_child, depth + 1)?
            } else {
                right_child
            };
            merged.insert(key, child);
        }
        let id = self.branch(merged)?;
        self.unions.insert(pair, id);
        Ok(id)
    }

    fn schema(&mut self, node: &'a Value, depth: usize) -> Result<usize, AppError> {
        if !self.reachable.contains(node) {
            return Ok(EMPTY);
        }
        let identity = node as *const Value as usize;
        if let Some(id) = self.schemas.get(&identity) {
            return Ok(*id);
        }
        if depth > 256 {
            return Err(AppError::SchemaSecretAnalysisTooComplex);
        }
        charge(&mut self.work.schema_nodes, 1, self.limit)?;
        if !self.active.insert(identity) {
            return Err(super::config_secret_graph::invalid());
        }
        let opaque = node.get("writeOnly").and_then(Value::as_bool) == Some(true)
            || node.get("format").and_then(Value::as_str) == Some("password")
            || node
                .get("items")
                .is_some_and(|items| self.reachable.contains(items));
        let mut result = if opaque { TERMINAL } else { EMPTY };
        if !opaque {
            if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
                let target = reference
                    .strip_prefix('#')
                    .and_then(|pointer| self.root.pointer(pointer))
                    .ok_or_else(super::config_secret_graph::invalid)?;
                result = self.schema(target, depth + 1)?;
            }
            for keyword in ["allOf", "oneOf", "anyOf"] {
                if let Some(parts) = node.get(keyword).and_then(Value::as_array) {
                    for part in parts {
                        let child = self.schema(part, depth + 1)?;
                        result = self.union(result, child, 0)?;
                    }
                }
            }
            if let Some(properties) = node.get("properties").and_then(Value::as_object) {
                // Build siblings together: repeated growing-map unions would
                // otherwise make even a flat object unnecessarily quadratic.
                let mut children = BTreeMap::new();
                for (key, child) in properties {
                    let id = self.schema(child, depth + 1)?;
                    if id != EMPTY {
                        children.insert(key.clone(), id);
                    }
                }
                let properties = self.branch(children)?;
                result = self.union(result, properties, 0)?;
            }
        }
        self.active.remove(&identity);
        self.schemas.insert(identity, result);
        Ok(result)
    }

    fn emit(&mut self, id: usize) -> Result<Vec<Vec<String>>, AppError> {
        enum Frame {
            Node(usize, Option<String>),
            Pop,
        }
        let mut output = Vec::new();
        let mut path = Vec::<String>::new();
        let mut pending = vec![Frame::Node(id, None)];
        while let Some(frame) = pending.pop() {
            let Frame::Node(id, key) = frame else {
                path.pop();
                continue;
            };
            if let Some(key) = key {
                path.push(key);
                pending.push(Frame::Pop);
            }
            match &self.nodes[id] {
                Trie::Empty => {}
                Trie::Terminal => {
                    charge(
                        &mut self.work.output_work,
                        1 + path.iter().map(|part| part.len() + 1).sum::<usize>(),
                        self.limit,
                    )?;
                    output.push(path.clone());
                }
                Trie::Branch(children) => {
                    for (key, child) in children.iter().rev() {
                        charge(&mut self.work.output_work, 1 + key.len(), self.limit)?;
                        pending.push(Frame::Node(*child, Some(key.clone())));
                    }
                }
            }
        }
        Ok(output)
    }
}

pub(super) fn paths(schema: &Value) -> Result<Vec<Vec<String>>, AppError> {
    let mut analysis = Analysis::new(schema, WORK_LIMIT)?;
    let result = analysis.schema(schema, 0).and_then(|id| analysis.emit(id));
    tracing::debug!(
        schema_nodes = analysis.work.schema_nodes,
        union_work = analysis.work.union_work,
        output_work = analysis.work.output_work,
        trie_nodes = analysis.nodes.len(),
        complexity_limited = matches!(&result, Err(AppError::SchemaSecretAnalysisTooComplex)),
        "secret schema analysis work"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn work_limits_are_explicit_and_deterministic() {
        let schema = json!({"properties": {"secret": {"writeOnly": true}}});
        let mut analysis = Analysis::new(&schema, 2).unwrap();
        assert!(matches!(
            analysis.schema(&schema, 0),
            Err(AppError::SchemaSecretAnalysisTooComplex)
        ));
        let mut analysis = Analysis::new(&schema, WORK_LIMIT).unwrap();
        let id = analysis.schema(&schema, 0).unwrap();
        analysis.limit = 1;
        assert!(matches!(
            analysis.emit(id),
            Err(AppError::SchemaSecretAnalysisTooComplex)
        ));
    }

    #[test]
    fn canonical_unions_are_shared_and_work_is_counted() {
        let schema = json!({});
        let mut analysis = Analysis::new(&schema, WORK_LIMIT).unwrap();
        let a = analysis
            .branch(BTreeMap::from([("a".into(), TERMINAL)]))
            .unwrap();
        let b = analysis
            .branch(BTreeMap::from([("b".into(), TERMINAL)]))
            .unwrap();
        let first = analysis
            .branch(BTreeMap::from([("nested".into(), a)]))
            .unwrap();
        let second = analysis
            .branch(BTreeMap::from([("nested".into(), b)]))
            .unwrap();
        let joined = analysis.union(first, second, 0).unwrap();
        let before = analysis.work.union_work;
        assert_eq!(analysis.union(second, first, 0).unwrap(), joined);
        assert_eq!(analysis.work.union_work, before + 1);
        let expected = analysis
            .branch(BTreeMap::from([
                ("a".into(), TERMINAL),
                ("b".into(), TERMINAL),
            ]))
            .unwrap();
        let expected = analysis
            .branch(BTreeMap::from([("nested".into(), expected)]))
            .unwrap();
        assert_eq!(joined, expected);
        assert_eq!(
            analysis.emit(joined).unwrap(),
            vec![
                vec!["nested".to_string(), "a".to_string()],
                vec!["nested".to_string(), "b".to_string()]
            ]
        );
    }

    #[test]
    fn shared_diamond_cost_tracks_schema_not_route_count() {
        let mut definitions = serde_json::Map::new();
        definitions.insert(
            "node0".into(),
            json!({"properties": {"secret": {"writeOnly": true}}}),
        );
        for depth in 1..=32 {
            let target = format!("#/$defs/node{}", depth - 1);
            definitions.insert(
                format!("node{depth}"),
                json!({"allOf": [{"$ref": target}, {"$ref": target}]}),
            );
        }
        let schema = json!({"$defs": definitions, "$ref": "#/$defs/node32"});
        let mut analysis = Analysis::new(&schema, WORK_LIMIT).unwrap();
        let id = analysis.schema(&schema, 0).unwrap();
        assert_eq!(analysis.emit(id).unwrap(), vec![vec!["secret".to_string()]]);
        assert!(analysis.work.schema_nodes <= 100);
        assert!(analysis.work.union_work <= 200);
        assert!(analysis.nodes.len() <= 3);
    }
}
