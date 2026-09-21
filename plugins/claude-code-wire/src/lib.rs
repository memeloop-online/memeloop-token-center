//! Claude Code wire shim plugin.
//!
//! Rewrites the final serialized Anthropic Messages request body into the
//! exact wire format of the official Claude Code CLI so that an
//! anthropic-claude (OAuth subscription) upstream sees byte-identical
//! traffic. The logic is a bit-for-bit port of the pi-black reference
//! implementation (src/claude-code-protocol.ts):
//!
//! - system block surgery: strip a stale billing + Agent SDK block pair (or
//!   the single legacy pi OAuth block), then prepend a fresh billing block
//!   (with the cc_version fingerprint and the cch=00000 placeholder) and the
//!   Agent SDK block.
//! - cch checksum: the normalized body (model blanked, max_tokens removed)
//!   is hashed with seeded XXH64; the low 20 bits as 5 lowercase hex digits
//!   replace the placeholder. The hash input derives from the exact bytes we
//!   hand back; the host forwards them untouched, so the checksum stays
//!   self-consistent for a validator that recomputes it from the wire bytes.
//! - optional metadata.user_id identity (device_id/account_uuid from plugin
//!   configuration, session_id derived from tenant_id + key_id).
//! - canonical Claude Code fingerprint headers (user-agent, x-app,
//!   x-claude-code-session-id, x-client-request-id, x-stainless-*), all
//!   overridable through plugin configuration.
//!
//! Fail-closed: any input that cannot be rewritten safely (missing
//! model/max_tokens, unsupported system shape, invalid configuration)
//! returns Err and the host rejects the request.
wit_bindgen::generate!({
    world: "memeloop:token-center/wire-shim-plugin@0.3.0",
    // Plugin-local copy: two versions of the memeloop:token-center package
    // cannot live in one directory, so the frozen 0.2.0 package sits under
    // wit/deps (see build.sh, which refreshes these from the host repo).
    path: "wit",
});

use std::borrow::ToOwned;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;
use exports::memeloop::token_center0_3_0::wire_shim_v1::FinalizeResult;
#[cfg(target_arch = "wasm32")]
use exports::memeloop::token_center0_3_0::wire_shim_v1::Guest;
#[cfg(target_arch = "wasm32")]
use memeloop::token_center0_3_0::host;
use memeloop::token_center0_3_0::types::RequestContext;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const CCH_PLACEHOLDER: &str = "cch=00000";
const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;
const BILLING_PREFIX: &str = "x-anthropic-billing-header: ";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const AGENT_SDK_SYSTEM_PROMPT: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const LEGACY_PI_OAUTH_SYSTEM_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

// ---------------------------------------------------------------------------
// XXH64 (hand-written u64 wrapping port of pi-black's BigInt version)
// ---------------------------------------------------------------------------

const PRIME64_1: u64 = 0x9e37_79b1_85eb_ca87;
const PRIME64_2: u64 = 0xc2b2_ae3d_27d4_eb4f;
const PRIME64_3: u64 = 0x1656_67b1_9e37_79f9;
const PRIME64_4: u64 = 0x85eb_ca77_c2b2_ae63;
const PRIME64_5: u64 = 0x27d4_eb2f_1656_67c5;

fn read_u32_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from(u32::from_le_bytes(
        bytes[offset..offset + 4].try_into().expect("4-byte lane"),
    ))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8-byte lane"))
}

#[inline]
fn xxh64_round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(PRIME64_2))
        .rotate_left(31)
        .wrapping_mul(PRIME64_1)
}

#[inline]
fn xxh64_merge_round(accumulator: u64, value: u64) -> u64 {
    (accumulator ^ xxh64_round(0, value))
        .wrapping_mul(PRIME64_1)
        .wrapping_add(PRIME64_4)
}

