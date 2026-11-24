use axum::body::Bytes;
use futures_util::{Stream, StreamExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MediaBodyReadError {
    Transport,
    TooLarge,
}

/// Collect the exact bytes from the existing response reader. The caller owns
/// content-length checks, archive capture, and protocol-specific settlement.
pub(super) async fn collect_bounded<S, E>(
    stream: S,
    limit: usize,
) -> Result<Bytes, MediaBodyReadError>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    futures_util::pin_mut!(stream);
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| MediaBodyReadError::Transport)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(MediaBodyReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}
