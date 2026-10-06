use bytes::Bytes;
use sqlx::Row;
use uuid::Uuid;

use super::*;
use crate::{AppState, config::Config, db::ArchiveSpoolIdentity};

const PEPPER: &[u8] = b"existing-test-pepper-over-thirty-two-bytes";

#[tokio::test]
async fn failed_terminal_captures_keep_legacy_bodies_and_drain_budget_after_binding() {
    for compressed in [false, true] {
        for (status_code, error_code) in [
            (503_i64, None),
            (502, Some("stream_incomplete")),
            (200, Some("stream_incomplete")),
            (429, None),
        ] {
            let (_dir, mut state, pool, identity) = fixture().await;
            sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) SELECT id, created_at, tenant_id, key_id FROM request_records WHERE id = $1")
                .bind(identity.request_id.to_string()).execute(&pool).await.unwrap();
            std::sync::Arc::make_mut(&mut state.config).archive_object_compression_enabled =
                compressed;
            let request = Bytes::from_static(b"{\"input\":\"diagnostic request\"}");
            let response =
                Bytes::from_static(b"{\"error\":{\"message\":\"upstream request failed\"}}");
            for (purpose, body) in [
                (BufferedArchivePurpose::Request, request.clone()),
                (BufferedArchivePurpose::Response, response.clone()),
            ] {
                assert!(capture_buffered(&state, identity, purpose, body).await);
            }
            assert!(!process_one_for_test(&state).await);
            finish(&pool, identity).await;
            sqlx::query("UPDATE request_records SET status_code = $1, error_code = $2, request_object = $3, input_tokens = 45, output_tokens = 67, cost_micros = 123 WHERE id = $4")
                .bind(status_code).bind(error_code).bind(format!("gap://{}/request", identity.request_id))
                .bind(identity.request_id.to_string()).execute(&pool).await.unwrap();
            let retained: i64 = sqlx::query_scalar(
                "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(retained > 0);
            assert!(process_one_for_test(&state).await);
            assert!(process_one_for_test(&state).await);
            for _ in 0..2 {
                assert!(!process_one_for_test(&state).await);
            }
            let budget = sqlx::query("SELECT cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1")
                .fetch_one(&pool).await.unwrap();
            assert_eq!(budget.get::<i64, _>("cipher_bytes"), 0);
            assert_eq!(budget.get::<i64, _>("request_cipher_bytes"), 0);
            let row = sqlx::query("SELECT request_object, response_object, status_code, error_code, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = $1")
                .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
            assert_eq!(row.get::<i64, _>("status_code"), status_code);
            assert_eq!(
                row.get::<Option<String>, _>("error_code").as_deref(),
                error_code
            );
            assert_eq!(row.get::<i64, _>("input_tokens"), 45);
            assert_eq!(row.get::<i64, _>("output_tokens"), 67);
            assert_eq!(row.get::<i64, _>("cost_micros"), 123);
            for (column, body, purpose) in [
                ("request_object", request, "request"),
                ("response_object", response, "response"),
            ] {
                let locator: String = row.get(column);
                assert!(!locator.starts_with("gap://"));
                assert!(!locator.contains("/cas/"));
                assert_eq!(locator.ends_with(".mtcz1"), compressed);
                assert_eq!(state.archive.get(&locator).await.unwrap(), body);
                let bound: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE owner_kind = 'proxy_request' AND owner_id = $1 AND purpose = $2 AND state = 'bound' AND bound_locator = $3")
                    .bind(identity.request_id.to_string()).bind(purpose).bind(&locator)
                    .fetch_one(&pool).await.unwrap();
                assert_eq!(bound, 1);
            }
            let spools: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM request_archive_spools WHERE request_id = $1 AND state = 'bound' AND attempts = 1 AND cipher_bytes = 0) + (SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1 AND state = 'bound' AND attempts = 1 AND cipher_bytes = 0)")
                .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
            assert_eq!(spools, 2);
            let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_bound'")
                .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
            assert_eq!(events, 2);
            let gaps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1 AND event_kind = 'archive_gap'")
                .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
            assert_eq!(gaps, 0);
        }
    }
}