fn xxhash64(bytes: &[u8], seed: u64) -> u64 {
    let mut offset = 0usize;
    let mut hash: u64;

    if bytes.len() >= 32 {
        let mut v1 = seed.wrapping_add(PRIME64_1).wrapping_add(PRIME64_2);
        let mut v2 = seed.wrapping_add(PRIME64_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME64_1);
        while offset <= bytes.len() - 32 {
            v1 = xxh64_round(v1, read_u64_le(bytes, offset));
            v2 = xxh64_round(v2, read_u64_le(bytes, offset + 8));
            v3 = xxh64_round(v3, read_u64_le(bytes, offset + 16));
            v4 = xxh64_round(v4, read_u64_le(bytes, offset + 24));
            offset += 32;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        hash = xxh64_merge_round(hash, v1);
        hash = xxh64_merge_round(hash, v2);
        hash = xxh64_merge_round(hash, v3);
        hash = xxh64_merge_round(hash, v4);
    } else {
        hash = seed.wrapping_add(PRIME64_5);
    }

    hash = hash.wrapping_add(bytes.len() as u64);
    while offset + 8 <= bytes.len() {
        let lane = xxh64_round(0, read_u64_le(bytes, offset));
        hash = (hash ^ lane)
            .rotate_left(27)
            .wrapping_mul(PRIME64_1)
            .wrapping_add(PRIME64_4);
        offset += 8;
    }
    if offset + 4 <= bytes.len() {
        hash ^= read_u32_le(bytes, offset).wrapping_mul(PRIME64_1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME64_2)
            .wrapping_add(PRIME64_3);
        offset += 4;
    }
    while offset < bytes.len() {
        hash ^= u64::from(bytes[offset]).wrapping_mul(PRIME64_5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME64_1);
        offset += 1;
    }

    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME64_2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME64_3);
    hash ^= hash >> 32;
    hash
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Configuration {
    enabled: bool,
    claude_code_version: String,
    entrypoint: String,
    device_id: Option<String>,
    account_uuid: Option<String>,
    stainless_lang: String,
    stainless_package_version: String,
    stainless_os: String,
    stainless_arch: String,
    stainless_runtime: String,
    stainless_runtime_version: String,
    stainless_timeout: String,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            enabled: true,
            claude_code_version: "2.1.258".to_owned(),
            entrypoint: "sdk-cli".to_owned(),
            device_id: None,
            account_uuid: None,
            stainless_lang: "js".to_owned(),
            stainless_package_version: "0.60.0".to_owned(),
            stainless_os: "MacOS".to_owned(),
            stainless_arch: "arm64".to_owned(),
            stainless_runtime: "node".to_owned(),
            stainless_runtime_version: "v22.14.0".to_owned(),
            stainless_timeout: "600".to_owned(),
        }
    }
}

struct Identity<'a> {
    device_id: &'a str,
    account_uuid: &'a str,
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|inner| !inner.is_empty())
}

