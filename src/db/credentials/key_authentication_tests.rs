use super::*;
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

impl LogCapture {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_env_filter("memeloop_token_center=debug")
            .with_writer(self.clone())
            .finish()
    }

    fn phases(&self, forbidden: &[&str]) -> Vec<serde_json::Value> {
        let output = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        for value in forbidden {
            assert!(!output.contains(value));
        }
        output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| {
                event["fields"]["message"] == "client key authentication database observation"
            })
            .collect()
    }
}

#[tokio::test]
async fn acquisition_timeout_is_distinct_from_query_failure_and_keeps_overload() {
    let directory = tempfile::tempdir().unwrap();
    let canary = "private-auth-pool-path-canary";
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join(format!("{canary}.db")).display()
    );
    let mut database = Database::connect_with_max(&url, 1).await.unwrap();
    database.pool = database
        .pool
        .options()
        .clone()
        .acquire_timeout(Duration::from_millis(20))
        .connect_lazy_with(database.pool.connect_options().as_ref().clone());
    let held = database.pool.acquire().await.unwrap();
    let credential = "private-auth-credential-canary";
    let pepper = b"private-auth-pepper-canary";
    let hash = format!("{:?}", crypto::hash_credential(credential, pepper).0);
    let capture = LogCapture::default();
    let request_id = Uuid::now_v7();
    let result = async {
        database
            .authenticate_key(credential, pepper)
            .instrument(tracing::info_span!("downstream_key_authentication", %request_id))
            .await
    }
    .with_subscriber(capture.subscriber())
    .await;
    assert!(matches!(
        result,
        Err(AppError::Capacity {
            code: "database_pool_exhausted",
            ..
        })
    ));
    let events = capture.phases(&[
        canary,
        &url,
        credential,
        "private-auth-pepper-canary",
        &hash,
    ]);
    assert_eq!(events.len(), 1);
    let fields = &events[0]["fields"];
    assert_eq!(fields["phase"], "acquire");
    assert_eq!(fields["outcome"], "failed");
    assert_eq!(fields["error_kind"], "pool_timeout");
    assert_eq!(fields["pool_role"], "serve");
    assert_eq!(fields["pool_max"], 1);
    assert_eq!(fields["pool_size"], 1);
    assert_eq!(fields["pool_idle"], 0);
    assert_eq!(fields["pool_in_use"], 1);
    assert_eq!(fields["acquire_timeout_ms"], 20);
    assert!(fields["acquire_elapsed_ms"].as_u64().unwrap() >= 20);
    assert!(fields["query_elapsed_ms"].is_null());
    assert_eq!(events[0]["level"], "WARN");
    assert_eq!(events[0]["span"]["request_id"], request_id.to_string());
    drop(held);
}

#[tokio::test]
async fn query_failure_follows_successful_acquisition_without_driver_text() {
    let database = Database::connect_with_max("sqlite::memory:", 1)
        .await
        .unwrap();
    let capture = LogCapture::default();
    let result = database
        .authenticate_key(
            "private-auth-query-credential",
            b"private-auth-query-pepper",
        )
        .with_subscriber(capture.subscriber())
        .await;
    assert!(matches!(result, Err(AppError::Internal { .. })));
    let events = capture.phases(&[
        "private-auth-query-credential",
        "private-auth-query-pepper",
        "no such table",
    ]);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["level"], "WARN");
    assert_eq!(events[0]["fields"]["phase"], "query");
    assert_eq!(events[0]["fields"]["outcome"], "failed");
    assert_eq!(events[0]["fields"]["error_kind"], "database");
    for event in events {
        assert!(event["fields"]["acquire_elapsed_ms"].is_u64());
        assert!(event["fields"]["query_elapsed_ms"].is_u64());
        assert_eq!(event["fields"]["pool_max"], 1);
        assert_eq!(event["fields"]["acquire_timeout_ms"], 10000);
    }
}

#[tokio::test]
async fn diagnostic_authentication_keeps_revocation_generation_and_tenant_checks() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("auth-security.db").display()
    );
    let database = Database::connect_with_max(&url, 1).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"private-auth-security-pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "private-auth-tenant".into(),
                principal_external_id: "private-auth-principal".into(),
                alias: "private-auth-alias".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let capture = LogCapture::default();
    sqlx::query("PRAGMA query_only = ON")
        .execute(&database.pool)
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .with_subscriber(capture.subscriber())
        .await
        .unwrap();
    assert_eq!(key.key_id, issued.key_id);
    let events = capture.phases(&[
        &issued.key,
        "private-auth-security-pepper",
        "private-auth-tenant",
        "private-auth-principal",
        "private-auth-alias",
    ]);
    assert_eq!(events.len(), 1);
    for event in events {
        assert_eq!(event["level"], "DEBUG");
        assert_eq!(event["fields"]["phase"], "query");
        assert!(event["fields"]["acquire_elapsed_ms"].is_u64());
        assert!(event["fields"]["query_elapsed_ms"].is_u64());
        assert_eq!(event["fields"]["outcome"], "completed");
    }
    sqlx::query("PRAGMA query_only = OFF")
        .execute(&database.pool)
        .await
        .unwrap();
    for (disable, restore) in [
        (
            "UPDATE key_credentials SET revoked_at = 1",
            "UPDATE key_credentials SET revoked_at = NULL",
        ),
        (
            "UPDATE key_records SET credential_generation = credential_generation + 1",
            "UPDATE key_records SET credential_generation = credential_generation - 1",
        ),
        (
            "UPDATE tenants SET status = 'archived'",
            "UPDATE tenants SET status = 'active'",
        ),
        (
            "UPDATE key_records SET status = 'suspended'",
            "UPDATE key_records SET status = 'active'",
        ),
    ] {
        sqlx::query(disable).execute(&database.pool).await.unwrap();
        assert!(matches!(
            database.authenticate_key(&issued.key, pepper).await,
            Err(AppError::Unauthorized)
        ));
        sqlx::query(restore).execute(&database.pool).await.unwrap();
    }
    assert!(matches!(
        database
            .authenticate_key(&issued.key, b"wrong-pepper")
            .await,
        Err(AppError::Unauthorized)
    ));
    database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
}
