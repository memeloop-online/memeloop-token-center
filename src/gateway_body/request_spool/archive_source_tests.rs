use super::*;
use crate::{
    config::Config,
    db::{ArchiveSpoolIdentity, Database},
    gateway_body::memory::ProxyMemoryBudget,
};
use std::time::Duration;

#[tokio::test]
async fn late_request_capture_retires_source_metadata_without_rebinding_after_commit_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("late-request.db").display()
    );
    let archive = crate::archive::ArchiveStore::memory_for_test();
    let database = Database::connect(&url)
        .await
        .unwrap()
        .with_archive(archive.clone());
    database.migrate().await.unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    let identity = ArchiveSpoolIdentity {
        request_id: uuid::Uuid::now_v7(),
        tenant_id: uuid::Uuid::now_v7(),
        reservation_id: uuid::Uuid::now_v7(),
    };
    let body = Bytes::from_static(br#"{"messages":[{"role":"user","content":"late source"}]}"#);
    let original = archive
        .put_tenant_content(identity.tenant_id, body.clone())
        .await
        .unwrap();
    let response = archive.put_tenant_content(identity.tenant_id, Bytes::from_static(br#"{"choices":[{"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}]}"#)).await.unwrap();
    let prepared = archive
        .prepare_session_body_streamed(
            identity.tenant_id,
            identity.request_id,
            crate::conversation::ConversationHints::default(),
            original.object_locator.clone(),
            Some(response.object_locator),
            true,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.coverage_complete);
    let proof = crate::archive::SemanticBindingProof {
        tenant_id: identity.tenant_id,
        request_id: identity.request_id,
        node_hash: prepared.nodes.last().unwrap().node_hash.clone(),
        projected_at: 1,
    };
    // Hold the existing-root fast path after it read the old root, retire
    // during its verification window, then require the real typed outcome.
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let publisher_db = database.clone();
    let original_len = body.len();
    let original_digest = *blake3::hash(&body).as_bytes();
    let publisher = tokio::spawn(Database::with_existing_source_root_gate(
        entered.clone(),
        resume.clone(),
        async move {
            publisher_db
                .publish_request_source(
                    identity,
                    "unused-existing-fastpath",
                    original_len,
                    original_digest,
                )
                .await
        },
    ));
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    archive
        .retire_covered_raw_roots(&prepared.publication, &proof)
        .await
        .unwrap();
    resume.notify_one();
    let (existing, settlement) = tokio::time::timeout(Duration::from_secs(5), publisher)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(existing, original);
    assert!(
        matches!(
            settlement,
            crate::archive::RawPublicationSettlement::Retired { .. }
        ),
        "old root read is not a current publication ACK"
    );
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                Some(&memory),
                0,
                Some(&identity.request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    let locator = source.object_locator().unwrap().to_owned();
    sqlx::query("INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id) VALUES ($1,$2,$3,1,'openai','test',0,0,0,$4,$5)")
        .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(uuid::Uuid::now_v7().to_string()).bind(&locator).bind(identity.reservation_id.to_string()).execute(&pool).await.unwrap();
    source.retain_for_archive(database.clone(), identity);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM request_archive_spools WHERE request_id=$1",
            )
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if count == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source.release_retained();
    drop(memory);
    store.resume_writes_for_test(archive.clone());
    release.send(()).unwrap();
    assert!(source.upload_outcome().await);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM request_archive_spools WHERE request_id=$1",
            )
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if count == 0 && budget.snapshot().0 == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late typed retirement clears old source metadata after durable handoff");
    // Replay a source completion whose SQL actually committed but lost its ACK.
    sqlx::query("INSERT INTO request_archive_spools (request_id,tenant_id,reservation_id,state,next_attempt_at,created_at,updated_at,expires_at,body_byte_count,body_blake3) VALUES ($1,$2,$3,'capturing',1,1,1,9223372036854775807,$4,$5)")
        .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.reservation_id.to_string()).bind(body.len() as i64).bind(blake3::hash(&body).to_hex().to_string()).execute(&pool).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        Database::with_semantic_coverage_fault(
            "retired_request_source_committed",
            database.complete_request_source(
                identity,
                &locator,
                body.len(),
                *blake3::hash(&body).as_bytes(),
                true,
            ),
        ),
    )
    .await
    .unwrap();
    database
        .complete_request_source(
            identity,
            &locator,
            body.len(),
            *blake3::hash(&body).as_bytes(),
            true,
        )
        .await;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_archive_spools WHERE request_id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "retired completion must never recreate a bound source"
    );
    assert!(
        archive
            .raw_capture_reference(identity.tenant_id, identity.request_id, "request")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        archive
            .get_tenant_content(identity.tenant_id, &original, body.len())
            .await
            .unwrap(),
        body
    );
    assert_eq!(source.read_all().await.unwrap(), body);
    let row = sqlx::query("SELECT request_object,completed_at FROM request_records WHERE id=$1")
        .bind(identity.request_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    use sqlx::Row as _;
    assert_eq!(
        row.get::<String, _>("request_object"),
        locator,
        "no new raw SQL binding or financial completion"
    );
    assert_eq!(row.get::<Option<i64>, _>("completed_at"), None);
    drop(source);
    assert_eq!(budget.snapshot().0, 0);
    pool.close().await;
    database.close().await;
}

