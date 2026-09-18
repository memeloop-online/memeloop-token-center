use super::*;
use memeloop_token_center::generation;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn configure(f: &ReconcileFixture, base_url: &str, submitted: bool, cooldown: i64) {
    let locator = f
        .state
        .archive
        .put_content(bytes::Bytes::from_static(b"{\"input\":{}}"))
        .await
        .unwrap();
    sqlx::query("UPDATE upstream_accounts SET driver = 'volcengine-seedance', config_json = $1 WHERE id = (SELECT upstream_account_id FROM generation_jobs WHERE id = $2)")
        .bind(json!({"base_url":base_url}).to_string()).bind(f.job.to_string()).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE generation_jobs SET driver = 'volcengine-seedance', request_object = $1, status = $2, upstream_job_id = $3 WHERE id = $4")
        .bind(locator).bind(if submitted { "running" } else { "queued" })
        .bind(if submitted { Some("media-provider-job") } else { None })
        .bind(f.job.to_string()).execute(&f.pool).await.unwrap();
    sqlx::query("INSERT INTO upstream_account_health (upstream_account_id,consecutive_failures,cooldown_until,probe_lease_until,probe_lease_token,credential_generation,transport_revision,last_failure_kind,updated_at) SELECT job.upstream_account_id,1,$1,0,'',1,account.updated_at,'connection',$2 FROM generation_jobs job JOIN upstream_accounts account ON account.id = job.upstream_account_id WHERE job.id = $3")
        .bind(cooldown).bind(unix_millis()).bind(f.job.to_string()).execute(&f.pool).await.unwrap();
}

#[tokio::test]
async fn queued_cooldown_is_zero_post_and_never_marks_submission_unknown() {
    let f = ReconcileFixture::new_with_submitting(false).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    configure(&f, &upstream.uri(), false, unix_millis() + 60_000).await;
    assert!(
        generation::process_one(&f.state, "cooldown-worker")
            .await
            .unwrap()
    );
    let row = sqlx::query("SELECT status, submission_nonce, upstream_job_id, error_code FROM generation_jobs WHERE id=$1")
        .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "queued");
    assert!(row.get::<Option<String>, _>("submission_nonce").is_none());
    assert!(row.get::<Option<String>, _>("upstream_job_id").is_none());
    assert_ne!(
        row.get::<Option<String>, _>("error_code").as_deref(),
        Some("shutdown_delivery_unknown")
    );
    upstream.verify().await;
}

#[tokio::test]
async fn existing_provider_id_only_polls_and_valid_pending_cas_heals_probe() {
    let f = ReconcileFixture::new_with_submitting(false).await;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/api/v3/contents/generations/tasks/media-provider-job",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status":"queued"})))
        .expect(1)
        .mount(&upstream)
        .await;
    configure(&f, &upstream.uri(), true, 0).await;
    assert!(
        generation::process_one(&f.state, "pending-worker")
            .await
            .unwrap()
    );
    let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_account_health WHERE upstream_account_id = (SELECT upstream_account_id FROM generation_jobs WHERE id=$1) AND consecutive_failures > 0")
        .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(failures, 0);
    upstream.verify().await;
}

#[tokio::test]
async fn losing_job_cas_after_valid_poll_never_heals_account() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = ReconcileFixture::new_with_submitting(false).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    configure(
        &f,
        &format!("http://{}", listener.local_addr().unwrap()),
        true,
        0,
    )
    .await;
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        assert!(request[..count].starts_with(b"GET "));
        entered.send(()).unwrap();
        released.await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"status\":\"queued\"}").await.unwrap();
    });
    let state = f.state.clone();
    let worker =
        tokio::spawn(async move { generation::process_one(&state, "losing-worker").await });
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        "UPDATE generation_jobs SET lease_owner='winning-worker',lease_expires_at=$1 WHERE id=$2",
    )
    .bind(unix_millis() + 60_000)
    .bind(f.job.to_string())
    .execute(&f.pool)
    .await
    .unwrap();
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    server.await.unwrap();
    let failures: i64 = sqlx::query_scalar("SELECT consecutive_failures FROM upstream_account_health WHERE upstream_account_id=(SELECT upstream_account_id FROM generation_jobs WHERE id=$1)")
        .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        failures, 1,
        "a valid response cannot override a lost durable job fence"
    );
}
