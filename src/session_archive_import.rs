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

async fn validate_plan_directory(
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata = tokio::fs::symlink_metadata(directory).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("session archive plan directory must be a real directory".into());
    }
    Ok(())
}

async fn build_import_plan(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    lower_bound: i64,
    input: &mut ReadOnlyInput,
    source_seal: &SealedInput,
) -> Result<
    (SessionArchiveImportStats, Option<SealedImportPlan>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (guard, mut connection) = create_import_plan(options.plan_directory).await?;
    create_import_plan_schema(&mut connection).await?;
    let mut transaction = connection.begin().await?;
    let mut stats = SessionArchiveImportStats::default();
    let mut source_hasher = blake3::Hasher::new();
    let mut record_count = 0_u64;
    let mut serialized_bytes = 0_u64;

    while let Some(line) = read_bounded_line(&mut input.reader, options.max_line_bytes).await? {
        source_hasher.update(&line);
        let Some((record, record_digest)) = parse_record(&line)? else {
            continue;
        };
        stats.scanned += 1;
        if !archive_record_inside_overlap(&record, lower_bound) {
            stats.before_overlap += 1;
            continue;
        }
        stats.eligible += 1;
        let matched = match match_record(db, options, &record, &record_digest).await {
            Ok(matched) => matched,
            Err(AppError::BadRequest(_)) if options.allow_unmapped => {
                stats.unmapped += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let plan_correlation = match &matched {
            SessionArchiveImportMatch::Correlated(correlation) => {
                preflight_gap_compatibility(db, options, &record, correlation.as_ref()).await?;
                stats.mapped += 1;
                stats.replayed += u64::from(correlation.replay());
                ImportPlanCorrelation::from(correlation.as_ref())
            }
            SessionArchiveImportMatch::Quarantine(target)
                if options.quarantine_unknown_identities =>
            {
                stats.quarantined += 1;
                ImportPlanCorrelation::Quarantined {
                    target: ImportPlanQuarantineTarget::from(target),
                }
            }
            SessionArchiveImportMatch::Quarantine(_) => {
                stats.unmapped += 1;
                continue;
            }
        };

        // Stage one payload at a time and transfer ownership of its buffer to
        // Bytes. This keeps request/response serialization buffers from
        // overlapping and avoids a second full-payload copy before upload.
        let request = payload_bytes(&record.request)?;
        let request_digest = request.as_deref().map(digest);
        // These are the only durable writes allowed before both seals validate.
        // Content addressing makes an orphan harmless if planning later fails.
        let request_object = match request {
            Some(body) => Some(archive.put_content(Bytes::from(body)).await?),
            None => None,
        };
        let response = payload_bytes(&record.response)?;
        let response_digest = response.as_deref().map(digest);
        let response_object = match response {
            Some(body) => Some(archive.put_content(Bytes::from(body)).await?),
            None => None,
        };
        let source_started_at = record.started_at.timestamp_millis();
        let completed_at = record.completed_at.timestamp_millis();
        let source_completed_at = Some(completed_at);
        let duration_ms = Some(completed_at - source_started_at);
        let (input_tokens, output_tokens) = archive_usage(&record);
        let plan_record = ImportPlanRecord {
            version: IMPORT_PLAN_VERSION,
            external_request_id: record.request_id.clone(),
            correlation: plan_correlation,
            record_digest,
            request_digest,
            response_digest,
            request_object,
            response_object,
            request_is_structured: structured_request(&record.request).is_some(),
            conversation_hints: conversation_hints(&record),
            client_name: first_facet(&record, "client")
                .or_else(|| metadata_string(&record, "client"))
                .map(str::to_owned),
            source_started_at,
            source_completed_at,
            source_checkpoint_ms: completed_at,
            protocol: archive_protocol(&record),
            model: nonempty(&record.requested_model)
                .or_else(|| nonempty(&record.model))
                .unwrap_or("unknown")
                .to_owned(),
            status_code: (record.status_code > 0).then_some(record.status_code),
            duration_ms,
            input_tokens,
            output_tokens,
            error_code: archive_error_code(&record),
        };
        validate_plan_record(&plan_record)?;
        let encoded = serde_json::to_vec(&plan_record)?;
        if encoded.len() > MAX_PLAN_RECORD_BYTES {
            return Err("session archive import plan record exceeds 512 KiB".into());
        }
        serialized_bytes = serialized_bytes.saturating_add(encoded.len() as u64);
        if serialized_bytes > options.max_plan_bytes {
            return Err("session archive import plan exceeds its configured size limit".into());
        }
        record_count += 1;
        sqlx::query(
            "INSERT INTO import_plan_records (sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(i64::try_from(record_count)?)
        .bind(plan_record.source_started_at)
        .bind(plan_record.source_checkpoint_ms)
        .bind(&plan_record.external_request_id)
        .bind(encoded)
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("session archive import plan is not unique: {error}"),
            )
        })?;
        if record_count.is_multiple_of(PLAN_SIZE_CHECK_INTERVAL)
            && plan_database_bytes(&mut transaction).await? > options.max_plan_bytes
        {
            return Err("session archive import plan exceeds its configured size limit".into());
        }
    }

    input
        .verify_seal(source_seal, source_hasher.finalize())
        .await?;
    if stats.unmapped > 0 {
        transaction.rollback().await?;
        connection.close().await?;
        return Ok((stats, None));
    }
    let header = ImportPlanHeader {
        version: IMPORT_PLAN_VERSION,
        tenant_external_id: options.tenant_external_id.to_owned(),
        cpamp_source: options.cpamp_source.to_owned(),
        archive_source: options.archive_source.to_owned(),
        source_size_bytes: source_seal.identity.size,
        source_blake3: source_seal.digest.to_hex().to_string(),
        record_count,
        quarantine_records: stats.quarantined,
        quarantine_batch_id: (stats.quarantined > 0).then(|| {
            quarantine_batch_id(
                options.tenant_external_id,
                options.archive_source,
                source_seal.digest.to_hex().as_ref(),
            )
        }),
        tenant_binding_kind: options.quarantine_tenant_binding_kind.map(str::to_owned),
        tenant_binding_proof: options.quarantine_tenant_binding_proof.map(str::to_owned),
        approved_by_service_id: options.quarantine_approved_by_service_id,
    };
    sqlx::query("INSERT INTO import_plan_metadata (singleton, header_json) VALUES (1, $1)")
        .bind(serde_json::to_vec(&header)?)
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    connection.close().await?;
    let sealed = seal_import_plan(guard, record_count, options.max_plan_bytes).await?;
    Ok((stats, Some(sealed)))
}