#[tokio::test]
async fn inline_failure_summary_keeps_captured_request_and_drains_its_budget() {
    let (_dir, state, pool, identity) = fixture().await;
    let request = Bytes::from_static(b"{\"input\":\"diagnostic request\"}");
    assert!(
        capture_buffered(
            &state,
            identity,
            BufferedArchivePurpose::Request,
            request.clone()
        )
        .await
    );
    finish(&pool, identity).await;
    let summary = "inline-json:{\"error\":{\"message\":\"upstream rejected the request\"}}";
    sqlx::query("UPDATE request_records SET status_code = 429, request_object = $1, response_object = $2 WHERE id = $3")
        .bind(format!("gap://{}/request", identity.request_id)).bind(summary)
        .bind(identity.request_id.to_string()).execute(&pool).await.unwrap();
    assert!(process_one_for_test(&state).await);
    assert!(!process_one_for_test(&state).await);
    let row =
        sqlx::query("SELECT request_object, response_object FROM request_records WHERE id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let locator: String = row.get("request_object");
    assert!(!locator.contains("/cas/"));
    assert_eq!(state.archive.get(&locator).await.unwrap(), request);
    assert_eq!(row.get::<String, _>("response_object"), summary);
    let budget: i64 = sqlx::query_scalar(
        "SELECT cipher_bytes FROM response_archive_spool_budget WHERE singleton = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(budget, 0);
}

#[tokio::test]
async fn corrupt_or_missing_spool_chunks_never_publish_a_locator() {
    for (missing, status_code) in [(false, 200_i64), (true, 200), (false, 502), (true, 502)] {
        let (_dir, state, pool, identity) = fixture().await;
        assert!(
            capture_buffered(
                &state,
                identity,
                BufferedArchivePurpose::Response,
                Bytes::from_static(b"complete text")
            )
            .await
        );
        finish(&pool, identity).await;
        sqlx::query("UPDATE request_records SET status_code = $1 WHERE id = $2")
            .bind(status_code)
            .bind(identity.request_id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        if missing {
            sqlx::query("DELETE FROM response_archive_spool_chunks WHERE request_id = $1")
                .bind(identity.request_id.to_string())
                .execute(&pool)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE response_archive_spool_chunks SET ciphertext = 'corrupt' WHERE request_id = $1")
                .bind(identity.request_id.to_string()).execute(&pool).await.unwrap();
        }
        assert!(process_one_for_test(&state).await);
        let locator: String =
            sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
                .bind(identity.request_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(locator, format!("gap://{}/response", identity.request_id));
        let bound: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_archive_spools WHERE request_id = $1 AND state = 'bound'")
            .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(bound, 0);
    }
}

#[tokio::test]
async fn both_buffered_purposes_recover_exact_bytes_only_after_terminal() {
    buffered_purposes_recover(false).await;
}

#[tokio::test]
async fn compressed_objects_bind_both_purposes_and_recover_original_bytes() {
    buffered_purposes_recover(true).await;
}

async fn buffered_purposes_recover(compressed: bool) {
    let (_dir, mut state, pool, identity) = fixture().await;
    std::sync::Arc::make_mut(&mut state.config).archive_object_compression_enabled = compressed;
    let request = Bytes::from_static(b"{\"messages\":[{\"content\":\"private request\"}]}");
    let response = Bytes::from_static(b"{\"output\":\"complete private response\"}");
    assert!(
        capture_buffered(
            &state,
            identity,
            BufferedArchivePurpose::Request,
            request.clone()
        )
        .await
    );
    assert!(
        capture_buffered(
            &state,
            identity,
            BufferedArchivePurpose::Response,
            response.clone()
        )
        .await
    );
    assert!(!process_one_for_test(&state).await);
    sqlx::query("UPDATE request_records SET request_object = $1 WHERE id = $2")
        .bind(format!("gap://{}/request", identity.request_id))
        .bind(identity.request_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    finish(&pool, identity).await;
    let permits = state.proxy_archive_stream_permits.clone();
    let held = permits
        .clone()
        .acquire_many_owned(permits.available_permits() as u32)
        .await
        .unwrap();
    assert!(!process_one_for_test(&state).await);
    for table in ["request_archive_spools", "response_archive_spools"] {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT state, attempts FROM {table} WHERE request_id = $1"
        )))
        .bind(identity.request_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("state"), "pending");
        assert_eq!(row.get::<i64, _>("attempts"), 0);
    }
    drop(held);
    assert!(process_one_for_test(&state).await);
    assert!(process_one_for_test(&state).await);
    let row =
        sqlx::query("SELECT request_object, response_object FROM request_records WHERE id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let request_locator: String = row.get("request_object");
    let response_locator: String = row.get("response_object");
    assert_eq!(request_locator.ends_with(".mtcz1"), compressed);
    assert_eq!(response_locator.ends_with(".mtcz1"), compressed);
    assert!(request_locator.contains(&format!("tenants/{}/cas/v1/", identity.tenant_id)));
    assert!(response_locator.contains(&format!("tenants/{}/cas/v1/", identity.tenant_id)));
    assert_ne!(request_locator, response_locator);
    assert_eq!(state.archive.get(&request_locator).await.unwrap(), request);
    assert_eq!(
        state.archive.get(&response_locator).await.unwrap(),
        response
    );
    let staging_states: Vec<String> = sqlx::query_scalar(
        "SELECT state FROM archive_staging_attempts WHERE owner_id = $1 ORDER BY purpose",
    )
    .bind(identity.request_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        staging_states,
        vec!["cleanup_pending".to_owned(), "cleanup_pending".to_owned()]
    );
}

#[test]
fn request_cipher_domain_is_distinct_and_schema71_response_aad_is_unchanged() {
    let identity = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let payload = Bytes::from_static(b"private archive");
    let request =
        encrypt_buffered(identity, BufferedArchivePurpose::Request, &payload, PEPPER).unwrap();
    assert!(
        cipher::open(
            identity,
            0,
            &request[0].ciphertext,
            payload.len() as i64,
            PEPPER
        )
        .is_err()
    );
    assert_eq!(
        cipher::open_for_purpose(
            identity,
            0,
            &request[0].ciphertext,
            payload.len() as i64,
            PEPPER,
            BufferedArchivePurpose::Request
        )
        .unwrap(),
        payload
    );
    let legacy_aad = format!(
        "memeloop-token-center/response-archive-spool/v1/{}/{}/{}/0",
        identity.tenant_id, identity.request_id, identity.reservation_id
    );
    let legacy = crate::provider::seal_private_json(
        &serde_json::json!({"bytes":"cHJpdmF0ZSBhcmNoaXZl"}),
        PEPPER,
        legacy_aad.as_bytes(),
    )
    .unwrap();
    assert_eq!(
        cipher::open_for_purpose(
            identity,
            0,
            &legacy,
            payload.len() as i64,
            PEPPER,
            BufferedArchivePurpose::Response
        )
        .unwrap(),
        payload
    );
    assert!(
        cipher::open_for_purpose(
            identity,
            0,
            &legacy,
            payload.len() as i64,
            PEPPER,
            BufferedArchivePurpose::Request
        )
        .is_err()
    );
}

#[test]
fn encrypted_chunks_bind_every_owner_and_sequence_without_plaintext() {
    let id = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    let payload = b"private response never stored in cleartext";
    let envelope = cipher::seal(id, 0, payload, PEPPER).unwrap();
    assert!(envelope.starts_with("v2."));
    assert!(!envelope.contains("private response"));
    assert_eq!(
        cipher::open(id, 0, &envelope, payload.len() as i64, PEPPER).unwrap(),
        payload.as_slice()
    );
    for other in [
        ArchiveSpoolIdentity {
            tenant_id: Uuid::new_v4(),
            ..id
        },
        ArchiveSpoolIdentity {
            request_id: Uuid::new_v4(),
            ..id
        },
        ArchiveSpoolIdentity {
            reservation_id: Uuid::new_v4(),
            ..id
        },
    ] {
        assert!(cipher::open(other, 0, &envelope, payload.len() as i64, PEPPER).is_err());
    }
    assert!(cipher::open(id, 1, &envelope, payload.len() as i64, PEPPER).is_err());
    assert!(cipher::open(id, 0, &envelope, 1, PEPPER).is_err());
    assert!(
        cipher::open(
            id,
            0,
            &envelope,
            payload.len() as i64,
            b"wrong existing key material"
        )
        .is_err()
    );
}

async fn fixture() -> (
    tempfile::TempDir,
    AppState,
    sqlx::AnyPool,
    ArchiveSpoolIdentity,
) {
    fixture_with_compression(false).await
}

async fn fixture_with_compression(
    compression_enabled: bool,
) -> (
    tempfile::TempDir,
    AppState,
    sqlx::AnyPool,
    ArchiveSpoolIdentity,
) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("spool.db").display()
    );
    let mut config = Config::for_test(url.clone());
    config.archive_spool_compression_enabled = compression_enabled;
    let state = AppState::initialize(config).await.unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    let identity = ArchiveSpoolIdentity {
        request_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    };
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'test', 0, 0, 0, 'gap://test/request', $4)")
        .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string())
        .bind(Uuid::new_v4().to_string()).bind(identity.reservation_id.to_string())
        .execute(&pool).await.unwrap();
    (dir, state, pool, identity)
}

