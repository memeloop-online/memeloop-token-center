//! Real publisher/reaper contract, restricted to one isolated synthetic bucket.
use super::*;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{ObjectStore, ObjectStoreExt, aws::AmazonS3Builder, path::Path};
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "requires exclusive isolated RustFS bucket; no shared/prod bucket"]
async fn rustfs_native_publisher_release_reclaims_exact_nonzero_temporary() {
    let endpoint = std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap();
    let bucket = std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap();
    let proxy = crate::recovery_object_proxy::RecoveryObjectProxy::start(&endpoint).await;
    let delete_fault = proxy.fail_one_response_carrier_delete_after_put(&bucket);
    let upstream = MockServer::start().await;
    let raw_response = serde_json::to_vec(&completed_response_with_usage(309, 7)).unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(raw_response.clone(), "application/json"),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let mut fixture = response_usage_fixture("native-exact-release", &upstream, 256).await;
    let mut config = (*fixture.state.config).clone();
    config.archive_backend = ArchiveBackend::S3;
    config.s3_endpoint = Some(proxy.endpoint().to_owned());
    config.s3_bucket = Some(bucket.clone());
    config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    config.s3_region = "us-east-1".into();
    config.s3_allow_http = true;
    fixture.state.archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    fixture.state.db = fixture
        .state
        .db
        .clone()
        .with_archive(fixture.state.archive.clone());
    fixture.state.persistence_db = fixture
        .state
        .persistence_db
        .clone()
        .with_archive(fixture.state.archive.clone());
    fixture.state.routing_persistence_db = fixture
        .state
        .routing_persistence_db
        .clone()
        .with_archive(fixture.state.archive.clone());
    let object_store = AmazonS3Builder::new()
        .with_endpoint(proxy.endpoint())
        .with_bucket_name(&bucket)
        .with_region("us-east-1")
        .with_access_key_id(config.s3_access_key.as_deref().unwrap())
        .with_secret_access_key(config.s3_secret_key.as_deref().unwrap())
        .with_allow_http(true)
        .build()
        .unwrap();
    let releases = Path::from("carrier-releases/v1");
    assert!(
        object_store
            .list(Some(&releases))
            .take(1)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty(),
        "exclusive bucket must contain no earlier release responsibility"
    );
    let key = fixture
        .state
        .db
        .authenticate_key(&fixture.key, fixture.state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request = json!({"model":fixture.model,"input":"xxxxxxxxxxxxxxxxxxxxxxxxxx","stream":false,"max_output_tokens":16});
    let raw_request = serde_json::to_vec(&request).unwrap();
    // Actual HTTP router/authentication, mock upstream, original capture and finalizer.
    let response = send_response_usage_request(&fixture, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap()
            .as_ref(),
        raw_response.as_slice()
    );
    wait_for_request_settlement(&fixture, 1).await;
    drain_completed_response_archive(&fixture).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let request_id = rows[0].request_id;
    let archive = &fixture.state.archive;
    let request_cas = archive
        .raw_capture_reference(key.tenant_id, request_id, "request")
        .await
        .unwrap()
        .unwrap();
    let response_cas = archive
        .raw_capture_reference(key.tenant_id, request_id, "response")
        .await
        .unwrap()
        .unwrap();
    let request_sha = Sha256::digest(&raw_request);
    let response_sha = Sha256::digest(&raw_response);
    assert_eq!(
        Sha256::digest(
            archive
                .get_tenant_content(key.tenant_id, &request_cas, raw_request.len())
                .await
                .unwrap()
        ),
        request_sha
    );
    assert_eq!(
        Sha256::digest(
            archive
                .get_tenant_content(key.tenant_id, &response_cas, raw_response.len())
                .await
                .unwrap()
        ),
        response_sha
    );

    let binding = delete_fault.binding();
    assert_eq!(binding.backend, endpoint.trim_end_matches('/'));
    let object_key = binding.path.strip_prefix(&format!("/{bucket}/")).unwrap();
    assert!(object_key.starts_with(&format!("staging/proxy/{request_id}/response/")));
    let temporary = Path::from(object_key);
    let before = object_store.head(&temporary).await.unwrap();
    assert!(before.size > 0);
    assert!(
        delete_fault.rejected() > 0,
        "normal inline release must actually attempt cleanup"
    );
    let release_objects = object_store
        .list(Some(&releases))
        .take(5)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(
        release_objects.len(),
        1,
        "only this run's normal publisher may leave a release"
    );
    let release_path = &release_objects[0].location;
    let release_bytes = object_store
        .get(release_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let release: Value = serde_json::from_slice(&release_bytes).unwrap();
    assert_eq!(release["owner_id"], request_id.to_string());
    assert_eq!(release["purpose"], "response");
    assert_eq!(release["object_locator"], object_key);
    assert_eq!(release["byte_len"], raw_response.len());
    assert_eq!(
        release["blake3_digest"],
        blake3::hash(&raw_response).to_hex().to_string()
    );
    assert!(release["backend_identity"].is_null());
    let receipt_digest = blake3::hash(&release_bytes).to_hex().to_string();
    assert_eq!(
        release_path.as_ref(),
        format!("carrier-releases/v1/{receipt_digest}.json")
    );
    let owner_path = Path::from(format!("carrier-owners/v1/{receipt_digest}.json"));
    assert_eq!(
        object_store
            .get(&owner_path)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        release_bytes
    );
    let neighbor = format!("{}/neighbor-proof", object_key.rsplit_once('/').unwrap().0);
    archive
        .put(&neighbor, Bytes::from_static(b"neighbor remains"))
        .await
        .unwrap();

    // Original semantic worker and all existing once-only financial assertions.
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-usage-contract")).await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let leaf: String = sqlx::query_scalar(
        "SELECT leaf_node_hash FROM conversation_observations WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let authority = archive
        .semantic_archive_authority(key.tenant_id, request_id, &leaf)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        authority.coverage.projection_state,
        crate::archive::SemanticProjectionState::Complete
    );
    assert!(authority.coverage.observed_terminal && authority.coverage.gaps.is_empty());
    let manifest = archive
        .get_tenant_content(
            key.tenant_id,
            &authority.publication.manifest,
            4 * 1024 * 1024,
        )
        .await
        .unwrap();
    // Gate still armed: neither a successful projection nor a reaper pass may bypass it.
    let held = object_store.head(&temporary).await.unwrap();
    assert_eq!(
        (held.size, &held.e_tag, &held.version),
        (before.size, &before.e_tag, &before.version)
    );
    delete_fault.release();
    let reaper = crate::archive_reaper::ArchiveReaper::new(
        fixture.state.db.clone(),
        archive.clone(),
        crate::archive_staging::ArchiveStagingLeaseOwner::new("native-exact-release-test").unwrap(),
    );
    reaper.reap_once().await.unwrap();
    assert!(matches!(
        object_store.head(&temporary).await,
        Err(object_store::Error::NotFound { .. })
    ));
    assert!(matches!(
        object_store.head(release_path).await,
        Err(object_store::Error::NotFound { .. })
    ));
    assert!(matches!(
        object_store.head(&owner_path).await,
        Err(object_store::Error::NotFound { .. })
    ));
    assert_eq!(
        archive.get(&neighbor).await.unwrap(),
        b"neighbor remains".as_slice()
    );
    assert_eq!(
        Sha256::digest(
            archive
                .get_tenant_content(key.tenant_id, &request_cas, raw_request.len())
                .await
                .unwrap()
        ),
        request_sha
    );
    assert_eq!(
        Sha256::digest(
            archive
                .get_tenant_content(key.tenant_id, &response_cas, raw_response.len())
                .await
                .unwrap()
        ),
        response_sha
    );
    let after = archive
        .semantic_archive_authority(key.tenant_id, request_id, &leaf)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&authority).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    assert_eq!(
        archive
            .get_tenant_content(key.tenant_id, &after.publication.manifest, 4 * 1024 * 1024)
            .await
            .unwrap(),
        manifest
    );
    assert_exactly_once_side_effects(&fixture, request_id, Some("resp-usage-contract")).await;
    upstream.verify().await;
    println!(
        "native_release_nonzero encoded_bytes={} decoded_response_bytes={} exact_cleanup=true dual_cas_sha=true canonical_unchanged=true unique_financial=true",
        before.size,
        raw_response.len()
    );
    pool.close().await;
}
