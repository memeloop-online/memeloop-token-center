//! Process-owned multipart staging. This is not cross-pod durable storage.
use super::*;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub(crate) async fn capture(
    request: Request,
    directory: &std::path::Path,
    budget: &memory::ProxyMemoryBudget,
) -> Result<Request, GatewayBodyAdmissionError> {
    let reservation = budget
        .streaming_working_set(256 * 1024)
        .await
        .map_err(|_| GatewayBodyAdmissionError::CapacityExhausted)?;
    let unavailable = || GatewayBodyAdmissionError::RequestSpoolUnavailable;
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|_| unavailable())?;
    let directory = directory.to_owned();
    let file = tokio::task::spawn_blocking(move || tempfile::tempfile_in(directory))
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
    let mut file = tokio::fs::File::from_std(file);
    let declared = declared_content_length(request.headers());
    let (mut parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            rejected_body(
                GatewayBodyRouteClass::Other,
                declared,
                usize::MAX,
                GatewayBodyRejectionReason::BodyReadRejected,
            )
        })?;
        file.write_all(&chunk).await.map_err(|_| unavailable())?;
    }
    file.flush().await.map_err(|_| unavailable())?;
    file.rewind().await.map_err(|_| unavailable())?;
    // File and memory ownership follow the request body, including cancellation.
    let owner = reservation.clone();
    let stream = futures_util::stream::try_unfold((file, owner), |(mut file, owner)| async move {
        let mut bytes = vec![0_u8; 64 * 1024];
        let read = file.read(&mut bytes).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        bytes.truncate(read);
        Ok(Some((bytes::Bytes::from(bytes), (file, owner))))
    });
    parts.extensions.insert(reservation);
    Ok(Request::from_parts(parts, Body::from_stream(stream)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multipart_larger_than_hot_budget_is_replayed_exactly_with_fixed_memory() {
        let directory = tempfile::tempdir().unwrap();
        let budget = memory::ProxyMemoryBudget::new(256 * 1024);
        let chunks = futures_util::stream::iter(
            (0..80).map(|n| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![n; 64 * 1024]))),
        );
        let request = Request::post("/v1/audio/transcriptions")
            .header(header::CONTENT_LENGTH, 80 * 64 * 1024)
            .body(Body::from_stream(chunks))
            .unwrap();
        let request = capture(request, directory.path(), &budget).await.unwrap();
        assert_eq!(budget.snapshot().0, 256 * 1024);
        let mut stream = request.into_body().into_data_stream();
        let mut offset = 0;
        while let Some(chunk) = stream.next().await {
            for value in chunk.unwrap() {
                assert_eq!(value, (offset / (64 * 1024)) as u8);
                offset += 1;
            }
        }
        drop(stream);
        assert_eq!(offset, 80 * 64 * 1024);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn dropping_replay_releases_file_and_budget() {
        let directory = tempfile::tempdir().unwrap();
        let budget = memory::ProxyMemoryBudget::new(256 * 1024);
        let request = Request::post("/v1/audio/transcriptions")
            .body(Body::from("audio-owner"))
            .unwrap();
        let request = capture(request, directory.path(), &budget).await.unwrap();
        drop(request);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