#[tokio::test]
async fn enabled_response_writer_persists_compressed_chunks_and_dual_read_uploads_exact_bytes() {
    let (_dir, state, pool, identity) = fixture_with_compression(true).await;
    let body = Bytes::from(
        serde_json::to_vec(&vec![
            serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "synthetic repeated JSON content for writer coverage",
            });
            900
        ])
        .unwrap(),
    );
    let mut writer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    writer.append_for_test(vec![body.clone()]).await.unwrap();
    writer.seal_for_test().await.unwrap();

    let row = sqlx::query(
        "SELECT s.byte_count, s.cipher_bytes, b.cipher_bytes AS budget_bytes, 1024 + SUM(LENGTH(c.ciphertext) + 512) AS actual_bytes, MIN(CASE WHEN c.ciphertext LIKE 'zstd1.%' THEN 1 ELSE 0 END) AS compressed FROM response_archive_spools s JOIN response_archive_spool_chunks c ON c.request_id = s.request_id CROSS JOIN response_archive_spool_budget b WHERE s.request_id = $1 AND b.singleton = 1 GROUP BY s.byte_count, s.cipher_bytes, b.cipher_bytes",
    )
    .bind(identity.request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("byte_count"), body.len() as i64);
    let actual = row.get::<i64, _>("actual_bytes");
    assert_eq!(row.get::<i64, _>("cipher_bytes"), actual);
    assert_eq!(row.get::<i64, _>("budget_bytes"), actual);
    assert_eq!(row.get::<i64, _>("compressed"), 1);

    finish(&pool, identity).await;
    assert!(process_one_for_test(&state).await);
    let locator: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state.archive.get(&locator).await.unwrap(), body);
}

