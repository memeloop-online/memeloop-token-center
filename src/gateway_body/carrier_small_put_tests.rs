//! Real S3 operation/identity contracts; only synthetic fixture bytes are used.
use super::*;
use crate::archive_staging::{ArchiveStagingOwner, ArchiveStagingPurpose};
#[path = "../../tests/support/recovery_object_proxy.rs"]
mod recovery_proxy;
use recovery_proxy::RecoveryObjectProxy;

fn key() -> ArchiveStagingKey {
    ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(uuid::Uuid::now_v7()),
        ArchiveStagingPurpose::Response,
        uuid::Uuid::now_v7(),
    )
    .unwrap()
}
async fn config(proxy: &RecoveryObjectProxy) -> crate::config::Config {
    let mut c = crate::config::Config::for_test("sqlite::memory:".into());
    c.archive_backend = crate::config::ArchiveBackend::S3;
    c.s3_endpoint = Some(proxy.endpoint().into());
    c.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
    c.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    c.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    c.s3_allow_http = true;
    c
}
async fn source(
    archive: &ArchiveStore,
    size: usize,
    upload: Arc<crate::api::proxy::persistence::Persistence>,
    staging: ArchiveStagingKey,
) -> (Arc<BodyCarrier>, Arc<ProxyMemoryBudget>) {
    let memory = Arc::new(ProxyMemoryBudget::new(size + 65536));
    let mut carrier = BodyCarrier::capture(
        stream::iter([Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; size]))]),
        CarrierCapture::new(memory.clone(), None, staging),
    )
    .await
    .unwrap();
    let inner = Arc::get_mut(&mut carrier).unwrap();
    inner.archive = Some(archive.clone());
    inner.workspace = Some(upload);
    (carrier, memory)
}
#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn real_s3_small_spill_uses_one_put_and_large_boundary_keeps_multipart() {
    let endpoint = std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap();
    let proxy = RecoveryObjectProxy::start(&endpoint).await;
    let archive = ArchiveStore::from_config(&config(&proxy).await)
        .await
        .unwrap();
    let upload = Arc::new(crate::api::proxy::persistence::Persistence::default());
    for size in [
        1,
        65536,
        crate::archive::ARCHIVE_MULTIPART_PART_BYTES,
        crate::archive::ARCHIVE_MULTIPART_PART_BYTES + 1,
    ] {
        let before = proxy.counts();
        let (carrier, memory) = source(&archive, size, upload.clone(), key()).await;
        assert!(carrier.offload_one().await.unwrap());
        assert_eq!(memory.snapshot().0, 0);
        let after = proxy.counts();
        let delta =
            |k: &str| after.get(k).copied().unwrap_or(0) - before.get(k).copied().unwrap_or(0);
        if size <= crate::archive::ARCHIVE_MULTIPART_PART_BYTES {
            assert_eq!(delta("body_put"), 1);
            assert_eq!(delta("multipart_create"), 0);
        } else {
            assert_eq!(delta("body_put"), 0);
            assert_eq!(delta("multipart_create"), 1);
            assert_eq!(delta("multipart_complete"), 1);
        }
        let guard = upload
            .multipart_workspace(crate::api::proxy::persistence::WorkspaceRole::RequestRaw)
            .await
            .unwrap();
        let mut stream = carrier.open_stream_with_workspace(guard);
        let mut hasher = blake3::Hasher::new();
        let mut count = 0;
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.unwrap();
            hasher.update(&bytes);
            count += bytes.len();
        }
        assert_eq!(count, size);
        assert_eq!(hasher.finalize().to_hex().as_str(), carrier.digest());
        drop(stream);
        drop(carrier);
        assert_eq!(upload.upload_workspace.snapshot().0, 0);
    }
    eprintln!(
        "synthetic_s3_operations {:?}; request_latency_us {:?}",
        proxy.counts(),
        proxy.timings()
    );
}

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn real_s3_small_spill_lost_ack_retains_source_and_routes_both_backends() {
    let endpoint = std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap();
    for training in [false, true] {
        let proxy = RecoveryObjectProxy::start(&endpoint).await;
        let mut c = config(&proxy).await;
        if training {
            c.training_s3 = Some(crate::config::TrainingS3Config {
                endpoint: proxy.endpoint().into(),
                bucket: c.s3_bucket.clone().unwrap(),
                region: "us-east-1".into(),
                access_key: c.s3_access_key.clone().unwrap(),
                secret_key: c.s3_secret_key.clone().unwrap(),
                allow_http: true,
            });
            c.training_staging_writes = true;
            c.training_cas_writes = true;
        }
        let archive = ArchiveStore::from_config(&c).await.unwrap();
        let upload = Arc::new(crate::api::proxy::persistence::Persistence::default());
        let staging = archive.select_staging_key(key()).unwrap();
        let (carrier, memory) = source(&archive, 4096, upload.clone(), staging).await;
        proxy.lose_body_put_ack(true);
        assert!(carrier.offload_one().await.is_err());
        assert!(
            memory.snapshot().0 >= 4096,
            "lost ACK must retain original allocation"
        );
        assert!(matches!(
            &*carrier.segments[0].lock().unwrap(),
            Segment::Ram(_)
        ));
        assert!(
            carrier
                .owned_objects
                .lock()
                .unwrap()
                .iter()
                .all(|(_, settled)| !settled)
        );
        assert_eq!(upload.upload_workspace.snapshot().0, 0);
        let bytes = carrier.read_range(0, 4096).await.unwrap();
        assert_eq!(bytes, Bytes::from(vec![b'x'; 4096]));
        drop(bytes);
        proxy.lose_body_put_ack(false);
        assert!(carrier.offload_one().await.unwrap());
        assert_eq!(memory.snapshot().0, 0);
        let reference = match &*carrier.segments[0].lock().unwrap() {
            Segment::Object(r) => r.clone(),
            _ => panic!("confirmed PUT must replace segment"),
        };
        assert_eq!(
            reference.object_locator.starts_with("stores/staging/"),
            training
        );
        let tenant = uuid::Uuid::now_v7();
        let request = staging.owner.id();
        let object = archive
            .promote_staged_text_to_cas(tenant, &reference)
            .await
            .unwrap();
        archive
            .publish_raw_capture(tenant, request, "response", &object)
            .await
            .unwrap();
        let root = archive
            .raw_capture_reference(tenant, request, "response")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            archive
                .get_tenant_content(tenant, &root, 4096)
                .await
                .unwrap(),
            Bytes::from(vec![b'x'; 4096])
        );
        drop(carrier);
        assert_eq!(upload.upload_workspace.snapshot().0, 0);
    }
}

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn real_s3_small_spill_compares_previous_multipart_at_same_distribution_and_concurrency() {
    let endpoint = std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap();
    for previous in [true, false] {
        let proxy = RecoveryObjectProxy::start(&endpoint).await;
        let archive = ArchiveStore::from_config(&config(&proxy).await)
            .await
            .unwrap();
        let upload = Arc::new(crate::api::proxy::persistence::Persistence::default());
        let sizes: Vec<usize> = [1024, 4096, 16384, 65536]
            .into_iter()
            .cycle()
            .take(16)
            .collect();
        let total: usize = sizes.iter().sum();
        let started = std::time::Instant::now();
        let samples: Vec<u64> = stream::iter(sizes)
            .map(|size| {
                let archive = archive.clone();
                let upload = upload.clone();
                async move {
                    let (carrier, memory) = source(&archive, size, upload.clone(), key()).await;
                    let began = std::time::Instant::now();
                    if previous {
                        // Exact prior offload sequence, same receipt and envelope.
                        let workspace = upload
                            .multipart_workspace(
                                crate::api::proxy::persistence::WorkspaceRole::CarrierSpill,
                            )
                            .await
                            .unwrap();
                        let bytes = match &*carrier.segments[0].lock().unwrap() {
                            Segment::Ram(b) => b.clone(),
                            _ => unreachable!(),
                        };
                        let locator = format!(
                            "{}/carrier-{}",
                            carrier.staging.canonical_prefix(),
                            uuid::Uuid::now_v7()
                        );
                        let owner =
                            CarrierObjectOwner::new(carrier.staging, locator.clone(), &bytes)
                                .unwrap();
                        archive.begin_carrier_object(&owner).await.unwrap();
                        let mut writer = archive.start_writer(&locator).await.unwrap();
                        writer.retain_workspace(workspace);
                        writer.write(bytes.clone()).await.unwrap();
                        let staged = writer.finish_staged().await.unwrap();
                        assert_eq!(staged.size_bytes, size as u64);
                        assert_eq!(staged.blake3_digest, carrier.digest());
                        assert_eq!(
                            archive
                                .get_bounded(&staged.object_locator, size)
                                .await
                                .unwrap(),
                            bytes
                        );
                        drop(bytes);
                        drop(carrier);
                    } else {
                        assert!(carrier.offload_one().await.unwrap());
                        let locator = match &*carrier.segments[0].lock().unwrap() {
                            Segment::Object(r) => r.object_locator.clone(),
                            _ => unreachable!(),
                        };
                        assert_eq!(
                            archive.get_bounded(&locator, size).await.unwrap(),
                            Bytes::from(vec![b'x'; size])
                        );
                        drop(carrier);
                    }
                    assert_eq!(memory.snapshot().0, 0);
                    began.elapsed().as_micros() as u64
                }
            })
            .buffer_unordered(2)
            .collect()
            .await;
        let elapsed = started.elapsed();
        let mut ordered = samples;
        ordered.sort_unstable();
        let counts = proxy.counts();
        if previous {
            assert_eq!(counts.get("multipart_create"), Some(&16));
            assert_eq!(counts.get("multipart_complete"), Some(&16));
        } else {
            assert_eq!(counts.get("body_put"), Some(&16));
            assert_eq!(counts.get("multipart_create").copied().unwrap_or(0), 0);
        }
        assert_eq!(upload.upload_workspace.snapshot().0, 0);
        eprintln!(
            "synthetic_spill_comparison previous={previous} concurrency=2 objects=16 bytes={total} elapsed_us={} throughput_bytes_s={:.2} lifetime_p50_us={} lifetime_p95_us={} lifetime_p99_us={} operations={counts:?}",
            elapsed.as_micros(),
            total as f64 / elapsed.as_secs_f64(),
            ordered[7],
            ordered[15],
            ordered[15]
        );
    }
}
