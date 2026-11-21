use super::*;

#[tokio::test]
#[ignore = "requires CI PostgreSQL"]
async fn postgres_source_binding_uses_owned_partition_and_preserves_replay() {
    let url = std::env::var("MTC_TEST_POSTGRES_URL").expect("CI PostgreSQL required");
    let isolated = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let database = Database::connect(isolated.url()).await.unwrap();
    let fixture = Fixture::with_database(database, tempfile::tempdir().unwrap()).await;
    let db = &fixture.database;
    let request_id = Uuid::now_v7();
    let locator =
        crate::gateway_body::request_spool::request_body_object_key(&request_id.to_string())
            .unwrap();
    let reservation = db
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &fixture.key,
            price: &fixture.price,
            input_token_ceiling: 7,
            output_token_ceiling: 11,
            protocol: "openai",
            model: "isolation",
            request_object: &locator,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let identity = ArchiveSpoolIdentity {
        request_id,
        tenant_id: fixture.key.tenant_id,
        reservation_id: reservation.id,
    };
    let created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id=$1")
            .bind(request_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let historical = created_at - 30 * 86_400_000;
    // The locator remains the single authority when history contains another
    // physical row bearing the same request UUID in a different partition.
    sqlx::query("INSERT INTO request_records (id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id) SELECT id,tenant_id,key_id,$2,protocol,model,0,0,0,request_object,reservation_id FROM request_records WHERE id=$1 AND created_at=$3")
        .bind(request_id.to_string()).bind(historical).bind(created_at).execute(&db.pool).await.unwrap();
    let mut wrong = identity;
    wrong.tenant_id = Uuid::now_v7();
    assert!(
        !db.record_request_source_state(
            wrong,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            None,
            false
        )
        .await
        .unwrap()
    );
    wrong = identity;
    wrong.reservation_id = Uuid::now_v7();
    assert!(
        !db.record_request_source_state(
            wrong,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            None,
            false
        )
        .await
        .unwrap()
    );
    sqlx::query("UPDATE request_records SET request_object='different-source' WHERE id=$1 AND created_at=$2")
        .bind(request_id.to_string()).bind(created_at).execute(&db.pool).await.unwrap();
    assert!(
        !db.record_request_source_state(
            identity,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            None,
            false
        )
        .await
        .unwrap()
    );
    sqlx::query("UPDATE request_records SET request_object=$3 WHERE id=$1 AND created_at=$2")
        .bind(request_id.to_string())
        .bind(created_at)
        .bind(&locator)
        .execute(&db.pool)
        .await
        .unwrap();
    let absent: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM archive_staging_attempts WHERE attempt_id=$1")
            .bind(request_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(absent, 0, "ownership mismatch writes no staging state");
    assert!(
        db.record_request_source_state(
            identity,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            None,
            false
        )
        .await
        .unwrap()
    );
    assert!(
        db.record_request_source_state(
            identity,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            Some(true),
            false
        )
        .await
        .unwrap()
    );
    assert!(
        !db.record_request_source_state(
            identity,
            &locator,
            2,
            *blake3::hash(b"{}").as_bytes(),
            Some(true),
            false
        )
        .await
        .unwrap()
    );
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM request_events WHERE request_id=$1 AND event_kind='archive_bound'",
    )
    .bind(request_id.to_string())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(events, 1, "metadata replay emits one bound event");
    let status: String = sqlx::query_scalar("SELECT status FROM usage_reservations WHERE id=$1")
        .bind(reservation.id.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "reserved");
    let plan=sqlx::query_scalar::<_,serde_json::Value>("EXPLAIN (FORMAT JSON) UPDATE request_records SET request_object=request_object WHERE id=$1 AND tenant_id=$2 AND reservation_id=$3 AND request_object=$4 AND created_at=$5 AND key_id=$6 RETURNING key_id")
        .bind(request_id.to_string()).bind(fixture.key.tenant_id.to_string()).bind(reservation.id.to_string()).bind(&locator).bind(created_at).bind(fixture.key.key_id.to_string()).fetch_one(&db.pool).await.unwrap();
    fn scanned_relations(node: &serde_json::Value, out: &mut Vec<String>) {
        if node.get("Node Type").and_then(|v| v.as_str()) != Some("ModifyTable")
            && let Some(relation) = node.get("Relation Name").and_then(|v| v.as_str())
        {
            out.push(relation.into());
        }
        if let Some(children) = node.get("Plans").and_then(|v| v.as_array()) {
            for child in children {
                scanned_relations(child, out);
            }
        }
    }
    let mut relations = Vec::new();
    scanned_relations(&plan[0]["Plan"], &mut relations);
    assert_eq!(
        relations.len(),
        1,
        "source ownership targets one physical partition: {plan}"
    );
    println!("SOURCE_BINDING_PARTITION_PLAN {plan}");
    db.close().await;
    isolated.cleanup().await;
}
