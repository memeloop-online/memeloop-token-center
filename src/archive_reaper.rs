//! Independent durable cleanup loop for archive staging attempts.
//!
//! Database claims and reference proofs finish before object-store I/O begins.
//! Every object operation is bounded well inside the cleanup lease, and the
//! database fences the final transition if the lease nevertheless expires.

use std::{sync::Arc, time::Duration};

use tokio::sync::watch;

use crate::{
    archive::{ArchiveStagingObjectStore, ArchiveStore},
    archive_staging::{
        ARCHIVE_STAGING_CLEANUP_HEARTBEAT_MILLIS, ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS,
        ArchiveStagingCleanupErrorCode, ArchiveStagingCleanupLease, ArchiveStagingEmptyResult,
        ArchiveStagingLeaseOwner, ArchiveStagingReferenceProof,
    },
    db::Database,
    error::AppError,
};

pub const ARCHIVE_REAPER_INTERVAL: Duration = Duration::from_secs(10);
pub const ARCHIVE_REAPER_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
pub const ARCHIVE_REAPER_CLAIM_BATCH: usize = 4;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveReaperPass {
    pub promoted: u64,
    pub claimed: u64,
    pub cleaned: u64,
}

#[derive(Clone)]
pub struct ArchiveReaper {
    database: Database,
    archive: Arc<dyn ArchiveStagingObjectStore>,
    owner: ArchiveStagingLeaseOwner,
}

impl ArchiveReaper {
    pub fn new(database: Database, archive: ArchiveStore, owner: ArchiveStagingLeaseOwner) -> Self {
        Self::with_store(database, Arc::new(archive), owner)
    }

    /// Allows an object-store adapter to preserve the same typed deletion
    /// boundary. This is also the failure-injection seam for reaper tests.
    pub fn with_store(
        database: Database,
        archive: Arc<dyn ArchiveStagingObjectStore>,
        owner: ArchiveStagingLeaseOwner,
    ) -> Self {
        Self {
            database,
            archive,
            owner,
        }
    }

    /// Runs one bounded promote-and-claim pass. Per-attempt failures are
    /// persisted and do not prevent another candidate in the batch from being
    /// claimed.
    pub async fn reap_once(&self) -> Result<ArchiveReaperPass, AppError> {
        let promoted = self
            .database
            .promote_stale_archive_staging_attempts()
            .await?;
        let mut pass = ArchiveReaperPass {
            promoted,
            ..ArchiveReaperPass::default()
        };

        for _ in 0..ARCHIVE_REAPER_CLAIM_BATCH {
            let Some(lease) = self
                .database
                .claim_archive_staging_cleanup(self.owner.clone())
                .await?
            else {
                break;
            };
            pass.claimed = pass.claimed.saturating_add(1);
            if self.cleanup_claim(lease).await == CleanupClaimResult::Cleaned {
                pass.cleaned = pass.cleaned.saturating_add(1);
            }
        }
        Ok(pass)
    }

    /// Runs independently from generation processing until shutdown is
    /// requested. Shutdown cancels the current pass; a claimed row remains
    /// durably leased for later recovery and cannot be incorrectly finalized.
    /// No in-memory SQL transaction spans the cancellable object-store I/O.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        let mut interval = tokio::time::interval(ARCHIVE_REAPER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'reaper: loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                break 'reaper;
                            }
                        }
                        result = self.reap_once() => {
                            if result.is_err() {
                                // Do not log database URLs or dynamic object-store errors.
                                tracing::error!(error_code = "reaper_pass_failed", "archive staging reaper pass failed");
                            }
                        }
                    }
                }
            }
        }
    }

    async fn cleanup_claim(&self, lease: ArchiveStagingCleanupLease) -> CleanupClaimResult {
        let failure_lease = lease.clone();
        let proof = match self
            .database
            .prove_archive_staging_unreferenced(lease)
            .await
        {
            Ok(ArchiveStagingReferenceProof::Unreferenced(proof)) => proof,
            Ok(ArchiveStagingReferenceProof::Protected { .. }) => {
                // The proof method already released the lease and persisted the
                // fixed `reference_present` code plus backoff.
                return CleanupClaimResult::Protected;
            }
            Err(_error) => {
                self.persist_failure(
                    &failure_lease,
                    ArchiveStagingCleanupErrorCode::ReferenceCheckFailed,
                )
                .await;
                return CleanupClaimResult::Failed;
            }
        };

        let key = failure_lease.attempt.key;
        let store_result = tokio::time::timeout(ARCHIVE_REAPER_OPERATION_TIMEOUT, async {
            self.archive
                .delete_archive_staging_segment(key)
                .await
                .map_err(|_| ArchiveStagingCleanupErrorCode::DeleteFailed)?;
            match self.archive.archive_staging_segment_is_empty(key).await {
                Ok(true) => Ok(()),
                Ok(false) | Err(_) => Err(ArchiveStagingCleanupErrorCode::VerificationFailed),
            }
        })
        .await;

        match store_result {
            Err(_) => {
                self.persist_failure(
                    &failure_lease,
                    ArchiveStagingCleanupErrorCode::ObjectStoreUnavailable,
                )
                .await;
                CleanupClaimResult::Failed
            }
            Ok(Err(code)) => {
                self.persist_failure(&failure_lease, code).await;
                CleanupClaimResult::Failed
            }
            Ok(Ok(())) => match self.database.record_archive_staging_empty(proof).await {
                Ok(ArchiveStagingEmptyResult::Cleaned) => CleanupClaimResult::Cleaned,
                Ok(ArchiveStagingEmptyResult::FirstObservation { .. }) => {
                    CleanupClaimResult::ObservedEmpty
                }
                Err(_error) => {
                    // The database is the final lease fence. Never compensate a
                    // rejected finalize with an unfenced state transition.
                    tracing::error!(
                        error_code = "cleanup_finalize_fenced",
                        "archive staging cleanup finalize was rejected"
                    );
                    CleanupClaimResult::Failed
                }
            },
        }
    }

    async fn persist_failure(
        &self,
        lease: &ArchiveStagingCleanupLease,
        code: ArchiveStagingCleanupErrorCode,
    ) {
        if self
            .database
            .record_archive_staging_cleanup_failure(lease, code)
            .await
            .is_err()
        {
            tracing::error!(
                error_code = "cleanup_failure_fenced",
                "archive staging cleanup failure could not be persisted"
            );
            return;
        }
        tracing::warn!(
            error_code = code.as_str(),
            "archive staging cleanup will retry"
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupClaimResult {
    Protected,
    ObservedEmpty,
    Cleaned,
    Failed,
}

const _: () = assert!(
    ARCHIVE_REAPER_OPERATION_TIMEOUT.as_millis() < ARCHIVE_STAGING_CLEANUP_HEARTBEAT_MILLIS as u128
        && ARCHIVE_REAPER_OPERATION_TIMEOUT.as_millis()
            < ARCHIVE_STAGING_CLEANUP_LEASE_MILLIS as u128
);