async fn finish(pool: &sqlx::AnyPool, identity: ArchiveSpoolIdentity) {
    sqlx::query("UPDATE request_records SET completed_at=2, status_code=200, response_object=$1 WHERE id=$2")
        .bind(format!("gap://{}/response", identity.request_id))
        .bind(identity.request_id.to_string()).execute(pool).await.unwrap();
}

#[tokio::test]
async fn shutdown_at_claim_admission_preserves_attempts_and_never_starts_a_writer() {
    let (_dir, state, pool, identity) = fixture().await;
    let mut producer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    producer
        .append_for_test(vec![Bytes::from_static(b"data: [DONE]\n\n")])
        .await
        .unwrap();
    producer.seal_for_test().await.unwrap();
    finish(&pool, identity).await;

    for _ in 0..10 {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        // The seam executes after the real SELECT, while the transaction owns
        // the budget lock, immediately before UPDATE would consume an attempt.
        assert!(
            !upload::process_one_with_admission(&state, Uuid::new_v4(), Some(&receiver), || {
                sender.send(true).unwrap();
                !*receiver.borrow()
            })
            .await
        );
        let row = sqlx::query("SELECT state, attempts, lease_token FROM response_archive_spools WHERE request_id = $1")
            .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(row.get::<String, _>("state"), "pending");
        assert_eq!(row.get::<i64, _>("attempts"), 0);
        assert!(row.get::<Option<String>, _>("lease_token").is_none());
        let writers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE owner_id = $1 AND purpose = 'response'")
            .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(
            writers, 0,
            "no staging attempt means no object writer was opened"
        );
    }

    // Once the admission decision is true, shutdown arriving before COMMIT
    // must drain exactly that one upload, never strand a paid retry attempt.
    let (sender, receiver) = tokio::sync::watch::channel(false);
    assert!(
        upload::process_one_with_admission(&state, Uuid::new_v4(), Some(&receiver), || {
            sender.send(true).unwrap();
            true
        })
        .await
    );
    assert!(!upload::process_one(&state, Uuid::new_v4()).await);
    let row =
        sqlx::query("SELECT state, attempts FROM response_archive_spools WHERE request_id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("state"), "bound");
    assert_eq!(row.get::<i64, _>("attempts"), 1);
    let writers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE owner_id = $1 AND purpose = 'response'")
        .bind(identity.request_id.to_string()).fetch_one(&pool).await.unwrap();
    assert_eq!(writers, 1);
}

#[tokio::test]
async fn nine_burst_frames_are_durable_before_any_object_store_consumer() {
    let (_dir, state, pool, identity) = fixture().await;
    // This regression covers durable burst buffering without a consumer. The
    // production ACK deadline has its own paused-time contract in producer.rs.
    let mut producer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    let frames: Vec<Bytes> = (0..9)
        .map(|i| Bytes::from(format!("data: {{\"n\":{i}}}\n\n")))
        .collect();
    producer.append_for_test(frames.clone()).await.unwrap();
    producer.seal_for_test().await.unwrap();
    finish(&pool, identity).await;
    let row =
        sqlx::query("SELECT state,chunk_count FROM response_archive_spools WHERE request_id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(row.get::<i64, _>("chunk_count"), 1);
    // A fresh worker can recover the persisted stream with the same existing
    // pepper; no in-memory queue/producer state or extra credential is needed.
    assert!(upload::process_one(&state, Uuid::new_v4()).await);
    let locator: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!locator.starts_with("gap://"));
    let output = state.archive.get(&locator).await.unwrap();
    let expected: Vec<u8> = frames.iter().flat_map(|f| f.iter().copied()).collect();
    assert_eq!(output.as_ref(), expected);
    upload::process_one(&state, Uuid::new_v4()).await;
    let complete: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM response_archive_spools WHERE request_id=$1 AND state='bound'",
    )
    .bind(identity.request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(complete, 1);
}

#[tokio::test]
async fn crlf_boundaries_and_multiple_frames_replay_exact_bytes() {
    let (_dir, state, pool, identity) = fixture().await;
    let mut producer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    let first = Bytes::from_static(b"data: [DONE]\r\n\r");
    let second = Bytes::from_static(b"\n: heartbeat\n\n");
    producer.append_for_test(vec![first.clone()]).await.unwrap();
    producer
        .append_for_test(vec![second.clone()])
        .await
        .unwrap();
    producer.seal_for_test().await.unwrap();
    finish(&pool, identity).await;
    upload::process_one(&state, Uuid::new_v4()).await;
    let locator: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        state.archive.get(&locator).await.unwrap().as_ref(),
        [first.as_ref(), second.as_ref()].concat()
    );
}

