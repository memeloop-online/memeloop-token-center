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

/// 会话正文里的一个片段。文本直接放在 `text` 里。
/// 媒体片段保存序号、媒体类型、字节长度、blake3 和对象 locator。
/// `media_bytes` 只在写入对象之前短暂存在，不会进入 CNPG。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBodyPart {
    pub part_index: u32,
    pub kind: String,
    pub text: Option<String>,
    pub media_type: Option<String>,
    pub byte_length: Option<u64>,
    pub content_blake3: Option<String>,
    pub object_locator: Option<String>,
    pub call_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub media_bytes: Option<Vec<u8>>,
}

/// 一轮对话。同一个用户回合里的多个片段按 `part_index` 排在一起。
/// `origin` 是 `request`（来自原始请求 JSON）或 `return`（来自原始返回 JSON）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBodyTurn {
    pub role: String,
    pub origin: String,
    pub parts: Vec<SessionBodyPart>,
}

/// 从原始请求 JSON 和原始返回 JSON 整理出的会话正文。
/// 这不是那两份原始文档。各上游的私有字段已经去掉。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBody {
    pub turns: Vec<SessionBodyTurn>,
}

/// 训练导出用的一轮。只包含 CNPG 里已经写下的文本和媒体 locator。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrainingUtterance {
    pub role: String,
    pub origin: String,
    pub parts: Vec<Value>,
}

/// 会话正文归档里一条媒体引用。相同 blake3 对应一个对象。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMediaLocator {
    pub turn_id: String,
    pub part_index: i64,
    pub media_type: String,
    pub byte_length: i64,
    pub content_blake3: String,
    pub object_locator: String,
}

const PRIVATE_PROVIDER_KEYS: &[&str] = &[
    "encrypted_content",
    "encrypted_reasoning",
    "thought_signature",
    "reasoning_opaque",
];

/// 把原始请求 JSON 和原始返回 JSON 收成会话正文。
/// 不读对象存储。媒体字节留在 `media_bytes`，由 worker 写成 blake3 对象后再填 locator。
/// 以后若 CNPG 中的会话正文需要重做，可以对已经读出的两份文档再调用这里。这里不做历史回填。
pub fn session_body_from_documents(request: &Value, return_document: &Value) -> SessionBody {
    let mut turns = Vec::new();
    append_request_turns(&mut turns, request);
    append_return_turns(&mut turns, return_document);
    SessionBody { turns }
}

/// 原始返回 JSON 的字节。JSON 直接解析；SSE 收成一份 JSON，供 [`session_body_from_documents`] 使用。
pub fn raw_return_document(bytes: &[u8]) -> Option<Value> {
    let trimmed = trim_ascii_start(bytes);
    if trimmed
        .first()
        .is_some_and(|byte| matches!(byte, b'{' | b'['))
    {
        return serde_json::from_slice(trimmed).ok();
    }
    if !bytes.windows(5).any(|window| window == b"data:") {
        return None;
    }
    let mut completed = None;
    let mut text = String::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        let line = trim_ascii(line);
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = trim_ascii(payload);
        if payload == b"[DONE]" || payload.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(payload) else {
            continue;
        };
        if let Some(response) = value.get("response").filter(|response| {
            response.get("output").is_some() || response.get("choices").is_some()
        }) {
            completed = Some(response.clone());
            continue;
        }
        if value.get("output").is_some() || value.get("choices").is_some() {
            completed = Some(value);
            continue;
        }
        if let Some(fragment) = value
            .pointer("/delta/content")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
            })
        {
            text.push_str(fragment);
        }
    }
    if let Some(value) = completed {
        return Some(value);
    }
    if text.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }]
    }))
}

impl SessionBody {
    /// 取出尚未写入对象存储的媒体字节。序号用于把 locator 填回对应片段。
    pub fn take_media_bytes(&mut self) -> Vec<(usize, usize, Vec<u8>)> {
        let mut pending = Vec::new();
        for (turn_index, turn) in self.turns.iter_mut().enumerate() {
            for (part_index, part) in turn.parts.iter_mut().enumerate() {
                if let Some(bytes) = part.media_bytes.take() {
                    pending.push((turn_index, part_index, bytes));
                }
            }
        }
        pending
    }