#[tokio::test]
async fn archive_source_survives_request_drop_without_pinning_upload_bytes_on_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("source.db").display()
    );
    let archive = crate::archive::ArchiveStore::memory_for_test();
    let database = Database::connect_with_max(&database_url, 4)
        .await
        .unwrap()
        .with_archive(archive.clone());
    database.migrate().await.unwrap();
    let blocker = sqlx::AnyPool::connect(&database_url).await.unwrap();
    let held = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let request_id = uuid::Uuid::now_v7();
    let body = Bytes::from(vec![b'x'; 128 * 1024]);
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                Some(&memory),
                0,
                Some(&request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    admission.register_source(request_id, &source);
    let raw = source.read_all().await.unwrap();
    let shared = source.shared_bytes(raw.clone());
    assert_eq!(raw.as_ptr(), shared.as_ptr());
    assert!(Arc::ptr_eq(&source, &admission.source(request_id).unwrap()));
    let locator = source.object_locator().unwrap().to_owned();
    let identity = ArchiveSpoolIdentity {
        request_id,
        tenant_id: uuid::Uuid::now_v7(),
        reservation_id: uuid::Uuid::now_v7(),
    };
    source.retain_for_archive(database.clone(), identity);
    drop(shared);
    drop(raw);
    source.release_retained();
    drop(memory);
    assert!(
        budget.snapshot().0 >= body.len(),
        "the pending RAM carrier must keep its real credit"
    );
    // Mirror the fake's gated PUT into the same real backend used by root
    // publication, while its independent SQL handoff remains blocked.
    store.resume_writes_for_test(archive.clone());
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), source.upload_outcome())
            .await
            .unwrap()
    );
    let root = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(root) = archive
                .raw_capture_reference(identity.tenant_id, request_id, "request")
                .await
                .unwrap()
                && budget.snapshot().0 == 0
            {
                break root;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("verified durable root releases source RAM without waiting for SQL");
    assert_eq!(root.size_bytes, body.len() as u64);
    assert_eq!(root.blake3_digest, blake3::hash(&body).to_hex().to_string());
    archive
        .verify_tenant_content(identity.tenant_id, &root)
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), source.read_all())
            .await
            .unwrap()
            .unwrap(),
        body
    );
    assert_eq!(budget.snapshot().0, 0);
    drop(source);
    assert!(admission.source(request_id).is_none());
    assert_eq!(store.objects.lock().unwrap().get(&locator), Some(&body));
    held.rollback().await.unwrap();
    blocker.close().await;
    database.close().await;
}