#[tokio::test]
async fn rejected_capture_never_publishes_a_complete_prefix() {
    let (_dir, state, pool, identity) = fixture().await;
    let mut producer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    producer
        .append_for_test(vec![Bytes::from_static(b"prefix")])
        .await
        .unwrap();
    producer::mark_gap_for_test(&state, identity, "capture_failed")
        .await
        .unwrap();
    assert!(
        producer
            .append_for_test(vec![Bytes::from_static(b"suffix")])
            .await
            .is_err()
    );
    assert!(producer.seal_for_test().await.is_err());
    finish(&pool, identity).await;
    assert!(!upload::process_one(&state, Uuid::new_v4()).await);
    let locator: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(locator, format!("gap://{}/response", identity.request_id));
    let staging_attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE owner_id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(staging_attempts, 0, "gap rows never enter CAS staging");
}

#[tokio::test]
async fn cancelled_producer_fences_a_begin_that_committed_late() {
    let (_dir, state, pool, identity) = fixture().await;
    let (fence_probe, fence_entered, release_fence) = fence_probe::install(&state);
    let (entering, release) = pause_next_begin_ack_for_test(&state);
    let memory = state.proxy_memory_budget.reservation();
    let producer = ResponseArchiveProducer::begin(&state, identity, memory).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), entering)
        .await
        .expect("writer begin must commit before the deterministic pause")
        .unwrap();
    drop(producer);
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), fence_entered)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fence_probe.calls(), 1);
    release_fence.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let row = sqlx::query(
                "SELECT state, expires_at, updated_at FROM response_archive_spools WHERE request_id = $1",
            )
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if row.get::<String, _>("state") == "gap" {
                assert!(row.get::<i64, _>("expires_at") <= row.get::<i64, _>("updated_at"));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a late committed begin must be fenced after producer cancellation");
    assert_eq!(fence_probe.calls(), 1);
}