    pub fn set_media_locator(&mut self, turn_index: usize, part_index: usize, locator: String) {
        if let Some(part) = self
            .turns
            .get_mut(turn_index)
            .and_then(|turn| turn.parts.get_mut(part_index))
        {
            part.object_locator = Some(locator);
        }
    }

    /// 前缀按回合的内容哈希连接。相同文本回合只占一次。
    pub fn into_prefix(&self) -> (Vec<SemanticAtom>, Vec<PrefixNode>) {
        let atoms: Vec<SemanticAtom> = self.turns.iter().map(atom_from_turn).collect();
        let nodes = build_prefix(&atoms);
        (atoms, nodes)
    }

    pub fn media_locators(&self, atoms: &[SemanticAtom]) -> Vec<SessionMediaLocator> {
        self.turns
            .iter()
            .zip(atoms)
            .flat_map(|(turn, atom)| {
                turn.parts.iter().filter_map(|part| {
                    let content_blake3 = part.content_blake3.as_deref()?;
                    let object_locator = part.object_locator.as_deref()?;
                    let media_type = part
                        .media_type
                        .as_deref()
                        .unwrap_or("application/octet-stream");
                    let byte_length = i64::try_from(part.byte_length.unwrap_or(0)).ok()?;
                    Some(SessionMediaLocator {
                        turn_id: atom.content_hash.clone(),
                        part_index: i64::from(part.part_index),
                        media_type: media_type.to_owned(),
                        byte_length,
                        content_blake3: content_blake3.to_owned(),
                        object_locator: object_locator.to_owned(),
                    })
                })
            })
            .collect()
    }
}

/// 训练导出只看已经写进 CNPG 的回合 JSON。不打开对象存储。
pub fn training_dialogue(turns: &[Value]) -> Vec<TrainingUtterance> {
    turns
        .iter()
        .filter_map(|turn| {
            let role = turn.get("role")?.as_str()?.to_owned();
            let origin = turn
                .get("origin")
                .and_then(Value::as_str)
                .unwrap_or("request")
                .to_owned();
            let parts = turn
                .get("parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Some(TrainingUtterance {
                role,
                origin,
                parts,
            })
        })
        .collect()
}

fn append_request_turns(turns: &mut Vec<SessionBodyTurn>, request: &Value) {
    let Some(value) = request.get("messages").or_else(|| request.get("input")) else {
        return;
    };
    match value {
        Value::String(text) => turns.push(text_turn("user", "request", text)),
        Value::Array(items) => append_entry_turns(turns, items, "request"),
        Value::Object(_) => turns.push(turn_from_entry(value, "request")),
        _ => {}
    }
}

fn append_return_turns(turns: &mut Vec<SessionBodyTurn>, document: &Value) {
    if let Some(output) = document.get("output").and_then(Value::as_array)
        && !output.is_empty()
    {
        append_entry_turns(turns, output, "return");
        return;
    }
    if let Some(message) = document.pointer("/choices/0/message") {
        turns.push(turn_from_entry(message, "return"));
        return;
    }
    if document.get("role").is_some() || document.get("content").is_some() {
        turns.push(turn_from_entry(document, "return"));
    }
}

fn append_entry_turns(turns: &mut Vec<SessionBodyTurn>, items: &[Value], origin: &str) {
    let mut bare_parts = Vec::new();
    for item in items {
        if is_bare_content_part(item) {
            bare_parts.push(item);
            continue;
        }
        flush_bare_parts(turns, &mut bare_parts, origin);
        turns.push(turn_from_entry(item, origin));
    }
    flush_bare_parts(turns, &mut bare_parts, origin);
}

fn flush_bare_parts(turns: &mut Vec<SessionBodyTurn>, parts: &mut Vec<&Value>, origin: &str) {
    if parts.is_empty() {
        return;
    }
    let content = Value::Array(parts.iter().map(|part| (*part).clone()).collect());
    turns.push(turn_from_content("user", origin, &content, None, None));
    parts.clear();
}

fn is_bare_content_part(value: &Value) -> bool {
    value.get("role").is_none()
        && value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "text"
                        | "input_text"
                        | "output_text"
                        | "input_image"
                        | "image"
                        | "image_url"
                        | "input_audio"
                        | "audio"
                        | "input_video"
                        | "video"
                        | "input_file"
                        | "file"
                )
            })
}

