use super::*;
use crate::db::requests::conversations::prefix_storage::tests::{key, load, observe};
use serde_json::json;
use std::io::Write as _;

async fn conversion_contract(url: &str) {
    let database = Database::connect_with_max(url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let owner = key(&database, "conversion-tenant-a").await;
    let other = key(&database, "conversion-tenant-b").await;
    let body = json!({"input": (0..4096).map(|index| json!({"role":"user","content":format!("migration atom {index}")})).collect::<Vec<_>>()});
    let hashes: Vec<_> = extract_atoms(&body)
        .into_iter()
        .map(|atom| atom.content_hash)
        .collect();
    let full_json = serde_json::to_string(&hashes).unwrap();
    let bounded_json = serde_json::to_string(&hashes[..1024]).unwrap();
    let now = unix_millis();
    let hints = ConversationHints::default();
    let mut owners = Vec::new();
    for key in [&owner, &other] {
        let request = Uuid::now_v7();
        let cluster = observe(&database, key, &body, &hints, request, now, true).await;
        let leaf = build_prefix(&extract_atoms(&body)).pop().unwrap().node_hash;
        owners.push((key, cluster, leaf));
    }
    // Recreate a real pre127 input surface in this isolated test database.
    let drop_guard = if matches!(database.backend, DatabaseBackend::PostgreSql) {
        "DROP TRIGGER conversation_prefix_format ON conversation_observations; DROP FUNCTION enforce_conversation_prefix_format();"
    } else {
        "DROP TRIGGER conversation_prefix_format_insert; DROP TRIGGER conversation_prefix_format_update;"
    };
    sqlx::raw_sql(drop_guard)
        .execute(&database.pool)
        .await
        .unwrap();
    let mut historical_requests = Vec::new();
    for index in 0..130 {
        let (key, cluster, leaf) = &owners[index % 2];
        let request = Uuid::now_v7();
        let empty = index % 3 == 2;
        let encoded = if empty {
            "[]"
        } else if index % 3 == 0 {
            &full_json
        } else {
            &bounded_json
        };
        sqlx::query("INSERT INTO conversation_observations (id,cluster_id,request_id,key_id,leaf_node_hash,atom_hashes_json,created_at,inference_version,turn_id,branch_id,compaction,labels_json) VALUES ($1,$2,$3,$4,$5,$6,$7,2,$8,'branch-kept',1,'{\"keep\":\"metadata\"}')")
            .bind(format!("a-{index:05}")).bind(cluster.to_string()).bind(request.to_string()).bind(key.key_id.to_string())
            .bind(if empty { None } else { Some(leaf.as_str()) }).bind(encoded).bind(now).bind(format!("turn-{index}"))
            .execute(&database.pool).await.unwrap();
        historical_requests.push((request, key.tenant_id, empty));
    }
    sqlx::query("DROP TABLE proxy_operational_terminals")
        .execute(&database.pool)
        .await
        .unwrap();
    for statement in [
        "DROP TABLE application_plugin_release_resolutions",
        "DROP TABLE session_generated_titles",
        "DROP TABLE session_response_summaries",
        "DROP TABLE plugin_installer_network_snapshots",
        "DROP TABLE plugin_installer_network_preferences",
        "DROP TABLE pending_proxy_settlements",
    ] {
        sqlx::query(statement)
            .execute(&database.pool)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM schema_migrations WHERE version >= 127")
        .execute(&database.pool)
        .await
        .unwrap();
    let original: Vec<String> =
        sqlx::query_scalar("SELECT atom_hashes_json FROM conversation_observations ORDER BY id")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    // Simulate interruption after conversion but before migration commit/version ACK.
    let mut transaction = database.begin_write_transaction().await.unwrap();
    convert(&mut transaction).await.unwrap();
    transaction.rollback().await.unwrap();
    let after_crash: Vec<String> =
        sqlx::query_scalar("SELECT atom_hashes_json FROM conversation_observations ORDER BY id")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        original, after_crash,
        "interrupted conversion must preserve all historical arrays"
    );

    // Put an invalid legacy row last, after multiple valid pages. Failure must
    // roll back earlier updates and leave schema126, rather than discard history.
    let invalid_id = Uuid::now_v7();
    sqlx::query("INSERT INTO conversation_observations (id,cluster_id,request_id,key_id,leaf_node_hash,atom_hashes_json,created_at,inference_version) VALUES ('z-invalid',$1,$2,$3,$4,'[]',$5,2)")
        .bind(owners[0].1.to_string()).bind(invalid_id.to_string()).bind(owner.key_id.to_string()).bind(&owners[0].2).bind(now)
        .execute(&database.pool).await.unwrap();
    assert!(database.migrate().await.is_err());
    let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(version, 126);
    let after_failure: Vec<String> = sqlx::query_scalar("SELECT atom_hashes_json FROM conversation_observations WHERE id <> 'z-invalid' ORDER BY id")
        .fetch_all(&database.pool).await.unwrap();
    assert_eq!(original, after_failure);
    sqlx::query(
        "UPDATE conversation_observations SET atom_hashes_json = $1 WHERE id = 'z-invalid'",
    )
    .bind(&full_json)
    .execute(&database.pool)
    .await
    .unwrap();
    // Key/cluster tenants must agree, even when both tenants own identical hashes.
    sqlx::query("UPDATE conversation_observations SET key_id = $1 WHERE id = 'z-invalid'")
        .bind(other.key_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    assert!(database.migrate().await.is_err());
    sqlx::query("UPDATE conversation_observations SET key_id = $1 WHERE id = 'z-invalid'")
        .bind(owner.key_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
    let ledger_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let atoms_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    let nodes_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM context_nodes")
        .fetch_one(&database.pool)
        .await
        .unwrap();
    database.migrate().await.unwrap();
    for (request, tenant, empty) in historical_requests {
        let (encoded, fingerprint, full) = load(&database, tenant, request).await;
        assert!(prefix_storage::decode(&encoded).is_ok());
        if empty {
            assert!(full.is_empty() && fingerprint.is_empty());
        } else {
            assert_eq!(full, hashes);
            assert_eq!(fingerprint, hashes[..1024]);
        }
        let metadata = sqlx::query("SELECT turn_id,branch_id,compaction,labels_json FROM conversation_observations WHERE request_id = $1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(metadata.get::<String, _>("turn_id").starts_with("turn-"));
        assert_eq!(metadata.get::<String, _>("branch_id"), "branch-kept");
        assert_eq!(metadata.get::<i64, _>("compaction"), 1);
        assert_eq!(
            metadata.get::<String, _>("labels_json"),
            r#"{"keep":"metadata"}"#
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM ledger_entries")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        ledger_before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM semantic_atoms")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        atoms_before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM context_nodes")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        nodes_before
    );
    let latest: Vec<String> =
        sqlx::query_scalar("SELECT atom_hashes_json FROM conversation_observations ORDER BY id")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    database.migrate().await.unwrap();
    let replayed: Vec<String> =
        sqlx::query_scalar("SELECT atom_hashes_json FROM conversation_observations ORDER BY id")
            .fetch_all(&database.pool)
            .await
            .unwrap();
    assert_eq!(latest, replayed);
    assert!(
        sqlx::query(
            "UPDATE conversation_observations SET atom_hashes_json = '[]' WHERE id = 'z-invalid'"
        )
        .execute(&database.pool)
        .await
        .is_err(),
        "old writers must be rejected after cutover"
    );
    assert!(
        prefix_storage::decode("[]").is_err(),
        "runtime is latest-only after migration"
    );
    writeln!(std::io::stdout(), "PREFIX_MIGRATION legacy_rows=131 prefix_atoms=4096 tenants=2 pages=3 crash_rollback=verified invalid_row_rollback=verified ownership=verified exact_readback=verified ledger_atoms_nodes=unchanged replay=verified").unwrap();
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_legacy_conversion_is_atomic_verified_and_replayable() {
    let directory = tempfile::tempdir().unwrap();
    conversion_contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("conversion.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_legacy_conversion_is_atomic_verified_and_replayable() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("prefix_conversion_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    conversion_contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