#[tokio::test]
async fn cancelled_writer_returns_success_only_when_its_single_fence_succeeds() {
    for fail_fence in [false, true] {
        let (_dir, state, pool, identity) = fixture().await;
        if fail_fence {
            sqlx::query("CREATE TRIGGER reject_cancel_fence BEFORE UPDATE OF state ON response_archive_spools WHEN NEW.state = 'gap' BEGIN SELECT RAISE(ABORT, 'synthetic fence failure'); END")
                .execute(&pool).await.unwrap();
        }
        let (probe, entered, release) = fence_probe::install(&state);
        let memory = state.proxy_memory_budget.reservation();
        let producer = ResponseArchiveProducer::begin(&state, identity, memory).unwrap();
        let cancelled = tokio::spawn(producer.cancel_and_wait_for_test());
        tokio::time::timeout(std::time::Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.calls(), 1);
        release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), cancelled)
            .await
            .unwrap()
            .unwrap()
            .expect("writer task must finish without a join failure");
        assert_eq!(
            result.is_err(),
            fail_fence,
            "cancellation is not a database error, but a failed fence is"
        );
        assert_eq!(probe.calls(), 1);
        let spool_state: String =
            sqlx::query_scalar("SELECT state FROM response_archive_spools WHERE request_id = $1")
                .bind(identity.request_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(spool_state, if fail_fence { "capturing" } else { "gap" });
    }
}

