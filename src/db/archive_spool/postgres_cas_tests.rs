use super::*;
use crate::archive_staging::ArchiveStagingState;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

#[tokio::test]
async fn postgres_cas_commit_disconnect_rolls_back_or_replays_the_exact_committed_receipt() {
    let Some(fixture) = PgFixture::new_with_schema(true).await else {
        return;
    };
    let key_id = Uuid::new_v4();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id) VALUES ($1, $2, $3, 1, 'responses', 'cas-commit-fault', 45, 67, 123, 'inline-json:{}', $4)")
        .bind(fixture.id.request_id.to_string()).bind(fixture.id.tenant_id.to_string())
        .bind(key_id.to_string()).bind(fixture.id.reservation_id.to_string())
        .execute(&fixture.db.pool).await.unwrap();
    sqlx::query("INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, 1, $2, $3)")
        .bind(fixture.id.request_id.to_string()).bind(fixture.id.tenant_id.to_string())
        .bind(key_id.to_string()).execute(&fixture.db.pool).await.unwrap();
    fixture.capture().await;
    assert!(
        fixture
            .db
            .seal_response_archive_spool(fixture.id, 1, 1)
            .await
            .unwrap()
    );
    fixture.terminal().await;
    let task = fixture
        .db
        .claim_response_archive_spool(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(fixture.id.request_id),
        ArchiveStagingPurpose::Response,
        Uuid::new_v4(),
    )
    .unwrap();
    let lease = match fixture
        .db
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::new_v4(),
            lease_owner: ArchiveStagingLeaseOwner::new("pg-cas-commit-fault").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        _ => panic!("expected fresh staging lease"),
    };
    let staging_before = fixture
        .db
        .archive_staging_attempt(key.attempt_id)
        .await
        .unwrap()
        .unwrap();
    let digest = blake3::hash(b"x").to_hex().to_string();
    let locator = format!(
        "tenants/{}/cas/v1/blake3/{}/{}",
        fixture.id.tenant_id,
        &digest[..2],
        digest
    );
    fixture.install_commit_barrier().await;
    let mut blocker = fixture.admin.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let database = fixture.db.clone();
    let worker_task = task.clone();
    let worker_lease = lease.clone();
    let worker_locator = locator.clone();
    let interrupted = tokio::spawn(async move {
        database
            .complete_response_archive_spool_cas(&worker_task, &worker_lease, &worker_locator)
            .await
    });
    fixture.wait_for_commit().await;
    let killed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT pg_terminate_backend(pid) AS terminated FROM pg_stat_activity WHERE application_name = $1 AND UPPER(query) LIKE 'COMMIT%' AND wait_event_type = 'Lock' AND wait_event = 'advisory') stopped WHERE terminated")
        .bind(&fixture.schema).fetch_one(&fixture.admin).await.unwrap();
    assert_eq!(killed, 1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), interrupted)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    fixture.wait_state("uploading").await;
    let current: String =
        sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
            .bind(fixture.id.request_id.to_string())
            .fetch_one(&fixture.db.pool)
            .await
            .unwrap();
    assert_eq!(current, format!("gap://{}/response", fixture.id.request_id));
    assert_eq!(
        fixture
            .db
            .archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap(),
        staging_before
    );
    let events: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_events WHERE request_id = $1")
            .bind(fixture.id.request_id.to_string())
            .fetch_one(&fixture.db.pool)
            .await
            .unwrap();
    assert_eq!(events, 0);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut relay_url = url::Url::parse(&fixture.url).unwrap();
    let upstream_host = relay_url.host_str().unwrap().to_owned();
    let upstream_port = relay_url.port().unwrap_or(5432);
    relay_url.set_host(Some("127.0.0.1")).unwrap();
    relay_url
        .set_port(Some(listener.local_addr().unwrap().port()))
        .unwrap();
    let parameters: Vec<(String, String)> = relay_url
        .query_pairs()
        .filter(|(name, _)| name != "sslmode")
        .map(|pair| (pair.0.into_owned(), pair.1.into_owned()))
        .collect();
    relay_url.set_query(None);
    relay_url
        .query_pairs_mut()
        .extend_pairs(parameters)
        .append_pair("sslmode", "disable");
    let (suppress, suppress_receiver) = oneshot::channel();
    let (suppressed, suppression_ready) = oneshot::channel();
    let (server_reply, saw_server_reply) = oneshot::channel();
    let (disconnect, disconnect_receiver) = oneshot::channel();
    let relay = tokio::spawn(async move {
        let (mut downstream, _) = listener.accept().await.unwrap();
        let mut upstream = TcpStream::connect((upstream_host.as_str(), upstream_port))
            .await
            .unwrap();
        let (mut client_read, mut client_write) = downstream.split();
        let (mut server_read, mut server_write) = upstream.split();
        let forward_requests = tokio::io::copy(&mut client_read, &mut server_write);
        let suppress_responses = async {
            tokio::select! {
                _ = tokio::io::copy(&mut server_read, &mut client_write) => panic!("connection ended before commit fault was armed"),
                result = suppress_receiver => result.unwrap(),
            }
            suppressed.send(()).unwrap();
            let mut reply = [0_u8; 1024];
            assert!(server_read.read(&mut reply).await.unwrap() > 0);
            server_reply.send(()).unwrap();
            tokio::io::copy(&mut server_read, &mut tokio::io::sink())
                .await
                .unwrap();
        };
        tokio::select! {
            _ = forward_requests => panic!("client disconnected before explicit commit-ACK fault"),
            () = suppress_responses => panic!("server disconnected before explicit commit-ACK fault"),
            result = disconnect_receiver => result.unwrap(),
        }
    });
    let mut database = fixture.db.clone();
    database.pool = schema_pool(relay_url.as_str(), &fixture.schema).await;
    let fault_pool = database.pool.clone();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    let worker_task = task.clone();
    let worker_lease = lease.clone();
    let worker_locator = locator.clone();
    let unacknowledged = tokio::spawn(async move {
        database
            .complete_response_archive_spool_cas(&worker_task, &worker_lease, &worker_locator)
            .await
    });
    fixture.wait_for_commit().await;
    suppress.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), suppression_ready)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(fixture.gate)
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    fixture.wait_state("bound").await;
    tokio::time::timeout(Duration::from_secs(5), saw_server_reply)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !unacknowledged.is_finished(),
        "server committed while its reply was suppressed"
    );
    disconnect.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), relay)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), unacknowledged)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fault_pool.close().await;

    let staging_committed = fixture
        .db
        .archive_staging_attempt(key.attempt_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(staging_committed.key, key);
    assert_eq!(staging_committed.state, ArchiveStagingState::CleanupPending);
    assert_eq!(staging_committed.bound_locator, None);
    assert!(staging_committed.bound_at.is_some());
    assert_eq!(
        staging_committed.next_cleanup_at,
        staging_committed.bound_at
    );
    let events_before: Vec<(String, String, String, String, String)> = sqlx::query_as("SELECT event_id, event_kind, tenant_id, key_id, request_id FROM request_events WHERE request_id = $1 ORDER BY event_id")
        .bind(fixture.id.request_id.to_string()).fetch_all(&fixture.db.pool).await.unwrap();
    assert_eq!(events_before.len(), 1);
    assert_eq!(events_before[0].1, "archive_bound");
    assert_eq!(events_before[0].2, fixture.id.tenant_id.to_string());
    assert_eq!(events_before[0].3, key_id.to_string());
    assert_eq!(events_before[0].4, fixture.id.request_id.to_string());
    let mut restarted = fixture.db.clone();
    restarted.pool = schema_pool(&fixture.url, &fixture.schema).await;
    for _ in 0..2 {
        assert!(
            restarted
                .complete_response_archive_spool_cas(&task, &lease, &locator)
                .await
                .unwrap()
        );
    }
    restarted
        .retry_response_archive_spool(&task, "upload_failed")
        .await
        .unwrap();
    restarted
        .fail_response_archive_spool(fixture.id, "capture_failed")
        .await
        .unwrap();
    let row = sqlx::query("SELECT r.response_object, r.status_code, r.cost_micros, r.input_tokens, r.output_tokens, s.state, s.bound_locator, s.lease_owner, s.lease_token, s.lease_expires_at FROM request_records r JOIN response_archive_spools s ON s.request_id = r.id WHERE r.id = $1")
        .bind(fixture.id.request_id.to_string()).fetch_one(&restarted.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("response_object"), locator);
    assert_eq!(row.get::<String, _>("bound_locator"), locator);
    assert_eq!(row.get::<String, _>("state"), "bound");
    assert_eq!(row.get::<Option<String>, _>("lease_owner"), None);
    assert_eq!(row.get::<Option<String>, _>("lease_token"), None);
    assert_eq!(row.get::<Option<i64>, _>("lease_expires_at"), None);
    assert_eq!(row.get::<i64, _>("status_code"), 200);
    assert_eq!(row.get::<i64, _>("cost_micros"), 123);
    assert_eq!(row.get::<i64, _>("input_tokens"), 45);
    assert_eq!(row.get::<i64, _>("output_tokens"), 67);
    assert_eq!(
        restarted
            .archive_staging_attempt(key.attempt_id)
            .await
            .unwrap()
            .unwrap(),
        staging_committed
    );
    let events_after: Vec<(String, String, String, String, String)> = sqlx::query_as("SELECT event_id, event_kind, tenant_id, key_id, request_id FROM request_events WHERE request_id = $1 ORDER BY event_id")
        .bind(fixture.id.request_id.to_string()).fetch_all(&restarted.pool).await.unwrap();
    assert_eq!(events_after, events_before);
    restarted.pool.close().await;
    fixture.finish().await;
}