async fn create_import_plan(
    directory: &Path,
) -> Result<(PlanPathGuard, SqliteConnection), Box<dyn std::error::Error + Send + Sync>> {
    validate_plan_directory(directory).await?;
    let mut created = None;
    for _ in 0..8 {
        let path = directory.join(format!(
            ".mtc-session-archive-plan-{}.sqlite",
            Uuid::now_v7()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&path) {
            Ok(file) => {
                drop(file);
                created = Some(PlanPathGuard { path });
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let guard = created.ok_or("could not allocate a unique session archive plan")?;
    let connect_options = SqliteConnectOptions::new()
        .filename(&guard.path)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Delete)
        .synchronous(SqliteSynchronous::Full)
        .foreign_keys(true);
    let connection = SqliteConnection::connect_with(&connect_options).await?;
    Ok((guard, connection))
}

async fn create_import_plan_schema(
    connection: &mut SqliteConnection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::query(
        "CREATE TABLE import_plan_metadata (singleton INTEGER PRIMARY KEY CHECK(singleton = 1), header_json BLOB NOT NULL); CREATE TABLE import_plan_records (sequence INTEGER PRIMARY KEY CHECK(sequence > 0), source_started_at INTEGER NOT NULL CHECK(source_started_at >= 0), source_checkpoint_ms INTEGER NOT NULL CHECK(source_checkpoint_ms >= source_started_at), external_request_id TEXT NOT NULL UNIQUE, record_json BLOB NOT NULL); CREATE INDEX import_plan_apply_order ON import_plan_records(source_checkpoint_ms, external_request_id, sequence)",
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn plan_database_bytes(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
) -> Result<u64, sqlx::Error> {
    let page_count: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&mut **transaction)
        .await?;
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut **transaction)
        .await?;
    Ok((page_count.max(0) as u64).saturating_mul(page_size.max(0) as u64))
}

async fn seal_import_plan(
    guard: PlanPathGuard,
    record_count: u64,
    maximum: u64,
) -> Result<SealedImportPlan, Box<dyn std::error::Error + Send + Sync>> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&guard.path)
        .await?;
    file.sync_all().await?;
    drop(file);
    #[cfg(unix)]
    tokio::fs::set_permissions(&guard.path, std::fs::Permissions::from_mode(0o400)).await?;
    #[cfg(not(unix))]
    {
        let mut permissions = tokio::fs::metadata(&guard.path).await?.permissions();
        permissions.set_readonly(true);
        tokio::fs::set_permissions(&guard.path, permissions).await?;
    }
    let (identity, size, digest) = hash_plan_file(&guard.path).await?;
    if size > maximum {
        return Err("session archive import plan exceeds its configured size limit".into());
    }
    Ok(SealedImportPlan {
        path: guard,
        identity,
        size,
        digest,
        record_count,
    })
}

async fn hash_plan_file(
    path: &Path,
) -> Result<(InputIdentity, u64, blake3::Hash), Box<dyn std::error::Error + Send + Sync>> {
    let mut file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
    let identity = InputIdentity::from_metadata(&file.metadata().await?);
    let path_identity = InputIdentity::from_metadata(&tokio::fs::metadata(path).await?);
    if identity != path_identity || !file.metadata().await?.is_file() {
        return Err(plan_changed_error().into());
    }
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let final_identity = InputIdentity::from_metadata(&file.metadata().await?);
    let final_path_identity = InputIdentity::from_metadata(&tokio::fs::metadata(path).await?);
    if identity != final_identity || identity != final_path_identity || size != identity.size {
        return Err(plan_changed_error().into());
    }
    Ok((identity, size, hasher.finalize()))
}

fn plan_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "session archive import plan changed after it was sealed; no database apply is permitted",
    )
}

async fn verify_plan_file(
    plan: &SealedImportPlan,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        let mode = tokio::fs::metadata(&plan.path.path)
            .await?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o400 {
            return Err(plan_changed_error().into());
        }
    }
    let (identity, size, digest) = hash_plan_file(&plan.path.path).await?;
    if identity != plan.identity || size != plan.size || digest != plan.digest {
        return Err(plan_changed_error().into());
    }
    Ok(())
}

async fn open_validated_plan(
    plan: SealedImportPlan,
    options: &SessionArchiveImportOptions<'_>,
    source_seal: &SealedInput,
) -> Result<ValidatedImportPlan, Box<dyn std::error::Error + Send + Sync>> {
    verify_plan_file(&plan).await?;
    let connect_options = SqliteConnectOptions::new()
        .filename(&plan.path.path)
        .read_only(true);
    let mut connection = SqliteConnection::connect_with(&connect_options).await?;
    sqlx::query("PRAGMA query_only = ON")
        .execute(&mut connection)
        .await?;
    // Keep one read transaction from the complete validation scan through the
    // final apply read. SQLite's rollback-journal shared lock prevents another
    // SQLite writer from changing later rows after earlier rows have committed.
    // Apply therefore reads the exact snapshot that was parsed and hashed.
    sqlx::query("BEGIN").execute(&mut connection).await?;
    let header =
        validate_plan_contents(&mut connection, options, source_seal, plan.record_count).await?;
    // Hash again after SQLite parsed every row. This closes path replacement or
    // in-place mutation races before the first target database transaction.
    verify_plan_file(&plan).await?;
    #[cfg(unix)]
    tokio::fs::remove_file(&plan.path.path).await?;
    Ok(ValidatedImportPlan {
        connection,
        record_count: plan.record_count,
        header,
        _path: plan.path,
    })
}

