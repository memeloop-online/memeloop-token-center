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
    // Cursor is only a scan optimization: immutable remote objects remain the
    // recovery authority, and every new process starts scanning from the prefix.
    static CURSOR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let after = CURSOR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let keys = match state.archive.quota_reset_receipts(after.as_deref()).await {
        Ok(keys) => keys,
        Err(_) => {
            tracing::warn!("quota reset receipt listing deferred");
            return;
        }
    };
    *CURSOR.lock().unwrap_or_else(|e| e.into_inner()) = if keys.len() == 100 {
        keys.last().cloned()
    } else {
        None
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

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn immutable_outcomes_accept_duplicate_and_reject_conflict() {
        let archive = ArchiveStore::memory_for_test();
        let receipt = Receipt {
            version: 1,
            tenant: Uuid::now_v7(),
            account: Uuid::now_v7(),
            operation: Uuid::now_v7(),
            owner: Uuid::now_v7(),
            accepted: false,
            error: Some("reset_dispatch_owner_ended".into()),
        };
        receipt.publish(&archive).await.unwrap();
        receipt.publish(&archive).await.unwrap();
        let mut conflict = receipt.clone();
        conflict.accepted = true;
        conflict.error = None;
        assert!(conflict.publish(&archive).await.is_err());
        let stored: Receipt =
            serde_json::from_slice(&archive.get_bounded(&receipt.key(), 4096).await.unwrap())
                .unwrap();
        assert!(!stored.accepted);
        let mut invalid = receipt.clone();
        invalid.error = Some("https://supplier.invalid/private-response".into());
        assert!(invalid.publish(&archive).await.is_err());
        receipt.retire(&archive).await.unwrap();
    }
}
