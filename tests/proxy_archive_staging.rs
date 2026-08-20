use memeloop_token_center::{
    archive::ArchiveStore,
    archive_staging::{
        ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner,
        ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingState, ArchiveStagingWriteLease,
        BeginArchiveStagingInput, BeginArchiveStagingResult,
    },
    config::Config,
    db::{
        AttachProxyArchiveResult, CreateKeyInput, Database, FinishProxyRequest,
        FinishProxyRequestResult, StartProxyRequest,
    },
    error::AppError,
    model::{KeyPolicy, TokenUsage},
};
use rust_decimal::Decimal;
use sqlx::{AnyPool, any::AnyPoolOptions};
use uuid::Uuid;

fn staging_input(request_id: Uuid, purpose: ArchiveStagingPurpose) -> BeginArchiveStagingInput {
    let attempt_id = Uuid::now_v7();
    BeginArchiveStagingInput {
        key: ArchiveStagingKey::new(
            ArchiveStagingOwner::ProxyRequest(request_id),
            purpose,
            attempt_id,
        )
        .unwrap(),
        intent_digest: ArchiveStagingIntentDigest::new(format!("{:064x}", attempt_id.as_u128()))
            .unwrap(),
        lease_token: Uuid::now_v7(),
        lease_owner: ArchiveStagingLeaseOwner::new("proxy-staging-test").unwrap(),
    }
}

async fn begin(
    database: &Database,
    request_id: Uuid,
    purpose: ArchiveStagingPurpose,
) -> ArchiveStagingWriteLease {
    match database
        .begin_archive_staging_attempt(staging_input(request_id, purpose))
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        result => panic!("unexpected begin result: {result:?}"),
    }
}

async fn complete_object(archive: &ArchiveStore, lease: &ArchiveStagingWriteLease, body: &[u8]) {
    let locator = format!("{}/body", lease.key.canonical_prefix());
    let mut writer = archive.start_writer(&locator).await.unwrap();
    writer.write(body.to_vec().into()).await.unwrap();
    let staged = writer.finish_staged().await.unwrap();
    assert_eq!(staged.object_locator, locator);
}

