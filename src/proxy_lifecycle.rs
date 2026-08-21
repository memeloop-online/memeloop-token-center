use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
        BeginArchiveStagingInput, BeginArchiveStagingResult,
    },
    db::{AttachProxyArchiveResult, Database, FinishProxyRequest, FinishProxyRequestResult},
    error::AppError,
    model::UsageReservation,
};

pub(crate) const MAX_PROXY_STREAM_LIFETIME: Duration = Duration::from_secs(20 * 60);
pub(crate) const MAX_PROXY_FINALIZATION_LIFETIME: Duration = Duration::from_secs(30);
pub(crate) const MAX_PROXY_LIFETIME: Duration =
    MAX_PROXY_STREAM_LIFETIME.saturating_add(MAX_PROXY_FINALIZATION_LIFETIME);
pub(crate) const MAX_DOWNSTREAM_SEND_WAIT: Duration = Duration::from_secs(30);
pub(crate) const MAX_UNCONFIRMED_DELIVERY_BYTES: usize = 64 * 1024;

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
        ArchiveStagingPurpose::Request | ArchiveStagingPurpose::Response
    ) {
        return Err(AppError::BadRequest(
            "proxy archive purpose must be request or response".into(),
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
            Err(AppError::Internal) => tokio::time::sleep(delay).await,
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

pub(crate) async fn abandon_proxy_archive_attempt(
    database: &Database,
    attempt: &ProxyArchiveAttempt,
) {
    if database
        .abandon_archive_staging_attempt(&attempt.lease)
        .await
        .is_err()
    {
        tracing::warn!(
            error_code = "archive_staging_abandon_failed",
            "proxy archive staging attempt could not be scheduled for cleanup"
        );
    }
}

pub(crate) async fn finish_proxy_request_with_retry(
    database: &Database,
    input: FinishProxyRequest<'_>,
    archive_attempt: Option<&ProxyArchiveAttempt>,
) -> Result<FinishProxyRequestResult, AppError> {
    for delay in RETRY_DELAYS {
        match database
            .finish_proxy_request_with_archive_staging(
                input.clone(),
                archive_attempt.map(|attempt| &attempt.lease),
            )
            .await
        {
            Ok(result) => return Ok(result),
            Err(AppError::Internal) => tokio::time::sleep(delay).await,
            Err(error) => return Err(error),
        }
    }
    database
        .finish_proxy_request_with_archive_staging(
            input,
            archive_attempt.map(|attempt| &attempt.lease),
        )
        .await
}

/// A conclusive terminal loss may relinquish the fenced attempt. An internal
/// error can mean that commit succeeded but its ACK was lost, so unknown
/// outcomes leave the attempt untouched for exact replay or stale promotion.
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
    ) || matches!(result, Err(error) if !matches!(error, AppError::Internal))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_proxy_delivery_with_retry(
    database: &Database,
    request_id: Uuid,
    tenant_id: Uuid,
    reservation: &UsageReservation,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<&str>,
) -> Result<(), AppError> {
    for delay in RETRY_DELAYS {
        match database
            .prepare_proxy_delivery(
                request_id,
                tenant_id,
                reservation,
                input_token_ceiling,
                output_token_ceiling,
                requested_service_tier,
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(AppError::Internal) => tokio::time::sleep(delay).await,
            Err(error) => return Err(error),
        }
    }
    database
        .prepare_proxy_delivery(
            request_id,
            tenant_id,
            reservation,
            input_token_ceiling,
            output_token_ceiling,
            requested_service_tier,
        )
        .await
}

pub(crate) async fn confirm_proxy_delivery_with_retry(
    database: &Database,
    request_id: Uuid,
    tenant_id: Uuid,
    reservation: &UsageReservation,
) -> Result<(), AppError> {
    for delay in RETRY_DELAYS {
        match database
            .mark_proxy_delivery_started(request_id, tenant_id, reservation)
            .await
        {
            Ok(()) => return Ok(()),
            Err(AppError::Internal) => tokio::time::sleep(delay).await,
            Err(error) => return Err(error),
        }
    }
    database
        .mark_proxy_delivery_started(request_id, tenant_id, reservation)
        .await
}

pub(crate) async fn attach_proxy_archive_with_retry(
    database: &Database,
    request_id: Uuid,
    tenant_id: Uuid,
    reservation_id: Uuid,
    expected_request_object: &str,
    attempt: &ProxyArchiveAttempt,
) -> Result<AttachProxyArchiveResult, AppError> {
    for delay in RETRY_DELAYS {
        match database
            .attach_proxy_request_archive_staged(
                request_id,
                tenant_id,
                reservation_id,
                expected_request_object,
                &attempt.lease,
                &attempt.object_locator,
            )
            .await
        {
            Ok(result) => return Ok(result),
            Err(AppError::Internal) => tokio::time::sleep(delay).await,
            Err(error) => return Err(error),
        }
    }
    database
        .attach_proxy_request_archive_staged(
            request_id,
            tenant_id,
            reservation_id,
            expected_request_object,
            &attempt.lease,
            &attempt.object_locator,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

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
