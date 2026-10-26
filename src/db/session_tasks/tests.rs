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
        db.enqueue_unnamed_session_titles(key.tenant_id, std::slice::from_ref(&identity))
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
            std::slice::from_ref(&parent),
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
            std::slice::from_ref(&child),
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
            std::slice::from_ref(&parent),
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

#[tokio::test]
async fn metadata_batches_exceed_one_hundred_and_retain_unresolved_parents() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("batches.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let owner = key(&db, "batches").await;
    let parent_thread = Uuid::now_v7();
    let child_thread = Uuid::now_v7();
    let edges = [ThreadParent {
        child_thread_id: child_thread,
        parent_thread_id: parent_thread,
        evidence: "thread_spawn_edges".into(),
    }];
    assert_eq!(
        db.import_session_parents(owner.tenant_id, &edges)
            .await
            .unwrap()["linked"],
        0
    );
    let parent = session(&db, &owner, parent_thread).await;
    let child = session(&db, &owner, child_thread).await;
    let mut identities = vec![parent.clone(), child.clone()];
    let mut threads = vec![parent_thread.to_string(), child_thread.to_string()];
    for _ in 0..99 {
        let thread = Uuid::now_v7();
        identities.push(session(&db, &owner, thread).await);
        threads.push(thread.to_string());
    }
    let resolved = db
        .resolve_session_threads(owner.tenant_id, None, &threads)
        .await
        .unwrap();
    assert_eq!(resolved["sessions"].as_array().unwrap().len(), 101);
    let batch = db
        .enqueue_unnamed_session_titles(owner.tenant_id, &identities)
        .await
        .unwrap();
    assert_eq!(batch["results"].as_array().unwrap().len(), 101);
    assert!(
        batch["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "already_queued")
    );
    assert_eq!(
        db.import_session_parents(owner.tenant_id, &edges)
            .await
            .unwrap()["linked"],
        1
    );
    assert_eq!(
        db.import_session_parents(owner.tenant_id, &edges)
            .await
            .unwrap()["linked"],
        0
    );
    for identity in &identities {
        fact(&db, owner.tenant_id, identity, 86_400_001).await;
    }
    let analysis = db
        .analyze_session_tasks(
            owner.tenant_id,
            &identities,
            "related",
            None,
            &SessionTaskOptions {
                include_subtrees: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(analysis["totals"]["requests"], 101);
    assert_eq!(analysis["identities"].as_array().unwrap().len(), 101);
    let other_key = key(&db, "batches").await;
    let other_child = session(&db, &other_key, child_thread).await;
    db.import_session_parents(owner.tenant_id, &edges)
        .await
        .unwrap();
    fact(&db, owner.tenant_id, &other_child, 86_400_001).await;
    assert_eq!(
        db.analyze_session_tasks(
            owner.tenant_id,
            std::slice::from_ref(&parent),
            "subtree",
            None,
            &SessionTaskOptions::default()
        )
        .await
        .unwrap()["totals"]["requests"],
        3
    );
    assert_eq!(
        db.analyze_session_tasks(
            owner.tenant_id,
            &[parent],
            "subtree",
            Some(owner.key_id),
            &SessionTaskOptions::default()
        )
        .await
        .unwrap()["totals"]["requests"],
        2
    );
}

#[tokio::test]
async fn fixed_offset_days_and_idempotent_names_preserve_exact_identity() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("offset.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let owner = key(&db, "offset").await;
    let thread = Uuid::now_v7();
    let identity = session(&db, &owner, thread).await;
    fact(&db, owner.tenant_id, &identity, 86_400_000 - 1).await;
    fact(&db, owner.tenant_id, &identity, 86_400_000 + 1).await;
    let analysis = db
        .analyze_session_tasks(
            owner.tenant_id,
            std::slice::from_ref(&identity),
            "session",
            None,
            &SessionTaskOptions {
                timezone_offset_minutes: Some(480),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(analysis["active_days"], 1);
    assert_eq!(analysis["calendar_span_days"], 1);
    assert_eq!(analysis["daily"][0]["day_start_at"], 57_600_000);
    let revision = db
        .session_title(owner.tenant_id, owner.key_id, identity.session_id)
        .await
        .unwrap()
        .revision;
    let update = |explicit: Uuid| SessionNameUpdate {
        title: "同步名称".into(),
        source: "codex".into(),
        explicit_session_id: Some(explicit.to_string()),
        expected_revision: revision,
        expected_unnamed: true,
        idempotency_key: Some("collector-revision-1".into()),
    };
    let named = db
        .update_session_name(owner.tenant_id, &identity, update(thread))
        .await
        .unwrap();
    let retry = db
        .update_session_name(owner.tenant_id, &identity, update(thread))
        .await
        .unwrap();
    assert_eq!(retry.revision, named.revision);
    assert!(matches!(
        db.update_session_name(owner.tenant_id, &identity, update(Uuid::now_v7()))
            .await,
        Err(AppError::NotFound)
    ));
    sqlx::query("UPDATE conversation_observations SET session_name='Observed' WHERE key_id=$1 AND cluster_id=$2")
        .bind(owner.key_id.to_string()).bind(identity.session_id.to_string()).execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.update_session_name(
            owner.tenant_id,
            &identity,
            SessionNameUpdate {
                title: "Must not overwrite".into(),
                source: "manual".into(),
                expected_revision: named.revision,
                expected_unnamed: true,
                ..Default::default()
            }
        )
        .await,
        Err(AppError::Conflict(_))
    ));
}

#[tokio::test]
async fn postgres_task_days_honor_iana_dst_and_unknown_costs() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let db = Database::connect(&url).await.unwrap();
    db.migrate().await.unwrap();
    let owner = key(&db, &format!("task-dst-{}", Uuid::now_v7())).await;
    let thread = Uuid::now_v7();
    let identity = session(&db, &owner, thread).await;
    let child_thread = Uuid::now_v7();
    let child = session(&db, &owner, child_thread).await;
    db.import_session_parents(
        owner.tenant_id,
        &[ThreadParent {
            child_thread_id: child_thread,
            parent_thread_id: thread,
            evidence: "thread_spawn_edges".into(),
        }],
    )
    .await
    .unwrap();
    let revision = db
        .session_title(owner.tenant_id, owner.key_id, identity.session_id)
        .await
        .unwrap()
        .revision;
    db.update_session_name(
        owner.tenant_id,
        &identity,
        SessionNameUpdate {
            title: "DST task".into(),
            source: "codex".into(),
            explicit_session_id: Some(thread.to_string()),
            expected_revision: revision,
            expected_unnamed: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for timestamp in [
        1_772_944_200_000,
        1_772_947_800_000,
        1_773_027_000_000,
        1_773_030_600_000,
    ] {
        fact(&db, owner.tenant_id, &identity, timestamp).await;
    }
    sqlx::query("UPDATE request_stats_facts SET currency='',cost_micros=0 WHERE tenant_id=$1 AND created_at=$2")
        .bind(owner.tenant_id.to_string()).bind(1_773_030_600_000_i64).execute(&db.pool).await.unwrap();
    let analysis = db
        .analyze_session_tasks(
            owner.tenant_id,
            &[identity.clone(), child],
            "related",
            None,
            &SessionTaskOptions {
                timezone: "America/New_York".into(),
                include_subtrees: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(analysis["timezone"], "America/New_York");
    assert_eq!(analysis["active_days"], 3);
    assert_eq!(analysis["calendar_span_days"], 3);
    assert_eq!(analysis["daily"][1]["usage"]["requests"], 2);
    let middle = analysis["daily"][1]["day_start_at"].as_i64().unwrap();
    let last = analysis["daily"][2]["day_start_at"].as_i64().unwrap();
    assert_eq!(last - middle, 23 * 3_600_000);
    assert_eq!(analysis["totals"]["requests"], 4);
    assert_eq!(analysis["totals"]["unknown_cost_requests"], 1);
    assert_eq!(analysis["totals"]["costs"]["USD"], 21);
    let fixed = db
        .analyze_session_tasks(
            owner.tenant_id,
            &[identity],
            "session",
            None,
            &SessionTaskOptions {
                timezone_offset_minutes: Some(-300),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fixed["active_days"], 2);
    assert_eq!(fixed["calendar_span_days"], 2);
}

#[tokio::test]
async fn archive_identity_union_deduplicates_native_facts_and_keeps_unknown_cost() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("archive-union.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let owner = key(&db, "archive-union").await;
    let native = session(&db, &owner, Uuid::now_v7()).await;
    let archive = session(&db, &owner, Uuid::now_v7()).await;
    fact(&db, owner.tenant_id, &native, 1).await;
    let native_id: String =
        sqlx::query_scalar("SELECT request_id FROM request_stats_facts WHERE tenant_id=$1")
            .bind(owner.tenant_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for request in [native_id, Uuid::now_v7().to_string()] {
        sqlx::query("INSERT INTO session_archive_unlinked_requests (tenant_id,source,external_request_id,archive_request_id,key_id,principal_id,conversation_cluster_id,source_started_at,protocol,model,input_tokens,output_tokens,imported_at) VALUES ($1,'fixture',$2,$2,$3,$4,$5,1,'openai','model',3,4,1)")
            .bind(owner.tenant_id.to_string()).bind(request).bind(owner.key_id.to_string()).bind(owner.principal_id.to_string()).bind(archive.session_id.to_string())
            .execute(&db.pool).await.unwrap();
    }
    let analysis = db
        .analyze_session_tasks(
            owner.tenant_id,
            &[native, archive],
            "related",
            None,
            &SessionTaskOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(analysis["totals"]["requests"], 2);
    assert_eq!(analysis["totals"]["input_tokens"], 13);
    assert_eq!(analysis["totals"]["unknown_cost_requests"], 1);
    assert_eq!(analysis["totals"]["costs"]["USD"], 7);
}