async fn validate_plan_contents(
    connection: &mut SqliteConnection,
    options: &SessionArchiveImportOptions<'_>,
    source_seal: &SealedInput,
    expected_records: u64,
) -> Result<ImportPlanHeader, Box<dyn std::error::Error + Send + Sync>> {
    let header_bytes: Vec<u8> =
        sqlx::query_scalar("SELECT header_json FROM import_plan_metadata WHERE singleton = 1")
            .fetch_one(&mut *connection)
            .await?;
    let header: ImportPlanHeader = serde_json::from_slice(&header_bytes)?;
    if header.version != IMPORT_PLAN_VERSION
        || header.tenant_external_id != options.tenant_external_id
        || header.cpamp_source != options.cpamp_source
        || header.archive_source != options.archive_source
        || header.source_size_bytes != source_seal.identity.size
        || header.source_blake3 != source_seal.digest.to_hex().as_str()
        || header.record_count != expected_records
        || header.quarantine_records > header.record_count
        || (header.quarantine_records > 0
            && (header.quarantine_batch_id.is_none()
                || header
                    .tenant_binding_kind
                    .as_deref()
                    .is_none_or(|value| !valid_plan_text(value, 128))
                || header
                    .tenant_binding_proof
                    .as_deref()
                    .is_none_or(|value| !is_digest_hex(value))))
        || (header.quarantine_records == 0
            && (header.quarantine_batch_id.is_some()
                || header.tenant_binding_kind.is_some()
                || header.tenant_binding_proof.is_some()
                || header.approved_by_service_id.is_some()))
    {
        return Err(plan_changed_error().into());
    }

    let mut count = 0_u64;
    let mut rows = sqlx::query(
        "SELECT sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json FROM import_plan_records ORDER BY sequence ASC",
    )
    .fetch(&mut *connection);
    while let Some(row) = rows.try_next().await? {
        count += 1;
        let sequence: i64 = row.try_get("sequence")?;
        if sequence != i64::try_from(count)? {
            return Err(plan_changed_error().into());
        }
        let bytes: Vec<u8> = row.try_get("record_json")?;
        if bytes.len() > MAX_PLAN_RECORD_BYTES {
            return Err(plan_changed_error().into());
        }
        let record: ImportPlanRecord = serde_json::from_slice(&bytes)?;
        validate_plan_record(&record)?;
        if row.try_get::<i64, _>("source_started_at")? != record.source_started_at
            || row.try_get::<i64, _>("source_checkpoint_ms")? != record.source_checkpoint_ms
            || row.try_get::<String, _>("external_request_id")? != record.external_request_id
        {
            return Err(plan_changed_error().into());
        }
    }
    if count != expected_records {
        return Err(plan_changed_error().into());
    }
    Ok(header)
}

fn validate_plan_record(record: &ImportPlanRecord) -> Result<(), io::Error> {
    let correlation_valid = match &record.correlation {
        ImportPlanCorrelation::Exact {
            target,
            identity_proof_kind,
            identity_proof_digest,
            correlation_proof_digest,
        } => {
            target.tenant_id == target.key.tenant_id
                && !target.target_request_id.is_nil()
                && is_digest_hex(&target.external_event_hash)
                && valid_plan_text(&target.source_model, 512)
                && valid_plan_text(identity_proof_kind, 200)
                && is_digest_hex(identity_proof_digest)
                && is_digest_hex(correlation_proof_digest)
        }
        ImportPlanCorrelation::Unlinked { target } => {
            target.tenant_id == target.key.tenant_id
                && !target.archive_request_id.is_nil()
                && valid_plan_text(&target.identity_proof_kind, 200)
                && is_digest_hex(&target.identity_proof_digest)
                && is_digest_hex(&target.correlation_proof_digest)
        }
        ImportPlanCorrelation::Quarantined { target } => {
            !target.tenant_id.is_nil()
                && !target.quarantine_id.is_nil()
                && matches!(
                    target.reason_code.as_str(),
                    "missing_credential_hash" | "unproven_identity"
                )
                && target
                    .identity_claim_digest
                    .as_deref()
                    .is_none_or(is_digest_hex)
                && is_digest_hex(&target.proof_digest)
        }
    };
    let timing_valid = match (record.source_completed_at, record.duration_ms) {
        (Some(completed), Some(duration)) => completed
            .checked_sub(record.source_started_at)
            .is_some_and(|expected| {
                expected >= 0 && duration == expected && record.source_checkpoint_ms == completed
            }),
        (None, None) => record.source_checkpoint_ms == record.source_started_at,
        _ => false,
    };
    if record.version != IMPORT_PLAN_VERSION
        || record.external_request_id.is_empty()
        || record.external_request_id.len() > 512
        || record.external_request_id.chars().any(char::is_control)
        || !correlation_valid
        || !is_digest_hex(&record.record_digest)
        || record
            .request_digest
            .as_deref()
            .is_some_and(|value| !is_digest_hex(value))
        || record
            .response_digest
            .as_deref()
            .is_some_and(|value| !is_digest_hex(value))
        || !plan_object_matches(
            record.request_object.as_deref(),
            record.request_digest.as_deref(),
        )
        || !plan_object_matches(
            record.response_object.as_deref(),
            record.response_digest.as_deref(),
        )
        || (record.request_is_structured && record.request_object.is_none())
        || !timing_valid
        || !valid_plan_text(&record.protocol, 512)
        || !valid_plan_text(&record.model, 512)
        || record.status_code.is_some_and(|status| status <= 0)
        || record.input_tokens < 0
        || record.output_tokens < 0
        || record
            .error_code
            .as_deref()
            .is_some_and(|value| !valid_plan_text(value, 200))
    {
        return Err(plan_changed_error());
    }
    Ok(())
}

fn valid_plan_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn plan_object_matches(location: Option<&str>, digest: Option<&str>) -> bool {
    match (location, digest) {
        (None, None) => true,
        (Some(location), Some(digest)) if is_digest_hex(digest) => {
            location == content_location(digest)
        }
        _ => false,
    }
}

