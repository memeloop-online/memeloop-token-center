//! Run only in the separately authorized, empty Stage165 fixture database.
use super::*;
use sha2::{Digest, Sha256};

#[tokio::test]
#[ignore = "sole Stage owner supplies the isolated database, bucket and fixed synthetic inputs"]
async fn stage165_prepare_reviewed_exact_fixture() {
    let directory = std::path::PathBuf::from(
        std::env::var("MTC_STAGE_EXACT_INPUT_DIR").expect("fixed Stage input directory"),
    );
    let run_uid =
        Uuid::parse_str(&std::env::var("MTC_STAGE_EXACT_RUN_UID").expect("run-owned fixture UID"))
            .unwrap();
    let output = directory.join("prepared");
    std::fs::create_dir(&output).expect("a fixture intent must never be overwritten");
    let request = std::fs::read(directory.join("request.json")).unwrap();
    let response = std::fs::read(directory.join("response.json")).unwrap();
    let request_json: serde_json::Value = serde_json::from_slice(&request).unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(response_json["status"], "incomplete");
    let mut config = Config::from_env().unwrap();
    assert_eq!(
        config.s3_bucket.as_deref(),
        Some("mtc-exact-recovery-20261126")
    );
    assert!(matches!(
        config.archive_backend,
        crate::config::ArchiveBackend::S3
    ));
    assert!(
        config.training_s3.is_none(),
        "fixture cannot borrow another object backend"
    );
    config.run_migrations_on_start = false;
    let check = Database::connect(&config.database_url).await.unwrap();
    let database: String = sqlx::query_scalar("SELECT current_database()::text")
        .fetch_one(&check.pool)
        .await
        .unwrap();
    assert_eq!(database, "mtc_exact_recovery_20261126");
    let schema: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&check.pool)
        .await
        .unwrap();
    assert_eq!(schema, 165);
    let occupied: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM request_record_locators)")
        .fetch_one(&check.pool)
        .await
        .unwrap();
    assert!(
        !occupied,
        "this fixture cannot share a customer or previous fixture database"
    );
    check.close().await;
    let fixture = Fixture::new_with_runtime_config(
        config,
        false,
        None,
        None,
        ArchiveBeforeTerminal::Pending,
        true,
    )
    .await;
    let db = &fixture.state.db;
    let mut locators = Vec::new();
    for (purpose, body) in [
        (
            crate::archive_staging::ArchiveStagingPurpose::Request,
            request.as_slice(),
        ),
        (
            crate::archive_staging::ArchiveStagingPurpose::Response,
            response.as_slice(),
        ),
    ] {
        let attempt =
            crate::proxy_lifecycle::begin_proxy_archive_attempt(db, fixture.request_id, purpose)
                .await
                .unwrap();
        let original = if purpose == crate::archive_staging::ArchiveStagingPurpose::Response {
            let mut writer = fixture
                .state
                .archive
                .start_compressed_writer(&attempt.object_locator)
                .await
                .unwrap();
            writer.write(Bytes::copy_from_slice(body)).await.unwrap();
            writer.finish_staged().await.unwrap()
        } else {
            let mut writer = fixture
                .state
                .archive
                .start_writer(&attempt.object_locator)
                .await
                .unwrap();
            writer.write(Bytes::copy_from_slice(body)).await.unwrap();
            writer.finish_staged().await.unwrap()
        };
        assert!(
            db.bind_archive_staging_attempt(&attempt.lease, &original.object_locator)
                .await
                .unwrap()
        );
        locators.push(original.object_locator);
    }
    let created: i64 = sqlx::query_scalar(
        "SELECT created_at FROM request_record_locators WHERE id=$1 AND tenant_id=$2 AND key_id=$3",
    )
    .bind(fixture.request_id.to_string())
    .bind(fixture.key.tenant_id.to_string())
    .bind(fixture.key.key_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    // Only this empty synthetic record receives the real bound source locators.
    assert_eq!(sqlx::query("UPDATE request_records SET request_object=$1,response_object=$2 WHERE id=$3 AND created_at=$4 AND tenant_id=$5 AND key_id=$6")
        .bind(&locators[0]).bind(&locators[1]).bind(fixture.request_id.to_string()).bind(created).bind(fixture.key.tenant_id.to_string()).bind(fixture.key.key_id.to_string()).execute(&db.pool).await.unwrap().rows_affected(),1);
    // This is a synthetic legacy starting state, not a production repair.
    // Normalize only the never-prepared empty marker. ACK, lease timestamps,
    // attempts and financial records are never manufactured or changed here.
    let empty: String = sqlx::query_scalar(
        "SELECT semantic_snapshot_json FROM conversation_projection_outbox WHERE request_id=$1",
    )
    .bind(fixture.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let empty_decoded: ConversationSemanticSnapshot = serde_json::from_str(&empty).unwrap();
    assert!(empty_semantic_snapshot(&empty_decoded));
    let initialized=sqlx::query("UPDATE conversation_projection_outbox SET semantic_snapshot_json=NULL,lease_owner=NULL WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3 AND projected_at IS NULL AND attempts=0 AND semantic_snapshot_json=$4 AND semantic_payload_snapshot_json IS NULL AND lease_owner='terminal-v118' AND lease_expires_at=9223372036854775807 AND terminal_lease_owner IS NULL AND terminal_lease_expires_at IS NULL AND NOT EXISTS(SELECT 1 FROM conversation_observations WHERE request_id=$1) AND EXISTS(SELECT 1 FROM request_records r WHERE r.id=$1 AND r.tenant_id=$2 AND r.key_id=$3 AND r.created_at=$5 AND r.request_object=$6 AND r.response_object=$7)")
        .bind(fixture.request_id.to_string()).bind(fixture.key.tenant_id.to_string()).bind(fixture.key.key_id.to_string()).bind(&empty).bind(created).bind(&locators[0]).bind(&locators[1]).execute(&db.pool).await.unwrap().rows_affected();
    assert_eq!(initialized, 1);
    let owner = Uuid::now_v7();
    assert!(
        db.claim_exact_session_body_rebuild(
            fixture.key.tenant_id,
            fixture.key.key_id,
            fixture.request_id,
            None,
            &locators[0],
            &locators[1],
            owner,
            None,
            None
        )
        .await
        .unwrap()
    );
    let body = crate::conversation::session_body_from_documents(&request_json, &response_json);
    let (atoms, nodes) = body.into_prefix();
    let media = body.media_locators(&atoms);
    assert!(
        db.prepare_session_body_archive(owner, fixture.request_id, atoms, &nodes, &media)
            .await
            .unwrap()
    );
    let row = sqlx::query("SELECT projected_at,semantic_snapshot_json,lease_owner,lease_expires_at,terminal_lease_owner,terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3")
        .bind(fixture.request_id.to_string()).bind(fixture.key.tenant_id.to_string()).bind(fixture.key.key_id.to_string()).fetch_one(&db.pool).await.unwrap();
    let snapshot: String = row.try_get("semantic_snapshot_json").unwrap();
    assert!(
        row.try_get::<Option<i64>, _>("projected_at")
            .unwrap()
            .is_none()
    );
    let primary: String = row.try_get("lease_owner").unwrap();
    assert_eq!(
        primary,
        owner.to_string(),
        "reviewed fixture needs a real primary claim, not the metered sentinel"
    );
    let expiry: i64 = row.try_get("lease_expires_at").unwrap();
    let source = |index: usize, bytes: &[u8]| serde_json::json!({"locator":locators[index],"decoded_bytes":bytes.len(),"decoded_sha256":format!("{:x}",Sha256::digest(bytes))});
    let plan = serde_json::json!({"format_version":1,"tenant_id":fixture.key.tenant_id,"key_id":fixture.key.key_id,"request_id":fixture.request_id,"expected_projected_at":0,"request":source(0,&request),"response":source(1,&response)});
    let plan_bytes = serde_json::to_vec_pretty(&plan).unwrap();
    std::fs::write(output.join("plan.json"), &plan_bytes).unwrap();
    let prior = serde_json::json!({"format_version":1,"identity":blake3::hash(&plan_bytes).to_hex().to_string(),"request_id":fixture.request_id,"claim_intent":true,"claimed":true,"lease_owner":owner,"canonical_rebuilt":false,"financial_replay":false,"fixture_only":true,"fixture_run_uid":run_uid,"synthetic_empty_marker_normalized":true,"empty_snapshot_before":empty,"claim_method":"real_initial_exact_claim_after_synthetic_empty_marker_normalization","prepared_snapshot_sha256":format!("{:x}",Sha256::digest(snapshot.as_bytes())),"actual_lease_expires_at":expiry});
    std::fs::write(
        output.join("prior.json"),
        serde_json::to_vec_pretty(&prior).unwrap(),
    )
    .unwrap();
    let financial: serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('terminal',(to_jsonb(t)-'session_id'),'ledger',(SELECT jsonb_agg(to_jsonb(e)) FROM ledger_entries e WHERE e.source=t.reservation_id),'balance',(SELECT to_jsonb(a) FROM credit_accounts a WHERE a.id=t.account_id))::text FROM terminal_projection_outbox t WHERE t.request_id=$1")
        .bind(fixture.request_id.to_string()).fetch_one(&db.pool).await.map(|v:String|serde_json::from_str(&v).unwrap()).unwrap();
    std::fs::write(
        output.join("financial-before.json"),
        serde_json::to_vec_pretty(&financial).unwrap(),
    )
    .unwrap();
    println!(
        "stage_exact_prepared request_id={} owner={} ready_after_db_ms={} snapshot_sha256={}",
        fixture.request_id, owner, expiry, prior["prepared_snapshot_sha256"]
    );
    // Do not shorten the real lease or run the writer. The operator waits for
    // DB time to pass expiry, then separately inspects and reviews the handover.
}