fn is_simple_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_valid_device_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Mirrors pi-black's accountUuid regex:
/// ^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$ (i)
fn is_valid_account_uuid(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let hex = |part: &str| {
        part.bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    };
    parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts.iter().all(|part| hex(part))
        && matches!(parts[2].as_bytes()[0], b'1'..=b'5')
        && matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

fn is_header_safe(value: &str) -> bool {
    !value.contains(['\r', '\n'])
}

impl Configuration {
    fn parse(config_json: &str) -> Result<Self, String> {
        serde_json::from_str(config_json)
            .map_err(|_| "invalid claude-code-wire configuration JSON".to_owned())
    }

    fn validate(&self) -> Result<(), String> {
        if !is_simple_token(&self.claude_code_version) {
            return Err("invalid claude_code_version in configuration".into());
        }
        if !is_simple_token(&self.entrypoint) {
            return Err("invalid entrypoint in configuration".into());
        }
        for value in [
            &self.stainless_lang,
            &self.stainless_package_version,
            &self.stainless_os,
            &self.stainless_arch,
            &self.stainless_runtime,
            &self.stainless_runtime_version,
            &self.stainless_timeout,
        ] {
            if !is_header_safe(value) {
                return Err("invalid x-stainless value in configuration".into());
            }
        }
        self.identity()?;
        Ok(())
    }

    /// Identity is written only when both parts are configured (pi-black
    /// shows metadata.user_id is optional); a half-configured or malformed
    /// identity is a configuration error, not a silent skip.
    fn identity(&self) -> Result<Option<Identity<'_>>, String> {
        let device_id = non_empty(&self.device_id);
        let account_uuid = non_empty(&self.account_uuid);
        match (device_id, account_uuid) {
            (None, None) => Ok(None),
            (Some(device_id), Some(account_uuid)) => {
                if !is_valid_device_id(device_id) {
                    return Err("device_id must be 64 lowercase hex characters".into());
                }
                if !is_valid_account_uuid(account_uuid) {
                    return Err("account_uuid must be a canonical UUID".into());
                }
                Ok(Some(Identity {
                    device_id,
                    account_uuid,
                }))
            }
            _ => Err("device_id and account_uuid must be configured together".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Billing header fingerprint
// ---------------------------------------------------------------------------

/// First user message text: string content verbatim, or the concatenation of
/// its text blocks. Mirrors pi-black's firstUserPrompt.
fn first_user_prompt(messages: Option<&Value>) -> String {
    let Some(list) = messages.and_then(Value::as_array) else {
        return String::new();
    };
    for message in list {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        return match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect(),
            _ => String::new(),
        };
    }
    String::new()
}

/// Appends the UTF-8 encoding of one selected UTF-16 code unit. JavaScript
/// string indexing yields a lone surrogate for split astral characters and
/// TextEncoder re-encodes that as U+FFFD; reproduce both behaviors exactly.
fn push_utf16_unit(input: &mut Vec<u8>, unit: u16) {
    if (0xD800..0xE000).contains(&unit) {
        input.extend_from_slice("\u{FFFD}".as_bytes());
    } else {
        let scalar = char::from_u32(u32::from(unit)).expect("non-surrogate BMP scalar");
        let mut buffer = [0u8; 4];
        input.extend_from_slice(scalar.encode_utf8(&mut buffer).as_bytes());
    }
}

/// cc_version fingerprint: first 3 hex chars of
/// SHA256("59cf53e54c78" + selected + version), where selected is the first
/// user prompt's UTF-16 code units at indices 4, 7 and 20 ('0' when short).
fn version_fingerprint(prompt: &str, version: &str) -> String {
    let units: Vec<u16> = prompt.encode_utf16().collect();
    let mut input = Vec::with_capacity(FINGERPRINT_SALT.len() + 9 + version.len());
    input.extend_from_slice(FINGERPRINT_SALT.as_bytes());
    for index in [4usize, 7, 20] {
        // '0' (0x30) fills missing indices, like pi-black's prompt[index] || "0".
        push_utf16_unit(&mut input, units.get(index).copied().unwrap_or(0x30));
    }
    input.extend_from_slice(version.as_bytes());
    let digest = Sha256::digest(&input);
    format!("{:02x}{:01x}", digest[0], digest[1] >> 4)
}

fn billing_header(prompt: &str, configuration: &Configuration) -> String {
    format!(
        "{BILLING_PREFIX}cc_version={}.{}; cc_entrypoint={}; {CCH_PLACEHOLDER};",
        configuration.claude_code_version,
        version_fingerprint(prompt, &configuration.claude_code_version),
        configuration.entrypoint
    )
}

fn text_block(text: String) -> Value {
    // Key order (type, text) matches the pi-black object literals.
    json!({ "type": "text", "text": text })
}

// ---------------------------------------------------------------------------
// Session / identity
// ---------------------------------------------------------------------------

fn format_uuid_v4(bytes: &mut [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Stable per-key session id: first 16 bytes of SHA256(tenant_id ++ key_id)
/// shaped as a UUIDv4. Same key -> same session, different keys -> different
/// sessions; no KV dependency.
fn derive_session_id(tenant_id: &str, key_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tenant_id.as_bytes());
    hasher.update(key_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    format_uuid_v4(&mut bytes)
}

// ---------------------------------------------------------------------------
// system surgery + metadata (transformClaudeCodePayload)
// ---------------------------------------------------------------------------

/// Replaces the system array with [billing block, Agent SDK block, ...rest],
/// stripping a stale billing + Agent SDK pair or the single legacy pi OAuth
/// block first. Re-inserting an existing "system" key keeps its original
/// position, matching JavaScript's {...payload, system} key order.
fn transform_system(
    obj: &mut Map<String, Value>,
    configuration: &Configuration,
) -> Result<(), String> {
    let existing: Vec<Value> = match obj.get("system") {
        None => Vec::new(),
        Some(Value::Array(blocks)) => blocks.clone(),
        // A string system prompt would be silently dropped by the reference
        // client transform; at the gateway that is a semantic change we must
        // not make, so fail closed instead.
        Some(_) => return Err("system field is not an array of content blocks".into()),
    };
    let text_at = |index: usize| {
        existing
            .get(index)
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
    };
    let remaining: &[Value] = if text_at(0).is_some_and(|text| text.starts_with(BILLING_PREFIX))
        && text_at(1) == Some(AGENT_SDK_SYSTEM_PROMPT)
    {
        &existing[2..]
    } else if text_at(0) == Some(LEGACY_PI_OAUTH_SYSTEM_PROMPT) {
        &existing[1..]
    } else {
        &existing[..]
    };
    let prompt = first_user_prompt(obj.get("messages"));
    let mut system = Vec::with_capacity(remaining.len() + 2);
    system.push(text_block(billing_header(&prompt, configuration)));
    system.push(text_block(AGENT_SDK_SYSTEM_PROMPT.to_owned()));
    system.extend_from_slice(remaining);
    obj.insert("system".to_owned(), Value::Array(system));
    Ok(())
}

/// metadata.user_id = JSON string {"device_id":...,"account_uuid":...,
/// "session_id":...}, written only when an identity is configured. The whole
/// metadata value is replaced, like pi-black's transformed.metadata = {...}.
fn write_metadata(
    obj: &mut Map<String, Value>,
    identity: &Identity,
    session_id: &str,
) -> Result<(), String> {
    let user_id = serde_json::to_string(&json!({
        "device_id": identity.device_id,
        "account_uuid": identity.account_uuid,
        "session_id": session_id,
    }))
    .map_err(|_| "failed to serialize metadata user_id".to_owned())?;
    obj.insert("metadata".to_owned(), json!({ "user_id": user_id }));
    Ok(())
}

// ---------------------------------------------------------------------------
// cch patch (patchClaudeCodeCch)
// ---------------------------------------------------------------------------

/// Matches pi-black's /; cch=[0-9a-f]{5};$/u suffix test.
fn has_patched_cch_suffix(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(tail) = bytes.len().checked_sub(12).map(|start| &bytes[start..]) else {
        return false;
    };
    tail.starts_with(b"; cch=")
        && tail[11] == b';'
        && tail[6..11]
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Port of pi-black's patchClaudeCodeCch: validates the billing block,
/// replaces the cch=00000 placeholder with the seeded XXH64 checksum of the
/// normalized body (model blanked, max_tokens removed), and returns the
/// final serialized text.
fn patch_cch(serialized: &str) -> Result<String, String> {
    let mut body: Value = serde_json::from_str(serialized)
        .map_err(|_| "expected the serialized request to be a JSON object".to_owned())?;
    let obj = body
        .as_object_mut()
        .ok_or_else(|| "expected the serialized request to be a JSON object".to_owned())?;
    let billing_text = obj
        .get("system")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| "request is missing the billing system block".to_owned())?;
    if !billing_text.starts_with(BILLING_PREFIX) {
        return Err("request has an invalid billing block".into());
    }
    if !billing_text.contains(CCH_PLACEHOLDER) {
        if has_patched_cch_suffix(billing_text) {
            return Ok(serialized.to_owned());
        }
        return Err("request has an invalid cch billing value".into());
    }
    if obj.get("model").and_then(Value::as_str).is_none() || !obj.contains_key("max_tokens") {
        return Err("request is missing model or max_tokens".into());
    }

    let mut normalized = obj.clone();
    normalized.insert("model".to_owned(), Value::String(String::new()));
    normalized.shift_remove("max_tokens");
    let hash_input = serde_json::to_string(&Value::Object(normalized))
        .map_err(|_| "failed to serialize the normalized request".to_owned())?;
    let hash = xxhash64(hash_input.as_bytes(), CCH_SEED);
    let cch = format!("{:05x}", hash & 0x000f_ffff);

    let text_value = obj
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.first_mut())
        .and_then(|block| block.get_mut("text"))
        .ok_or_else(|| "request is missing the billing system block".to_owned())?;
    let Some(billing) = text_value.as_str() else {
        return Err("request has an invalid billing block".into());
    };
    *text_value = Value::String(billing.replacen(CCH_PLACEHOLDER, &format!("cch={cch}"), 1));
    serde_json::to_string(obj).map_err(|_| "failed to serialize the final request".to_owned())
}

/// Full body rewrite: system surgery + optional metadata, then the cch patch
/// on the serialized text (mirroring pi-black's payload hook followed by the
/// fetch wrapper).
fn finalize_body(
    request_json: &str,
    configuration: &Configuration,
    session_id: &str,
) -> Result<String, String> {
    let mut body: Value = serde_json::from_str(request_json)
        .map_err(|_| "request body is not a JSON object".to_owned())?;
    {
        let obj = body
            .as_object_mut()
            .ok_or_else(|| "request body is not a JSON object".to_owned())?;
        transform_system(obj, configuration)?;
        if let Some(identity) = configuration.identity()? {
            write_metadata(obj, &identity, session_id)?;
        }
    }
    let serialized = serde_json::to_string(&body)
        .map_err(|_| "failed to serialize the rewritten request".to_owned())?;
    patch_cch(&serialized)
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Canonical Claude Code fingerprint headers. Empty x-stainless values are
/// omitted; x-stainless-retry-count starts at 0 and the host rewrites it to
/// the zero-based attempt index on retries.
fn build_headers(
    configuration: &Configuration,
    session_id: &str,
    request_id_bytes: [u8; 16],
) -> Vec<(String, String)> {
    let mut request_id = request_id_bytes;
    let mut headers = vec![
        (
            "user-agent".to_owned(),
            format!(
                "claude-cli/{} (external, {})",
                configuration.claude_code_version, configuration.entrypoint
            ),
        ),
        ("x-app".to_owned(), "cli".to_owned()),
        (
            "x-claude-code-session-id".to_owned(),
            session_id.to_owned(),
        ),
        (
            "x-client-request-id".to_owned(),
            format_uuid_v4(&mut request_id),
        ),
    ];
    let stainless = [
        ("x-stainless-lang", &configuration.stainless_lang),
        (
            "x-stainless-package-version",
            &configuration.stainless_package_version,
        ),
        ("x-stainless-os", &configuration.stainless_os),
        ("x-stainless-arch", &configuration.stainless_arch),
        ("x-stainless-runtime", &configuration.stainless_runtime),
        (
            "x-stainless-runtime-version",
            &configuration.stainless_runtime_version,
        ),
        ("x-stainless-timeout", &configuration.stainless_timeout),
    ];
    for (name, value) in stainless {
        if !value.is_empty() {
            headers.push((name.to_owned(), value.clone()));
        }
    }
    headers.push(("x-stainless-retry-count".to_owned(), "0".to_owned()));
    headers
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn finalize_with(
    configuration: &Configuration,
    context: &RequestContext,
    request_json: &str,
    random16: [u8; 16],
) -> Result<FinalizeResult, String> {
    let session_id = derive_session_id(&context.tenant_id, &context.key_id);
    let body = finalize_body(request_json, configuration, &session_id)?;
    Ok(FinalizeResult {
        request_json: body,
        set_headers: build_headers(configuration, &session_id, random16),
    })
}

fn prepare(config_json: &str) -> Result<Configuration, String> {
    let configuration = Configuration::parse(config_json)?;
    configuration.validate()?;
    if !configuration.enabled {
        // Fail closed: an unrewritten request must never reach an
        // anthropic-claude OAuth upstream.
        return Err("claude-code-wire is disabled by configuration".into());
    }
    Ok(configuration)
}

struct ClaudeCodeWire;

#[cfg(target_arch = "wasm32")]
impl Guest for ClaudeCodeWire {
    fn finalize(
        context: RequestContext,
        request_json: String,
        _headers_json: String,
    ) -> Result<FinalizeResult, String> {
        // The headers snapshot is informational; the shim never echoes
        // inbound fingerprint headers, so there is nothing to consume.
        let configuration = prepare(&context.config_json)?;
        let random = host::random_bytes(16)
            .map_err(|error| format!("host random-bytes failed: {error}"))?;
        let random16: [u8; 16] = random
            .try_into()
            .map_err(|_| "host random-bytes returned a short buffer".to_owned())?;
        finalize_with(&configuration, &context, &request_json, random16)
    }
}

// Component ABI symbols are valid Wasm exports, not native ELF symbol names.
// Keep native algorithm tests linkable while exporting the actual component.
#[cfg(target_arch = "wasm32")]
export!(ClaudeCodeWire);

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> RequestContext {
        RequestContext {
            tenant_id: "tenant-1".to_owned(),
            principal_id: "principal-1".to_owned(),
            key_id: "key-1".to_owned(),
            protocol: "anthropic".to_owned(),
            model: "claude-sonnet-4-5".to_owned(),
            config_json: "{}".to_owned(),
        }
    }

    fn sample_request() -> &'static str {
        r#"{"model":"claude-sonnet-4-5","max_tokens":1024,"stream":true,"system":[{"type":"text","text":"You are a helpful assistant."}],"messages":[{"role":"user","content":[{"type":"text","text":"hello world, this is a prompt"}]}]}"#
    }

    // ------------------------------------------------------------------
    // XXH64
    // ------------------------------------------------------------------

    #[test]
    fn xxh64_known_vectors() {
        // Official XXH64 test vector: empty input, seed 0.
        assert_eq!(xxhash64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxhash64(b"a", 0), 0xd24e_c4f1_a98c_6e5b);
    }

    #[test]
    fn xxh64_matches_reference_crate() {
        let mut bytes = Vec::new();
        for len in [0usize, 1, 3, 4, 5, 8, 9, 16, 31, 32, 33, 64, 100, 1_000, 70_000] {
            bytes.clear();
            let mut state = len as u64;
            for _ in 0..len {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                bytes.push((state >> 33) as u8);
            }
            for seed in [0u64, 42, CCH_SEED, u64::MAX] {
                assert_eq!(
                    xxhash64(&bytes, seed),
                    xxhash_rust::xxh64::xxh64(&bytes, seed),
                    "len={len} seed={seed:#x}"
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // cc_version fingerprint
    // ------------------------------------------------------------------

    #[test]
    fn fingerprint_matches_reference_vectors() {
        // Generated independently with python hashlib, replicating
        // pi-black's claudeCodeVersionFingerprint.
        assert_eq!(
            version_fingerprint("hello world, this is a prompt", "2.1.258"),
            "2d2"
        );
        assert_eq!(version_fingerprint("short", "2.1.258"), "587");
        assert_eq!(version_fingerprint("", "2.1.258"), "1e2");
        assert_eq!(
            version_fingerprint("你好世界这是一条测试消息啊", "2.1.258"),
            "f0d"
        );
    }

    #[test]
    fn fingerprint_uses_utf16_code_units() {
        // An astral character is two UTF-16 code units in JS; index 4/7/20
        // land inside ASCII here so only the length shift matters, but the
        // result must differ from a naive char-index implementation.
        let emoji_prompt = "\u{1F600}abc defghijklmnopqrstuvwxyz";
        let fp = version_fingerprint(emoji_prompt, "2.1.258");
        assert_eq!(fp.len(), 3);
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    // ------------------------------------------------------------------
    // first_user_prompt
    // ------------------------------------------------------------------

    #[test]
    fn first_user_prompt_picks_first_user_text() {
        let messages = json!([
            {"role": "assistant", "content": "nope"},
            {"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "ab"}, {"type": "text", "text": "cd"}]},
            {"role": "user", "content": "later"}
        ]);
        assert_eq!(first_user_prompt(Some(&messages)), "abcd");
        let plain = json!([{"role": "user", "content": "direct"}]);
        assert_eq!(first_user_prompt(Some(&plain)), "direct");
        assert_eq!(first_user_prompt(None), "");
    }

    // ------------------------------------------------------------------
    // system surgery
    // ------------------------------------------------------------------

    fn system_texts(value: &Value) -> Vec<String> {
        value
            .get("system")
            .and_then(Value::as_array)
            .expect("system array")
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .expect("text")
                    .to_owned()
            })
            .collect()
    }

    fn run_body(request: &str, config: &Configuration) -> Result<Value, String> {
        let out = finalize_body(request, config, "session-x")?;
        serde_json::from_str(&out).map_err(|_| "output is not valid JSON".to_owned())
    }

    #[test]
    fn system_blocks_are_prepended_in_order() {
        let out = run_body(sample_request(), &Configuration::default()).expect("rewrite");
        let texts = system_texts(&out);
        assert_eq!(texts.len(), 3);
        assert!(texts[0].starts_with(BILLING_PREFIX));
        assert!(texts[0].contains("cc_version=2.1.258.2d2"));
        assert!(texts[0].contains("cc_entrypoint=sdk-cli"));
        assert!(!texts[0].contains(CCH_PLACEHOLDER), "cch must be patched");
        assert!(texts[0].contains("; cch="));
        assert_eq!(texts[1], AGENT_SDK_SYSTEM_PROMPT);
        assert_eq!(texts[2], "You are a helpful assistant.");
    }

    #[test]
    fn stale_billing_and_sdk_blocks_are_replaced() {
        let request = r#"{"model":"m","max_tokens":1,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.200.aaa; cc_entrypoint=sdk-cli; cch=12345;"},{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK."},{"type":"text","text":"keep me"}],"messages":[{"role":"user","content":"hello world, this is a prompt"}]}"#;
        let out = run_body(request, &Configuration::default()).expect("rewrite");
        let texts = system_texts(&out);
        assert_eq!(texts.len(), 3);
        assert!(texts[0].contains("cc_version=2.1.258."));
        assert_eq!(texts[1], AGENT_SDK_SYSTEM_PROMPT);
        assert_eq!(texts[2], "keep me");
    }

    #[test]
    fn legacy_pi_oauth_prompt_is_replaced() {
        let request = r#"{"model":"m","max_tokens":1,"system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},{"type":"text","text":"keep me too"}],"messages":[{"role":"user","content":"hello world, this is a prompt"}]}"#;
        let out = run_body(request, &Configuration::default()).expect("rewrite");
        let texts = system_texts(&out);
        assert_eq!(texts.len(), 3);
        assert_eq!(texts[2], "keep me too");
    }

    #[test]
    fn string_system_prompt_fails_closed() {
        let request = r#"{"model":"m","max_tokens":1,"system":"plain string","messages":[]}"#;
        assert!(run_body(request, &Configuration::default()).is_err());
    }

    // ------------------------------------------------------------------
    // cch self-consistency
    // ------------------------------------------------------------------

    /// Recompute the checksum exactly the way a validator would: parse the
    /// wire bytes, blank model, drop max_tokens, seeded XXH64, low 20 bits.
    fn recomputed_cch(wire: &str) -> String {
        let mut body: Value = serde_json::from_str(wire).expect("wire json");
        let obj = body.as_object_mut().expect("object");
        obj.insert("model".to_owned(), Value::String(String::new()));
        obj.shift_remove("max_tokens");
        // The hash input is the body with the cch placeholder still in
        // place (pi-black hashes before substituting the value), so restore
        // the placeholder before serializing.
        let billing = obj["system"][0]["text"].as_str().expect("billing");
        let restored = {
            let bytes = billing.as_bytes();
            let tail = &bytes[bytes.len() - 12..];
            assert!(tail.starts_with(b"; cch=") && tail[11] == b';');
            // Drop the 5 hex digits and trailing semicolon (6 chars), then
            // restore the placeholder digits.
            format!("{}00000;", &billing[..billing.len() - 6])
        };
        obj["system"][0]["text"] = Value::String(restored);
        let input = serde_json::to_string(&obj).expect("serialize");
        format!("{:05x}", xxhash64(input.as_bytes(), CCH_SEED) & 0x000f_ffff)
    }

    #[test]
    fn cch_is_self_consistent_with_wire_bytes() {
        for request in [
            sample_request(),
            r#"{"model":"m","max_tokens":4096,"messages":[{"role":"user","content":"short"}],"temperature":0.3,"tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]}"#,
            r#"{"max_tokens":1,"model":"m","metadata":{"user_id":"original"},"messages":[],"system":[]}"#,
        ] {
            let wire = finalize_body(request, &Configuration::default(), "s").expect("rewrite");
            let body: Value = serde_json::from_str(&wire).expect("json");
            let billing = body["system"][0]["text"].as_str().expect("billing");
            let expected = recomputed_cch(&wire);
            assert!(
                billing.contains(&format!("; cch={expected};")),
                "billing {billing} should carry cch {expected}"
            );
            assert!(!wire.contains(CCH_PLACEHOLDER));
        }
    }

    #[test]
    fn already_patched_cch_is_returned_unchanged() {
        let wire = finalize_body(sample_request(), &Configuration::default(), "s").expect("ok");
        assert_eq!(patch_cch(&wire).expect("repatch"), wire);
    }

    #[test]
    fn missing_model_or_max_tokens_fails_closed() {
        let config = Configuration::default();
        // model without max_tokens
        assert!(finalize_body(r#"{"model":"m","system":[],"messages":[]}"#, &config, "s").is_err());
        // max_tokens without model
        assert!(finalize_body(r#"{"max_tokens":1,"system":[],"messages":[]}"#, &config, "s").is_err());
        // non-object body
        assert!(finalize_body("[1,2]", &config, "s").is_err());
    }

    // ------------------------------------------------------------------
    // metadata / session / headers
    // ------------------------------------------------------------------

    #[test]
    fn metadata_written_only_with_full_identity() {
        let mut config = Configuration::default();
        config.device_id = Some("a".repeat(64));
        config.account_uuid = Some("123e4567-e89b-42d3-a456-426614174000".to_owned());
        let out = run_body(sample_request(), &config).expect("rewrite");
        let user_id = out["metadata"]["user_id"].as_str().expect("user_id");
        let parsed: Value = serde_json::from_str(user_id).expect("user_id json");
        assert_eq!(parsed["device_id"], json!("a".repeat(64)));
        assert_eq!(parsed["account_uuid"], json!("123e4567-e89b-42d3-a456-426614174000"));
        assert_eq!(parsed["session_id"], json!("session-x"));

        // No identity configured -> metadata untouched/absent.
        let out = run_body(sample_request(), &Configuration::default()).expect("rewrite");
        assert!(out.get("metadata").is_none());
    }

    #[test]
    fn session_id_is_stable_uuid_shaped() {
        let one = derive_session_id("t", "k");
        assert_eq!(one, derive_session_id("t", "k"));
        assert_ne!(one, derive_session_id("t", "k2"));
        assert_ne!(one, derive_session_id("t2", "k"));
        assert_eq!(one.len(), 36);
        assert_eq!(one.as_bytes()[14], b'4');
        assert!(matches!(one.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn headers_match_claude_code_wire() {
        let headers = build_headers(&Configuration::default(), "session-y", [7u8; 16]);
        let get = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            get("user-agent").as_deref(),
            Some("claude-cli/2.1.258 (external, sdk-cli)")
        );
        assert_eq!(get("x-app").as_deref(), Some("cli"));
        assert_eq!(get("x-claude-code-session-id").as_deref(), Some("session-y"));
        assert_eq!(get("x-stainless-lang").as_deref(), Some("js"));
        assert_eq!(get("x-stainless-runtime").as_deref(), Some("node"));
        assert_eq!(get("x-stainless-retry-count").as_deref(), Some("0"));
        assert!(get("x-client-request-id").is_some());
        // Empty stainless values are omitted rather than sent blank.
        let mut config = Configuration::default();
        config.stainless_arch = String::new();
        let headers = build_headers(&config, "session-y", [7u8; 16]);
        assert!(headers.iter().all(|(key, _)| key != "x-stainless-arch"));
    }

    // ------------------------------------------------------------------
    // configuration / fail-closed entry
    // ------------------------------------------------------------------

    #[test]
    fn configuration_validation() {
        // Half-configured identity is an error.
        assert!(Configuration {
            device_id: Some("a".repeat(64)),
            ..Configuration::default()
        }
        .validate()
        .is_err());
        // Malformed device id.
        assert!(Configuration {
            device_id: Some("zz".repeat(32)),
            account_uuid: Some("123e4567-e89b-42d3-a456-426614174000".to_owned()),
            ..Configuration::default()
        }
        .validate()
        .is_err());
        // Unknown fields rejected.
        assert!(Configuration::parse("{\"nope\": 1}").is_err());
        // Defaults are valid.
        Configuration::parse("{}").unwrap().validate().unwrap();
    }

    #[test]
    fn disabled_plugin_fails_closed() {
        assert!(prepare("{\"enabled\": false}").is_err());
    }

    #[test]
    fn finalize_with_produces_body_and_headers() {
        let result = finalize_with(
            &Configuration::default(),
            &context(),
            sample_request(),
            [9u8; 16],
        )
        .expect("finalize");
        assert!(result.request_json.contains("cch="));
        assert!(result
            .set_headers
            .iter()
            .any(|(name, _)| name == "user-agent"));
        // The session id in headers matches the derived per-key session.
        let session = derive_session_id("tenant-1", "key-1");
        assert!(result
            .set_headers
            .contains(&("x-claude-code-session-id".to_owned(), session)));
    }

}