fn is_digest_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn apply_validated_plan(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    plan: &mut ValidatedImportPlan,
) -> Result<ApplyStats, Box<dyn std::error::Error + Send + Sync>> {
    let mut applied = ApplyStats::default();
    let mut seen = 0_u64;
    let mut rows = sqlx::query(
        "SELECT sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json FROM import_plan_records ORDER BY source_checkpoint_ms ASC, external_request_id ASC, sequence ASC",
    )
    .fetch(&mut plan.connection);
    while let Some(row) = rows.try_next().await? {
        seen += 1;
        let encoded: Vec<u8> = row.try_get("record_json")?;
        let record: ImportPlanRecord = serde_json::from_slice(&encoded)?;
        validate_plan_record(&record)?;
        if row.try_get::<i64, _>("source_started_at")? != record.source_started_at
            || row.try_get::<i64, _>("source_checkpoint_ms")? != record.source_checkpoint_ms
            || row.try_get::<String, _>("external_request_id")? != record.external_request_id
        {
            return Err(plan_changed_error().into());
        }
        let request_json = load_structured_plan_request(archive, options, &record).await?;
        let committed = match &record.correlation {
            ImportPlanCorrelation::Exact {
                target,
                identity_proof_kind,
                identity_proof_digest,
                correlation_proof_digest,
            } => {
                let target: SessionArchiveTarget = target.clone().into();
                db.commit_session_archive_request(SessionArchiveCommitInput {
                    tenant_external_id: options.tenant_external_id,
                    archive_source: options.archive_source,
                    external_request_id: &record.external_request_id,
                    target: &target,
                    record_digest: &record.record_digest,
                    request_digest: record.request_digest.as_deref(),
                    response_digest: record.response_digest.as_deref(),
                    request_object: record.request_object.as_deref(),
                    response_object: record.response_object.as_deref(),
                    request_json: request_json.as_ref(),
                    conversation_hints: &record.conversation_hints,
                    client_name: record.client_name.as_deref(),
                    source_started_at: record.source_started_at,
                    source_completed_at: record.source_completed_at,
                    identity_proof_kind,
                    identity_proof_digest,
                    correlation_proof_digest,
                })
                .await?
            }
            ImportPlanCorrelation::Unlinked { target } => {
                let target: SessionArchiveUnlinkedTarget = target.clone().into();
                db.commit_session_archive_unlinked_request(SessionArchiveUnlinkedCommitInput {
                    tenant_external_id: options.tenant_external_id,
                    archive_source: options.archive_source,
                    external_request_id: &record.external_request_id,
                    target: &target,
                    record_digest: &record.record_digest,
                    request_digest: record.request_digest.as_deref(),
                    response_digest: record.response_digest.as_deref(),
                    request_object: record.request_object.as_deref(),
                    response_object: record.response_object.as_deref(),
                    request_json: request_json.as_ref(),
                    conversation_hints: &record.conversation_hints,
                    client_name: record.client_name.as_deref(),
                    source_started_at: record.source_started_at,
                    metadata: SessionArchiveUnlinkedMetadata {
                        source_completed_at: record.source_completed_at,
                        protocol: &record.protocol,
                        model: &record.model,
                        status_code: record.status_code,
                        duration_ms: record.duration_ms,
                        input_tokens: record.input_tokens,
                        output_tokens: record.output_tokens,
                        error_code: record.error_code.as_deref(),
                    },
                })
                .await?
            }
            ImportPlanCorrelation::Quarantined { target } => {
                let target: SessionArchiveQuarantineTarget = target.clone().into();
                let header = &plan.header;
                let batch_id = header.quarantine_batch_id.ok_or_else(plan_changed_error)?;
                let source_size_bytes = i64::try_from(header.source_size_bytes)?;
                let eligible_records = i64::try_from(header.record_count)?;
                let quarantine_records = i64::try_from(header.quarantine_records)?;
                let sequence: i64 = row.try_get("sequence")?;
                let committed = db
                    .commit_session_archive_quarantine(SessionArchiveQuarantineCommitInput {
                        batch: SessionArchiveQuarantineBatchInput {
                            batch_id,
                            tenant_external_id: options.tenant_external_id,
                            archive_source: options.archive_source,
                            cpamp_source: options.cpamp_source,
                            source_digest: &header.source_blake3,
                            source_size_bytes,
                            eligible_records,
                            quarantine_records,
                            tenant_binding_kind: header
                                .tenant_binding_kind
                                .as_deref()
                                .ok_or_else(plan_changed_error)?,
                            tenant_binding_proof: header
                                .tenant_binding_proof
                                .as_deref()
                                .ok_or_else(plan_changed_error)?,
                            approved_by_service_id: header.approved_by_service_id,
                        },
                        sequence,
                        target: &target,
                        external_request_id: &record.external_request_id,
                        record_digest: &record.record_digest,
                        source_started_at: record.source_started_at,
                        source_completed_at: record.source_completed_at,
                        protocol: &record.protocol,
                        model: &record.model,
                        status_code: record.status_code,
                        duration_ms: record.duration_ms,
                        input_tokens: record.input_tokens,
                        output_tokens: record.output_tokens,
                        error_code: record.error_code.as_deref(),
                        request_digest: record.request_digest.as_deref(),
                        response_digest: record.response_digest.as_deref(),
                        request_object: record.request_object.as_deref(),
                        response_object: record.response_object.as_deref(),
                    })
                    .await?;
                applied.quarantine_imported += u64::from(committed);
                applied.quarantine_replayed += u64::from(!committed);
                false
            }
        };
        applied.imported += u64::from(committed);
    }
    if seen != plan.record_count {
        return Err(plan_changed_error().into());
    }
    Ok(applied)
}

#[derive(Default)]
struct ApplyStats {
    imported: u64,
    quarantine_imported: u64,
    quarantine_replayed: u64,
}

async fn load_structured_plan_request(
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    record: &ImportPlanRecord,
) -> Result<Option<Value>, Box<dyn std::error::Error + Send + Sync>> {
    if !record.request_is_structured {
        return Ok(None);
    }
    let location = record
        .request_object
        .as_deref()
        .ok_or_else(plan_changed_error)?;
    let expected_digest = record
        .request_digest
        .as_deref()
        .ok_or_else(plan_changed_error)?;
    let body = archive
        .get_bounded(location, options.max_line_bytes)
        .await?;
    if digest(&body) != expected_digest {
        return Err("planned request CAS object failed its content digest".into());
    }
    let value: Value = serde_json::from_slice(&body)?;
    if !matches!(value, Value::Array(_) | Value::Object(_)) {
        return Err(plan_changed_error().into());
    }
    Ok(Some(value))
}

