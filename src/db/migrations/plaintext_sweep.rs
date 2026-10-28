//! One-time conversion of the sealed state that earlier builds persisted.
//!
//! Every value handled here used to be stored as a ChaCha20-Poly1305 envelope
//! keyed by `MTC_KEY_PEPPER`: OAuth login sessions, idempotent rotation
//! responses, client credential recovery copies, installer network selections
//! and proxy group members. The runtime now stores plain JSON, so a row that
//! still holds an envelope has to be rewritten exactly once — otherwise its
//! owner silently loses the state.
//!
//! The sweep runs in the startup migration connection and records itself as
//! schema version 142, so it never executes twice. Upstream credentials are
//! deliberately untouched: `upstream_credentials.credential_ciphertext` keeps
//! its envelope.

use serde_json::Value;
use sqlx::Row;

use super::{Database, unix_millis};
use crate::plain_json::encode_handle;
use crate::provider::open_envelope;

/// Schema version recorded once the sweep completes. It is not part of the
/// ordered migration list because the conversion is Rust code, not SQL.
const SWEEP_VERSION: i64 = 142;
const SWEEP_NAME: &str = "plaintext state instead of sealed envelopes";

const MEMBERS_AAD: &[u8] = b"mtc/transport-proxy-members/v1";
const KEY_PROVISIONING_AAD: &[u8] = b"memeloop-token-center/key-provisioning-response/v1";
const ROTATION_AAD: &str = "memeloop-token-center/credential-rotation-response/v1";
const INSTALLER_NETWORK_AAD: &str = "mtc/plugin-installer-network/v1";

/// Login flows each sealed their session state under a flow-specific domain.
const FLOW_AADS: &[(&str, &[u8], &[u8])] = &[
    (
        "claude_manual_pkce",
        b"memeloop-token-center/claude-manual-state/v1",
        b"memeloop-token-center/claude-manual-ready/v1",
    ),
    (
        "generic_authorization_code",
        b"memeloop-token-center/authorization-code/state/v1",
        b"memeloop-token-center/authorization-code/ready/v1",
    ),
    (
        "kimi-oauth",
        b"mtc/kimi-device/state/v1",
        b"mtc/kimi-device/ready/v1",
    ),
    (
        "openai_codex_device",
        b"memeloop-token-center/openai-codex-device-state/v1",
        b"memeloop-token-center/openai-codex-device-ready/v1",
    ),
];

#[derive(Default)]
pub(super) struct SweepReport {
    pub(super) rewritten: usize,
    pub(super) dropped: usize,
    pub(super) failed: usize,
}

impl Database {
    /// Rewrites every sealed row that a running gateway can still read.
    ///
    /// Safe to call on every start: the version marker short-circuits it once
    /// the conversion has been recorded, and each statement only matches values
    /// that still start with an envelope version prefix.
    pub async fn convert_sealed_state(&self, pepper: &[u8]) -> Result<(), sqlx::Error> {
        let applied = sqlx::query("SELECT version FROM schema_migrations WHERE version = $1")
            .bind(SWEEP_VERSION)
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if applied {
            return Ok(());
        }
        let mut report = SweepReport::default();
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, crate::db::DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(734627102948312)")
                .execute(&mut *transaction)
                .await?;
        }
        report.rewritten += convert_members(&mut transaction, pepper, &mut report).await?;
        report.rewritten += convert_key_recovery(&mut transaction, pepper, &mut report).await?;
        report.rewritten += convert_rotation_replays(&mut transaction, pepper).await?;
        report.rewritten += convert_installer_network(&mut transaction, pepper).await?;
        convert_login_sessions(&mut transaction, pepper, &mut report).await?;
        sqlx::query(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(SWEEP_VERSION)
        .bind(SWEEP_NAME)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        tracing::info!(
            rewritten = report.rewritten,
            dropped = report.dropped,
            failed = report.failed,
            "converted sealed gateway state to plain storage"
        );
        Ok(())
    }
}

