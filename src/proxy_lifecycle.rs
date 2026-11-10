use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
        BeginArchiveStagingInput, BeginArchiveStagingResult,
    },
    db::{Database, FinishProxyRequest, FinishProxyRequestResult, ProxyRequestUpstreamAttribution},
    error::AppError,
};

mod owner;
pub(crate) mod pending_terminal;

#[derive(Default)]
pub(crate) struct ProxyLifecycleCounter {
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl ProxyLifecycleCounter {
    pub(crate) fn start(self: &Arc<Self>) -> ProxyLifecycleActivity {
        self.active.fetch_add(1, Ordering::AcqRel);
        ProxyLifecycleActivity {
            counter: self.clone(),
            owner: None,
        }
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) async fn wait_for_idle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.active() == 0 {
                return;
            }
            idle.await;
        }
    }
}

pub(crate) struct ProxyLifecycleActivity {
    counter: Arc<ProxyLifecycleCounter>,
    owner: Option<owner::RequestOwnerLease>,
}

impl ProxyLifecycleActivity {
    pub(crate) async fn acquire_owner(
        &mut self,
        database: Database,
        identity: crate::db::ProxyRequestOwner,
        lost: tokio::sync::watch::Sender<bool>,
    ) -> Result<(), AppError> {
        self.owner = Some(owner::RequestOwnerLease::acquire(database, identity, lost).await?);
        Ok(())
    }
}

impl Drop for ProxyLifecycleActivity {
    fn drop(&mut self) {
        if self.counter.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.counter.idle.notify_waiters();
        }
    }
}

pub(crate) const MAX_PROXY_FINALIZATION_LIFETIME: Duration = Duration::from_secs(30);
pub(crate) const MAX_DOWNSTREAM_SEND_WAIT: Duration = Duration::from_secs(30);
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(10),
    Duration::from_millis(50),
    Duration::from_millis(200),
];

#[derive(Clone, Debug)]
pub(crate) struct ProxyArchiveAttempt {
    pub(crate) lease: ArchiveStagingWriteLease,
    pub(crate) object_locator: String,
}

pub(crate) async fn begin_proxy_archive_attempt(
    database: &Database,
    request_id: Uuid,
    purpose: ArchiveStagingPurpose,
) -> Result<ProxyArchiveAttempt, AppError> {
    if !matches!(
        purpose,
        ArchiveStagingPurpose::Request
            | ArchiveStagingPurpose::Response
            | ArchiveStagingPurpose::Media
    ) {
        return Err(AppError::BadRequest(
            "proxy archive purpose must be request, response, or media".into(),
        ));
    }
    let attempt_id = Uuid::now_v7();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(request_id),
        purpose,
        attempt_id,
    )?;
    let mut digest = Sha256::new();
    digest.update(b"memeloop-token-center/proxy-archive-intent/v1\0");
    digest.update(request_id.as_bytes());
    digest.update(purpose.as_str().as_bytes());
    digest.update(attempt_id.as_bytes());
    let input = BeginArchiveStagingInput {
        key,
        intent_digest: ArchiveStagingIntentDigest::new(format!("{:x}", digest.finalize()))?,
        lease_token: Uuid::now_v7(),
        lease_owner: ArchiveStagingLeaseOwner::new(format!("proxy:{request_id}"))?,
    };
    let result = begin_proxy_archive_attempt_with_retry(database, input).await?;
    let lease = match result {
        BeginArchiveStagingResult::Created(lease) | BeginArchiveStagingResult::Replayed(lease) => {
            lease
        }
        BeginArchiveStagingResult::Existing(_) => {
            return Err(AppError::Conflict(
                "proxy archive staging attempt is no longer writable".into(),
            ));
        }
    };
    Ok(ProxyArchiveAttempt {
        object_locator: format!("{}/body", key.canonical_prefix()),
        lease,
    })
}

async fn begin_proxy_archive_attempt_with_retry(
    database: &Database,
    input: BeginArchiveStagingInput,
) -> Result<BeginArchiveStagingResult, AppError> {
    for delay in RETRY_DELAYS {
        match database.begin_archive_staging_attempt(input.clone()).await {
            Ok(result) => return Ok(result),
            Err(AppError::Internal {
                code: "proxy_lifecycle_begin_proxy_archive_attempt_with_retry_1",
                ..
            }) => {
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    database.begin_archive_staging_attempt(input).await
}

pub(crate) async fn heartbeat_proxy_archive_attempt(
    database: &Database,
    attempt: &mut ProxyArchiveAttempt,
) -> Result<bool, AppError> {
    database
        .heartbeat_archive_staging_write(&mut attempt.lease)
        .await
}

pub(crate) async fn finish_proxy_request_with_retry(
    database: &Database,
    input: FinishProxyRequest<'_>,
    archive_attempt: Option<&ProxyArchiveAttempt>,
    upstream_attribution: ProxyRequestUpstreamAttribution,
) -> Result<FinishProxyRequestResult, AppError> {
    for delay in RETRY_DELAYS {
        match database
            .finish_proxy_request_with_archive_staging_and_upstream_attribution(
                input.clone(),
                archive_attempt.map(|attempt| &attempt.lease),
                upstream_attribution,
            )
            .await
        {
            Ok(result) => return Ok(result),
            Err(AppError::Internal {
                code: "proxy_lifecycle_finish_proxy_request_with_retry_1",
                ..
            }) => {
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
    database
        .finish_proxy_request_with_archive_staging_and_upstream_attribution(
            input,
            archive_attempt.map(|attempt| &attempt.lease),
            upstream_attribution,
        )
        .await
}

/// A conclusive terminal loss may relinquish the fenced attempt. An internal
/// error can mean that commit succeeded but its ACK was lost, so unknown
/// outcomes leave the attempt untouched for exact replay or stale promotion.
#[cfg(test)]
pub(crate) fn response_archive_requires_cleanup(
    result: &Result<FinishProxyRequestResult, AppError>,
    stored_response: &str,
) -> bool {
    matches!(
        result,
        Ok(FinishProxyRequestResult::AlreadyFinished {
            response_object,
            ..
        }) if response_object != stored_response
    ) || matches!(result, Err(error) if !matches!(error, AppError::Internal { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_idle_notification_retains_all_accepted_activities() {
        let counter = Arc::new(ProxyLifecycleCounter::default());
        let first = counter.start();
        let second = counter.start();
        let mut drain = Box::pin(counter.wait_for_idle());
        assert!(futures_util::poll!(&mut drain).is_pending());
        drop(first);
        assert!(futures_util::poll!(&mut drain).is_pending());
        drop(second);
        assert!(futures_util::poll!(&mut drain).is_ready());
        counter.wait_for_idle().await;
    }

    #[tokio::test]
    async fn proxy_staging_intent_does_not_persist_a_body_digest() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("proxy-intent.db").display()
        );
        let database = Database::connect_with_max(&database_url, 1).await.unwrap();
        database.migrate().await.unwrap();
        let request_id = Uuid::now_v7();
        let attempt =
            begin_proxy_archive_attempt(&database, request_id, ArchiveStagingPurpose::Request)
                .await
                .unwrap();
        let persisted = database
            .archive_staging_attempt(attempt.lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap();
        let sensitive_body_sha256 = ArchiveStagingIntentDigest::new(format!(
            "{:x}",
            Sha256::digest(b"low entropy secret prompt")
        ))
        .unwrap();

        assert_ne!(persisted.intent_digest, sensitive_body_sha256);
        assert_eq!(persisted.key, attempt.lease.key);
    }
}