async fn preflight_pass(
    db: &Database,
    options: &SessionArchiveImportOptions<'_>,
    lower_bound: i64,
    input: &mut ReadOnlyInput,
) -> Result<(SessionArchiveImportStats, blake3::Hash), Box<dyn std::error::Error + Send + Sync>> {
    let mut stats = SessionArchiveImportStats::default();
    let mut hasher = blake3::Hasher::new();
    while let Some(line) = read_bounded_line(&mut input.reader, options.max_line_bytes).await? {
        hasher.update(&line);
        let Some((record, digest)) = parse_record(&line)? else {
            continue;
        };
        stats.scanned += 1;
        if !archive_record_inside_overlap(&record, lower_bound) {
            stats.before_overlap += 1;
            continue;
        }
        stats.eligible += 1;
        match match_record(db, options, &record, &digest).await {
            Ok(SessionArchiveImportMatch::Correlated(correlation)) => {
                preflight_gap_compatibility(db, options, &record, correlation.as_ref()).await?;
                stats.mapped += 1;
                stats.replayed += u64::from(correlation.replay());
            }
            Ok(SessionArchiveImportMatch::Quarantine(_))
                if options.quarantine_unknown_identities =>
            {
                stats.quarantined += 1;
            }
            Ok(SessionArchiveImportMatch::Quarantine(_)) => stats.unmapped += 1,
            Err(AppError::BadRequest(_)) if options.allow_unmapped => stats.unmapped += 1,
            Err(AppError::BadRequest(error)) => return Err(AppError::BadRequest(error).into()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok((stats, hasher.finalize()))
}

async fn match_record(
    db: &Database,
    options: &SessionArchiveImportOptions<'_>,
    record: &ArchiveRecord,
    record_digest: &str,
) -> Result<SessionArchiveImportMatch, AppError> {
    let source_key_hash = archived_credential_hash(record)?;
    db.match_session_archive_import(SessionArchiveImportMatchInput {
        tenant_external_id: options.tenant_external_id,
        cpamp_source: options.cpamp_source,
        archive_source: options.archive_source,
        external_request_id: &record.request_id,
        started_at: record.started_at.timestamp_millis(),
        requested_model: nonempty(&record.requested_model),
        resolved_model: nonempty(&record.model),
        source_key_hash: source_key_hash.as_deref(),
        input_tokens: None,
        output_tokens: None,
        record_digest,
        time_tolerance_ms: options.time_tolerance_ms,
    })
    .await
}

async fn preflight_gap_compatibility(
    db: &Database,
    options: &SessionArchiveImportOptions<'_>,
    record: &ArchiveRecord,
    correlation: &SessionArchiveCorrelation,
) -> Result<(), AppError> {
    let SessionArchiveCorrelation::Exact { target, .. } = correlation else {
        return Ok(());
    };
    if target.replay {
        return Ok(());
    }

    let request_object = payload_content_location(&record.request)?;
    let response_object = payload_content_location(&record.response)?;
    let current = db
        .request_archive_refs_for_tenant(options.tenant_external_id, target.target_request_id)
        .await?;
    gap_compatible(&current.request_object, request_object.as_deref())?;
    if let Some(current_response) = current.response_object.as_deref() {
        gap_compatible(current_response, response_object.as_deref())?;
    }
    Ok(())
}

fn payload_content_location(value: &Value) -> Result<Option<String>, AppError> {
    payload_bytes(value)
        .map(|body| body.map(|body| content_location(&digest(&body))))
        .map_err(|_| AppError::Internal)
}

fn content_location(digest: &str) -> String {
    format!("objects/blake3/{}/{digest}", &digest[..2])
}

fn quarantine_batch_id(tenant_external_id: &str, source: &str, source_digest: &str) -> Uuid {
    let digest = blake3::hash(
        format!(
            "memeloop-session-archive-quarantine-batch-v1\0{tenant_external_id}\0{source}\0{source_digest}"
        )
        .as_bytes(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn gap_compatible(current: &str, replacement: Option<&str>) -> Result<(), AppError> {
    if replacement.is_none() || current.starts_with("gap://") || Some(current) == replacement {
        return Ok(());
    }
    Err(AppError::BadRequest(
        "archive import refused to overwrite an existing object".into(),
    ))
}

fn parse_record(
    line: &[u8],
) -> Result<Option<(ArchiveRecord, String)>, Box<dyn std::error::Error + Send + Sync>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let record: ArchiveRecord = serde_json::from_slice(line)?;
    if !matches!(record.schema_version, 1 | 2) {
        return Err(format!(
            "unsupported cpa-session-archive schema {}",
            record.schema_version
        )
        .into());
    }
    if record.request_id.is_empty()
        || record.request_id.len() > 512
        || record.request_id.chars().any(char::is_control)
        || record.session_id.len() > 1024
        || record.session_id.chars().any(char::is_control)
        || record.completed_at < record.started_at
    {
        return Err("archive record contains an invalid identity or time range".into());
    }
    let mut canonical_hasher = blake3::Hasher::new();
    serde_json::to_writer(&mut canonical_hasher, &record)?;
    let record_digest = canonical_hasher.finalize().to_hex().to_string();
    Ok(Some((record, record_digest)))
}

fn archive_record_inside_overlap(record: &ArchiveRecord, lower_bound: i64) -> bool {
    let started_at = record.started_at.timestamp_millis();
    let completed_at = record.completed_at.timestamp_millis();
    // This exactly mirrors the source delta cursor: long-running or late-completed
    // records remain eligible when either endpoint is inside the overlap window.
    started_at >= lower_bound || completed_at >= lower_bound
}

fn archived_credential_hash(record: &ArchiveRecord) -> Result<Option<String>, AppError> {
    let candidate = match record.schema_version {
        // Schema 1 defines key_id itself as the legacy bare digest.  Do not
        // broaden that older envelope by interpreting labels or prefixes.
        1 => nonempty(&record.key_id),
        2 => match nonempty(&record.credential_hash) {
            // An explicit schema-2 field is authoritative: malformed data must
            // not fall back to key_id.  Only the specified sha256 prefix is
            // stripped, and the digest is canonicalized for proof stability.
            Some(value) => {
                return normalize_schema_v2_sha256(value).map(Some).ok_or_else(|| {
                    AppError::BadRequest("archive credential hash is malformed".into())
                });
            }
            None => nonempty(&record.key_id),
        },
        _ => None,
    };
    candidate
        .map(|value| {
            normalize_bare_sha256(value)
                .ok_or_else(|| AppError::BadRequest("archive credential hash is malformed".into()))
        })
        .transpose()
}

fn normalize_bare_sha256(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn normalize_schema_v2_sha256(value: &str) -> Option<String> {
    normalize_bare_sha256(value.strip_prefix("sha256:").unwrap_or(value))
}

fn payload_bytes(value: &Value) -> Result<Option<Vec<u8>>, serde_json::Error> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.as_bytes().to_vec())),
        value => serde_json::to_vec(value).map(Some),
    }
}

fn structured_request(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(_) | Value::Object(_) => Some(value),
        _ => None,
    }
}

fn conversation_hints(record: &ArchiveRecord) -> ConversationHints {
    let turn_id = first_facet(record, "turn.id").map(str::to_owned);
    let parent_turn_id = first_facet(record, "parent.turn.id")
        .or_else(|| metadata_string(record, "parent_response_id"))
        .map(str::to_owned);
    let branch_id = first_facet(record, "branch.id").map(str::to_owned);
    let compaction = record
        .facets
        .get("request.kind")
        .is_some_and(|values| values.iter().any(|value| value.contains("compaction")));
    ConversationHints {
        session_id: nonempty(&record.session_id).map(str::to_owned),
        turn_id,
        parent_turn_id,
        branch_id,
        compaction,
    }
}

fn first_facet<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .facets
        .get(name)
        .and_then(|values| values.iter().find_map(|value| nonempty(value)))
}

