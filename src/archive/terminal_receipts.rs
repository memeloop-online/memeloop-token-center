use futures_util::{Stream, StreamExt};

use super::{ArchiveStore, path::archive_path};
use crate::error::AppError;

impl ArchiveStore {
    /// Stream only the internal receipt namespace; never enumerate request bodies.
    pub(crate) fn terminal_receipts(
        &self,
        namespace: &str,
    ) -> Result<impl Stream<Item = Result<String, AppError>> + Send + '_, AppError> {
        let prefix = archive_path(&format!("terminal-receipts/v1/{namespace}"))?;
        Ok(self.inner.list(Some(&prefix)).filter_map(move |entry| {
            let prefix = prefix.clone();
            async move {
                match entry {
                    Ok(entry) if entry.location.prefix_matches(&prefix) => {
                        Some(Ok(entry.location.to_string()))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error.into())),
                }
            }
        }))
    }
}
