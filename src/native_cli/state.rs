//! Encrypted carrier for an independently captured official CLI account home.
//! This is not a tar extractor, filesystem importer or activation API.
//! Only the reviewed capture/provisioning worker may construct this carrier.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::AppError,
    provider::{open_private_json, seal_private_json},
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

fn aad(binding: &StateBinding) -> Result<Vec<u8>, AppError> {
    if binding.tenant_external_id.is_empty()
        || binding.tenant_external_id.len() > 200
        || binding.tenant_external_id.trim() != binding.tenant_external_id
        || binding.tenant_external_id.chars().any(char::is_control)
        || binding.account_id.is_nil()
    {
        return Err(invalid());
    }
    let mut aad = b"memeloop-token-center/native-cli-state/v1\0".to_vec();
    aad.extend(serde_json::to_vec(binding).map_err(|_| invalid())?);
    Ok(aad)
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
    binding: &StateBinding,
    key_material: &[u8],
) -> Result<String, AppError> {
    validate(state)?;
    seal_private_json(state, key_material, &aad(binding)?)
}

pub fn open(
    envelope: &str,
    expected_binding: &StateBinding,
    key_material: &[u8],
) -> Result<AccountState, AppError> {
    if envelope.len() > MAX_ENVELOPE {
        return Err(invalid());
    }
    let state = open_private_json(envelope, key_material, &aad(expected_binding)?)
        .map_err(|_| invalid())?;
    validate(&state)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> StateBinding {
        StateBinding {
            tenant_external_id: "tenant-a".into(),
            account_id: Uuid::new_v4(),
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
    fn encrypted_state_is_bound_to_account_tenant_generation_and_provider() {
        let binding = binding();
        let envelope = seal(&fixture(), &binding, b"synthetic-key").unwrap();
        assert!(!envelope.contains("synthetic-secret"));
        assert_eq!(
            open(&envelope, &binding, b"synthetic-key").unwrap().files[0].content,
            b"synthetic-secret"
        );
        let mut wrong = binding.clone();
        wrong.tenant_external_id = "tenant-b".into();
        assert!(open(&envelope, &wrong, b"synthetic-key").is_err());
        wrong = binding.clone();
        wrong.account_id = Uuid::new_v4();
        assert!(open(&envelope, &wrong, b"synthetic-key").is_err());
        wrong = binding.clone();
        wrong.generation += 1;
        assert!(open(&envelope, &wrong, b"synthetic-key").is_err());
        wrong = binding;
        wrong.provider = CliProvider::Cursor;
        assert!(open(&envelope, &wrong, b"synthetic-key").is_err());
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