fn turn_from_entry(entry: &Value, origin: &str) -> SessionBodyTurn {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let role = entry.get("role").and_then(Value::as_str).unwrap_or(
        if matches!(kind, "function_call" | "function_call_output") {
            "tool"
        } else if origin == "return" {
            "assistant"
        } else {
            "user"
        },
    );
    let call_id = entry
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tool_call_id = entry
        .get("tool_call_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if kind == "function_call" {
        return tool_call_turn(origin, entry, call_id);
    }
    if matches!(kind, "function_call_output") || role == "tool" {
        let text = entry
            .get("output")
            .or_else(|| entry.get("content"))
            .map(value_text);
        return SessionBodyTurn {
            role: "tool".into(),
            origin: origin.to_owned(),
            parts: vec![SessionBodyPart {
                part_index: 0,
                kind: "tool_result".into(),
                text,
                call_id,
                tool_call_id,
                ..empty_part()
            }],
        };
    }
    let content = entry.get("content").cloned().unwrap_or(Value::Null);
    let mut turn = if content.is_null() {
        SessionBodyTurn {
            role: role.to_owned(),
            origin: origin.to_owned(),
            parts: Vec::new(),
        }
    } else {
        turn_from_content(role, origin, &content, call_id, tool_call_id)
    };
    if let Some(calls) = entry.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let index = turn.parts.len() as u32;
            turn.parts.push(tool_call_part(index, call));
        }
    }
    turn
}

fn tool_call_turn(origin: &str, entry: &Value, call_id: Option<String>) -> SessionBodyTurn {
    SessionBodyTurn {
        role: "assistant".into(),
        origin: origin.to_owned(),
        parts: vec![tool_call_part(0, entry).with_call_id(call_id)],
    }
}

fn tool_call_part(part_index: u32, call: &Value) -> SessionBodyPart {
    let call_id = call
        .get("call_id")
        .or_else(|| call.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let name = call
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| call.pointer("/function/name").and_then(Value::as_str))
        .unwrap_or("");
    let arguments = call
        .get("arguments")
        .or_else(|| call.pointer("/function/arguments"))
        .map(value_text)
        .unwrap_or_default();
    SessionBodyPart {
        part_index,
        kind: "tool_call".into(),
        text: Some(format!("{name}\n{arguments}")),
        call_id,
        ..empty_part()
    }
}

fn text_turn(role: &str, origin: &str, text: &str) -> SessionBodyTurn {
    SessionBodyTurn {
        role: role.to_owned(),
        origin: origin.to_owned(),
        parts: vec![SessionBodyPart {
            part_index: 0,
            kind: "text".into(),
            text: Some(text.to_owned()),
            ..empty_part()
        }],
    }
}

fn turn_from_content(
    role: &str,
    origin: &str,
    content: &Value,
    call_id: Option<String>,
    tool_call_id: Option<String>,
) -> SessionBodyTurn {
    let mut parts = Vec::new();
    match content {
        Value::String(text) => parts.push(SessionBodyPart {
            part_index: 0,
            kind: "text".into(),
            text: Some(text.clone()),
            call_id: call_id.clone(),
            tool_call_id: tool_call_id.clone(),
            ..empty_part()
        }),
        Value::Array(items) => {
            for item in items {
                push_content_part(&mut parts, item, call_id.clone(), tool_call_id.clone());
            }
        }
        other => push_content_part(&mut parts, other, call_id, tool_call_id),
    }
    SessionBodyTurn {
        role: role.to_owned(),
        origin: origin.to_owned(),
        parts,
    }
}

fn push_content_part(
    parts: &mut Vec<SessionBodyPart>,
    item: &Value,
    call_id: Option<String>,
    tool_call_id: Option<String>,
) {
    let cleaned = strip_private_provider_fields(item.clone());
    if part_is_private_only(&cleaned) {
        return;
    }
    let index = parts.len() as u32;
    if let Some((media_type, bytes)) = media_bytes_from_part(&cleaned) {
        let content_blake3 = blake3::hash(&bytes).to_hex().to_string();
        let byte_length = bytes.len() as u64;
        parts.push(SessionBodyPart {
            part_index: index,
            kind: media_kind(&media_type),
            media_type: Some(media_type),
            byte_length: Some(byte_length),
            content_blake3: Some(content_blake3),
            media_bytes: Some(bytes),
            call_id,
            tool_call_id,
            ..empty_part()
        });
        return;
    }
    let text = match &cleaned {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| map.get("content").and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    };
    if text.as_deref().is_none_or(str::is_empty) && !cleaned.is_string() {
        return;
    }
    parts.push(SessionBodyPart {
        part_index: index,
        kind: "text".into(),
        text,
        call_id,
        tool_call_id,
        ..empty_part()
    });
}

