use bytes::Bytes;
use sqlx::Row;
use uuid::Uuid;

use super::*;
use crate::{AppState, config::Config, db::ArchiveSpoolIdentity};

const PEPPER: &[u8] = b"existing-test-pepper-over-thirty-two-bytes";

#[tokio::test]
async fn both_buffered_purposes_recover_exact_bytes_only_after_terminal() {
    let (_dir, state, pool, identity) = fixture().await;
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
    assert_ne!(request_locator, response_locator);
    assert_eq!(state.archive.get(&request_locator).await.unwrap(), request);
    assert_eq!(
        state.archive.get(&response_locator).await.unwrap(),
        response
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
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("spool.db").display()
    );
    let state = AppState::initialize(Config::for_test(url.clone()))
        .await
        .unwrap();
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
    upload::process_one(&state, Uuid::new_v4()).await;
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
    upload::process_one(&state, Uuid::new_v4()).await;
    let locator: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id=$1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(locator, format!("gap://{}/response", identity.request_id));
}

#[tokio::test]
async fn late_begin_ack_is_fenced_as_gap_and_cannot_leave_a_capturing_spool() {
    let (_dir, state, pool, identity) = fixture().await;
    let (entering, release) = pause_next_begin_ack_for_test(&state);
    let begin_state = state.clone();
    let begin =
        tokio::spawn(async move { ResponseArchiveProducer::begin(&begin_state, identity).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), entering)
        .await
        .expect("begin transaction must commit before its ACK is paused")
        .unwrap();
    let state_before_timeout: String =
        sqlx::query_scalar("SELECT state FROM response_archive_spools WHERE request_id = $1")
            .bind(identity.request_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state_before_timeout, "capturing");

    // Freeze time only after the real SQLite transaction committed and the
    // acknowledgement seam stopped the task from completing. No database I/O
    // runs while Tokio is allowed to auto-advance the clock.
    tokio::time::pause();
    tokio::time::advance(ACK_TIMEOUT).await;
    assert!(begin.await.unwrap().is_none());
    tokio::time::resume();
    release.send(()).unwrap();
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
    .expect("unobserved successful begin must be made immediately GC-eligible");
}
