use super::*;
use crate::api::proxy::persistence::{Persistence, WorkspaceRole};

async fn fixture() -> (Arc<BodyCarrier>, Arc<ProxyMemoryBudget>, Arc<Persistence>) {
    let memory = Arc::new(ProxyMemoryBudget::new(256 * 1024));
    let workspace = Arc::new(Persistence::default());
    let key = ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let credit = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
    let mut source = BodyCarrier::capture_owned(
        stream::iter([Ok::<_, std::io::Error>((
            Bytes::from(vec![b'x'; 128 * 1024]),
            Some(credit),
        ))]),
        CarrierCapture::new(memory.clone(), None, key),
    )
    .await
    .unwrap();
    let inner = Arc::get_mut(&mut source).unwrap();
    inner.archive = Some(ArchiveStore::memory_for_test());
    inner.workspace = Some(workspace.clone());
    (source, memory, workspace)
}

#[tokio::test(start_paused = true)]
async fn resource_wait_wakes_immediately_after_source_ram_is_released() {
    let (source, memory, workspace) = fixture().await;
    let outside = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
    let a = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let b = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut waiting = Box::pin(source.reserve_workspace(64 * 1024));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    drop(outside);
    let acquired = match futures_util::poll!(&mut waiting) {
        std::task::Poll::Ready(Ok(lease)) => lease,
        _ => panic!("available source credit must wake without advancing retry time"),
    };
    drop(waiting);
    drop(acquired);
    drop(a);
    drop(b);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}

#[tokio::test(start_paused = true)]
async fn workspace_release_offloads_self_owned_source_without_waiting_for_its_own_ram() {
    let (source, memory, workspace) = fixture().await;
    let outside = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
    let a = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let b = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut waiting = Box::pin(source.reserve_workspace(64 * 1024));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    let before = tokio::time::Instant::now();
    drop(a);
    let lease = waiting.await.unwrap();
    assert!(before.elapsed() < std::time::Duration::from_millis(100));
    assert!(matches!(
        &*source.segments[0].lock().unwrap(),
        Segment::Object(_)
    ));
    assert_eq!(source.len(), 128 * 1024);
    drop(lease);
    drop(outside);
    drop(b);
    let mut stream = source.open_stream();
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        bytes += chunk.len();
        hash.update(&chunk);
    }
    assert_eq!(bytes, source.len() as usize);
    assert_eq!(hash.finalize().to_hex().as_str(), source.digest());
    drop(stream);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}

#[tokio::test(start_paused = true)]
async fn cancelled_resource_race_drops_both_fifo_waiters_and_keeps_original() {
    let (source, memory, workspace) = fixture().await;
    let outside = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
    let a = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let b = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut waiting = Box::pin(source.reserve_workspace(64 * 1024));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    assert!(
        workspace
            .render()
            .contains("archive_workspace_waiting{operation=\"carrier_spill\"} 1")
    );
    drop(waiting);
    assert!(
        workspace
            .render()
            .contains("archive_workspace_waiting{operation=\"carrier_spill\"} 0")
    );
    assert!(matches!(
        &*source.segments[0].lock().unwrap(),
        Segment::Ram(_)
    ));
    drop(outside);
    let free = memory
        .try_reserve_ingress_bytes(128 * 1024)
        .expect("cancelled acquisition cannot retain free credit");
    drop(free);
    drop(a);
    drop(b);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}
