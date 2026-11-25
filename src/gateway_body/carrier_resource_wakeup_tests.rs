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
async fn legacy_resource_retry_stays_asleep_after_source_ram_is_released() {
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
    assert!(
        futures_util::poll!(&mut waiting).is_pending(),
        "old retry still sleeps despite available RAM"
    );
    tokio::time::advance(std::time::Duration::from_millis(100)).await;
    let acquired = waiting.await.unwrap();
    drop(acquired);
    drop(a);
    drop(b);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}