fn open(value: &str, pepper: &[u8], aad: &[u8]) -> Option<Value> {
    open_envelope::<Value>(value, pepper, aad).ok()
}

fn sealed_where(column: &str) -> String {
    format!("substr({column}, 1, 3) IN ('v1.', 'v2.')")
}

async fn convert_members(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    pepper: &[u8],
    report: &mut SweepReport,
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT id, members_ciphertext FROM transport_proxy_groups WHERE {}",
        sealed_where("members_ciphertext")
    )))
    .fetch_all(&mut **transaction)
    .await?;
    let mut converted = 0;
    for row in rows {
        let id: String = row.try_get("id")?;
        let stored: String = row.try_get("members_ciphertext")?;
        let Some(members) = open(&stored, pepper, MEMBERS_AAD) else {
            report.failed += 1;
            continue;
        };
        sqlx::query("UPDATE transport_proxy_groups SET members_ciphertext = $1 WHERE id = $2")
            .bind(serde_json::to_string(&members).unwrap_or_default())
            .bind(id)
            .execute(&mut **transaction)
            .await?;
        converted += 1;
    }
    Ok(converted)
}

async fn convert_key_recovery(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    pepper: &[u8],
    report: &mut SweepReport,
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT credential_id, ciphertext FROM key_credential_recovery_secrets WHERE {}",
        sealed_where("ciphertext")
    )))
    .fetch_all(&mut **transaction)
    .await?;
    let mut converted = 0;
    for row in rows {
        let credential_id: String = row.try_get("credential_id")?;
        let stored: String = row.try_get("ciphertext")?;
        let Some(issued) = open(&stored, pepper, KEY_PROVISIONING_AAD) else {
            report.failed += 1;
            continue;
        };
        sqlx::query(
            "UPDATE key_credential_recovery_secrets SET ciphertext = $1 WHERE credential_id = $2",
        )
        .bind(serde_json::to_string(&issued).unwrap_or_default())
        .bind(credential_id)
        .execute(&mut **transaction)
        .await?;
        converted += 1;
    }
    Ok(converted)
}

async fn convert_rotation_replays(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    pepper: &[u8],
) -> Result<usize, sqlx::Error> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT idempotency_key, resource_kind, resource_id, request_hash, response_ciphertext, expires_at FROM credential_rotation_replays WHERE {}",
        sealed_where("response_ciphertext")
    )))
    .fetch_all(&mut **transaction)
    .await?;
    let mut converted = 0;
    for row in rows {
        let key: String = row.try_get("idempotency_key")?;
        let stored: String = row.try_get("response_ciphertext")?;
        let aad = format!(
            "{ROTATION_AAD}\0{}\0{}\0{key}\0{}\0{}",
            row.try_get::<String, _>("resource_kind")?,
            row.try_get::<String, _>("resource_id")?,
            row.try_get::<String, _>("request_hash")?,
            row.try_get::<i64, _>("expires_at")?,
        );
        // A replay that cannot be opened is dead weight: the caller is told to
        // rotate again with a fresh Idempotency-Key, exactly as if it expired.
        let plaintext = open(&stored, pepper, aad.as_bytes())
            .map(|value| serde_json::to_string(&value).unwrap_or_default());
        let opened = plaintext.is_some();
        sqlx::query(
            "UPDATE credential_rotation_replays SET response_ciphertext = $1 WHERE idempotency_key = $2",
        )
        .bind(plaintext)
        .bind(key)
        .execute(&mut **transaction)
        .await?;
        if opened {
            converted += 1;
        }
    }
    Ok(converted)
}