fn part_is_private_only(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "reasoning" | "compaction"))
        && value.get("text").is_none()
        && value.get("content").is_none()
}

fn media_kind(media_type: &str) -> String {
    if media_type.starts_with("audio/") {
        "audio".into()
    } else if media_type.starts_with("video/") {
        "video".into()
    } else if media_type.starts_with("image/") {
        "image".into()
    } else {
        "file".into()
    }
}

fn media_bytes_from_part(value: &Value) -> Option<(String, Vec<u8>)> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let declared = kind_media_type(kind);
    let candidates = [
        value.get("image_url").and_then(Value::as_str),
        value.pointer("/image_url/url").and_then(Value::as_str),
        value.get("url").and_then(Value::as_str),
        value.get("data").and_then(Value::as_str),
        value.pointer("/source/data").and_then(Value::as_str),
        value.get("file_data").and_then(Value::as_str),
        value.get("result").and_then(Value::as_str),
        value.get("b64_json").and_then(Value::as_str),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(decoded) = decode_media_payload(candidate, declared) {
            return Some(decoded);
        }
    }
    if declared.is_some()
        && let Some(nested) = value.get("content").and_then(Value::as_str)
    {
        return decode_media_payload(nested, declared);
    }
    None
}

fn kind_media_type(kind: &str) -> Option<&'static str> {
    match kind {
        "input_image" | "image" | "image_url" | "image_generation_call" => Some("image/png"),
        "input_audio" | "audio" => Some("audio/wav"),
        "input_video" | "video" => Some("video/mp4"),
        "input_file" | "file" => Some("application/octet-stream"),
        _ => None,
    }
}

fn decode_media_payload(payload: &str, fallback_type: Option<&str>) -> Option<(String, Vec<u8>)> {
    let payload = payload.trim();
    if let Some(rest) = payload.strip_prefix("data:") {
        let (header, data) = rest.split_once(',')?;
        let media_type = header
            .split(';')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("application/octet-stream");
        if !header_is_media(media_type) && fallback_type.is_none() {
            return None;
        }
        let bytes = decode_base64(data.trim())?;
        if bytes.is_empty() {
            return None;
        }
        return Some((media_type.to_owned(), bytes));
    }
    let media_type = fallback_type?;
    if payload.len() < 16 {
        return None;
    }
    let bytes = decode_base64(payload)?;
    if bytes.is_empty() {
        return None;
    }
    Some((media_type.to_owned(), bytes))
}

fn header_is_media(media_type: &str) -> bool {
    let media_type = media_type.split(';').next().unwrap_or(media_type);
    media_type.starts_with("image/")
        || media_type.starts_with("audio/")
        || media_type.starts_with("video/")
        || media_type == "application/octet-stream"
        || media_type.starts_with("application/pdf")
}

fn decode_base64(payload: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(payload))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload))
        .ok()
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn empty_part() -> SessionBodyPart {
    SessionBodyPart {
        part_index: 0,
        kind: String::new(),
        text: None,
        media_type: None,
        byte_length: None,
        content_blake3: None,
        object_locator: None,
        call_id: None,
        tool_call_id: None,
        media_bytes: None,
    }
}

impl SessionBodyPart {
    fn with_call_id(mut self, call_id: Option<String>) -> Self {
        if self.call_id.is_none() {
            self.call_id = call_id;
        }
        self
    }
}

fn atom_from_turn(turn: &SessionBodyTurn) -> SemanticAtom {
    let mut content = turn_value(turn);
    let instance_hash = digest_value(&content);
    strip_instance_ids(&mut content);
    let content_hash = digest_value(&content);
    SemanticAtom {
        role: turn.role.clone(),
        kind: "turn".into(),
        content,
        content_hash,
        instance_hash,
    }
}

fn turn_value(turn: &SessionBodyTurn) -> Value {
    let mut map = Map::new();
    map.insert("role".into(), Value::String(turn.role.clone()));
    map.insert("origin".into(), Value::String(turn.origin.clone()));
    map.insert(
        "parts".into(),
        Value::Array(turn.parts.iter().map(part_value).collect()),
    );
    Value::Object(map)
}

