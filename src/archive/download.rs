use std::{ops::Range, pin::Pin};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use object_store::{GetOptions, GetRange, ObjectStore};

use super::{ArchiveStore, path::archive_path};
use crate::error::AppError;

/// A bounded-memory object-store read. The stream keeps filesystem and S3
/// responses incremental instead of collecting a generated image or video in
/// the API process.
pub struct ArchiveDownload {
    pub object_size: u64,
    pub range: Range<u64>,
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
}

impl ArchiveStore {
    pub async fn open_stream(
        &self,
        location: &str,
        range: Option<Range<u64>>,
    ) -> Result<ArchiveDownload, AppError> {
        let path = archive_path(location)?;
        let requested_range = range.clone();
        let result = self
            .inner
            .get_opts(
                &path,
                GetOptions {
                    range: range.map(GetRange::Bounded),
                    ..GetOptions::default()
                },
            )
            .await?;
        let object_size = result.meta.size;
        let returned_range = result.range.clone();
        let expected_bytes =
            validate_download_range(object_size, requested_range.as_ref(), &returned_range)?;
        let stream = verified_download_stream(result.into_stream(), expected_bytes);
        Ok(ArchiveDownload {
            object_size,
            range: returned_range,
            stream,
        })
    }
}

pub(super) fn validate_download_range(
    object_size: u64,
    requested: Option<&Range<u64>>,
    returned: &Range<u64>,
) -> Result<u64, AppError> {
    let expected = requested.cloned().unwrap_or(0..object_size);
    if returned.start > returned.end
        || returned.end > object_size
        || expected.start > expected.end
        || expected.end > object_size
        || *returned != expected
    {
        return Err(AppError::Storage(
            "archive download range mismatch".to_owned(),
        ));
    }
    Ok(expected.end - expected.start)
}

pub(super) fn verified_download_stream(
    source: Pin<Box<dyn Stream<Item = object_store::Result<Bytes>> + Send>>,
    expected_bytes: u64,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> {
    let verified = stream::unfold(
        (source, expected_bytes, false),
        |(mut source, mut remaining, done)| async move {
            if done {
                return None;
            }

            if remaining == 0 {
                loop {
                    match source.next().await {
                        Some(Ok(chunk)) if chunk.is_empty() => continue,
                        None => return None,
                        Some(Ok(_)) => {
                            return Some((
                                Err(std::io::Error::other("archive download length mismatch")),
                                (source, 0, true),
                            ));
                        }
                        Some(Err(_)) => {
                            return Some((
                                Err(std::io::Error::other("archive download stream failed")),
                                (source, 0, true),
                            ));
                        }
                    }
                }
            }

            loop {
                match source.next().await {
                    Some(Err(_)) => {
                        return Some((
                            Err(std::io::Error::other("archive download stream failed")),
                            (source, remaining, true),
                        ));
                    }
                    Some(Ok(chunk)) if chunk.is_empty() => continue,
                    Some(Ok(chunk)) => {
                        let chunk_len = match u64::try_from(chunk.len()) {
                            Ok(length) => length,
                            Err(_) => {
                                return Some((
                                    Err(std::io::Error::other("archive download length mismatch")),
                                    (source, remaining, true),
                                ));
                            }
                        };
                        if chunk_len > remaining {
                            return Some((
                                Err(std::io::Error::other("archive download length mismatch")),
                                (source, remaining, true),
                            ));
                        }
                        remaining -= chunk_len;
                        if remaining == 0 {
                            // Read one item ahead before releasing the final
                            // chunk. This catches a backend that advertises the
                            // right range but returns additional bytes.
                            loop {
                                match source.next().await {
                                    Some(Ok(extra)) if extra.is_empty() => continue,
                                    None => return Some((Ok(chunk), (source, 0, true))),
                                    Some(Ok(_)) => {
                                        return Some((
                                            Err(std::io::Error::other(
                                                "archive download length mismatch",
                                            )),
                                            (source, 0, true),
                                        ));
                                    }
                                    Some(Err(_)) => {
                                        return Some((
                                            Err(std::io::Error::other(
                                                "archive download stream failed",
                                            )),
                                            (source, 0, true),
                                        ));
                                    }
                                }
                            }
                        }
                        return Some((Ok(chunk), (source, remaining, false)));
                    }
                    None => {
                        return Some((
                            Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "archive download ended before the advertised range",
                            )),
                            (source, remaining, true),
                        ));
                    }
                }
            }
        },
    );
    Box::pin(verified)
}
