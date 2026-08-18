//! Typed identities and fencing tokens for durable object-store staging.
//!
//! A deletable prefix is reconstructed exclusively from UUIDs and closed enums.
//! Database text is parsed back into those types before a cleanup worker sees a
//! path; callers never supply a raw prefix to the deletion boundary.

use uuid::Uuid;

use crate::error::AppError;

pub const ARCHIVE_STAGING_WRITE_LEASE_MILLIS: i64 = 90_000;
pub const ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS: i64 = 20_000;
pub const ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS: i64 = 5 * 60 * 1_000;
pub const ARCHIVE_STAGING_CLEANUP_HEARTBEAT_MILLIS: i64 = 60_000;
pub const ARCHIVE_STAGING_EMPTY_STABILITY_MILLIS: i64 = 60_000;
pub const ARCHIVE_STAGING_STALE_DELETE_GRACE_MILLIS: i64 = 30 * 60 * 1_000;
pub const ARCHIVE_STAGING_CLAIM_BATCH: i64 = 64;

const MAX_LOCATOR_BYTES: usize = 1_024;
const MAX_LEASE_OWNER_BYTES: usize = 128;
const CLEANUP_BACKOFF_BASE_MILLIS: i64 = 5_000;
const CLEANUP_BACKOFF_MAX_MILLIS: i64 = 6 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ArchiveStagingOwner {
    ProxyRequest(Uuid),
    SynchronousRequest(Uuid),
    GenerationJob(Uuid),
}

