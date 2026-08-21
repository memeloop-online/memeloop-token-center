mod apply;
mod correlation;
mod parsing;
mod plan;

use apply::*;
use correlation::*;
use parsing::*;
use plan::*;

#[cfg(test)]
include!("tests.rs");
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{
    Connection, Row,
    sqlite::{SqliteConnectOptions, SqliteConnection, SqliteJournalMode, SqliteSynchronous},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use uuid::Uuid;

use crate::{
    archive::ArchiveStore,
    conversation::ConversationHints,
    db::{
        Database, SessionArchiveCommitInput, SessionArchiveCorrelation, SessionArchiveImportMatch,
        SessionArchiveImportMatchInput, SessionArchiveQuarantineBatchInput,
        SessionArchiveQuarantineCommitInput, SessionArchiveQuarantineTarget, SessionArchiveTarget,
        SessionArchiveUnlinkedCommitInput, SessionArchiveUnlinkedMetadata,
        SessionArchiveUnlinkedTarget,
    },
    error::AppError,
    model::{AuthenticatedKey, KeyPolicy},
};

const IMPORT_PLAN_VERSION: i64 = 4;
const MAX_PLAN_RECORD_BYTES: usize = 512 * 1024;
const PLAN_SIZE_CHECK_INTERVAL: u64 = 32;
pub const MAX_SESSION_ARCHIVE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SESSION_ARCHIVE_PLAN_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SessionArchiveImportOptions<'a> {
    pub input: &'a Path,
    pub plan_directory: &'a Path,
    pub tenant_external_id: &'a str,
    pub cpamp_source: &'a str,
    pub archive_source: &'a str,
    pub overlap_ms: i64,
    pub time_tolerance_ms: i64,
    pub max_line_bytes: usize,
    pub max_plan_bytes: u64,
    pub allow_unmapped: bool,
    pub quarantine_unknown_identities: bool,
    pub quarantine_tenant_binding_kind: Option<&'a str>,
    pub quarantine_tenant_binding_proof: Option<&'a str>,
    pub quarantine_approved_by_service_id: Option<Uuid>,
    pub apply: bool,
}

