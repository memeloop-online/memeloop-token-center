use std::collections::BTreeMap;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, value::RawValue};

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
    let Some(value) = request.get("messages").or_else(|| request.get("input")) else {
        return Vec::new();
    };
    match value {
        // Clone one entry at a time. Cloning the array first keeps every
        // message alive twice while the atoms are built.
        Value::Array(values) => values.iter().cloned().map(atom_from_entry).collect(),
        Value::String(text) => {
            vec![atom_from_entry(
                serde_json::json!({"role": "user", "content": text}),
            )]
        }
        Value::Object(_) => vec![atom_from_entry(value.clone())],
        _ => Vec::new(),
    }
}

fn atom_from_entry(entry: Value) -> SemanticAtom {
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
    // One canonical copy. Volatile ids are removed in place after the exact
    // hash, instead of cloning the message for a second canonical tree.
    let mut content = canonicalize(entry, false);
    let instance_hash = digest_value(&content);
    strip_volatile_ids(&mut content);
    let content_hash = digest_value(&content);
    SemanticAtom {
        role,
        kind,
        content,
        content_hash,
        instance_hash,
    }
}

fn strip_volatile_ids(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                strip_volatile_ids(value);
            }
        }
        Value::Object(values) => {
            values.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "id" | "call_id" | "tool_call_id" | "request_id" | "created_at"
                )
            });
            for value in values.values_mut() {
                strip_volatile_ids(value);
            }
        }
        _ => {}
    }
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
    let mut hasher = blake3::Hasher::new();
    // Same compact bytes as `to_vec`, without a second buffer of the message.
    serde_json::to_writer(HasherWriter(&mut hasher), value)
        .expect("serializing a JSON value cannot fail");
    hasher.finalize().to_hex().to_string()
}

struct HasherWriter<'a>(&'a mut blake3::Hasher);

impl std::io::Write for HasherWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) enum RequestAtomError<E> {
    Json(serde_json::Error),
    Visitor(E),
}

/// Visits semantic atoms one message at a time.
///
/// `messages` wins over `input`, matching [`extract_atoms`]. Other fields,
/// including a large tool list, stay raw slices and are never parsed. The
/// callback owns each atom; dropping it releases that message before the next
/// one is parsed.
pub(crate) fn try_for_each_request_atom<E>(
    request_json: &[u8],
    mut visit: impl FnMut(SemanticAtom) -> Result<(), E>,
) -> Result<(), RequestAtomError<E>> {
    let Some(range) = select_message_field(request_json).map_err(RequestAtomError::Json)? else {
        return Ok(());
    };
    let raw = &request_json[range];
    match first_significant(raw) {
        None => Err(RequestAtomError::Json(json_error())),
        Some(b'[') => for_each_array(raw, &mut visit),
        Some(b'"') => {
            let text: String = serde_json::from_slice(raw).map_err(RequestAtomError::Json)?;
            visit(atom_from_entry(
                serde_json::json!({"role": "user", "content": text}),
            ))
            .map_err(RequestAtomError::Visitor)
        }
        Some(b'{') => {
            let value: Value = serde_json::from_slice(raw).map_err(RequestAtomError::Json)?;
            visit(atom_from_entry(value)).map_err(RequestAtomError::Visitor)
        }
        Some(_) => {
            let mut deserializer = serde_json::Deserializer::from_slice(raw);
            IgnoredAny::deserialize(&mut deserializer).map_err(RequestAtomError::Json)?;
            deserializer.end().map_err(RequestAtomError::Json)
        }
    }
}

fn for_each_array<E>(
    raw: &[u8],
    visit: &mut impl FnMut(SemanticAtom) -> Result<(), E>,
) -> Result<(), RequestAtomError<E>> {
    struct SeqVisitor<'a, F, E> {
        visit: &'a mut F,
        error: &'a mut Option<E>,
    }

    impl<'de, F, E> Visitor<'de> for SeqVisitor<'_, F, E>
    where
        F: FnMut(SemanticAtom) -> Result<(), E>,
    {
        type Value = ();

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("message array")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(value) = seq.next_element::<Value>()? {
                if let Err(failure) = (self.visit)(atom_from_entry(value)) {
                    *self.error = Some(failure);
                    return Err(de::Error::custom("request atom visitor failed"));
                }
            }
            Ok(())
        }
    }

    let mut visitor_error = None;
    let mut deserializer = serde_json::Deserializer::from_slice(raw);
    let parsed = deserializer.deserialize_seq(SeqVisitor {
        visit,
        error: &mut visitor_error,
    });
    if let Some(error) = visitor_error {
        return Err(RequestAtomError::Visitor(error));
    }
    parsed.map_err(RequestAtomError::Json)?;
    deserializer.end().map_err(RequestAtomError::Json)
}

fn json_error() -> serde_json::Error {
    serde_json::from_slice::<Value>(b"invalid").expect_err("invalid JSON must fail")
}