#[tokio::test]
async fn pending_sources_keep_last_reader_credit_and_cancel_a_pressure_waiter() {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = Arc::new(ProxyMemoryBudget::new(256 * 1024));
    let first_memory = budget.reservation();
    let first_body = Bytes::from(vec![b'a'; 128 * 1024]);
    let first = Arc::new(
        admission
            .capture_with_memory(
                Body::from(first_body.clone()),
                first_body.len(),
                None,
                Some(&first_memory),
                0,
                Some(&uuid::Uuid::now_v7().to_string()),
            )
            .await
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while store.entered_writes.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!first.durable_settled_for_test());
    let raw = first.read_all().await.unwrap();
    assert_eq!(raw, first_body);
    let shared = first.shared_bytes(raw.clone());
    assert_eq!(raw.as_ptr(), shared.as_ptr());
    let pending_bytes = budget.snapshot().0;
    assert!(pending_bytes >= first_body.len());
    let second_memory = budget.reservation();
    let active_memory = second_memory.clone();
    let active_admission = admission.clone();
    let (polled, first_poll) = tokio::sync::oneshot::channel();
    let input = Body::from_stream(futures_util::stream::once(async move {
        let _ = polled.send(());
        Ok::<_, std::convert::Infallible>(Bytes::from(vec![b'b'; 256 * 1024]))
    }));
    let capture = tokio::spawn(async move {
        active_admission
            .capture_with_memory(input, 256 * 1024, None, Some(&active_memory), 0, None)
            .await
    });
    first_poll.await.unwrap();
    assert!(!capture.is_finished());
    assert!(budget.snapshot().0 <= budget.snapshot().1);
    capture.abort();
    let Err(cancelled) = capture.await else {
        panic!("the pressure waiter must be cancelled");
    };
    assert!(cancelled.is_cancelled());
    drop(second_memory);
    assert_eq!(budget.snapshot().0, pending_bytes);
    assert_eq!(first.read_all().await.unwrap(), first_body);
    assert!(!first.durable_settled_for_test());
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), first.upload_outcome())
            .await
            .unwrap()
    );
    drop(raw);
    drop(first);
    drop(first_memory);
    assert_eq!(
        budget.snapshot().0,
        pending_bytes,
        "the last shared reader still owns the source"
    );
    drop(shared);
    tokio::time::timeout(Duration::from_secs(1), async {
        while budget.snapshot().0 != 0 || admission.snapshot().active_files != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all real source owners release their credit and leases");
    assert_eq!(admission.snapshot().used_bytes, 0);
}

#[tokio::test]
async fn failed_source_retries_the_original_bytes_without_releasing_their_budget() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("failure.db").display()
    );
    let archive = crate::archive::ArchiveStore::memory_for_test();
    let database = Database::connect_with_max(&database_url, 4)
        .await
        .unwrap()
        .with_archive(archive.clone());
    database.migrate().await.unwrap();
    let held = database.saturate_gateway_persistence_for_test().await;
    let store = MemoryObjectStore::new();
    store.fail_writes.store(true, Ordering::Release);
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let request_id = uuid::Uuid::now_v7();
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(vec![b'x'; 128 * 1024]),
                128 * 1024,
                None,
                Some(&memory),
                0,
                Some(&request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    let pepper = b"source-failure-real-admission-test-pepper";
    let issued = database
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: request_id.to_string(),
                principal_external_id: "member".into(),
                alias: "failed-source".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(
            "source",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    database
        .start_proxy_request_with_source(
            crate::db::StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai",
                model: "source",
                request_object: source.object_locator().unwrap(),
                upstream_account_id: None,
                model_route_id: None,
            },
            &source,
            None,
            Some(1),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), source.upload_outcome())
            .await
            .is_err()
    );
    let locator = source.object_locator().unwrap().to_owned();
    source.release_retained();
    drop(source);
    drop(memory);
    assert!(
        budget.snapshot().0 >= 128 * 1024,
        "failed object writes must retain the only RAM source and its real credit"
    );
    drop(held);
    store.resume_writes_for_test(archive);
    tokio::time::timeout(Duration::from_secs(3), async {
        while budget.snapshot().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        store.objects.lock().unwrap().get(&locator),
        Some(&Bytes::from(vec![b'x'; 128 * 1024]))
    );
    database.drain_gateway_persistence_for_test().await;
    database.close().await;
}