impl ArchiveStagingOwner {
    pub(crate) const fn kind(self) -> &'static str {
        match self {
            Self::ProxyRequest(_) => "proxy_request",
            Self::SynchronousRequest(_) => "synchronous_request",
            Self::GenerationJob(_) => "generation_job",
        }
    }

    pub(crate) const fn path_kind(self) -> &'static str {
        match self {
            Self::ProxyRequest(_) => "proxy",
            Self::SynchronousRequest(_) => "synchronous",
            Self::GenerationJob(_) => "generation",
        }
    }

    pub const fn id(self) -> Uuid {
        match self {
            Self::ProxyRequest(id) | Self::SynchronousRequest(id) | Self::GenerationJob(id) => id,
        }
    }

    pub(crate) fn parse(kind: &str, id: &str) -> Result<Self, AppError> {
        let id = parse_canonical_uuid(id)?;
        match kind {
            "proxy_request" => Ok(Self::ProxyRequest(id)),
            "synchronous_request" => Ok(Self::SynchronousRequest(id)),
            "generation_job" => Ok(Self::GenerationJob(id)),
            _ => Err(AppError::Internal),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ArchiveStagingPurpose {
    Request,
    Response,
    Result,
    Assets,
}

impl ArchiveStagingPurpose {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Result => "result",
            Self::Assets => "assets",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "request" => Ok(Self::Request),
            "response" => Ok(Self::Response),
            "result" => Ok(Self::Result),
            "assets" => Ok(Self::Assets),
            _ => Err(AppError::Internal),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ArchiveStagingKey {
    pub owner: ArchiveStagingOwner,
    pub purpose: ArchiveStagingPurpose,
    pub attempt_id: Uuid,
}

impl ArchiveStagingKey {
    pub fn new(
        owner: ArchiveStagingOwner,
        purpose: ArchiveStagingPurpose,
        attempt_id: Uuid,
    ) -> Result<Self, AppError> {
        if !valid_owner_purpose(owner, purpose) {
            return Err(AppError::BadRequest(
                "archive staging owner and purpose are incompatible".into(),
            ));
        }
        Ok(Self {
            owner,
            purpose,
            attempt_id,
        })
    }

    /// The only prefix a cleanup worker may pass to object storage.
    pub fn canonical_prefix(self) -> String {
        format!(
            "staging/{}/{}/{}/{}",
            self.owner.path_kind(),
            self.owner.id(),
            self.purpose.as_str(),
            self.attempt_id
        )
    }

    pub(crate) fn from_database(
        owner_kind: &str,
        owner_id: &str,
        purpose: &str,
        attempt_id: &str,
    ) -> Result<Self, AppError> {
        Self::new(
            ArchiveStagingOwner::parse(owner_kind, owner_id)?,
            ArchiveStagingPurpose::parse(purpose)?,
            parse_canonical_uuid(attempt_id)?,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveStagingIntentDigest(String);

impl ArchiveStagingIntentDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, AppError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(AppError::BadRequest(
                "archive staging intent digest must be lower-case SHA-256".into(),
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveStagingLeaseOwner(String);

impl ArchiveStagingLeaseOwner {
    pub fn new(value: impl Into<String>) -> Result<Self, AppError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_LEASE_OWNER_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
        {
            return Err(AppError::BadRequest(
                "archive staging lease owner must contain safe ASCII characters".into(),
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveStagingState {
    Writing,
    Bound,
    CleanupPending,
    Cleaned,
}

impl ArchiveStagingState {
    pub(crate) fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "writing" => Ok(Self::Writing),
            "bound" => Ok(Self::Bound),
            "cleanup_pending" => Ok(Self::CleanupPending),
            "cleaned" => Ok(Self::Cleaned),
            _ => Err(AppError::Internal),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveStagingAttempt {
    pub key: ArchiveStagingKey,
    pub intent_digest: ArchiveStagingIntentDigest,
    pub state: ArchiveStagingState,
    pub bound_locator: Option<String>,
    pub bound_at: Option<i64>,
    pub empty_observed_at: Option<i64>,
    pub cleanup_failures: u32,
    pub next_cleanup_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub cleaned_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveStagingWriteLease {
    pub key: ArchiveStagingKey,
    pub owner: ArchiveStagingLeaseOwner,
    pub token: Uuid,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveStagingCleanupLease {
    pub attempt: ArchiveStagingAttempt,
    pub owner: ArchiveStagingLeaseOwner,
    pub token: Uuid,
    pub expires_at: i64,
}

impl ArchiveStagingCleanupLease {
    pub fn canonical_prefix(&self) -> String {
        self.attempt.key.canonical_prefix()
    }
}

/// Capability returned only after the database has checked every normalized
/// application locator. Cleanup transitions require this proof object.
#[derive(Clone, Debug)]
pub struct ArchiveStagingUnreferencedLease {
    pub(crate) lease: ArchiveStagingCleanupLease,
    pub(crate) proved_at: i64,
}

impl ArchiveStagingUnreferencedLease {
    pub fn canonical_prefix(&self) -> String {
        self.lease.canonical_prefix()
    }
}

#[derive(Clone, Debug)]
pub struct BeginArchiveStagingInput {
    pub key: ArchiveStagingKey,
    pub intent_digest: ArchiveStagingIntentDigest,
    /// Stable across an exact retry. A different token for an existing attempt
    /// is a conflict and cannot take over an expired writer.
    pub lease_token: Uuid,
    pub lease_owner: ArchiveStagingLeaseOwner,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeginArchiveStagingResult {
    Created(ArchiveStagingWriteLease),
    Replayed(ArchiveStagingWriteLease),
    Existing(ArchiveStagingAttempt),
}

#[derive(Clone, Debug)]
pub enum ArchiveStagingReferenceProof {
    Unreferenced(ArchiveStagingUnreferencedLease),
    Protected { locator: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveStagingCleanupErrorCode {
    ObjectStoreUnavailable,
    DeleteFailed,
    VerificationFailed,
    ReferenceCheckFailed,
    ReferencePresent,
}

impl ArchiveStagingCleanupErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ObjectStoreUnavailable => "object_store_unavailable",
            Self::DeleteFailed => "delete_failed",
            Self::VerificationFailed => "verification_failed",
            Self::ReferenceCheckFailed => "reference_check_failed",
            Self::ReferencePresent => "reference_present",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveStagingEmptyResult {
    FirstObservation { confirm_after: i64 },
    Cleaned,
}

pub fn locator_matches_prefix(locator: &str, prefix: &str) -> bool {
    locator == prefix
        || locator
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub fn cleanup_backoff_millis(failure_count: u32) -> i64 {
    let shift = failure_count.saturating_sub(1).min(20);
    CLEANUP_BACKOFF_BASE_MILLIS
        .saturating_mul(1_i64 << shift)
        .min(CLEANUP_BACKOFF_MAX_MILLIS)
}

pub(crate) fn cleanup_backoff_with_jitter(attempt_id: Uuid, failure_count: u32) -> i64 {
    let base = cleanup_backoff_millis(failure_count);
    let jitter_bound = (base / 4).clamp(1, 60_000);
    let bytes = attempt_id.as_bytes();
    let sample = u64::from_be_bytes(bytes[..8].try_into().expect("UUID has sixteen bytes"));
    base.saturating_add(i64::try_from(sample % jitter_bound as u64).unwrap_or(0))
        .min(CLEANUP_BACKOFF_MAX_MILLIS)
}

pub(crate) fn validate_bound_locator(
    key: ArchiveStagingKey,
    locator: &str,
) -> Result<(), AppError> {
    if !valid_object_locator(locator) || !locator_matches_prefix(locator, &key.canonical_prefix()) {
        return Err(AppError::BadRequest(
            "archive staging binding must stay inside its canonical prefix".into(),
        ));
    }
    Ok(())
}

fn valid_owner_purpose(owner: ArchiveStagingOwner, purpose: ArchiveStagingPurpose) -> bool {
    matches!(
        (owner, purpose),
        (
            ArchiveStagingOwner::ProxyRequest(_),
            ArchiveStagingPurpose::Request | ArchiveStagingPurpose::Response
        ) | (
            ArchiveStagingOwner::SynchronousRequest(_),
            ArchiveStagingPurpose::Request | ArchiveStagingPurpose::Result
        ) | (
            ArchiveStagingOwner::GenerationJob(_),
            ArchiveStagingPurpose::Request | ArchiveStagingPurpose::Assets
        )
    )
}

fn parse_canonical_uuid(value: &str) -> Result<Uuid, AppError> {
    let parsed = Uuid::parse_str(value).map_err(|_| AppError::Internal)?;
    (parsed.to_string() == value)
        .then_some(parsed)
        .ok_or(AppError::Internal)
}

fn valid_object_locator(value: &str) -> bool {
    if value.is_empty()
        || value.len() > MAX_LOCATOR_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return false;
    }
    value.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && !segment.to_ascii_lowercase().contains("%2e")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_prefix_contains_only_typed_segments() {
        let owner_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff0").unwrap();
        let attempt_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff1").unwrap();
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(owner_id),
            ArchiveStagingPurpose::Response,
            attempt_id,
        )
        .unwrap();
        assert_eq!(
            key.canonical_prefix(),
            "staging/proxy/019fffff-ffff-7fff-bfff-fffffffffff0/response/019fffff-ffff-7fff-bfff-fffffffffff1"
        );
    }

    #[test]
    fn synchronous_request_and_result_have_distinct_prefixes() {
        let owner_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff0").unwrap();
        let attempt_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff1").unwrap();
        let request = ArchiveStagingKey::new(
            ArchiveStagingOwner::SynchronousRequest(owner_id),
            ArchiveStagingPurpose::Request,
            attempt_id,
        )
        .unwrap();
        let result = ArchiveStagingKey::new(
            ArchiveStagingOwner::SynchronousRequest(owner_id),
            ArchiveStagingPurpose::Result,
            attempt_id,
        )
        .unwrap();
        assert!(request.canonical_prefix().contains("/request/"));
        assert!(result.canonical_prefix().contains("/result/"));
        assert_ne!(request.canonical_prefix(), result.canonical_prefix());
        assert!(
            ArchiveStagingKey::new(
                ArchiveStagingOwner::SynchronousRequest(owner_id),
                ArchiveStagingPurpose::Assets,
                attempt_id,
            )
            .is_err()
        );
    }

    #[test]
    fn heartbeat_intervals_leave_more_than_two_missed_ticks_of_margin() {
        assert!(
            ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS.saturating_mul(3)
                < ARCHIVE_STAGING_WRITE_LEASE_MILLIS
        );
        assert!(
            ARCHIVE_STAGING_CLEANUP_HEARTBEAT_MILLIS.saturating_mul(3)
                < ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS
        );
    }

    #[test]
    fn binding_rejects_traversal_and_neighbor_prefixes() {
        let owner_id = Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff0").unwrap();
        let key = ArchiveStagingKey::new(
            ArchiveStagingOwner::GenerationJob(owner_id),
            ArchiveStagingPurpose::Assets,
            Uuid::parse_str("019fffff-ffff-7fff-bfff-fffffffffff1").unwrap(),
        )
        .unwrap();
        let prefix = key.canonical_prefix();
        assert!(validate_bound_locator(key, &format!("{prefix}/asset-0")).is_ok());
        for invalid in [
            format!("{prefix}2/asset-0"),
            format!("{prefix}/../asset-0"),
            format!("{prefix}/%2e%2e/asset-0"),
            format!("{prefix}//asset-0"),
        ] {
            assert!(validate_bound_locator(key, &invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn locator_matching_requires_an_exact_segment_boundary() {
        let prefix = "staging/proxy/id/request/attempt";
        assert!(locator_matches_prefix(prefix, prefix));
        assert!(locator_matches_prefix(
            "staging/proxy/id/request/attempt/body",
            prefix
        ));
        assert!(!locator_matches_prefix(
            "staging/proxy/id/request/attempt-neighbor/body",
            prefix
        ));
    }

    #[test]
    fn cleanup_backoff_is_bounded() {
        assert_eq!(cleanup_backoff_millis(1), 5_000);
        assert_eq!(cleanup_backoff_millis(2), 10_000);
        assert_eq!(cleanup_backoff_millis(63), 6 * 60 * 60 * 1_000);
    }
}
