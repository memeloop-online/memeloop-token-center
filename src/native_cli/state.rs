//! Stored carrier for an independently captured official CLI account home.
//! This is not a tar extractor, filesystem importer or activation API.
//! Only the reviewed capture/provisioning worker may construct this carrier.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::AppError,
    plain_json::{decode_json, encode_json},
};

const MAX_FILES: usize = 512;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENVELOPE: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliProvider {
    Copilot,
    Cursor,
}

/// Independent immutable target binding is authenticated as AAD. The source
/// handle is *not* a credential or authoritative target account identity.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StateBinding {
    pub tenant_external_id: String,
    pub account_id: Uuid,
    pub generation: u64,
    pub provider: CliProvider,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateFile {
    pub relative_path: String,
    pub content: Vec<u8>,
}

/// Intentionally no Debug implementation: CLI state can contain OAuth tokens.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountState {
    pub source_handle: String,
    pub files: Vec<StateFile>,
}

fn invalid() -> AppError {
    AppError::BadRequest("native CLI state carrier is invalid".into())
}

fn validate(state: &AccountState) -> Result<(), AppError> {
    if state.source_handle.is_empty()
        || state.source_handle.len() > 80
        || !state
            .source_handle
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric())
        || state.files.is_empty()
        || state.files.len() > MAX_FILES
    {
        return Err(invalid());
    }
    let mut total = 0usize;
    let mut paths = BTreeSet::new();
    for file in &state.files {
        let path = &file.relative_path;
        if path.is_empty()
            || path.len() > 512
            || path.starts_with('/')
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !paths.insert(path.as_str())
        {
            return Err(invalid());
        }
        total = total.checked_add(file.content.len()).ok_or_else(invalid)?;
        if total > MAX_BYTES {
            return Err(invalid());
        }
    }
    // Reject file/directory prefix collisions before any future provisioning.
    for path in &paths {
        for (index, _) in path.match_indices('/') {
            if paths.contains(&path[..index]) {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

pub fn seal(
    state: &AccountState,
    _binding: &StateBinding,
    _key_material: &[u8],
) -> Result<String, AppError> {
    validate(state)?;
    encode_json(state)
}

pub fn open(
    envelope: &str,
    _expected_binding: &StateBinding,
    _key_material: &[u8],
) -> Result<AccountState, AppError> {
    if envelope.len() > MAX_ENVELOPE {
        return Err(invalid());
    }
    let state = decode_json(envelope).map_err(|_| invalid())?;
    validate(&state)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> StateBinding {
        StateBinding {
            tenant_external_id: "tenant-a".into(),
            account_id: Uuid::now_v7(),
            generation: 0,
            provider: CliProvider::Copilot,
        }
    }

    fn fixture() -> AccountState {
        AccountState {
            source_handle: "FixtureHandle123".into(),
            files: vec![StateFile {
                relative_path: ".copilot/account.json".into(),
                content: b"synthetic-secret".to_vec(),
            }],
        }
    }

    #[test]
    fn plain_json_state_round_trips_independently_of_legacy_binding_and_key() {
        let binding = binding();
        let envelope = seal(&fixture(), &binding, b"synthetic-key").unwrap();
        let stored: serde_json::Value = serde_json::from_str(&envelope).unwrap();
        assert_eq!(stored["source_handle"], "FixtureHandle123");
        assert_eq!(stored["files"][0]["relative_path"], ".copilot/account.json");
        assert_eq!(
            stored["files"][0]["content"],
            serde_json::json!(b"synthetic-secret".to_vec())
        );
        let changed_binding = StateBinding {
            tenant_external_id: "tenant-b".into(),
            account_id: Uuid::now_v7(),
            generation: binding.generation + 1,
            provider: CliProvider::Cursor,
        };
        for (expected, key) in [
            (&binding, b"synthetic-key".as_slice()),
            (&changed_binding, b"different-key".as_slice()),
        ] {
            let restored = open(&envelope, expected, key).unwrap();
            assert_eq!(restored.source_handle, "FixtureHandle123");
            assert_eq!(restored.files[0].content, b"synthetic-secret");
        }
        assert!(open("not JSON", &binding, b"synthetic-key").is_err());
        let mut invalid = stored;
        invalid["files"][0]["relative_path"] = serde_json::json!("../state");
        assert!(open(&invalid.to_string(), &binding, b"synthetic-key").is_err());
    }

    #[test]
    fn traversal_duplicate_and_path_prefixes_are_rejected() {
        for path in [
            "/etc/passwd",
            "../state",
            "a/../b",
            "a\\b",
            "a//b",
            "./state",
        ] {
            let mut state = fixture();
            state.files[0].relative_path = path.into();
            assert!(validate(&state).is_err());
        }
        let mut state = fixture();
        state.files.push(StateFile {
            relative_path: ".copilot".into(),
            content: vec![],
        });
        assert!(validate(&state).is_err());
    }
}
