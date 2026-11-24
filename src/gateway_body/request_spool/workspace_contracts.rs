use super::*;
use crate::{
    archive_staging::*,
    gateway_body::{memory::ProxyMemoryBudget, normalized_encoding},
    response_archive_spool::ResponseArchiveProducer,
};
use futures_util::{StreamExt, stream};
use std::time::Duration;

#[tokio::test]
async fn raw_and_normalized_writers_share_workspace_without_consuming_response_source() {
    let (_dir, state, _pool, identity) = crate::response_archive_spool::tests::fixture().await;
    let raw_key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(identity.request_id),
        ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let normalized_key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(identity.request_id),
        ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let raw_location = format!("{}/body.json", raw_key.canonical_prefix());
    let normalized_location = format!("{}/normalized.json", normalized_key.canonical_prefix());
    let (raw_entered, raw_release, _raw_gate) =
        crate::archive::multipart_test_gate::install(&raw_location);
    let (normalized_entered, normalized_release, _normalized_gate) =
        crate::archive::multipart_test_gate::install(&normalized_location);
    let raw_bytes = Bytes::from_static(b"original request bytes");
    let raw = BodyCarrier::capture(
        stream::iter([Ok::<_, std::io::Error>(raw_bytes.clone())]),
        CarrierCapture::new(
            Arc::new(ProxyMemoryBudget::new(128 * 1024)),
            Some(state.archive.clone()),
            raw_key,
        ),
    )
    .await
    .unwrap();
    let raw_task = {
        let archive = state.archive.clone();
        let upload = state.persistence.clone();
        let location = raw_location.clone();
        tokio::spawn(async move {
            DurableRequestBodies::Archive(archive)
                .put_carrier(&location, &raw, &upload)
                .await
        })
    };
    let normalized = serde_json::json!({"input":"\0".repeat(16 * 1024)});
    let expected_normalized = serde_json::to_vec(&normalized).unwrap();
    let json_memory = Arc::new(ProxyMemoryBudget::new(64 * 1024));
    let previous = json_memory.reservation();
    assert!(previous.try_reserve_json_shape(16 * 1024 + 64, 2));
    let normalized_task = {
        let state = state.clone();
        let memory = json_memory.clone();
        tokio::spawn(async move {
            normalized_encoding::encode(
                normalized,
                previous,
                memory,
                state.archive,
                state.db,
                normalized_key,
                state.persistence,
            )
            .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        raw_entered.await.unwrap();
        normalized_entered.await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(
        state.persistence.stream_memory.snapshot().0,
        0,
        "neither old writer may reserve response source credit"
    );
    assert_eq!(
        state.persistence.upload_workspace.snapshot().0,
        crate::response_archive_spool::RAW_UPLOAD_WORKSPACE_BYTES
    );
    let response_bytes = Bytes::from_static(b"data: complete\n\ndata: [DONE]\n\n");
    let mut response = ResponseArchiveProducer::begin_original(&state, identity)
        .await
        .unwrap();
    assert!(response.try_offer(response_bytes.clone()));
    let response = response.seal().unwrap();
    raw_release.send(()).unwrap();
    normalized_release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), raw_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let normalized_source = tokio::time::timeout(Duration::from_secs(15), normalized_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(state.archive.get(&raw_location).await.unwrap(), raw_bytes);
    let mut replay = normalized_source.open_stream();
    let mut actual_normalized = Vec::new();
    while let Some(chunk) = replay.next().await {
        actual_normalized.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(actual_normalized, expected_normalized);
    drop(replay);
    drop(normalized_source);
    tokio::time::timeout(Duration::from_secs(15), response.wait())
        .await
        .unwrap();
    let reference = state
        .archive
        .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state
            .archive
            .get_tenant_content(identity.tenant_id, &reference, response_bytes.len())
            .await
            .unwrap(),
        response_bytes
    );
    assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
    assert_eq!(json_memory.snapshot().0, 0);
}

#[tokio::test]
async fn normalized_overflow_waits_only_on_workspace_and_cancels_without_leaking_source() {
    let (_dir, state, _pool, identity) = crate::response_archive_spool::tests::fixture().await;
    let occupied = state.persistence.multipart_workspace().await.unwrap();
    let occupied2 = state.persistence.multipart_workspace().await.unwrap();
    let json_memory = Arc::new(ProxyMemoryBudget::new(64 * 1024));
    let previous = json_memory.reservation();
    assert!(previous.try_reserve_json_shape(16 * 1024 + 64, 2));
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(identity.request_id),
        ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let value = serde_json::json!({"input":"\0".repeat(16 * 1024)});
    let task = {
        let state = state.clone();
        let memory = json_memory.clone();
        tokio::spawn(async move {
            normalized_encoding::encode(
                value,
                previous,
                memory,
                state.archive,
                state.db,
                key,
                state.persistence,
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !task.is_finished(),
        "required normalized replay cannot claim success before workspace is available"
    );
    assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("normalizer must be cancelled while workspace is occupied"),
    }
    assert_eq!(json_memory.snapshot().0, 0);
    assert_eq!(
        state.persistence.upload_workspace.snapshot().0,
        crate::response_archive_spool::RAW_UPLOAD_WORKSPACE_BYTES
    );
    drop(occupied);
    drop(occupied2);
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
    let previous = json_memory.reservation();
    assert!(previous.try_reserve_json_shape(16 * 1024 + 64, 2));
    let value = serde_json::json!({"input":"\0".repeat(16 * 1024)});
    let expected = serde_json::to_vec(&value).unwrap();
    let recovered = tokio::time::timeout(
        Duration::from_secs(15),
        normalized_encoding::encode(
            value,
            previous,
            json_memory.clone(),
            state.archive.clone(),
            state.db.clone(),
            key,
            state.persistence.clone(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let mut replay = recovered.open_stream();
    let mut actual = Vec::new();
    while let Some(chunk) = replay.next().await {
        actual.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(actual, expected);
    drop(replay);
    drop(recovered);
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
    assert_eq!(json_memory.snapshot().0, 0);
}