fn metadata_string<'a>(record: &'a ArchiveRecord, name: &str) -> Option<&'a str> {
    record
        .metadata
        .get(name)
        .and_then(Value::as_str)
        .and_then(nonempty)
}

fn archive_protocol(record: &ArchiveRecord) -> String {
    nonempty(&record.request_path)
        .or_else(|| first_facet(record, "request.path"))
        .or_else(|| metadata_string(record, "request_path"))
        .or_else(|| first_facet(record, "client"))
        .unwrap_or("session-archive")
        .to_owned()
}

fn archive_usage(record: &ArchiveRecord) -> (i64, i64) {
    let usage = record
        .response
        .pointer("/response/usage")
        .or_else(|| record.response.pointer("/usage"));
    let Some(usage) = usage else {
        return (0, 0);
    };
    let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_i64) else {
        return (0, 0);
    };
    let Some(output_tokens) = usage.get("output_tokens").and_then(Value::as_i64) else {
        return (0, 0);
    };
    if input_tokens < 0 || output_tokens < 0 {
        (0, 0)
    } else {
        (input_tokens, output_tokens)
    }
}

fn archive_error_code(record: &ArchiveRecord) -> Option<String> {
    let outcome = record.outcome.trim();
    let successful =
        outcome.eq_ignore_ascii_case("success") || outcome.eq_ignore_ascii_case("succeeded");
    if successful && record.status_code < 400 {
        return None;
    }
    if !outcome.is_empty() && !successful {
        let normalized: String = outcome
            .chars()
            .take(200)
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '-') {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect();
        return nonempty(&normalized).map(str::to_owned);
    }
    (record.status_code >= 400).then(|| format!("http_{}", record.status_code))
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn validate_name(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(format!("{label} contains unsupported characters"));
    }
    Ok(())
}

