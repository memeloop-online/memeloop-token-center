use super::*;
use crate::archive_staging::{
    ArchiveStagingIntentDigest, ArchiveStagingKey, ArchiveStagingLeaseOwner, ArchiveStagingOwner,
    ArchiveStagingPurpose, BeginArchiveStagingInput, BeginArchiveStagingResult,
};

#[tokio::test]
async fn sqlite_terminal_preserves_response_binding_in_both_completion_orders() {
    let directory = tempfile::tempdir().unwrap();
    let database = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("archive-binding.db").display()
    ))
    .await
    .unwrap();
    database.migrate().await.unwrap();
    completion_orders(&database).await;
}

#[tokio::test]
async fn postgres_terminal_preserves_response_binding_in_both_completion_orders() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
    let schema = format!("archive_binding_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&database_url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect(isolated.as_str()).await.unwrap();
    database.migrate().await.unwrap();
    completion_orders(&database).await;
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
}

async fn bind_response(database: &Database, identity: ArchiveSpoolIdentity) -> String {
    let task = database
        .claim_response_archive_spool(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.identity, identity);
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(identity.request_id),
        ArchiveStagingPurpose::Response,
        Uuid::now_v7(),
    )
    .unwrap();
    let lease = match database
        .begin_archive_staging_attempt(BeginArchiveStagingInput {
            key,
            intent_digest: ArchiveStagingIntentDigest::new("a".repeat(64)).unwrap(),
            lease_token: Uuid::now_v7(),
            lease_owner: ArchiveStagingLeaseOwner::new("terminal-binding-test").unwrap(),
        })
        .await
        .unwrap()
    {
        BeginArchiveStagingResult::Created(lease) => lease,
        _ => panic!("fresh response staging owner"),
    };
    let locator = format!("{}/response.json", key.canonical_prefix());
    assert!(
        database
            .complete_response_archive_spool(&task, &lease, &locator)
            .await
            .unwrap()
    );
    locator
}

async fn completion_orders(database: &Database) {
    for metered in [false, true] {
        for deferred in [false, true] {
            // None is a genuine gap: preserving an existing binding must
            // never fabricate one when no archive writer completed.
            for archive_first in [Some(true), Some(false), None] {
                let (key, reservation, request_id) = admitted(database, metered).await;
                let identity = ArchiveSpoolIdentity {
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation_id: reservation.id,
                };
                sqlx::query(
                    "UPDATE request_records SET request_object = 'inline-json:{}' WHERE id = $1",
                )
                .bind(request_id.to_string())
                .execute(&database.pool)
                .await
                .unwrap();
                if archive_first.is_some() {
                    assert!(
                        database
                            .capture_buffered_archive_spool(
                                identity,
                                crate::response_archive_spool::BufferedArchivePurpose::Response,
                                &[ArchiveSpoolChunk {
                                    seq: 0,
                                    ciphertext: "opaque-test-ciphertext".into(),
                                    byte_count: 3,
                                }],
                            )
                            .await
                            .unwrap()
                    );
                }
                let gap = format!("gap://{request_id}/response");
                let mut expected = gap.clone();
                if archive_first == Some(true) {
                    expected = bind_response(database, identity).await;
                }
                let mut input = finish(&key, &reservation, request_id);
                input.response_object = &gap;
                let result = if deferred {
                    database.finish_proxy_request_deferred(input.clone()).await
                } else {
                    database.finish_proxy_request(input.clone()).await
                };
                assert!(matches!(
                    result.unwrap(),
                    FinishProxyRequestResult::Finished { .. }
                ));
                let stored: String =
                    sqlx::query_scalar("SELECT response_object FROM request_records WHERE id = $1")
                        .bind(request_id.to_string())
                        .fetch_one(&database.pool)
                        .await
                        .unwrap();
                assert_eq!(
                    stored, expected,
                    "terminal must not erase an earlier binding"
                );
                if archive_first == Some(false) {
                    expected = bind_response(database, identity).await;
                }
                let replay = if deferred {
                    database.finish_proxy_request_deferred(input).await
                } else {
                    database.finish_proxy_request(input).await
                };
                assert!(matches!(
                    replay.unwrap(),
                    FinishProxyRequestResult::AlreadyFinished { .. }
                ));
                if deferred {
                    let owner = Uuid::now_v7();
                    assert!(
                        database
                            .claim_terminal_projection_tasks(owner, 32)
                            .await
                            .unwrap()
                            .contains(&request_id)
                    );
                    assert!(
                        database
                            .project_claimed_terminal_projection_task(owner, request_id)
                            .await
                            .unwrap()
                    );
                }
                assert_counts(database, request_id, reservation.id, 1).await;
                let row = sqlx::query("SELECT response_object, status_code, error_code, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = $1")
                    .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
                assert_eq!(row.get::<String, _>("response_object"), expected);
                assert_eq!(row.get::<i64, _>("status_code"), 200);
                assert!(row.get::<Option<String>, _>("error_code").is_none());
                assert_eq!(row.get::<i64, _>("input_tokens"), 7);
                assert_eq!(row.get::<i64, _>("output_tokens"), 3);
                assert_eq!(row.get::<i64, _>("cost_micros"), 10);
                let events = database
                    .all_request_events_after(0, None, 500)
                    .await
                    .unwrap();
                let finished = events
                    .iter()
                    .find(|event| event.request_id == request_id && event.event_kind == "finished")
                    .unwrap();
                assert_eq!(
                    finished.archive_state,
                    if archive_first.is_some() {
                        crate::model::RequestArchiveState::Bound
                    } else {
                        crate::model::RequestArchiveState::Gap
                    }
                );
                if archive_first.is_some() {
                    let bound: String = sqlx::query_scalar("SELECT bound_locator FROM response_archive_spools WHERE request_id = $1 AND state = 'bound'")
                        .bind(request_id.to_string()).fetch_one(&database.pool).await.unwrap();
                    assert_eq!(bound, expected);
                }
            }
        }
    }
}
