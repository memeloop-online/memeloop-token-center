use super::*;

async fn replay_retention_enqueue(fixture: &Fixture, observed_at: i64) {
    let hints = ConversationHints::default();
    let mut tx = fixture.state.db.begin_write_transaction().await.unwrap();
    enqueue_conversation_projection_in_transaction(
        &mut tx,
        ConversationProjectionEnqueueInput {
            request_id: fixture.request_id,
            key: &fixture.key,
            hints: &hints,
            client_name: None,
            upstream_response_id: None,
            observed_at,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

pub(super) async fn expired_outbox_contract(
    fixture: &Fixture,
    leaf: &str,
    frozen: &crate::model::RequestArchiveRefs,
    resolved: &crate::model::RequestArchiveRefs,
    other_locator: &crate::model::RequestArchiveRefs,
) {
    let db = &fixture.state.db;
    let immutable = serde_json::to_vec(frozen).unwrap();
    let unmatched_bytes = serde_json::to_vec(other_locator).unwrap();
    let authority_before = fixture
        .state
        .archive
        .semantic_archive_authority(fixture.key.tenant_id, fixture.request_id, leaf)
        .await
        .unwrap()
        .unwrap();
    // The proof is deliberately paused before its original owner row
    // fence. A claim, a new source/revision, or a changed row generation
    // must invalidate deletion without losing its recovery obligation.
    let original: (String, i64) = sqlx::query_as("SELECT COALESCE(semantic_payload_snapshot_json,semantic_snapshot_json),projected_at FROM conversation_projection_outbox WHERE request_id=$1")
    .bind(fixture.request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    for mutation in ["claim", "source_revision", "row_generation", "leaf"] {
        let gate = std::sync::Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let deleting = super::super::retention::with_retention_fence_gate(
            gate.clone(),
            db.delete_durably_retired_conversation_outbox(100, i64::MAX),
        );
        let change = async {
            gate.0.notified().await;
            match mutation {
                "claim" => {
                    sqlx::query("UPDATE conversation_projection_outbox SET lease_owner='session-body-rebuild',lease_expires_at=9223372036854775807 WHERE request_id=$1")
                    .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
                }
                "source_revision" => {
                    let mut changed: serde_json::Value = serde_json::from_str(&original.0).unwrap();
                    changed["semantic_coverage_pending"]["revision"] =
                        serde_json::json!(authority_before.publication.revision + 1);
                    sqlx::query("UPDATE conversation_projection_outbox SET semantic_snapshot_json=$2,semantic_payload_snapshot_json=NULL WHERE request_id=$1")
                    .bind(fixture.request_id.to_string()).bind(changed.to_string()).execute(&db.pool).await.unwrap();
                }
                "row_generation" => {
                    sqlx::query("UPDATE conversation_projection_outbox SET projected_at=projected_at+1 WHERE request_id=$1")
                    .bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
                }
                "leaf" => {
                    sqlx::query("UPDATE conversation_observations SET leaf_node_hash=$2 WHERE request_id=$1")
                    .bind(fixture.request_id.to_string()).bind("0".repeat(64)).execute(&db.pool).await.unwrap();
                }
                _ => unreachable!(),
            }
            gate.1.notify_one();
        };
        let (deleted, ()) = tokio::join!(deleting, change);
        assert_eq!(
            deleted.unwrap(),
            0,
            "changed {mutation} must retain the row"
        );
        sqlx::query("UPDATE conversation_projection_outbox SET semantic_snapshot_json=$2,semantic_payload_snapshot_json=NULL,projected_at=$3,lease_owner=NULL,lease_expires_at=NULL WHERE request_id=$1")
        .bind(fixture.request_id.to_string()).bind(&original.0).bind(original.1).execute(&db.pool).await.unwrap();
        sqlx::query("UPDATE conversation_observations SET leaf_node_hash=$2 WHERE request_id=$1")
            .bind(fixture.request_id.to_string())
            .bind(leaf)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let observed_at: i64 = sqlx::query_scalar(
        "SELECT observed_at FROM conversation_projection_outbox WHERE request_id=$1",
    )
    .bind(fixture.request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let ledger_before: (i64,i64)=sqlx::query_as("SELECT COUNT(*),CAST(COALESCE(SUM(amount_micros),0) AS BIGINT) FROM ledger_entries WHERE kind='usage' AND source=$1")
    .bind(fixture.identity.reservation_id.to_string()).fetch_one(&db.pool).await.unwrap();
    // A late same-identity enqueue before retention is an idempotent replay,
    // not permission to erase or re-ack another generation.
    replay_retention_enqueue(fixture, observed_at).await;
    // Unknown old rows cannot starve a later proven row. Only metadata
    // pages are retained between ticks, not their original snapshot bytes.
    let mut unknown_rows = Vec::new();
    for _ in 0..35 {
        let unknown = Uuid::now_v7();
        unknown_rows.push(unknown);
        sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,key_snapshot_json,semantic_snapshot_json,projected_at,statistics_outcome) SELECT $1,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,key_snapshot_json,$2,$3,statistics_outcome FROM conversation_projection_outbox WHERE request_id=$4")
        .bind(unknown.to_string()).bind("x".repeat(16384)).bind(original.1-1).bind(fixture.request_id.to_string()).execute(&db.pool).await.unwrap();
    }
    let mut scan = (None, None);
    let mut removed = 0;
    for _ in 0..4 {
        removed += db
            .delete_retired_outbox_page(10, i64::MAX, &mut scan)
            .await
            .unwrap();
    }
    assert_eq!(
        removed, 1,
        "metadata cursor must pass unknown heads and reach the proven row"
    );
    for unknown in unknown_rows {
        sqlx::query("DELETE FROM conversation_projection_outbox WHERE request_id=$1")
            .bind(unknown.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let authority_after = fixture
        .state
        .archive
        .semantic_archive_authority(fixture.key.tenant_id, fixture.request_id, leaf)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        authority_before.publication.revision,
        authority_after.publication.revision
    );
    assert_eq!(
        authority_before.semantic_head.node_hash,
        authority_after.semantic_head.node_hash
    );
    let after_expiry = db
        .resolve_history_raw_archive_roots(
            fixture.key.tenant_id,
            fixture.key.key_id,
            frozen.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&after_expiry).unwrap(),
        serde_json::to_vec(&resolved).unwrap(),
        "history authority must survive expiry of its SQL recovery bridge"
    );
    assert_eq!(serde_json::to_vec(&frozen).unwrap(), immutable);
    let unmatched_after_expiry = db
        .resolve_history_raw_archive_roots(
            fixture.key.tenant_id,
            fixture.key.key_id,
            other_locator.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&unmatched_after_expiry).unwrap(),
        unmatched_bytes,
        "retirement of source B never substitutes for frozen Bound A"
    );
    let history_identity = crate::request_history::HistoryIdentity {
        tenant_id: fixture.key.tenant_id,
        key_id: fixture.key.key_id,
        request_id: fixture.request_id,
        created_at: frozen.view.created_at,
        source: crate::request_history::HistorySource::Native
            .as_str()
            .to_owned(),
    };
    let native_after = db
        .resolve_history_snapshot_archive(&history_identity, frozen.clone())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&native_after).unwrap(),
        serde_json::to_vec(&resolved).unwrap()
    );
    let mut imported_identity = history_identity.clone();
    imported_identity.source = crate::request_history::HistorySource::Archive
        .as_str()
        .to_owned();
    let imported = db
        .resolve_history_snapshot_archive(&imported_identity, frozen.clone())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&imported).unwrap(),
        immutable,
        "a scoped imported source with identical UUID is never native authority"
    );
    let mut stale_identity = history_identity.clone();
    stale_identity.created_at += 1;
    let stale = db
        .resolve_history_snapshot_archive(&stale_identity, frozen.clone())
        .await
        .unwrap();
    assert_eq!(serde_json::to_vec(&stale).unwrap(), immutable);
    let mut stale_record = frozen.clone();
    stale_record.view.created_at = stale_identity.created_at;
    let stale_bytes = serde_json::to_vec(&stale_record).unwrap();
    let stale = db
        .resolve_history_snapshot_archive(&stale_identity, stale_record)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&stale).unwrap(),
        stale_bytes,
        "matching stale frozen fields cannot replace the authoritative native locator"
    );
    let manifest_after = fixture
        .state
        .archive
        .get_semantic_manifest_streamed(
            fixture.key.tenant_id,
            &authority_after.publication.manifest,
        )
        .await
        .unwrap();
    let sources_after = manifest_after.raw_sources.unwrap();
    for source in [sources_after.request, sources_after.response.unwrap()] {
        fixture
            .state
            .archive
            .verify_tenant_content(fixture.key.tenant_id, &source)
            .await
            .unwrap();
    }
    // Delete winning first leaves no stale ACK: the original enqueue owns
    // a fresh pending snapshot/lease, and a second replay keeps that task.
    replay_retention_enqueue(fixture, observed_at).await;
    replay_retention_enqueue(fixture, observed_at).await;
    let recreated:(Option<i64>,String,i64)=sqlx::query_as("SELECT projected_at,lease_owner,lease_expires_at FROM conversation_projection_outbox WHERE request_id=$1")
    .bind(fixture.request_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(recreated, (None, "terminal-v118".to_owned(), i64::MAX));
    assert_eq!(
        db.delete_durably_retired_conversation_outbox(100, i64::MAX)
            .await
            .unwrap(),
        0
    );
    let ledger_after: (i64,i64)=sqlx::query_as("SELECT COUNT(*),CAST(COALESCE(SUM(amount_micros),0) AS BIGINT) FROM ledger_entries WHERE kind='usage' AND source=$1")
    .bind(fixture.identity.reservation_id.to_string()).fetch_one(&db.pool).await.unwrap();
    assert_eq!(
        ledger_after, ledger_before,
        "retention/enqueue never replay financial settlement"
    );
    sqlx::query("DELETE FROM conversation_projection_outbox WHERE request_id=$1")
        .bind(fixture.request_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
}
