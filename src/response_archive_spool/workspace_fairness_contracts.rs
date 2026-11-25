//! Deterministic queue contracts: gates are semaphore ownership, never sleeps.
use super::*;
use crate::api::proxy::persistence::{Persistence, WorkspaceRole};

#[tokio::test]
async fn legacy_try_workspace_retries_stay_behind_existing_raw_waiters() {
    let persistence = Persistence::default();
    let held = persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let held_other = persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut front = Box::pin(persistence.multipart_workspace(WorkspaceRole::RequestRaw));
    assert!(futures_util::poll!(&mut front).is_pending());
    // This is the old response-stage/offload attempt: it never joins FIFO.
    assert!(
        persistence
            .try_multipart_workspace(WorkspaceRole::ResponseStage)
            .is_err()
    );
    drop(held);
    let mut current = front.await.unwrap();
    for _ in 0..3 {
        let mut next_raw = Box::pin(persistence.multipart_workspace(WorkspaceRole::RequestRaw));
        assert!(futures_util::poll!(&mut next_raw).is_pending());
        assert!(
            persistence
                .try_multipart_workspace(WorkspaceRole::ResponseStage)
                .is_err()
        );
        drop(current);
        current = next_raw.await.unwrap();
        assert!(
            persistence
                .try_multipart_workspace(WorkspaceRole::ResponseStage)
                .is_err()
        );
    }
    assert!(
        persistence
            .render()
            .contains("archive_workspace_waiting{operation=\"response_stage\"} 0")
    );
    drop(current);
    drop(held_other);
    assert_eq!(persistence.upload_workspace.snapshot().0, 0);
}

#[tokio::test]
async fn response_upload_joins_fifo_between_raw_writers_and_releases_cancelled_waits() {
    let (_directory, state, _pool, _) = crate::response_archive_spool::tests::fixture().await;
    let held = state
        .persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let other = state
        .persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut front = Box::pin(
        state
            .persistence
            .multipart_workspace(WorkspaceRole::RequestRaw),
    );
    assert!(futures_util::poll!(&mut front).is_pending());
    let mut response = Box::pin(response_upload_workspace(&state));
    assert!(futures_util::poll!(&mut response).is_pending());
    let mut after = Box::pin(
        state
            .persistence
            .multipart_workspace(WorkspaceRole::RequestRaw),
    );
    assert!(futures_util::poll!(&mut after).is_pending());
    assert_eq!(
        state.proxy_archive_stream_permits.available_permits(),
        crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
    );
    drop(held);
    let front = front.await.unwrap();
    assert!(futures_util::poll!(&mut response).is_pending());
    drop(front);
    let response = response.await.unwrap();
    assert!(futures_util::poll!(&mut after).is_pending());
    drop(response);
    drop(after.await.unwrap());
    drop(other);
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
    let upload_slots = state
        .proxy_archive_stream_permits
        .clone()
        .acquire_many_owned(crate::PROXY_ARCHIVE_STREAM_CONCURRENCY as u32)
        .await
        .unwrap();
    let mut unavailable_upload = Box::pin(response_upload_workspace(&state));
    assert!(futures_util::poll!(&mut unavailable_upload).is_pending());
    assert_eq!(
        state.persistence.upload_workspace.snapshot().0,
        0,
        "no workspace held while waiting for upload"
    );
    let raw = state
        .persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    drop(raw);
    drop(unavailable_upload);
    drop(upload_slots);
    assert_eq!(
        state.proxy_archive_stream_permits.available_permits(),
        crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
    );
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
}

#[tokio::test]
async fn complete_response_waits_in_fifo_then_publishes_exact_original_without_client_budget() {
    let (_directory, state, _pool, identity) =
        crate::response_archive_spool::tests::fixture().await;
    let held = state
        .persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let other = state
        .persistence
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut front = Box::pin(
        state
            .persistence
            .multipart_workspace(WorkspaceRole::RequestRaw),
    );
    assert!(futures_util::poll!(&mut front).is_pending());
    let mut producer = ResponseArchiveProducer::begin_original(&state, identity)
        .await
        .unwrap();
    let expected = Bytes::from(vec![b'p'; 512 * 1024]);
    for chunk in expected.chunks(4096) {
        assert!(producer.try_offer(Bytes::copy_from_slice(chunk)));
    }
    let settlement = producer.seal().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !state
            .persistence
            .render()
            .contains("archive_workspace_waiting{operation=\"response_stage\"} 1")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("complete response must enter FIFO rather than a silent try/spill loop");
    assert!(state.persistence.stream_memory.snapshot().0 > 0);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
    let mut after = Box::pin(
        state
            .persistence
            .multipart_workspace(WorkspaceRole::RequestRaw),
    );
    assert!(futures_util::poll!(&mut after).is_pending());
    drop(held);
    drop(front.await.unwrap());
    // The remaining occupied envelope stays held: only the single released
    // envelope can service response and the later raw writer in FIFO order.
    let after = tokio::time::timeout(std::time::Duration::from_secs(5), after)
        .await
        .unwrap()
        .unwrap();
    drop(after);
    drop(other);
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        settlement.wait_result_for_test(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let reference = state
        .archive
        .raw_capture_reference(identity.tenant_id, identity.request_id, "response")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reference.size_bytes, expected.len() as u64);
    assert_eq!(
        reference.blake3_digest,
        blake3::hash(&expected).to_hex().to_string()
    );
    assert_eq!(
        state
            .archive
            .get_tenant_content(identity.tenant_id, &reference, expected.len())
            .await
            .unwrap(),
        expected
    );
    assert!(
        !state
            .archive
            .raw_capture_failed(identity.tenant_id, identity.request_id, "response")
            .await
            .unwrap()
    );
    assert_eq!(state.persistence.stream_memory.snapshot().0, 0);
    assert_eq!(state.persistence.upload_workspace.snapshot().0, 0);
    assert_eq!(
        state.proxy_archive_stream_permits.available_permits(),
        crate::PROXY_ARCHIVE_STREAM_CONCURRENCY
    );
}