fn select_message_field(input: &[u8]) -> Result<Option<std::ops::Range<usize>>, serde_json::Error> {
    struct Selector<'a> {
        input: &'a [u8],
    }

    impl<'de> Visitor<'de> for Selector<'de> {
        type Value = Option<std::ops::Range<usize>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("conversation request JSON")
        }

        fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(None)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut messages = None;
            let mut input_field = None;
            while let Some(key) = map.next_key::<String>()? {
                let raw: &RawValue = map.next_value()?;
                let range = raw_range(self.input, raw).ok_or_else(|| {
                    de::Error::custom("semantic field is not inside the request body")
                })?;
                match key.as_str() {
                    "messages" => messages = Some(range),
                    "input" => input_field = Some(range),
                    _ => {}
                }
            }
            Ok(messages.or(input_field))
        }
    }

    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let selected = deserializer.deserialize_any(Selector { input })?;
    deserializer.end()?;
    Ok(selected)
}

fn raw_range(input: &[u8], raw: &RawValue) -> Option<std::ops::Range<usize>> {
    let text = raw.get().as_bytes();
    let base = input.as_ptr() as usize;
    let start = text.as_ptr() as usize;
    let end = start.checked_add(text.len())?;
    let input_end = base.checked_add(input.len())?;
    if start < base || end > input_end {
        return None;
    }
    Some(start - base..end - base)
}

fn first_significant(input: &[u8]) -> Option<u8> {
    input.get(skip_ws(input, 0)).copied()
}

fn skip_ws(input: &[u8], mut index: usize) -> usize {
    while index < input.len() && matches!(input[index], b' ' | b'\n' | b'\r' | b'\t') {
        index += 1;
    }
    index
}

pub(crate) fn digest_parts(parent: Option<&str>, atom: &str) -> String {
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

    #[test]
    fn digest_matches_compact_json_bytes() {
        let value = json!({"z": 1, "a": "line\n", "nested": {"text": "keep"}});
        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            digest_value(&value),
            blake3::hash(&bytes).to_hex().to_string()
        );
    }

    #[test]
    fn stripping_volatile_ids_matches_canonical_rebuild() {
        let entry = json!({
            "role": "assistant",
            "id": "outer",
            "content": [
                {"type": "text", "text": "keep", "call_id": "c"},
                {"type": "tool", "id": "inner", "created_at": 4, "request_id": "r"}
            ],
            "tool_call_id": "t"
        });
        let mut stripped = canonicalize(entry.clone(), false);
        strip_volatile_ids(&mut stripped);
        assert_eq!(stripped, canonicalize(entry, true));
    }

    #[test]
    fn streamed_request_atoms_match_extracted_atoms() {
        let samples = [
            json!({}),
            json!({"model": "x", "tools": [{"name": "big", "schema": {"a": [1, 2, {"b": "}"}]}}], "messages": [
                {"role": "user", "content": "one", "id": "volatile"},
                {"role": "assistant", "content": [{"type": "text", "text": "two", "call_id": "c"}], "tool_call_id": "t"}
            ]}),
            json!({"input": "plain text"}),
            json!({"input": {"role": "user", "content": "object", "created_at": 9}}),
            json!({"messages": null, "input": [{"role": "user", "content": "ignored"}]}),
            json!({"messages": [1, true, null, "x", {"role": "user", "content": "keep [}{] \"quotes\"", "created_at": 1}]}),
            json!([]),
            json!(1),
            json!("hi"),
            json!({"messages": []}),
        ];
        for sample in samples {
            let bytes = serde_json::to_vec(&sample).unwrap();
            assert_eq!(
                streamed_atoms(&bytes).unwrap(),
                extract_atoms(&sample),
                "{sample}"
            );
        }
        let duplicate = br#"{"input":[{"role":"user","content":"in"}],"tools":[1,2,{"x":"[}]"}],"messages":[{"role":"user","content":"first"}],"messages":[{"role":"assistant","content":"second","id":"z"}]}"#;
        let value: serde_json::Value = serde_json::from_slice(duplicate).unwrap();
        assert_eq!(streamed_atoms(duplicate).unwrap(), extract_atoms(&value));
        let pretty = b"{\n  \"messages\" : [\n    { \"role\" : \"user\", \"content\" : \" spaced \", \"id\": \"gone\" }\n  ]\n}";
        let pretty_value: serde_json::Value = serde_json::from_slice(pretty).unwrap();
        assert_eq!(
            streamed_atoms(pretty).unwrap(),
            extract_atoms(&pretty_value)
        );
        assert!(streamed_atoms(b"{").is_err());
        assert!(streamed_atoms(b"{\"messages\":[1,]}").is_err());
        assert!(streamed_atoms(b"{\"messages\":[1}").is_err());
    }

    fn streamed_atoms(bytes: &[u8]) -> Result<Vec<SemanticAtom>, serde_json::Error> {
        let mut atoms = Vec::new();
        try_for_each_request_atom(bytes, |atom| {
            atoms.push(atom);
            Ok::<(), serde_json::Error>(())
        })
        .map_err(|error| match error {
            RequestAtomError::Json(error) | RequestAtomError::Visitor(error) => error,
        })?;
        Ok(atoms)
    }
}