pub fn validate_session_archive_import_options(
    options: &SessionArchiveImportOptions<'_>,
) -> Result<(), String> {
    validate_name(options.tenant_external_id, "tenant external id")?;
    validate_name(options.cpamp_source, "CPAMP source")?;
    validate_name(options.archive_source, "archive source")?;
    if !(1024..=MAX_SESSION_ARCHIVE_LINE_BYTES).contains(&options.max_line_bytes) {
        return Err(format!(
            "max line bytes must be between 1 KiB and the compiled-in {} MiB hard limit",
            MAX_SESSION_ARCHIVE_LINE_BYTES / (1024 * 1024)
        ));
    }
    if !(1024 * 1024..=MAX_SESSION_ARCHIVE_PLAN_BYTES).contains(&options.max_plan_bytes) {
        return Err(format!(
            "max plan bytes must be between 1 MiB and the compiled-in {} GiB hard limit",
            MAX_SESSION_ARCHIVE_PLAN_BYTES / (1024 * 1024 * 1024)
        ));
    }
    if options.apply && options.allow_unmapped {
        return Err("allow-unmapped is diagnostic-only and cannot be combined with apply".into());
    }
    if options.quarantine_unknown_identities {
        let kind = options
            .quarantine_tenant_binding_kind
            .filter(|value| valid_plan_text(value, 128))
            .ok_or("quarantine requires a tenant binding kind")?;
        let proof = options
            .quarantine_tenant_binding_proof
            .filter(|value| is_digest_hex(value))
            .ok_or("quarantine requires a 64-hex tenant binding proof")?;
        let _ = (kind, proof);
    } else if options.quarantine_tenant_binding_kind.is_some()
        || options.quarantine_tenant_binding_proof.is_some()
        || options.quarantine_approved_by_service_id.is_some()
    {
        return Err("quarantine binding options require quarantine admission".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanHeader {
    version: i64,
    tenant_external_id: String,
    cpamp_source: String,
    archive_source: String,
    source_size_bytes: u64,
    source_blake3: String,
    record_count: u64,
    quarantine_records: u64,
    quarantine_batch_id: Option<Uuid>,
    tenant_binding_kind: Option<String>,
    tenant_binding_proof: Option<String>,
    approved_by_service_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanKey {
    key_id: Uuid,
    tenant_id: Uuid,
    principal_id: Uuid,
    account_id: Uuid,
    alias: String,
    currency: String,
    credential_generation: i64,
    policy: KeyPolicy,
}

impl From<&AuthenticatedKey> for ImportPlanKey {
    fn from(key: &AuthenticatedKey) -> Self {
        Self {
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            principal_id: key.principal_id,
            account_id: key.account_id,
            alias: key.alias.clone(),
            currency: key.currency.clone(),
            credential_generation: key.credential_generation,
            policy: key.policy.clone(),
        }
    }
}

impl From<ImportPlanKey> for AuthenticatedKey {
    fn from(key: ImportPlanKey) -> Self {
        Self {
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            principal_id: key.principal_id,
            account_id: key.account_id,
            alias: key.alias,
            currency: key.currency,
            credential_generation: key.credential_generation,
            policy: key.policy,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanTarget {
    tenant_id: Uuid,
    target_request_id: Uuid,
    request_created_at: i64,
    key: ImportPlanKey,
    external_event_hash: String,
    source_created_at: i64,
    source_model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanUnlinkedTarget {
    tenant_id: Uuid,
    archive_request_id: Uuid,
    key: ImportPlanKey,
    identity_proof_kind: String,
    identity_proof_digest: String,
    correlation_proof_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
enum ImportPlanCorrelation {
    Exact {
        target: ImportPlanTarget,
        identity_proof_kind: String,
        identity_proof_digest: String,
        correlation_proof_digest: String,
    },
    Unlinked {
        target: ImportPlanUnlinkedTarget,
    },
    Quarantined {
        target: ImportPlanQuarantineTarget,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanQuarantineTarget {
    tenant_id: Uuid,
    quarantine_id: Uuid,
    reason_code: String,
    identity_claim_digest: Option<String>,
    proof_digest: String,
}

impl From<&SessionArchiveCorrelation> for ImportPlanCorrelation {
    fn from(correlation: &SessionArchiveCorrelation) -> Self {
        match correlation {
            SessionArchiveCorrelation::Exact {
                target,
                identity_proof_kind,
                identity_proof_digest,
                correlation_proof_digest,
            } => Self::Exact {
                target: ImportPlanTarget::from(target),
                identity_proof_kind: identity_proof_kind.clone(),
                identity_proof_digest: identity_proof_digest.clone(),
                correlation_proof_digest: correlation_proof_digest.clone(),
            },
            SessionArchiveCorrelation::Unlinked(target) => Self::Unlinked {
                target: ImportPlanUnlinkedTarget {
                    tenant_id: target.tenant_id,
                    archive_request_id: target.archive_request_id,
                    key: ImportPlanKey::from(&target.key),
                    identity_proof_kind: target.identity_proof_kind.clone(),
                    identity_proof_digest: target.identity_proof_digest.clone(),
                    correlation_proof_digest: target.correlation_proof_digest.clone(),
                },
            },
        }
    }
}

impl From<&SessionArchiveQuarantineTarget> for ImportPlanQuarantineTarget {
    fn from(target: &SessionArchiveQuarantineTarget) -> Self {
        Self {
            tenant_id: target.tenant_id,
            quarantine_id: target.quarantine_id,
            reason_code: target.reason_code.clone(),
            identity_claim_digest: target.identity_claim_digest.clone(),
            proof_digest: target.proof_digest.clone(),
        }
    }
}

impl From<ImportPlanQuarantineTarget> for SessionArchiveQuarantineTarget {
    fn from(target: ImportPlanQuarantineTarget) -> Self {
        Self {
            tenant_id: target.tenant_id,
            quarantine_id: target.quarantine_id,
            reason_code: target.reason_code,
            identity_claim_digest: target.identity_claim_digest,
            proof_digest: target.proof_digest,
        }
    }
}

impl From<&SessionArchiveTarget> for ImportPlanTarget {
    fn from(target: &SessionArchiveTarget) -> Self {
        Self {
            tenant_id: target.tenant_id,
            target_request_id: target.target_request_id,
            request_created_at: target.request_created_at,
            key: ImportPlanKey::from(&target.key),
            external_event_hash: target.external_event_hash.clone(),
            source_created_at: target.source_created_at,
            source_model: target.source_model.clone(),
        }
    }
}

impl From<ImportPlanTarget> for SessionArchiveTarget {
    fn from(target: ImportPlanTarget) -> Self {
        Self {
            tenant_id: target.tenant_id,
            target_request_id: target.target_request_id,
            request_created_at: target.request_created_at,
            key: target.key.into(),
            external_event_hash: target.external_event_hash,
            source_created_at: target.source_created_at,
            source_model: target.source_model,
            replay: false,
        }
    }
}

impl From<ImportPlanUnlinkedTarget> for SessionArchiveUnlinkedTarget {
    fn from(target: ImportPlanUnlinkedTarget) -> Self {
        Self {
            tenant_id: target.tenant_id,
            archive_request_id: target.archive_request_id,
            key: target.key.into(),
            identity_proof_kind: target.identity_proof_kind,
            identity_proof_digest: target.identity_proof_digest,
            correlation_proof_digest: target.correlation_proof_digest,
            replay: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanRecord {
    version: i64,
    external_request_id: String,
    correlation: ImportPlanCorrelation,
    record_digest: String,
    request_digest: Option<String>,
    response_digest: Option<String>,
    request_object: Option<String>,
    response_object: Option<String>,
    request_is_structured: bool,
    conversation_hints: ConversationHints,
    client_name: Option<String>,
    source_started_at: i64,
    source_completed_at: Option<i64>,
    source_checkpoint_ms: i64,
    protocol: String,
    model: String,
    status_code: Option<i64>,
    duration_ms: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    error_code: Option<String>,
}

struct PlanPathGuard {
    path: PathBuf,
}

impl Drop for PlanPathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut sidecar = self.path.as_os_str().to_os_string();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(sidecar));
        }
    }
}

struct SealedImportPlan {
    path: PlanPathGuard,
    identity: InputIdentity,
    size: u64,
    digest: blake3::Hash,
    record_count: u64,
}

struct ValidatedImportPlan {
    connection: SqliteConnection,
    record_count: u64,
    header: ImportPlanHeader,
    _path: PlanPathGuard,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionArchiveImportStats {
    pub scanned: u64,
    pub eligible: u64,
    pub before_overlap: u64,
    pub mapped: u64,
    pub unmapped: u64,
    pub quarantined: u64,
    pub quarantine_imported: u64,
    pub quarantine_replayed: u64,
    pub replayed: u64,
    pub imported: u64,
    pub input_device: u64,
    pub input_inode: u64,
    pub input_size_bytes: u64,
    pub input_mtime_seconds: i64,
    pub input_mtime_nanoseconds: i64,
    pub input_blake3: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchiveRecord {
    schema_version: i64,
    session_id: String,
    request_id: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    #[serde(default)]
    key_id: String,
    #[serde(default)]
    principal_id: String,
    #[serde(default)]
    credential_hash: String,
    #[serde(default)]
    requested_model: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    status_code: i64,
    // Older importer releases ignored this top-level display hint when deriving
    // the canonical record digest. Read it for archive-only metadata, but keep
    // the digest stable so an unchanged historical record remains replayable.
    #[serde(default, skip_serializing)]
    request_path: String,
    #[serde(default)]
    metadata: serde_json::Map<String, Value>,
    #[serde(default)]
    facets: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    request: Value,
    #[serde(default)]
    response: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InputIdentity {
    device: u64,
    inode: u64,
    size: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: i64,
}

impl InputIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(not(unix))]
            device: 0,
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(not(unix))]
            inode: 0,
            size: metadata.len(),
            #[cfg(unix)]
            mtime_seconds: metadata.mtime(),
            #[cfg(not(unix))]
            mtime_seconds: metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|value| i64::try_from(value.as_secs()).ok())
                .unwrap_or_default(),
            #[cfg(unix)]
            mtime_nanoseconds: metadata.mtime_nsec(),
            #[cfg(not(unix))]
            mtime_nanoseconds: metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| i64::from(value.subsec_nanos()))
                .unwrap_or_default(),
        }
    }
}

struct SealedInput {
    identity: InputIdentity,
    digest: blake3::Hash,
}

struct ReadOnlyInput {
    path: PathBuf,
    identity: InputIdentity,
    reader: BufReader<tokio::fs::File>,
}

impl ReadOnlyInput {
    async fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
        let descriptor_metadata = file.metadata().await?;
        if !descriptor_metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive input must be a regular file",
            )
            .into());
        }
        let identity = InputIdentity::from_metadata(&descriptor_metadata);
        let path_identity = InputIdentity::from_metadata(&tokio::fs::metadata(path).await?);
        if identity != path_identity {
            return Err(input_changed_error().into());
        }
        Ok(Self {
            path: path.to_owned(),
            identity,
            reader: BufReader::new(file),
        })
    }

    async fn rewind(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.verify_identity().await?;
        self.reader.seek(io::SeekFrom::Start(0)).await?;
        Ok(())
    }

    async fn seal(
        &self,
        digest: blake3::Hash,
    ) -> Result<SealedInput, Box<dyn std::error::Error + Send + Sync>> {
        self.verify_identity().await?;
        Ok(SealedInput {
            identity: self.identity.clone(),
            digest,
        })
    }

    async fn verify_seal(
        &self,
        seal: &SealedInput,
        digest: blake3::Hash,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.verify_identity().await?;
        if self.identity != seal.identity || digest != seal.digest {
            return Err(input_changed_error().into());
        }
        Ok(())
    }

    async fn verify_identity(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let descriptor = InputIdentity::from_metadata(&self.reader.get_ref().metadata().await?);
        let path = InputIdentity::from_metadata(&tokio::fs::metadata(&self.path).await?);
        if descriptor != self.identity || path != self.identity {
            return Err(input_changed_error().into());
        }
        Ok(())
    }
}

fn input_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "archive input changed after preflight; no apply is permitted",
    )
}

