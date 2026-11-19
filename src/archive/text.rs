use std::ops::Range;

use bytes::Bytes;
use futures_util::StreamExt;
use uuid::Uuid;

use super::{ArchiveDownload, ArchiveStore, CasArchiveObject};
use crate::error::AppError;

impl ArchiveStore {
    pub async fn open_tenant_content(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
    ) -> Result<ArchiveDownload, AppError> {
        object.validate_tenant(tenant_id)?;
        let download = self.open_stream(&object.object_locator, None).await?;
        if download.object_size != object.size_bytes {
            return Err(AppError::storage("archive_cas_object_integrity_failure"));
        }
        let expected = object.blake3_digest.clone();
        let stream = futures_util::stream::try_unfold(
            (download.stream, blake3::Hasher::new(), false),
            move |(mut source, mut digest, finished)| {
                let expected = expected.clone();
                async move {
                    if finished {
                        return Ok(None);
                    }
                    match source.next().await {
                        Some(chunk) => {
                            let chunk = chunk?;
                            digest.update(&chunk);
                            Ok(Some((chunk, (source, digest, false))))
                        }
                        None => {
                            if digest.finalize().to_hex().as_str() != expected {
                                return Err(std::io::Error::other("archive CAS digest mismatch"));
                            }
                            Ok(Some((Bytes::new(), (source, digest, true))))
                        }
                    }
                }
            },
        );
        Ok(ArchiveDownload {
            object_size: download.object_size,
            range: download.range,
            stream: Box::pin(stream),
        })
    }

    pub async fn read_tenant_text_range(
        &self,
        tenant_id: Uuid,
        object: &CasArchiveObject,
        range: Range<u64>,
    ) -> Result<(String, u64), AppError> {
        if range.start > range.end {
            return Err(AppError::BadRequest(
                "invalid conversation text range".into(),
            ));
        }
        let mut stream = self.open_tenant_content(tenant_id, object).await?.stream;
        let mut pending = Vec::new();
        let mut result = String::new();
        let mut chars = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| AppError::storage("conversation_body_integrity_failure"))?;
            for part in chunk.chunks(64 * 1024) {
                pending.extend_from_slice(part);
                let valid = match std::str::from_utf8(&pending) {
                    Ok(text) => text.len(),
                    Err(error) if error.error_len().is_none() => error.valid_up_to(),
                    Err(_) => return Err(AppError::storage("conversation_body_encoding_invalid")),
                };
                let text = std::str::from_utf8(&pending[..valid])
                    .map_err(|_| AppError::storage("conversation_body_encoding_invalid"))?;
                for character in text.chars() {
                    if range.contains(&chars) {
                        result.push(character);
                    }
                    chars = chars.checked_add(1).ok_or_else(|| {
                        AppError::storage("conversation_body_character_count_invalid")
                    })?;
                }
                pending.drain(..valid);
            }
        }
        if !pending.is_empty() {
            return Err(AppError::storage("conversation_body_encoding_invalid"));
        }
        Ok((result, chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::tests::memory_store;

    #[tokio::test]
    async fn text_ranges_are_unicode_exact_and_verify_the_unreturned_tail() {
        let store = memory_store();
        let tenant = Uuid::now_v7();
        let text = format!("{}工具{}", "a".repeat(65535), "😀".repeat(100000));
        let object = store
            .put_tenant_content(tenant, Bytes::from(text.clone()))
            .await
            .unwrap();
        let (slice, chars) = store
            .read_tenant_text_range(tenant, &object, 65535..65539)
            .await
            .unwrap();
        assert_eq!(slice, "工具😀😀");
        assert_eq!(chars, text.chars().count() as u64);
        let mut incorrect = object.clone();
        incorrect.size_bytes += 1;
        assert!(
            store
                .read_tenant_text_range(tenant, &incorrect, 0..1)
                .await
                .is_err()
        );
        assert!(
            store
                .open_tenant_content(Uuid::now_v7(), &object)
                .await
                .is_err()
        );
    }
}
