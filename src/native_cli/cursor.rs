use std::{collections::BTreeSet, ffi::OsString, path::PathBuf, time::Duration};

use super::process::{RuntimeCommand, RuntimeError, run};

pub const VERSION: &str = "2026.07.23-e383d2b";
pub const ARCHIVE_SHA256: &str = "702ad595213bee5df0268be9f80a19f29fcceaa2a42fc55e39f2b5199051f0c4";
const NODE: &str = "/opt/cursor-agent/node";
const SCRIPT: &str = "/opt/cursor-agent/index.js";

/// Construct only inside the isolated official-runtime worker. Caller must
/// hold the per-account state lease throughout a call, including refresh.
pub struct CursorRuntime {
    home: PathBuf,
    workspace: PathBuf,
}

impl CursorRuntime {
    pub fn new(home: PathBuf, workspace: PathBuf) -> Result<Self, RuntimeError> {
        if !home.is_absolute() || !workspace.is_absolute() || home == workspace {
            return Err(RuntimeError::Configuration);
        }
        Ok(Self { home, workspace })
    }

    fn command(&self, args: &[&str]) -> RuntimeCommand {
        let mut arguments: Vec<OsString> = vec!["--use-system-ca".into(), SCRIPT.into()];
        arguments.extend(args.iter().map(OsString::from));
        RuntimeCommand {
            executable: NODE.into(),
            arguments,
            home: self.home.clone(),
            workspace: self.workspace.clone(),
            input: vec![],
            timeout: Duration::from_secs(30),
        }
    }

    pub async fn models(&self) -> Result<Vec<CursorModel>, RuntimeError> {
        let output = run(self.command(&["--list-models"])).await?;
        parse_models(&output.stdout)
    }

    /// Status output is intentionally not exposed: CLI text may include login,
    /// account identity or authentication URLs. Normalized quota parsing must
    /// be based on an audited fixture before displaying any of it.
    pub async fn status_probe(&self) -> Result<CursorStatus, RuntimeError> {
        let _ = run(self.command(&["status"])).await?;
        Ok(CursorStatus {
            command_succeeded: true,
            numeric_quota_available: false,
            reset_supported: false,
        })
    }

    /// Source text mode and documented result JSON expose no authoritative
    /// token accounting. Fail BEFORE spawning instead of consuming quota then
    /// inventing zero usage. Production activation requires an accounting
    /// contract and OS isolation, not merely a parser for final text.
    pub async fn generate(&self, _prompt: &str, _model: &str) -> Result<String, RuntimeError> {
        Err(RuntimeError::UsageUnavailable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorModel {
    pub id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorStatus {
    pub command_succeeded: bool,
    pub numeric_quota_available: bool,
    pub reset_supported: bool,
}

/// Deliberately stricter than the old plugin: no invented "auto" fallback,
/// no display text that could echo a credential, no unbounded model list.
pub fn parse_models(bytes: &[u8]) -> Result<Vec<CursorModel>, RuntimeError> {
    if bytes.len() > 256 * 1024 {
        return Err(RuntimeError::OutputLimit);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| RuntimeError::Protocol)?;
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
    {
        return Err(RuntimeError::Protocol);
    }
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        let value = line.trim().trim_start_matches(['-', '*', ' ']);
        if value.is_empty() || value.to_ascii_lowercase().starts_with("tip:") {
            continue;
        }
        let Some(id) = value.split_whitespace().next() else {
            continue;
        };
        // Only documented listing rows, not arbitrary diagnostic prose.
        if value != id && !value[id.len()..].trim_start().starts_with('-') {
            continue;
        }
        if !valid_model_id(id) {
            continue;
        }
        ids.insert(id.to_owned());
        if ids.len() > 512 {
            return Err(RuntimeError::OutputLimit);
        }
    }
    if ids.is_empty() {
        return Err(RuntimeError::Protocol);
    }
    Ok(ids.into_iter().map(|id| CursorModel { id }).collect())
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
        && !id.contains("://")
        && !matches!(
            id.to_ascii_lowercase().as_str(),
            "models" | "available" | "error"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_listing_deduplicates_without_fabricating_auto() {
        let models =
            parse_models(b"Available models:\n- sonnet-4 - Sonnet 4\nsonnet-4\nTip: help\n")
                .unwrap();
        assert_eq!(
            models,
            vec![CursorModel {
                id: "sonnet-4".into()
            }]
        );
        assert_eq!(
            parse_models(b"Available models:\n").unwrap_err(),
            RuntimeError::Protocol
        );
    }

    #[test]
    fn rejects_control_output_urls_and_option_injection() {
        assert!(parse_models(b"\x1b[31msecret").is_err());
        assert!(!valid_model_id("--api-key"));
        assert!(!valid_model_id("https://secret.example"));
        assert!(!valid_model_id("model\n--force"));
    }

    #[tokio::test]
    async fn generation_fails_before_runtime_spawn_without_usage_contract() {
        let runtime = CursorRuntime::new("/state/example".into(), "/work/example".into()).unwrap();
        assert_eq!(
            runtime.generate("never sent", "auto").await.unwrap_err(),
            RuntimeError::UsageUnavailable
        );
    }
}