fn record_input_seal(stats: &mut SessionArchiveImportStats, seal: &SealedInput) {
    stats.input_device = seal.identity.device;
    stats.input_inode = seal.identity.inode;
    stats.input_size_bytes = seal.identity.size;
    stats.input_mtime_seconds = seal.identity.mtime_seconds;
    stats.input_mtime_nanoseconds = seal.identity.mtime_nanoseconds;
    stats.input_blake3 = seal.digest.to_hex().to_string();
}

pub async fn import_session_archive(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
) -> Result<SessionArchiveImportStats, Box<dyn std::error::Error + Send + Sync>> {
    validate_session_archive_import_options(options)?;
    if options.apply {
        validate_plan_directory(options.plan_directory).await?;
    }
    let import_lock = db
        .acquire_session_archive_import_lock(options.tenant_external_id, options.archive_source)
        .await?;
    let result = import_session_archive_locked(db, archive, options).await;
    let release = import_lock.release().await;
    match (result, release) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(stats), Ok(())) => Ok(stats),
    }
}

async fn import_session_archive_locked(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
) -> Result<SessionArchiveImportStats, Box<dyn std::error::Error + Send + Sync>> {
    let lower_bound = db
        .session_archive_lower_bound(
            options.tenant_external_id,
            options.archive_source,
            options.overlap_ms,
        )
        .await?;

    // Pass one never writes. A missing/ambiguous CPAMP identity or a protected
    // request/response locator therefore stops the entire batch before any target
    // object or relational row can be changed.
    let mut input = ReadOnlyInput::open(options.input).await?;
    let (mut stats, first_digest) = preflight_pass(db, options, lower_bound, &mut input).await?;
    let seal = input.seal(first_digest).await?;
    record_input_seal(&mut stats, &seal);
    if stats.unmapped > 0 && !options.allow_unmapped {
        return Err(format!(
            "archive import stopped before writes: {} of {} eligible records were unmapped, ambiguous, or inconsistent",
            stats.unmapped, stats.eligible
        )
        .into());
    }
    if !options.apply {
        return Ok(stats);
    }

    // Re-read and re-match the complete source while writing only content-addressed
    // objects and a local bounded plan. No target database write is permitted until
    // the source reaches EOF with its original seal and the plan itself is sealed,
    // reopened read-only and completely validated.
    input.rewind().await?;
    let (second_stats, sealed_plan) =
        build_import_plan(db, archive, options, lower_bound, &mut input, &seal).await?;
    if second_stats.unmapped > 0 {
        return Err(format!(
            "archive import stopped before writes: {} of {} eligible records were unmapped, ambiguous, or inconsistent",
            second_stats.unmapped, second_stats.eligible
        )
        .into());
    }
    stats = second_stats;
    record_input_seal(&mut stats, &seal);
    let mut plan = open_validated_plan(
        sealed_plan.ok_or("archive import plan was not created")?,
        options,
        &seal,
    )
    .await?;
    let applied = apply_validated_plan(db, archive, options, &mut plan).await?;
    stats.imported = applied.imported;
    stats.quarantine_imported = applied.quarantine_imported;
    stats.quarantine_replayed = applied.quarantine_replayed;
    // The explicit read transaction is the immutable SQLite snapshot used by
    // apply. It never contains writes and is discarded after the final row.
    sqlx::query("ROLLBACK")
        .execute(&mut plan.connection)
        .await?;
    plan.connection.close().await?;
    Ok(stats)
}
