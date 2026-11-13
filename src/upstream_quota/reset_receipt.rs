//! Metadata-only outcomes. Recovery never contacts the supplier.
use crate::{archive::ArchiveStore, db::Database, error::AppError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub version: u8,
    pub tenant: Uuid,
    pub account: Uuid,
    pub operation: Uuid,
    pub owner: Uuid,
    pub accepted: bool,
    pub error: Option<String>,
}
impl Receipt {
    fn key(&self) -> String {
        format!(
            "quota-reset-results/v1/{}/{}/{}/{}.json",
            self.tenant, self.account, self.operation, self.owner
        )
    }
    fn valid(&self) -> bool {
        self.version == 1
            && if self.accepted {
                self.error.is_none()
            } else {
                self.error.as_deref().is_some_and(|error| {
                    matches!(
                        error,
                        "reset_dispatch_owner_ended"
                            | "quota_reset_deadline_elapsed"
                            | "reset_send_connect_timeout"
                            | "reset_send_connect_failed"
                            | "reset_send_timeout"
                            | "reset_send_request_invalid"
                            | "reset_send_transport_failed"
                    ) || error
                        .strip_prefix("reset_response_http_")
                        .and_then(|code| code.parse::<u16>().ok())
                        .is_some_and(|code| (100..=599).contains(&code))
                })
            }
    }
    pub(super) async fn publish(&self, archive: &ArchiveStore) -> Result<(), AppError> {
        if !self.valid() {
            return Err(AppError::storage("quota_reset_receipt_invalid"));
        }
        archive
            .publish_quota_reset_receipt(
                &self.key(),
                serde_json::to_vec(self)
                    .map_err(|_| AppError::internal("quota_reset_receipt_encode"))?
                    .into(),
            )
            .await
    }
    pub(super) async fn finish(&self, db: &Database) -> Result<bool, AppError> {
        db.apply_quota_reset_receipt(
            self.tenant,
            self.account,
            self.operation,
            self.owner,
            self.accepted,
            self.error.as_deref(),
        )
        .await
    }
    pub(super) async fn retire(&self, archive: &ArchiveStore) -> Result<(), AppError> {
        archive.delete(&self.key()).await
    }
}
pub(crate) async fn recover(state: &crate::AppState) {
    let keys = match state.archive.quota_reset_receipts().await {
        Ok(keys) => keys,
        Err(_) => {
            tracing::warn!("quota reset receipt listing deferred");
            return;
        }
    };
    for key in keys {
        let result = async {
            let bytes = state.archive.get_bounded(&key, 4096).await?;
            let receipt: Receipt = serde_json::from_slice(&bytes)
                .map_err(|_| AppError::storage("quota_reset_receipt_invalid"))?;
            if !receipt.valid() || receipt.key() != key {
                return Err(AppError::storage("quota_reset_receipt_invalid"));
            }
            if receipt.finish(&state.db).await? {
                receipt.retire(&state.archive).await?;
            }
            Ok::<_, AppError>(())
        }
        .await;
        if result.is_err() {
            tracing::warn!("quota reset receipt recovery deferred; evidence retained");
        }
    }
}
