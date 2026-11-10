use futures_util::{Stream, StreamExt};

use super::{ArchiveStore, path::archive_path};
use crate::error::AppError;

impl ArchiveStore {
    /// Stream only the internal receipt namespace; never enumerate request bodies.
    pub(crate) fn terminal_receipts(
        &self,
        namespace: &str,
    ) -> Result<
        impl Stream<Item = Result<(String, Option<[u8; 32]>), AppError>> + Send + '_,
        AppError,
    > {
        let prefix = archive_path(&format!("terminal-receipts/v1/{namespace}"))?;
        Ok(self.inner.list(Some(&prefix)).filter_map(move |entry| {
            let prefix = prefix.clone();
            async move {
                match entry {
                    Ok(entry) if entry.location.prefix_matches(&prefix) => {
                        Some(Ok((entry.location.to_string(), receipt_version(&entry))))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error.into())),
                }
            }
        }))
    }
}

fn receipt_version(entry: &object_store::ObjectMeta) -> Option<[u8; 32]> {
    (entry.e_tag.as_ref().is_some_and(|value| !value.is_empty())
        || entry
            .version
            .as_ref()
            .is_some_and(|value| !value.is_empty()))
    .then(|| {
        *blake3::hash(
            format!(
                "{:?}:{:?}:{}:{}",
                entry.e_tag, entry.version, entry.last_modified, entry.size
            )
            .as_bytes(),
        )
        .as_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_timestamp_cannot_hide_replaced_receipt_evidence() {
        let mut metadata = object_store::ObjectMeta {
            location: object_store::path::Path::from("receipt.json"),
            last_modified: chrono::Utc::now(),
            size: 100,
            e_tag: None,
            version: None,
        };
        assert_eq!(receipt_version(&metadata), None);
        metadata.e_tag = Some(String::new());
        assert_eq!(receipt_version(&metadata), None);
        metadata.e_tag = Some("before".into());
        let before = receipt_version(&metadata).unwrap();
        metadata.e_tag = Some("after".into());
        assert_ne!(receipt_version(&metadata).unwrap(), before);
        metadata.e_tag = None;
        metadata.version = Some("versioned-object".into());
        assert!(receipt_version(&metadata).is_some());
    }
}