#[tokio::test]
async fn archive_source_keys_match_the_existing_typed_cleanup_prefix() {
    let request_id = uuid::Uuid::now_v7();
    let key = crate::archive_staging::ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        request_id,
    )
    .unwrap();
    let locator = request_body_object_key(&request_id.to_string()).unwrap();
    assert_eq!(locator, format!("{}/body.json", key.canonical_prefix()));
    assert_eq!(source_request_id(&locator), Some(request_id));
    assert!(source_request_id(&format!("{}/other.json", key.canonical_prefix())).is_none());
    let config = Config::for_test("sqlite::memory:".into());
    let archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    archive
        .put(&locator, Bytes::from_static(b"{}"))
        .await
        .unwrap();
    assert_eq!(
        archive.get(&locator).await.unwrap(),
        Bytes::from_static(b"{}")
    );
}

#[tokio::test]
async fn qualified_request_source_streams_to_training_and_cleans_only_its_object() {
    let directory = tempfile::tempdir().unwrap();
    let archive = crate::archive::ArchiveStore::training_staging_memory_for_test();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_archive(archive.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let request_id = uuid::Uuid::now_v7();
    let body = Bytes::from(vec![b'x'; 128 * 1024]);
    let source = admission
        .capture_with_memory(
            Body::from(body.clone()),
            body.len(),
            Some(body.len()),
            Some(&memory),
            0,
            Some(&request_id.to_string()),
        )
        .await
        .unwrap();
    let locator = source.object_locator().unwrap().to_owned();
    assert!(locator.starts_with(&format!("stores/staging/{}/", "a".repeat(64))));
    assert_eq!(source_request_id(&locator), Some(request_id));
    assert!(source.upload_outcome().await);
    assert_eq!(archive.get(&locator).await.unwrap(), body);
    let bare = request_body_object_key(&request_id.to_string()).unwrap();
    assert!(archive.get(&bare).await.is_err(), "no legacy source copy");
    source.release_retained();
    assert_eq!(source.read_all().await.unwrap(), body);
    let typed = request_body_staging_key(&locator).unwrap();
    let neighbour = format!("{}/neighbour.json", typed.canonical_prefix());
    archive
        .put(&neighbour, Bytes::from_static(b"keep"))
        .await
        .unwrap();
    drop(source);
    tokio::time::timeout(Duration::from_secs(5), async {
        while archive.source_object_exists(&locator).await.unwrap() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        archive.get(&neighbour).await.unwrap(),
        Bytes::from_static(b"keep")
    );
}

#[test]
fn request_source_identity_accepts_only_the_exact_legacy_or_qualified_body() {
    let request_id = uuid::Uuid::now_v7();
    let bare = request_body_object_key(&request_id.to_string()).unwrap();
    let qualified = format!("stores/staging/{}/{bare}", "a".repeat(64));
    assert_eq!(source_request_id(&bare), Some(request_id));
    assert_eq!(source_request_id(&qualified), Some(request_id));
    assert!(source_request_id(&format!("stores/staging/z/{bare}")).is_none());
    assert!(source_request_id(&format!("stores/staging/{}/{qualified}", "a".repeat(64))).is_none());
    assert!(source_request_id(&qualified.replace("body.json", "other.json")).is_none());
}

async fn qualified_source_receipt_contract(database_url: &str) {
    use crate::archive_staging::{ArchiveStagingLeaseOwner, ArchiveStagingReferenceProof};
    let archive = crate::archive::ArchiveStore::training_staging_memory_for_test();
    let database = Database::connect_with_max(database_url, 4).await.unwrap();
    database.migrate().await.unwrap();
    let database = database.with_archive(archive.clone());
    let directory = tempfile::tempdir().unwrap();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_archive(archive.clone());
    let request_id = uuid::Uuid::now_v7();
    let body = Bytes::from_static(b"original source retained across database reopen");
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                None,
                0,
                Some(&request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    let locator = source.object_locator().unwrap().to_owned();
    let typed = request_body_staging_key(&locator).unwrap();
    let pepper = b"qualified-source-receipt-test";
    let issued = database
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: request_id.to_string(),
                principal_external_id: "member".into(),
                alias: "qualified-source".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(
            "source",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    let started = database
        .start_proxy_request_with_source(
            crate::db::StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai",
                model: "source",
                request_object: &locator,
                upstream_account_id: None,
                model_route_id: None,
            },
            &source,
            None,
            Some(1),
        )
        .await
        .unwrap();
    let identity = ArchiveSpoolIdentity {
        request_id,
        tenant_id: key.tenant_id,
        reservation_id: started.id,
    };
    assert!(source.upload_outcome().await);
    database.drain_gateway_persistence_for_test().await;
    let pool = sqlx::AnyPool::connect(database_url).await.unwrap();
    // Upload completion precedes the independent source receipt handoff. The
    // gateway task drain does not drain that worker; wait for its real commit.
    let stored = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let stage = database.archive_staging_attempt(request_id).await.unwrap();
            let state: Option<String> = sqlx::query_scalar(
                "SELECT state FROM request_archive_spools WHERE request_id = $1",
            )
            .bind(request_id.to_string())
            .fetch_optional(&pool)
            .await
            .unwrap();
            if let Some(stage) = stage
                && state.as_deref() == Some("bound")
            {
                break stage;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("source handoff must commit the bound receipt and staging identity");
    assert_eq!(
        stored.key, typed,
        "the durable cleanup identity includes the backend"
    );
    let bound: String = sqlx::query_scalar(
        "SELECT bound_locator FROM request_archive_spools WHERE request_id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bound, locator);
    let bare = request_body_object_key(&request_id.to_string()).unwrap();
    archive
        .put(&bare, Bytes::from_static(b"separate legacy object"))
        .await
        .unwrap();
    let neighbour = format!("{}/neighbour", typed.canonical_prefix());
    archive
        .put(&neighbour, Bytes::from_static(b"neighbour"))
        .await
        .unwrap();
    drop(source);
    database.close().await;
    let reopened = Database::connect_with_max(database_url, 4)
        .await
        .unwrap()
        .with_archive(archive.clone());
    assert_eq!(
        reopened
            .archive_staging_attempt(request_id)
            .await
            .unwrap()
            .unwrap()
            .key,
        typed
    );
    assert_eq!(archive.get(&bound).await.unwrap(), body);
    reopened
        .record_request_source(
            identity,
            &locator,
            body.len(),
            *blake3::hash(&body).as_bytes(),
            true,
        )
        .await
        .unwrap();
    // A replay cannot silently reinterpret the old NULL backend as training.
    sqlx::query(
        "UPDATE archive_staging_attempts SET backend_identity = NULL WHERE attempt_id = $1",
    )
    .bind(request_id.to_string())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        reopened
            .record_request_source(
                identity,
                &locator,
                body.len(),
                *blake3::hash(&body).as_bytes(),
                true
            )
            .await
            .is_err()
    );
    assert!(
        reopened
            .archive_staging_attempt(request_id)
            .await
            .unwrap()
            .unwrap()
            .key
            .backend_identity
            .is_none()
    );
    sqlx::query("UPDATE archive_staging_attempts SET backend_identity = $2 WHERE attempt_id = $1")
        .bind(request_id.to_string())
        .bind(typed.backend_identity_text())
        .execute(&pool)
        .await
        .unwrap();
    let claim = reopened
        .claim_archive_staging_cleanup(
            ArchiveStagingLeaseOwner::new("qualified-source-reaper").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.attempt.key, typed);
    assert_eq!(claim.canonical_prefix(), typed.canonical_prefix());
    assert!(
        matches!(
            reopened
                .prove_archive_staging_unreferenced(claim)
                .await
                .unwrap(),
            ArchiveStagingReferenceProof::Protected { .. }
        ),
        "pending referenced source is not garbage"
    );
    assert_eq!(archive.get(&bound).await.unwrap(), body);
    assert_eq!(
        archive.get(&bare).await.unwrap(),
        Bytes::from_static(b"separate legacy object")
    );
    assert_eq!(
        archive.get(&neighbour).await.unwrap(),
        Bytes::from_static(b"neighbour")
    );
    pool.close().await;
    reopened.close().await;
}

#[tokio::test]
async fn sqlite_qualified_source_receipt_reopens_with_training_cleanup_identity() {
    let directory = tempfile::tempdir().unwrap();
    qualified_source_receipt_contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("qualified.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_qualified_source_receipt_reopens_with_training_cleanup_identity() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("qualified_source_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    qualified_source_receipt_contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