#[tokio::test]
async fn sqlite_proxy_locators_and_staging_bindings_are_one_atomic_commit() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-archive-staging.db").display()
    );
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    sqlx::any::install_default_drivers();
    let inspection: AnyPool = AnyPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .unwrap();
    let archive = ArchiveStore::from_config(&Config::for_test(database_url))
        .await
        .unwrap();
    let pepper = b"proxy archive staging sqlite pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("proxy-staging-{}", Uuid::now_v7()),
                principal_external_id: "member".to_owned(),
                alias: "proxy-staging".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::from(10),
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
        .upsert_model_price("proxy-staging-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let placeholder = format!("gap://{request_id}/request");
    let request_lease = begin(&database, request_id, ArchiveStagingPurpose::Request).await;
    complete_object(&archive, &request_lease, b"private request body").await;
    assert_eq!(
        database
            .archive_staging_attempt(request_lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing,
        "a completed object is not published before its locator transaction"
    );
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: "proxy-staging-model",
            request_object: &placeholder,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let request_locator = format!("{}/body", request_lease.key.canonical_prefix());
    // Test-only SQL safety boundary: the attempt id is a typed UUID created by the service and
    // cannot contain SQL syntax. SQLite trigger definitions cannot use bind parameters.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER proxy_request_bind_fault BEFORE UPDATE OF state ON archive_staging_attempts WHEN NEW.attempt_id = '{}' BEGIN SELECT RAISE(ABORT, 'request bind fault'); END",
        request_lease.key.attempt_id
    )))
    .execute(&inspection)
    .await
    .unwrap();
    assert!(
        database
            .attach_proxy_request_archive_staged(
                request_id,
                key.tenant_id,
                reservation.id,
                &placeholder,
                &request_lease,
                &request_locator,
            )
            .await
            .is_err()
    );
    assert_eq!(
        database
            .archive_staging_attempt(request_lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT request_object FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&inspection)
            .await
            .unwrap(),
        placeholder
    );
    sqlx::query("DROP TRIGGER proxy_request_bind_fault")
        .execute(&inspection)
        .await
        .unwrap();
    assert_eq!(
        database
            .attach_proxy_request_archive_staged(
                request_id,
                key.tenant_id,
                reservation.id,
                &placeholder,
                &request_lease,
                &request_locator,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::Attached
    );
    assert_eq!(
        database
            .attach_proxy_request_archive_staged(
                request_id,
                key.tenant_id,
                reservation.id,
                &placeholder,
                &request_lease,
                &request_locator,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::AlreadyAttached
    );

    let response_lease = begin(&database, request_id, ArchiveStagingPurpose::Response).await;
    complete_object(&archive, &response_lease, b"private response body").await;
    let response_locator = format!("{}/body", response_lease.key.canonical_prefix());
    let finish = || FinishProxyRequest {
        request_id,
        tenant_id: key.tenant_id,
        reservation: &reservation,
        input_token_ceiling: 100,
        output_token_ceiling: 100,
        requested_service_tier: None,
        status_code: 200,
        duration_ms: 1,
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..TokenUsage::default()
        },
        charge_contract_ceiling: false,
        error_code: None,
        response_object: &response_locator,
        conversation: None,
    };
    // Test-only SQL safety boundary: `request_id` is a typed UUID, not external text, and SQLite
    // trigger definitions cannot use bind parameters.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER proxy_response_bind_fault BEFORE UPDATE OF response_object ON request_records WHEN NEW.id = '{request_id}' BEGIN SELECT RAISE(ABORT, 'response bind fault'); END"
    )))
    .execute(&inspection)
    .await
    .unwrap();
    assert!(
        database
            .finish_proxy_request_with_archive_staging(finish(), Some(&response_lease))
            .await
            .is_err()
    );
    assert_eq!(
        database
            .archive_staging_attempt(response_lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Writing,
        "a terminal SQL failure must roll back the staging bind"
    );
    sqlx::query("DROP TRIGGER proxy_response_bind_fault")
        .execute(&inspection)
        .await
        .unwrap();
    assert!(matches!(
        database
            .finish_proxy_request_with_archive_staging(finish(), Some(&response_lease))
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
    assert_eq!(
        database
            .archive_staging_attempt(response_lease.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::Bound
    );
    assert!(matches!(
        database
            .finish_proxy_request_with_archive_staging(finish(), Some(&response_lease))
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished { response_object, .. }
            if response_object == response_locator
    ));

    let loser = begin(&database, request_id, ArchiveStagingPurpose::Response).await;
    complete_object(&archive, &loser, b"terminal loser").await;
    let loser_locator = format!("{}/body", loser.key.canonical_prefix());
    let loser_finish = FinishProxyRequest {
        response_object: &loser_locator,
        ..finish()
    };
    assert!(matches!(
        database
            .finish_proxy_request_with_archive_staging(loser_finish, Some(&loser))
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished { response_object, .. }
            if response_object == response_locator
    ));
    assert!(
        database
            .abandon_archive_staging_attempt(&loser)
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .archive_staging_attempt(loser.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    );

    let fenced_request_id = Uuid::now_v7();
    let fenced = begin(&database, fenced_request_id, ArchiveStagingPurpose::Request).await;
    sqlx::query("UPDATE archive_staging_attempts SET lease_expires_at = 0 WHERE attempt_id = $1")
        .bind(fenced.key.attempt_id.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    assert!(
        !database
            .bind_archive_staging_attempt(
                &fenced,
                &format!("{}/body", fenced.key.canonical_prefix())
            )
            .await
            .unwrap()
    );
    assert_eq!(
        database
            .promote_stale_archive_staging_attempts()
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        database
            .archive_staging_attempt(fenced.key.attempt_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ArchiveStagingState::CleanupPending
    ));

    assert!(matches!(
        database
            .attach_proxy_request_archive_staged(
                request_id,
                key.tenant_id,
                reservation.id,
                &placeholder,
                &fenced,
                &format!("{}/body", fenced.key.canonical_prefix()),
            )
            .await,
        Err(AppError::BadRequest(_))
    ));
}
