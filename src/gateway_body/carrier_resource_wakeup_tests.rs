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

async fn real_fixture() -> (Arc<BodyCarrier>, Arc<ProxyMemoryBudget>, Arc<Persistence>) {
    let mut config = crate::config::Config::for_test("sqlite::memory:".into());
    config.archive_backend = crate::config::ArchiveBackend::S3;
    config.s3_endpoint = Some(std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap());
    config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
    config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    config.s3_allow_http = true;
    let (mut source, memory, workspace) = fixture().await;
    Arc::get_mut(&mut source).unwrap().archive =
        Some(ArchiveStore::from_config(&config).await.unwrap());
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

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn workspace_release_offloads_self_owned_source_without_waiting_for_its_own_ram() {
    let (source, memory, workspace) = real_fixture().await;
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
    drop(a);
    let lease = tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
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

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn derived_source_can_spill_its_parent_after_workspace_wakes() {
    let (source, memory, workspace) = real_fixture().await;
    let child = BodyCarrier::capture_owned(
        stream::empty::<Result<(Bytes, Option<OwnedSemaphorePermit>), std::io::Error>>(),
        CarrierCapture::derived_from(&source, source.staging),
    )
    .await
    .unwrap();
    let outside = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
    let a = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let b = workspace
        .multipart_workspace(WorkspaceRole::RequestRaw)
        .await
        .unwrap();
    let mut waiting = Box::pin(child.reserve_workspace(64 * 1024));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    drop(a);
    let lease = waiting.await.unwrap();
    assert!(matches!(
        &*source.segments[0].lock().unwrap(),
        Segment::Object(_)
    ));
    drop(lease);
    drop(outside);
    drop(b);
    drop(child);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}

#[path = "../../tests/support/recovery_object_proxy.rs"]
mod recovery_proxy;

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn actual_put_is_not_cancelled_when_ram_wakes_and_lost_ack_keeps_source() {
    use recovery_proxy::RecoveryObjectProxy;
    for lose_ack in [false, true] {
        let proxy =
            RecoveryObjectProxy::start(&std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap()).await;
        let mut config = crate::config::Config::for_test("sqlite::memory:".into());
        config.archive_backend = crate::config::ArchiveBackend::S3;
        config.s3_endpoint = Some(proxy.endpoint().into());
        config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
        config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
        config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
        config.s3_allow_http = true;
        let (mut source, memory, workspace) = fixture().await;
        Arc::get_mut(&mut source).unwrap().archive =
            Some(ArchiveStore::from_config(&config).await.unwrap());
        let outside = memory.try_reserve_ingress_bytes(128 * 1024).unwrap();
        let a = workspace
            .multipart_workspace(WorkspaceRole::RequestRaw)
            .await
            .unwrap();
        let b = workspace
            .multipart_workspace(WorkspaceRole::RequestRaw)
            .await
            .unwrap();
        let put = proxy.gate_body_put();
        proxy.lose_body_put_ack(lose_ack);
        let waiter = tokio::spawn({
            let source = source.clone();
            async move { source.reserve_workspace(64 * 1024).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !workspace
                .render()
                .contains("archive_workspace_waiting{operation=\"carrier_spill\"} 1")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(a);
        put.entered().await;
        drop(outside);
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "RAM availability cannot cancel a started PUT"
        );
        assert_eq!(proxy.counts().get("body_put"), Some(&1));
        put.release();
        let lease = tokio::time::timeout(std::time::Duration::from_secs(30), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if lose_ack {
            assert!(matches!(
                &*source.segments[0].lock().unwrap(),
                Segment::Ram(_)
            ));
            assert!(
                source
                    .owned_objects
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, settled)| !settled)
            );
        } else {
            assert!(matches!(
                &*source.segments[0].lock().unwrap(),
                Segment::Object(_)
            ));
            assert_eq!(proxy.counts().get("body_put"), Some(&1));
        }
        drop(lease);
        drop(b);
        proxy.lose_body_put_ack(false);
        let mut replay = source.open_stream();
        let mut hash = blake3::Hasher::new();
        let mut count = 0;
        while let Some(chunk) = replay.next().await {
            let chunk = chunk.unwrap();
            count += chunk.len();
            hash.update(&chunk);
        }
        assert_eq!(count, source.len() as usize);
        assert_eq!(hash.finalize().to_hex().as_str(), source.digest());
        drop(replay);
        drop(source);
        assert_eq!(memory.snapshot().0, 0);
        assert_eq!(workspace.upload_workspace.snapshot().0, 0);
    }
}

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn credited_long_response_preserves_opportunistic_spill_above_source_capacity() {
    let (template, _, workspace) = real_fixture().await;
    let archive = template.archive.clone().unwrap();
    let staging = template.staging;
    drop(template);
    let memory = Arc::new(ProxyMemoryBudget::new_archive_source(4 * 1024 * 1024));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let input = stream::iter(0..80).map({
        let memory = memory.clone();
        let peak = peak.clone();
        move |index| {
            let credit = memory
                .try_reserve_ingress_bytes(64 * 1024)
                .expect("paced credited stream must reclaim prior source allocations");
            peak.fetch_max(memory.snapshot().0, std::sync::atomic::Ordering::Relaxed);
            Ok::<_, std::io::Error>((Bytes::from(vec![index as u8; 64 * 1024]), Some(credit)))
        }
    });
    let source = BodyCarrier::capture_owned(
        input,
        CarrierCapture::new(memory.clone(), Some(archive), staging)
            .with_upload_workspace(workspace.clone()),
    )
    .await
    .unwrap();
    assert_eq!(source.len(), 5 * 1024 * 1024);
    assert!(peak.load(std::sync::atomic::Ordering::Relaxed) <= 4 * 1024 * 1024);
    assert!(
        source
            .segments
            .iter()
            .any(|s| matches!(&*s.lock().unwrap(), Segment::Object(_)))
    );
    let mut replay = source.open_stream();
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0;
    while let Some(chunk) = replay.next().await {
        let chunk = chunk.unwrap();
        bytes += chunk.len();
        hash.update(&chunk);
    }
    assert_eq!(bytes, 5 * 1024 * 1024);
    assert_eq!(hash.finalize().to_hex().as_str(), source.digest());
    drop(replay);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
    assert_eq!(workspace.upload_workspace.snapshot().0, 0);
}

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn request_capture_eof_does_not_join_owned_put_when_retained_credit_wakes() {
    use recovery_proxy::RecoveryObjectProxy;
    for (lose_ack, cancel_capture) in [(false, false), (true, false), (false, true)] {
        let proxy =
            RecoveryObjectProxy::start(&std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap()).await;
        let mut config = crate::config::Config::for_test("sqlite::memory:".into());
        config.archive_backend = crate::config::ArchiveBackend::S3;
        config.s3_endpoint = Some(proxy.endpoint().into());
        config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
        config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
        config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
        config.s3_allow_http = true;
        let archive = ArchiveStore::from_config(&config).await.unwrap();
        let archive_probe = archive.clone();
        let memory = Arc::new(ProxyMemoryBudget::new(512 * 1024));
        let persistence = Arc::new(Persistence::default());
        let outside = memory.try_reserve_carrier_ingress(64 * 1024).unwrap();
        let staging = ArchiveStagingKey::new(
            crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
            crate::archive_staging::ArchiveStagingPurpose::Request,
            uuid::Uuid::now_v7(),
        )
        .unwrap();
        let expected = [vec![b'a'; 64 * 1024], vec![b'b'; 64 * 1024]].concat();
        let put = proxy.gate_body_put();
        proxy.lose_body_put_ack(lose_ack);
        let capture = tokio::spawn({
            let memory = memory.clone();
            let persistence = persistence.clone();
            async move {
                BodyCarrier::capture_owned(
                    stream::iter([
                        Ok::<_, std::io::Error>((Bytes::from(vec![b'a'; 64 * 1024]), None)),
                        Ok((Bytes::from(vec![b'b'; 64 * 1024]), None)),
                    ]),
                    CarrierCapture::new(memory, Some(archive), staging)
                        .with_background_spill(persistence),
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), put.entered())
            .await
            .unwrap();
        assert!(!capture.is_finished());
        assert_eq!(persistence.pending_archives_for_test(), 1);
        if cancel_capture {
            capture.abort();
            assert!(matches!(capture.await, Err(error) if error.is_cancelled()));
            assert_eq!(persistence.pending_archives_for_test(), 1);
            assert_eq!(proxy.counts().get("body_put"), Some(&1));
            drop(outside);
            put.release();
            persistence.drain_for_test().await;
            assert_eq!(memory.snapshot().0, 0);
            assert_eq!(persistence.pending_archives_for_test(), 0);
            assert_eq!(proxy.counts().get("body_put"), Some(&1));
            assert!(
                archive_probe
                    .archive_staging_segment_is_empty(staging)
                    .await
                    .unwrap()
            );
            continue;
        }
        drop(outside);
        let source = tokio::time::timeout(std::time::Duration::from_secs(5), capture)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(proxy.counts().get("body_put"), Some(&1));
        assert_eq!(persistence.pending_archives_for_test(), 1);
        assert!(matches!(
            &*source.segments[0].lock().unwrap(),
            Segment::Ram(_)
        ));
        let mut replay = source.open_stream();
        let mut actual = Vec::new();
        while let Some(chunk) = replay.next().await {
            actual.extend_from_slice(&chunk.unwrap());
        }
        drop(replay);
        assert_eq!(
            actual, expected,
            "forwardable original must precede PUT ACK"
        );
        assert_eq!(source.digest(), blake3::hash(&expected).to_hex().as_str());
        assert!(memory.snapshot().0 <= 512 * 1024);
        // This production cross-segment reader needs its own accounted output.
        // While the same PUT owns offload, newly freed RAM must still wake it.
        let occupied = memory.try_reserve_ingress_bytes(384 * 1024).unwrap();
        let mut cross_segment = Box::pin(source.read_leased_chunk(64 * 1024 - 16, 32));
        assert!(futures_util::poll!(&mut cross_segment).is_pending());
        drop(occupied);
        let cross_segment = tokio::time::timeout(std::time::Duration::from_secs(5), cross_segment)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cross_segment.as_ref(),
            &expected[64 * 1024 - 16..64 * 1024 + 16]
        );
        drop(cross_segment);
        assert_eq!(proxy.counts().get("body_put"), Some(&1));
        assert_eq!(persistence.pending_archives_for_test(), 1);
        put.release();
        persistence.drain_for_test().await;
        assert_eq!(persistence.pending_archives_for_test(), 0);
        if lose_ack {
            assert!(matches!(
                &*source.segments[0].lock().unwrap(),
                Segment::Ram(_)
            ));
            assert!(
                source
                    .owned_objects
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, settled)| !settled)
            );
            let owner = source.owned_objects.lock().unwrap()[0].0.clone();
            let encoded = serde_json::to_vec(&owner).unwrap();
            let receipt_id = blake3::hash(&encoded).to_hex();
            assert_eq!(
                archive_probe
                    .get(&format!("carrier-owners/v1/{receipt_id}.json"))
                    .await
                    .unwrap()
                    .as_ref(),
                encoded.as_slice()
            );
            assert!(
                archive_probe
                    .get(&format!("carrier-releases/v1/{receipt_id}.json"))
                    .await
                    .is_err()
            );
        } else {
            assert!(matches!(
                &*source.segments[0].lock().unwrap(),
                Segment::Object(_)
            ));
        }
        proxy.lose_body_put_ack(false);
        let mut replay = source.open_stream();
        let mut restored = Vec::new();
        while let Some(chunk) = replay.next().await {
            restored.extend_from_slice(&chunk.unwrap());
        }
        drop(replay);
        assert_eq!(restored, expected);
        drop(source);
        assert_eq!(memory.snapshot().0, 0);
        assert_eq!(persistence.upload_workspace.snapshot().0, 0);
    }
}

#[tokio::test(start_paused = true)]
async fn first_request_segment_waits_for_retained_credit_despite_free_global_ram() {
    let memory = Arc::new(ProxyMemoryBudget::new(512 * 1024));
    let persistence = Arc::new(Persistence::default());
    let outside = memory.try_reserve_carrier_ingress(128 * 1024).unwrap();
    let staging = ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let mut capture = Box::pin(BodyCarrier::capture_owned(
        stream::iter([Ok::<_, std::io::Error>((
            Bytes::from_static(b"first"),
            None,
        ))]),
        CarrierCapture::new(
            memory.clone(),
            Some(ArchiveStore::memory_for_test()),
            staging,
        )
        .with_background_spill(persistence.clone()),
    ));
    assert!(futures_util::poll!(&mut capture).is_pending());
    assert_eq!(memory.snapshot().0, 128 * 1024);
    assert_eq!(persistence.pending_archives_for_test(), 0);
    drop(outside);
    let source = capture.await.unwrap();
    assert_eq!(source.len(), 5);
    assert_eq!(source.digest(), blake3::hash(b"first").to_hex().as_str());
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
}

#[tokio::test(start_paused = true)]
async fn request_without_archive_preserves_capture_above_retained_window() {
    let memory = Arc::new(ProxyMemoryBudget::new(512 * 1024));
    let persistence = Arc::new(Persistence::default());
    let staging = ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let expected = vec![b'n'; 192 * 1024];
    let source = BodyCarrier::capture_owned(
        stream::iter([Ok::<_, std::io::Error>((
            Bytes::from(expected.clone()),
            None,
        ))]),
        CarrierCapture::new(memory.clone(), None, staging)
            .with_background_spill(persistence.clone()),
    )
    .await
    .unwrap();
    assert_eq!(source.len(), expected.len() as u64);
    let mut input = source.open_stream();
    let mut actual = Vec::new();
    while let Some(chunk) = input.next().await {
        actual.extend_from_slice(&chunk.unwrap());
    }
    drop(input);
    assert_eq!(actual, expected);
    assert_eq!(source.digest(), blake3::hash(&expected).to_hex().as_str());
    assert_eq!(persistence.pending_archives_for_test(), 0);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
}

#[tokio::test]
async fn sub_quantum_retained_window_preserves_existing_archive_spill() {
    let memory = Arc::new(ProxyMemoryBudget::new(128 * 1024));
    assert_eq!(memory.snapshot().3, 0);
    let persistence = Arc::new(Persistence::default());
    let staging = ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        uuid::Uuid::now_v7(),
    )
    .unwrap();
    let expected = vec![b's'; 192 * 1024];
    let source = BodyCarrier::capture_owned(
        stream::iter([Ok::<_, std::io::Error>((
            Bytes::from(expected.clone()),
            None,
        ))]),
        CarrierCapture::new(
            memory.clone(),
            Some(ArchiveStore::single_put_memory_transport_for_test()),
            staging,
        )
        .with_background_spill(persistence.clone()),
    )
    .await
    .unwrap();
    let mut input = source.open_stream();
    let mut actual = Vec::new();
    while let Some(chunk) = input.next().await {
        actual.extend_from_slice(&chunk.unwrap());
    }
    drop(input);
    assert_eq!(actual, expected);
    assert_eq!(source.digest(), blake3::hash(&expected).to_hex().as_str());
    assert_eq!(persistence.pending_archives_for_test(), 0);
    drop(source);
    assert_eq!(memory.snapshot().0, 0);
}