#[tokio::test]
async fn terminal_tail_survives_observer_cancellation_without_waiting_for_database_ack() {
    let (_dir, state, pool, identity) = fixture().await;
    let (entering, release) = pause_next_begin_ack_for_test(&state);
    let memory = state.proxy_memory_budget.reservation();
    let mut producer = ResponseArchiveProducer::begin(&state, identity, memory).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), entering)
        .await
        .expect("owned writer must commit begin before the test pause")
        .unwrap();
    assert!(
        producer.append(vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")]),
        "small frames merge in the preallocated partial chunk without waiting for the writer"
    );
    let state_before_terminal: String =
        sqlx::query_scalar("SELECT state FROM response_archive_spools WHERE request_id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state_before_terminal, "capturing");

    let settlement = producer
        .seal()
        .expect("terminal handoff must not wait for SQL");
    drop(settlement);
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let row = sqlx::query(
                "SELECT state, chunk_count, byte_count FROM response_archive_spools WHERE request_id = $1",
            )
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            if row.get::<String, _>("state") == "pending" {
                assert_eq!(row.get::<i64, _>("chunk_count"), 1);
                assert_eq!(row.get::<i64, _>("byte_count"), 6);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the runtime supervisor must persist and seal the accepted terminal tail");
}

#[tokio::test]
async fn owned_writer_queue_is_byte_bounded_and_releases_proxy_memory_after_gap() {
    let (_dir, state, pool, identity) = fixture().await;
    let (entering, release) = pause_next_begin_ack_for_test(&state);
    let baseline = state.proxy_memory_budget.snapshot().0;
    let memory = state.proxy_memory_budget.reservation();
    let mut producer = ResponseArchiveProducer::begin(&state, identity, memory).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), entering)
        .await
        .expect("writer begin must reach the deterministic pause")
        .unwrap();
    assert_eq!(
        state.proxy_memory_budget.snapshot().0 - baseline,
        super::CAPTURE_MEMORY_BYTES * crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT
    );

    let kib = Bytes::from(vec![b'x'; 1024]);
    assert!(
        producer.append(vec![kib.clone(); super::CAPTURE_QUEUE_CHUNKS * 64]),
        "many small frames must merge by bytes rather than consume message slots"
    );
    assert!(
        !producer.append(vec![kib; 64]),
        "a fourth complete queued chunk must fail closed while the writer is paused"
    );
    release.send(()).unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let state_name: Option<String> = sqlx::query_scalar(
                "SELECT state FROM response_archive_spools WHERE request_id = $1",
            )
            .bind(identity.request_id.to_string())
            .fetch_optional(&pool)
            .await
            .unwrap();
            if state_name.as_deref() == Some("gap") && producer.queue_memory_owners_for_test() == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queue failure must settle the writer and fence the spool");
    assert_eq!(
        state.proxy_memory_budget.snapshot().0 - baseline,
        super::CAPTURE_MEMORY_BYTES * crate::gateway_body::memory::CAPTURE_MEMORY_WEIGHT,
        "the live producer must keep the shared queue allocation charged after the writer exits"
    );
    assert!(
        !producer.append(vec![Bytes::from_static(b"late")]),
        "a producer must observe an already failed writer before buffering more bytes"
    );
    drop(producer);
    assert_eq!(
        state.proxy_memory_budget.snapshot().0,
        baseline,
        "the shared queue charge must release after both producer and writer settle"
    );
}

#[tokio::test]
async fn streaming_writer_batches_four_chunks_without_growing_its_memory_reservation() {
    assert_eq!(
        super::CAPTURE_DATABASE_BATCH_CHUNKS,
        super::CAPTURE_QUEUE_CHUNKS + 1
    );
    assert_eq!(
        super::CAPTURE_MEMORY_BYTES,
        (super::CAPTURE_DATABASE_BATCH_CHUNKS + 1) * super::CHUNK_BYTES,
        "the batch uses four complete-chunk permits plus the producer's partial chunk"
    );
    let maximum_ciphertext = super::cipher::sealed_len(super::CHUNK_BYTES).unwrap();
    let legacy_peak_bound = super::CAPTURE_MEMORY_BYTES + 2 * maximum_ciphertext;
    let batched_peak_bound =
        (super::CAPTURE_DATABASE_BATCH_CHUNKS + 1) * super::CHUNK_BYTES + 2 * maximum_ciphertext;
    assert_eq!(
        batched_peak_bound, legacy_peak_bound,
        "one-at-a-time sealing keeps the plaintext, base64 envelope, and ciphertext bound unchanged"
    );
    let (_dir, state, pool, identity) = fixture().await;
    let mut writer = ResponseArchiveProducer::begin_for_test(&state, identity)
        .await
        .unwrap();
    writer
        .append_for_test(vec![
            Bytes::from(vec![b'a'; super::CHUNK_BYTES]),
            Bytes::from(vec![b'b'; super::CHUNK_BYTES]),
            Bytes::from(vec![b'c'; super::CHUNK_BYTES]),
            Bytes::from(vec![b'd'; super::CHUNK_BYTES]),
        ])
        .await
        .unwrap();
    assert_eq!(
        writer.append_state_for_test(),
        (1, 4, (4 * super::CHUNK_BYTES) as i64)
    );
    writer.seal_for_test().await.unwrap();
    let row = sqlx::query(
        "SELECT state, chunk_count, byte_count FROM response_archive_spools WHERE request_id = $1",
    )
    .bind(identity.request_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("state"), "pending");
    assert_eq!(row.get::<i64, _>("chunk_count"), 4);
    assert_eq!(
        row.get::<i64, _>("byte_count"),
        (4 * super::CHUNK_BYTES) as i64
    );
}
