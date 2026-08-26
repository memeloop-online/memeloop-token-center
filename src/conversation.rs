use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationHints {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub parent_turn_id: Option<String>,
    pub branch_id: Option<String>,
    pub compaction: bool,
    /// An explicit client assertion that this turn was spawned as a subagent.
    /// The database only honors it when `parent_turn_id` resolves to an
    /// observation no later than this turn and owned by the same tenant,
    /// principal, and stable key.
    #[serde(default)]
    pub subagent: bool,
    /// Human-readable and explicitly reported execution metadata. These
    /// values are diagnostic labels only; they never participate in tenant,
    /// credential, routing, or billing authorization.
    #[serde(default)]
    pub session_name: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub span_id: Option<String>,
    #[serde(default)]
    pub parent_span_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub parent_agent_id: Option<String>,
    #[serde(default)]
    pub task_kind: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ConversationHints {
    pub fn execution_metadata(&self) -> Option<ExecutionMetadata> {
        let metadata = ExecutionMetadata {
            session_name: self.session_name.clone(),
            trace_id: self.trace_id.clone(),
            span_id: self.span_id.clone(),
            parent_span_id: self.parent_span_id.clone(),
            agent_id: self.agent_id.clone(),
            parent_agent_id: self.parent_agent_id.clone(),
            task_kind: self.task_kind.clone(),
            labels: self.labels.clone(),
            source: "declared".to_owned(),
        };
        metadata.has_values().then_some(metadata)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionMetadata {
    pub session_name: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub task_kind: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// `declared` means the downstream application supplied these values.
    /// Prefix-tree and relationship inference remains separately represented
    /// by conversation edges and their confidence/evidence.
    pub source: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionStructure {
    /// Stable session/thread identifier supplied by a compatible client (for
    /// example `X-Codex-Session-Id` or Responses `prompt_cache_key`).
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub parent_turn_id: Option<String>,
    pub response_id: Option<String>,
    pub branch_id: Option<String>,
    pub compaction: bool,
    pub client_name: Option<String>,
    /// Structural values originate in client/protocol hints. Prefix-tree and
    /// relationship inference remains represented by edges with confidence.
    pub source: String,
}

impl ExecutionStructure {
    pub fn has_values(&self) -> bool {
        self.session_id.is_some()
            || self.turn_id.is_some()
            || self.parent_turn_id.is_some()
            || self.response_id.is_some()
            || self.branch_id.is_some()
            || self.compaction
            || self.client_name.is_some()
    }
}

impl ExecutionMetadata {
    fn has_values(&self) -> bool {
        self.session_name.is_some()
            || self.trace_id.is_some()
            || self.span_id.is_some()
            || self.parent_span_id.is_some()
            || self.agent_id.is_some()
            || self.parent_agent_id.is_some()
            || self.task_kind.is_some()
            || !self.labels.is_empty()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SemanticAtom {
    pub role: String,
    pub kind: String,
    pub content: Value,
    pub content_hash: String,
    pub instance_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrefixNode {
    pub node_hash: String,
    pub parent_hash: Option<String>,
    pub atom_hash: String,
    pub depth: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Continues,
    Retry,
    Edit,
    Branch,
    Compacts,
    Subagent,
    Candidate,
}

pub fn extract_atoms(request: &Value) -> Vec<SemanticAtom> {
    let entries = request
        .get("messages")
        .or_else(|| request.get("input"))
        .map(entries_from_value)
        .unwrap_or_default();

    entries
        .into_iter()
        .map(|entry| {
            let role = entry
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
                .to_owned();
            let kind = entry
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
                .to_owned();
            let exact = canonicalize(entry.clone(), false);
            let semantic = canonicalize(entry, true);
            SemanticAtom {
                role,
                kind,
                content: semantic.clone(),
                content_hash: digest_value(&semantic),
                instance_hash: digest_value(&exact),
            }
        })
        .collect()
}

pub fn build_prefix(atoms: &[SemanticAtom]) -> Vec<PrefixNode> {
    let mut parent: Option<String> = None;
    atoms
        .iter()
        .enumerate()
        .map(|(index, atom)| {
            let node_hash = digest_parts(parent.as_deref(), &atom.content_hash);
            let node = PrefixNode {
                node_hash: node_hash.clone(),
                parent_hash: parent.clone(),
                atom_hash: atom.content_hash.clone(),
                depth: index + 1,
            };
            parent = Some(node_hash);
            node
        })
        .collect()
}

pub fn infer_relation(previous: &[SemanticAtom], current: &[SemanticAtom]) -> (RelationKind, f32) {
    let shared = previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left.content_hash == right.content_hash)
        .count();
    if shared == previous.len() && shared == current.len() {
        return (RelationKind::Retry, 0.98);
    }
    if shared == previous.len() && current.len() > previous.len() {
        return (RelationKind::Continues, 0.95);
    }
    if shared > 0 && shared + 1 >= previous.len().min(current.len()) {
        return (RelationKind::Edit, 0.82);
    }
    if shared >= 2 {
        return (RelationKind::Branch, 0.72);
    }
    (RelationKind::Candidate, 0.35)
}

fn entries_from_value(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(values) => values.clone(),
        Value::String(text) => vec![serde_json::json!({"role": "user", "content": text})],
        Value::Object(_) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn canonicalize(value: Value, remove_volatile_ids: bool) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| canonicalize(value, remove_volatile_ids))
                .collect(),
        ),
        Value::Object(values) => {
            let mut canonical = Map::new();
            let mut entries: Vec<_> = values.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, value) in entries {
                if remove_volatile_ids
                    && matches!(
                        key.as_str(),
                        "id" | "call_id" | "tool_call_id" | "request_id" | "created_at"
                    )
                {
                    continue;
                }
                canonical.insert(key, canonicalize(value, remove_volatile_ids));
            }
            Value::Object(canonical)
        }
        value => value,
    }
}

fn digest_value(value: &Value) -> String {
    blake3::hash(&serde_json::to_vec(value).expect("serializing a JSON value cannot fail"))
        .to_hex()
        .to_string()
}

fn digest_parts(parent: Option<&str>, atom: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(parent.unwrap_or("root").as_bytes());
    hash.update(&[0]);
    hash.update(atom.as_bytes());
    hash.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replayed_history_reuses_the_same_merkle_prefix() {
        let first = extract_atoms(&json!({
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two", "id": "volatile-a"}
            ]
        }));
        let second = extract_atoms(&json!({
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two", "id": "volatile-b"},
                {"role": "user", "content": "three"}
            ]
        }));
        let first_nodes = build_prefix(&first);
        let second_nodes = build_prefix(&second);

        assert_eq!(first_nodes[0].node_hash, second_nodes[0].node_hash);
        assert_eq!(first_nodes[1].node_hash, second_nodes[1].node_hash);
        assert_ne!(first[1].instance_hash, second[1].instance_hash);
        assert_eq!(
            infer_relation(&first, &second),
            (RelationKind::Continues, 0.95)
        );
    }

    #[test]
    fn unrelated_histories_remain_candidates_instead_of_forced_merges() {
        let first = extract_atoms(&json!({"messages": [{"role": "user", "content": "alpha"}]}));
        let second = extract_atoms(&json!({"messages": [{"role": "user", "content": "beta"}]}));

        assert_eq!(
            infer_relation(&first, &second),
            (RelationKind::Candidate, 0.35)
        );
    }
}
