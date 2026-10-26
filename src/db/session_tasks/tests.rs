use super::*;
use crate::conversation::ConversationHints;
use crate::db::requests::conversations::prefix_storage::tests::{key, observe};

async fn session(
    db: &Database,
    key: &crate::model::AuthenticatedKey,
    thread: Uuid,
) -> SessionIdentity {
    let session_id = observe(
        db,
        key,
        &json!({"input":[{"role":"user","content":thread.to_string()}]}),
        &ConversationHints {
            session_id: Some(thread.to_string()),
            ..Default::default()
        },
        Uuid::now_v7(),
        1,
        true,
    )
    .await;
    SessionIdentity {
        key_id: key.key_id,
        session_id,
    }
}

async fn fact(db: &Database, tenant: Uuid, identity: &SessionIdentity, created_at: i64) {
    sqlx::query("INSERT INTO request_stats_facts (request_id,tenant_id,key_id,session_id,created_at,model,protocol,status_class,error_code,upstream_account_id,model_route_id,duration_ms,input_tokens,output_tokens,currency,cost_micros) VALUES ($1,$2,$3,$4,$5,'model','openai','success','','','',17,10,20,'USD',7)")
        .bind(Uuid::now_v7().to_string()).bind(tenant.to_string()).bind(identity.key_id.to_string()).bind(identity.session_id.to_string()).bind(created_at).execute(&db.pool).await.unwrap();
}

#[tokio::test]
async fn exact_identity_name_cas_and_manual_priority() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("identity.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let key = key(&db, "identity").await;
    let thread = Uuid::now_v7();
    let identity = session(&db, &key, thread).await;
    let resolved = db
        .resolve_session_threads(key.tenant_id, None, &[thread.to_string()])
        .await
        .unwrap();
    assert_eq!(
        resolved["sessions"][0]["session_id"],
        json!(identity.session_id)
    );
    assert_ne!(identity.session_id, thread);
    let before = db
        .session_title(key.tenant_id, key.key_id, identity.session_id)
        .await
        .unwrap();
    let task = db
        .claim_session_title(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    let updated = db
        .update_session_name(
            key.tenant_id,
            &identity,
            SessionNameUpdate {
                title: "同名任务".into(),
                source: "codex".into(),
                explicit_session_id: Some(thread.to_string()),
                expected_revision: before.revision,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.source, "codex");
    db.finish_session_title(&task, Some("stale generation"), None)
        .await
        .unwrap();
    assert_eq!(
        db.session_title(key.tenant_id, key.key_id, identity.session_id)
            .await
            .unwrap()
            .title
            .as_deref(),
        Some("同名任务")
    );
    assert!(
        db.update_session_name(
            key.tenant_id,
            &identity,
            SessionNameUpdate {
                title: "stale sync".into(),
                source: "codex".into(),
                explicit_session_id: Some(thread.to_string()),
                expected_revision: before.revision,
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    let manual = db
        .update_session_name(
            key.tenant_id,
            &identity,
            SessionNameUpdate {
                title: "人工名称".into(),
                source: "manual".into(),
                explicit_session_id: None,
                expected_revision: updated.revision,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        db.update_session_name(
            key.tenant_id,
            &identity,
            SessionNameUpdate {
                title: "automatic overwrite".into(),
                source: "codex".into(),
                explicit_session_id: Some(thread.to_string()),
                expected_revision: manual.revision,
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    db.request_session_title(key.tenant_id, key.key_id, identity.session_id)
        .await
        .unwrap();
    assert!(
        db.claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.enqueue_unnamed_session_titles(key.tenant_id, &[identity.clone()])
            .await
            .unwrap()["queued"],
        0
    );
    assert_eq!(
        db.resolve_session_threads(Uuid::now_v7(), None, &[thread.to_string()])
            .await
            .unwrap()["sessions"],
        json!([])
    );
}

#[tokio::test]
async fn subtree_union_days_and_related_changes_do_not_copy_facts() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("union.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let key = key(&db, "union").await;
    let parent = session(&db, &key, Uuid::now_v7()).await;
    let child = session(&db, &key, Uuid::now_v7()).await;
    let other_key =
        crate::db::requests::conversations::prefix_storage::tests::key(&db, "foreign").await;
    let foreign = session(&db, &other_key, Uuid::now_v7()).await;
    fact(&db, key.tenant_id, &parent, 1).await;
    fact(&db, key.tenant_id, &child, 2 * 86400000 + 1).await;
    db.change_session_relationship(
        key.tenant_id,
        &child,
        &parent,
        "parent",
        Some("thread_spawn_edges"),
        false,
    )
    .await
    .unwrap();
    assert!(
        db.change_session_relationship(
            key.tenant_id,
            &parent,
            &child,
            "parent",
            Some("thread_spawn_edges"),
            false
        )
        .await
        .is_err()
    );
    assert!(
        db.change_session_relationship(key.tenant_id, &parent, &foreign, "related", None, false)
            .await
            .is_err()
    );
    db.change_session_relationship(key.tenant_id, &parent, &child, "related", None, false)
        .await
        .unwrap();
    let subtree = db
        .analyze_session_tasks(
            key.tenant_id,
            &[parent.clone()],
            "subtree",
            None,
            &SessionTaskOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(subtree["totals"]["requests"], 2);
    assert_eq!(subtree["calendar_span_days"], 3);
    assert_eq!(subtree["active_days"], 2);
    assert_eq!(subtree["totals"]["request_duration_sum_ms"], 34);
    assert_eq!(
        db.analyze_session_tasks(
            key.tenant_id,
            &[child.clone()],
            "session",
            None,
            &SessionTaskOptions::default()
        )
        .await
        .unwrap()["totals"]["requests"],
        1
    );
    assert_eq!(
        db.analyze_session_tasks(
            key.tenant_id,
            &[parent.clone(), child.clone(), parent.clone()],
            "related",
            None,
            &SessionTaskOptions::default()
        )
        .await
        .unwrap()["totals"]["requests"],
        2
    );
    db.change_session_relationship(key.tenant_id, &parent, &child, "related", None, true)
        .await
        .unwrap();
    assert_eq!(
        db.analyze_session_tasks(
            key.tenant_id,
            &[parent.clone()],
            "subtree",
            None,
            &SessionTaskOptions::default()
        )
        .await
        .unwrap()["totals"]["requests"],
        2
    );
    assert!(
        db.analyze_session_tasks(
            key.tenant_id,
            &[parent],
            "session",
            Some(Uuid::now_v7()),
            &SessionTaskOptions::default()
        )
        .await
        .is_err()
    );
    let facts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_stats_facts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(facts, 2);
}