async fn convert_installer_network(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    pepper: &[u8],
) -> Result<usize, sqlx::Error> {
    let mut converted = 0;
    let preferences = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT actor, preference_ciphertext FROM plugin_installer_network_preferences WHERE {}",
        sealed_where("preference_ciphertext")
    )))
    .fetch_all(&mut **transaction)
    .await?;
    for row in preferences {
        let actor: String = row.try_get("actor")?;
        let stored: String = row.try_get("preference_ciphertext")?;
        let aad = format!("{INSTALLER_NETWORK_AAD}/preference/{actor}/");
        let Some(preference) = open(&stored, pepper, aad.as_bytes()) else {
            continue;
        };
        sqlx::query(
            "UPDATE plugin_installer_network_preferences SET preference_ciphertext = $1 WHERE actor = $2",
        )
        .bind(serde_json::to_string(&preference).unwrap_or_default())
        .bind(actor)
        .execute(&mut **transaction)
        .await?;
        converted += 1;
    }
    let snapshots = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT operation_key, actor, snapshot_ciphertext FROM plugin_installer_network_snapshots WHERE {}",
        sealed_where("snapshot_ciphertext")
    )))
    .fetch_all(&mut **transaction)
    .await?;
    for row in snapshots {
        let operation_key: String = row.try_get("operation_key")?;
        let actor: String = row.try_get("actor")?;
        let stored: String = row.try_get("snapshot_ciphertext")?;
        let aad = format!("{INSTALLER_NETWORK_AAD}/snapshot/{actor}/{operation_key}");
        let Some(snapshot) = open(&stored, pepper, aad.as_bytes()) else {
            continue;
        };
        sqlx::query(
            "UPDATE plugin_installer_network_snapshots SET snapshot_ciphertext = $1 WHERE operation_key = $2 AND actor = $3",
        )
        .bind(serde_json::to_string(&snapshot).unwrap_or_default())
        .bind(operation_key)
        .bind(actor)
        .execute(&mut **transaction)
        .await?;
        converted += 1;
    }
    Ok(converted)
}

async fn convert_login_sessions(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    pepper: &[u8],
    report: &mut SweepReport,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, flow_kind, state_ciphertext, ready_ciphertext FROM oauth_login_sessions WHERE substr(state_ciphertext, 1, 3) IN ('v1.', 'v2.') OR substr(coalesce(ready_ciphertext, ''), 1, 3) IN ('v1.', 'v2.')",
    )
    .fetch_all(&mut **transaction)
    .await?;
    for row in rows {
        let id: String = row.try_get("id")?;
        let flow_kind: String = row.try_get("flow_kind")?;
        let state: String = row.try_get("state_ciphertext")?;
        let ready: Option<String> = row.try_get("ready_ciphertext")?;
        let state_plaintext = open_flow_state(&state, pepper, &flow_kind, false);
        let ready_plaintext = ready
            .as_deref()
            .and_then(|value| open_flow_state(value, pepper, &flow_kind, true));
        // A login the current build cannot resume is unusable: the operator
        // restarts the flow, which is cheaper than an unreadable session row.
        if state_plaintext.is_none() {
            sqlx::query("DELETE FROM oauth_login_sessions WHERE id = $1")
                .bind(id)
                .execute(&mut **transaction)
                .await?;
            report.dropped += 1;
            continue;
        }
        sqlx::query("UPDATE oauth_login_sessions SET state_ciphertext = $1, ready_ciphertext = $2 WHERE id = $3")
            .bind(state_plaintext)
            .bind(ready_plaintext)
            .bind(id)
            .execute(&mut **transaction)
            .await?;
        report.rewritten += 1;
    }
    Ok(())
}

fn open_flow_state(stored: &str, pepper: &[u8], flow_kind: &str, ready: bool) -> Option<String> {
    let mut candidates: Vec<&[u8]> = Vec::new();
    for (flow, state_aad, ready_aad) in FLOW_AADS {
        if *flow == flow_kind {
            candidates.push(if ready { ready_aad } else { state_aad });
        }
    }
    for (_, state_aad, ready_aad) in FLOW_AADS {
        candidates.push(if ready { ready_aad } else { state_aad });
    }
    for aad in candidates {
        if let Some(value) = open(stored, pepper, aad) {
            return encode_handle(&value).ok();
        }
    }
    None
}