async fn read_bounded_line(
    reader: &mut BufReader<tokio::fs::File>,
    maximum: usize,
) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
    if maximum > MAX_SESSION_ARCHIVE_LINE_BYTES {
        return Err(format!(
            "max line bytes exceeds the compiled-in {} MiB hard limit",
            MAX_SESSION_ARCHIVE_LINE_BYTES / (1024 * 1024)
        )
        .into());
    }
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > maximum {
            return Err(format!("archive JSONL record exceeds {maximum} bytes").into());
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn validation_options(
        max_line_bytes: usize,
        max_plan_bytes: u64,
    ) -> SessionArchiveImportOptions<'static> {
        SessionArchiveImportOptions {
            input: Path::new("/archive.jsonl"),
            plan_directory: Path::new("/plan"),
            tenant_external_id: "archive-fixture",
            cpamp_source: "cpamp-usage-events-v1",
            archive_source: "cpa-session-archive-v2",
            overlap_ms: 0,
            time_tolerance_ms: 0,
            max_line_bytes,
            max_plan_bytes,
            allow_unmapped: false,
            quarantine_unknown_identities: false,
            quarantine_tenant_binding_kind: None,
            quarantine_tenant_binding_proof: None,
            quarantine_approved_by_service_id: None,
            apply: false,
        }
    }

    #[test]
    fn import_resource_limits_accept_boundaries_and_reject_overrides() {
        for (line, plan) in [
            (1024, 1024 * 1024),
            (
                MAX_SESSION_ARCHIVE_LINE_BYTES,
                MAX_SESSION_ARCHIVE_PLAN_BYTES,
            ),
        ] {
            validate_session_archive_import_options(&validation_options(line, plan))
                .expect("compiled-in resource boundary must be accepted");
        }

        let line_error = validate_session_archive_import_options(&validation_options(
            MAX_SESSION_ARCHIVE_LINE_BYTES + 1,
            MAX_SESSION_ARCHIVE_PLAN_BYTES,
        ))
        .expect_err("line limit must not override the compiled-in ceiling");
        assert!(line_error.contains("compiled-in 16 MiB hard limit"));

        let plan_error = validate_session_archive_import_options(&validation_options(
            MAX_SESSION_ARCHIVE_LINE_BYTES,
            MAX_SESSION_ARCHIVE_PLAN_BYTES + 1,
        ))
        .expect_err("plan limit must not override the compiled-in ceiling");
        assert!(plan_error.contains("compiled-in 1 GiB hard limit"));
    }

    #[tokio::test]
    async fn bounded_line_accepts_exact_limit_and_rejects_one_byte_over() {
        let exact = tempfile::NamedTempFile::new().expect("exact-limit input");
        std::fs::write(exact.path(), [vec![b'a'; 1023], vec![b'\n']].concat())
            .expect("write exact-limit input");
        let mut exact_input = ReadOnlyInput::open(exact.path()).await.expect("open input");
        let line = read_bounded_line(&mut exact_input.reader, 1024)
            .await
            .expect("exact-limit line must be accepted")
            .expect("exact-limit line");
        assert_eq!(line.len(), 1024);

        let over = tempfile::NamedTempFile::new().expect("over-limit input");
        std::fs::write(over.path(), [vec![b'a'; 1024], vec![b'\n']].concat())
            .expect("write over-limit input");
        let mut over_input = ReadOnlyInput::open(over.path()).await.expect("open input");
        let error = read_bounded_line(&mut over_input.reader, 1024)
            .await
            .expect_err("one byte over the line limit must fail");
        assert!(error.to_string().contains("exceeds 1024 bytes"));

        let hard_limit_error =
            read_bounded_line(&mut over_input.reader, MAX_SESSION_ARCHIVE_LINE_BYTES + 1)
                .await
                .expect_err("bounded reader must enforce the compiled-in hard limit");
        assert!(hard_limit_error.to_string().contains("compiled-in 16 MiB"));
    }

    #[test]
    fn payload_strings_restore_raw_bytes_and_objects_restore_json() {
        assert_eq!(
            payload_bytes(&Value::String("raw".into())).unwrap(),
            Some(b"raw".to_vec())
        );
        assert_eq!(
            payload_bytes(&serde_json::json!({"a": 1})).unwrap(),
            Some(br#"{"a":1}"#.to_vec())
        );
        assert_eq!(payload_bytes(&Value::Null).unwrap(), None);
    }

    #[test]
    fn invalid_sources_are_rejected() {
        assert!(validate_name("archive-v2", "source").is_ok());
        assert!(validate_name("../archive", "source").is_err());
    }

    #[test]
    fn archive_schema_uses_only_its_verified_identity_field() {
        let v1: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "a".repeat(64),
            "credential_hash": "b".repeat(64)
        }))
        .expect("schema-v1 fixture");
        assert_eq!(archived_credential_hash(&v1).unwrap(), Some("a".repeat(64)));

        let v2: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "a".repeat(64),
            "credential_hash": "b".repeat(64),
            "principal_id": "untrusted-source-principal"
        }))
        .expect("schema-v2 fixture");
        assert_eq!(archived_credential_hash(&v2).unwrap(), Some("b".repeat(64)));

        let v2_prefixed: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "human-label",
            "principal_id": "another-untrusted-source-principal",
            "credential_hash": format!("sha256:{}", "A".repeat(64))
        }))
        .expect("prefixed schema-v2 fixture");
        assert_eq!(
            archived_credential_hash(&v2_prefixed).unwrap(),
            Some("a".repeat(64))
        );

        let v2_legacy_fallback: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "c".repeat(64)
        }))
        .expect("schema-v2 legacy fallback fixture");
        assert_eq!(
            archived_credential_hash(&v2_legacy_fallback).unwrap(),
            Some("c".repeat(64))
        );

        let v2_invalid_explicit: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "c".repeat(64),
            "credential_hash": "invalid-explicit-value"
        }))
        .expect("schema-v2 invalid explicit fixture");
        assert!(archived_credential_hash(&v2_invalid_explicit).is_err());

        let v2_unknown_prefix: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "c".repeat(64),
            "credential_hash": format!("SHA256:{}", "d".repeat(64))
        }))
        .expect("unknown prefix schema-v2 fixture");
        assert!(archived_credential_hash(&v2_unknown_prefix).is_err());

        let invalid_v1: ArchiveRecord = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "key_id": "human-label",
            "credential_hash": "b".repeat(64)
        }))
        .expect("invalid schema-v1 fixture");
        assert!(archived_credential_hash(&invalid_v1).is_err());
    }

    #[test]
    fn request_path_enriches_metadata_without_changing_legacy_record_digest() {
        let without_path = serde_json::json!({
            "schema_version": 2,
            "session_id": "s",
            "request_id": "r",
            "started_at": "2026-08-12T00:00:00Z",
            "completed_at": "2026-08-12T00:00:01Z",
            "credential_hash": "a".repeat(64),
            "requested_model": "gpt-fixture"
        });
        let mut with_path = without_path.clone();
        with_path["request_path"] = Value::String("/v1/responses".into());
        let (without_record, without_digest) =
            parse_record(format!("{}\n", serde_json::to_string(&without_path).unwrap()).as_bytes())
                .unwrap()
                .unwrap();
        let (with_record, with_digest) =
            parse_record(format!("{}\n", serde_json::to_string(&with_path).unwrap()).as_bytes())
                .unwrap()
                .unwrap();
        assert_eq!(
            without_digest,
            digest(&serde_json::to_vec(&without_record).unwrap()),
            "streaming canonical hashing must match the legacy buffered encoding"
        );
        assert_eq!(without_digest, with_digest);
        assert_eq!(archive_protocol(&without_record), "session-archive");
        assert_eq!(archive_protocol(&with_record), "/v1/responses");
    }

    #[tokio::test]
    async fn sealed_input_rejects_in_place_changes() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary input");
        file.write_all(b"first sealed content\n")
            .expect("write input");
        file.flush().expect("flush input");

        let mut input = ReadOnlyInput::open(file.path()).await.expect("open input");
        let line = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("read input")
            .expect("one line");
        let seal = input.seal(blake3::hash(&line)).await.expect("seal input");

        std::fs::write(file.path(), b"changed input bytes\n").expect("mutate input");
        let error = input
            .rewind()
            .await
            .expect_err("changed input must not be reused");
        assert!(error.to_string().contains("changed after preflight"));
        assert_eq!(seal.digest, blake3::hash(b"first sealed content\n"));
    }

    #[tokio::test]
    async fn sealed_input_rejects_path_replacement_and_digest_mismatch() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("archive.jsonl");
        std::fs::write(&path, b"sealed\n").expect("write input");
        let mut input = ReadOnlyInput::open(&path).await.expect("open input");
        let line = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("read input")
            .expect("one line");
        let seal = input.seal(blake3::hash(&line)).await.expect("seal input");

        input
            .verify_seal(&seal, blake3::hash(b"different\n"))
            .await
            .expect_err("whole-file digest mismatch must fail");

        let replacement = directory.path().join("replacement.jsonl");
        std::fs::write(&replacement, b"sealed\n").expect("write replacement");
        std::fs::rename(&replacement, &path).expect("replace input path");
        let error = input
            .verify_identity()
            .await
            .expect_err("path replacement must fail");
        assert!(error.to_string().contains("changed after preflight"));
    }

    #[tokio::test]
    async fn sealed_input_rejects_changes_during_the_planning_scan() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary input");
        file.write_all(b"first\nsecond\n").expect("write input");
        file.flush().expect("flush input");

        let mut input = ReadOnlyInput::open(file.path()).await.expect("open input");
        let mut preflight_hasher = blake3::Hasher::new();
        while let Some(line) = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("preflight read")
        {
            preflight_hasher.update(&line);
        }
        let seal = input
            .seal(preflight_hasher.finalize())
            .await
            .expect("seal input");
        input.rewind().await.expect("start planning scan");

        let mut apply_hasher = blake3::Hasher::new();
        let first = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("first planning read")
            .expect("first planning line");
        apply_hasher.update(&first);
        file.write_all(b"changed-during-apply\n")
            .expect("append during planning");
        file.flush().expect("flush mutation");
        while let Some(line) = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("remaining planning read")
        {
            apply_hasher.update(&line);
        }
        let error = input
            .verify_seal(&seal, apply_hasher.finalize())
            .await
            .expect_err("mid-planning mutation must fail final verification");
        assert!(error.to_string().contains("changed after preflight"));
    }

    async fn empty_sealed_test_plan(directory: &Path) -> SealedImportPlan {
        let (guard, mut connection) = create_import_plan(directory)
            .await
            .expect("create test plan");
        create_import_plan_schema(&mut connection)
            .await
            .expect("create test plan schema");
        connection.close().await.expect("close test plan");
        seal_import_plan(guard, 0, 16 * 1024 * 1024)
            .await
            .expect("seal test plan")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sealed_plan_rejects_permission_content_and_path_tampering() {
        let directory = tempfile::tempdir().expect("test plan directory");

        let permission_plan = empty_sealed_test_plan(directory.path()).await;
        tokio::fs::set_permissions(
            &permission_plan.path.path,
            std::fs::Permissions::from_mode(0o600),
        )
        .await
        .expect("make plan writable");
        verify_plan_file(&permission_plan)
            .await
            .expect_err("a writable plan must fail closed");

        let content_plan = empty_sealed_test_plan(directory.path()).await;
        tokio::fs::set_permissions(
            &content_plan.path.path,
            std::fs::Permissions::from_mode(0o600),
        )
        .await
        .expect("make content plan writable");
        let mut content = std::fs::OpenOptions::new()
            .write(true)
            .open(&content_plan.path.path)
            .expect("open plan for tamper");
        content.write_all(b"tamper").expect("tamper plan content");
        content.flush().expect("flush plan tamper");
        drop(content);
        tokio::fs::set_permissions(
            &content_plan.path.path,
            std::fs::Permissions::from_mode(0o400),
        )
        .await
        .expect("restore read-only mode");
        verify_plan_file(&content_plan)
            .await
            .expect_err("content-tampered plan must fail closed");

        let path_plan = empty_sealed_test_plan(directory.path()).await;
        let replacement_bytes = std::fs::read(&path_plan.path.path).expect("read sealed plan");
        let displaced = directory.path().join("displaced-plan.sqlite");
        std::fs::rename(&path_plan.path.path, &displaced).expect("displace sealed plan");
        std::fs::write(&path_plan.path.path, replacement_bytes).expect("replace plan path");
        std::fs::set_permissions(&path_plan.path.path, std::fs::Permissions::from_mode(0o400))
            .expect("make replacement read-only");
        verify_plan_file(&path_plan)
            .await
            .expect_err("path-replaced plan must fail closed");
        std::fs::remove_file(displaced).expect("remove displaced plan");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn validated_plan_is_unlinked_before_database_apply() {
        let directory = tempfile::tempdir().expect("test plan directory");
        let mut source = tempfile::NamedTempFile::new().expect("test source");
        source.write_all(b"sealed source\n").expect("write source");
        source.flush().expect("flush source");
        let mut input = ReadOnlyInput::open(source.path())
            .await
            .expect("open source");
        let line = read_bounded_line(&mut input.reader, 1024)
            .await
            .expect("read source")
            .expect("source line");
        let source_seal = input.seal(blake3::hash(&line)).await.expect("seal source");
        let options = SessionArchiveImportOptions {
            input: source.path(),
            plan_directory: directory.path(),
            tenant_external_id: "archive-fixture",
            cpamp_source: "cpamp-usage-events-v1",
            archive_source: "unlink-test",
            overlap_ms: 0,
            time_tolerance_ms: 0,
            max_line_bytes: 1024,
            max_plan_bytes: 16 * 1024 * 1024,
            allow_unmapped: false,
            quarantine_unknown_identities: false,
            quarantine_tenant_binding_kind: None,
            quarantine_tenant_binding_proof: None,
            quarantine_approved_by_service_id: None,
            apply: true,
        };
        let (guard, mut connection) = create_import_plan(directory.path())
            .await
            .expect("create valid plan");
        create_import_plan_schema(&mut connection)
            .await
            .expect("create valid plan schema");
        let header = ImportPlanHeader {
            version: IMPORT_PLAN_VERSION,
            tenant_external_id: options.tenant_external_id.to_owned(),
            cpamp_source: options.cpamp_source.to_owned(),
            archive_source: options.archive_source.to_owned(),
            source_size_bytes: source_seal.identity.size,
            source_blake3: source_seal.digest.to_hex().to_string(),
            record_count: 0,
            quarantine_records: 0,
            quarantine_batch_id: None,
            tenant_binding_kind: None,
            tenant_binding_proof: None,
            approved_by_service_id: None,
        };
        sqlx::query("INSERT INTO import_plan_metadata (singleton, header_json) VALUES (1, $1)")
            .bind(serde_json::to_vec(&header).expect("encode header"))
            .execute(&mut connection)
            .await
            .expect("insert valid plan header");
        connection.close().await.expect("close valid plan");
        let writer_options = SqliteConnectOptions::new()
            .filename(&guard.path)
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Delete);
        let mut preopened_writer = SqliteConnection::connect_with(&writer_options)
            .await
            .expect("preopen a competing SQLite writer");
        sqlx::query("PRAGMA busy_timeout = 50")
            .execute(&mut preopened_writer)
            .await
            .expect("bound competing writer wait");
        let sealed = seal_import_plan(guard, 0, options.max_plan_bytes)
            .await
            .expect("seal valid plan");
        let path = sealed.path.path.clone();
        let validated = open_validated_plan(sealed, &options, &source_seal)
            .await
            .expect("open validated plan");
        assert!(
            !path.exists(),
            "validated plan path must be unlinked before apply"
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query("UPDATE import_plan_metadata SET header_json = X'00' WHERE singleton = 1")
                .execute(&mut preopened_writer),
        )
        .await
        .expect("competing writer must not wait indefinitely")
        .expect_err("validated read snapshot must reject a preopened SQLite writer");
        preopened_writer
            .close()
            .await
            .expect("close competing writer");
        validated
            .connection
            .close()
            .await
            .expect("close validated plan");
    }
}