fn part_value(part: &SessionBodyPart) -> Value {
    let mut map = Map::new();
    map.insert("part_index".into(), Value::from(part.part_index));
    map.insert("kind".into(), Value::String(part.kind.clone()));
    if let Some(text) = &part.text {
        map.insert("text".into(), Value::String(text.clone()));
    }
    if let Some(media_type) = &part.media_type {
        map.insert("media_type".into(), Value::String(media_type.clone()));
    }
    if let Some(byte_length) = part.byte_length {
        map.insert("byte_length".into(), Value::from(byte_length));
    }
    if let Some(hash) = &part.content_blake3 {
        map.insert("content_blake3".into(), Value::String(hash.clone()));
    }
    if let Some(locator) = &part.object_locator {
        map.insert("object_locator".into(), Value::String(locator.clone()));
    }
    if let Some(call_id) = &part.call_id {
        map.insert("call_id".into(), Value::String(call_id.clone()));
    }
    if let Some(tool_call_id) = &part.tool_call_id {
        map.insert("tool_call_id".into(), Value::String(tool_call_id.clone()));
    }
    Value::Object(map)
}

fn strip_private_provider_fields(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(strip_private_provider_fields)
                .collect(),
        ),
        Value::Object(map) => {
            let mut cleaned = Map::new();
            for (key, child) in map {
                if PRIVATE_PROVIDER_KEYS.contains(&key.as_str()) {
                    continue;
                }
                cleaned.insert(key, strip_private_provider_fields(child));
            }
            Value::Object(cleaned)
        }
        other => other,
    }
}

fn strip_instance_ids(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                strip_instance_ids(item);
            }
        }
        Value::Object(map) => {
            map.retain(|key, _| !matches!(key.as_str(), "id" | "request_id" | "created_at"));
            for child in map.values_mut() {
                strip_instance_ids(child);
            }
        }
        _ => {}
    }
}

fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end]
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
    use base64::Engine;
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

    #[test]
    fn session_body_keeps_one_user_turn_tool_ids_and_the_return_reply() {
        let png = general_purpose_png();
        let request = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "第一段\n第二段"},
                        {"type": "input_image", "image_url": format!("data:image/png;base64,{png}")},
                        {"type": "input_text", "text": "接着说", "encrypted_content": "secret"}
                    ]
                },
                {"role": "user", "content": "另一回合"}
            ]
        });
        let return_document = json!({
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call-1",
                    "name": "lookup",
                    "arguments": "{\"q\":1}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call-1",
                    "output": "ok"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "这是本轮回复", "encrypted_content": "hidden"}]
                }
            ]
        });
        let mut body = session_body_from_documents(&request, &return_document);
        assert_eq!(body.turns[0].role, "user");
        assert_eq!(body.turns[0].parts.len(), 3);
        assert_eq!(
            body.turns[0].parts[0].text.as_deref(),
            Some("第一段\n第二段")
        );
        assert!(body.turns[0].parts[1].media_bytes.is_some());
        assert!(body.turns[0].parts.iter().all(|part| {
            part.text.as_deref() != Some("secret") && part.text.as_deref() != Some("hidden")
        }));
        assert_eq!(body.turns[1].parts[0].text.as_deref(), Some("另一回合"));
        let image = &mut body.turns[0].parts[1];
        image.object_locator = Some("objects/blake3/ab/hash".into());
        let (atoms, nodes) = body.into_prefix();
        assert_eq!(atoms.len(), 5);
        assert_eq!(nodes.len(), 5);
        let stored = atoms[0].content.to_string();
        assert!(!stored.contains("png-bytes") && !stored.contains("secret"));
        assert!(stored.contains("content_blake3"));
        assert!(atoms[2].content.to_string().contains("call-1"));
        assert_eq!(atoms[4].role, "assistant");
        let dialogue = training_dialogue(
            &atoms
                .iter()
                .map(|atom| atom.content.clone())
                .collect::<Vec<_>>(),
        );
        assert_eq!(dialogue.len(), 5);
        assert!(dialogue[0].parts[1].get("object_locator").is_some());
        assert!(dialogue[0].parts[1].get("media_bytes").is_none());
    }

    fn general_purpose_png() -> String {
        base64::engine::general_purpose::STANDARD.encode(b"png-bytes")
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
