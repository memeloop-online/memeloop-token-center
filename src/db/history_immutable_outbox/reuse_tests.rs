use super::*;
async fn original_native_envelope(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
    identity: &AnyRow,
) -> Result<(crate::request_history::HistoryIdentity, Value), AppError> {
    let revision: i64 = identity.try_get("revision")?;
    let terminal = sqlx::query("SELECT * FROM terminal_projection_outbox WHERE request_id=$1")
        .bind(request_id)
        .fetch_optional(&mut **transaction)
        .await?;
    let request =
        sqlx::query("SELECT * FROM request_records WHERE id=$1 AND tenant_id=$2 AND key_id=$3 AND created_at=$4")
            .bind(request_id)
            .bind(identity.try_get::<String, _>("tenant_id")?)
            .bind(identity.try_get::<String, _>("key_id")?)
            .bind(identity.try_get::<i64, _>("created_at")?)
            .fetch_one(&mut **transaction)
            .await?;
    let mut originals = serde_json::Map::new();
    originals.insert("request".into(), original_row(&request)?);
    let mut terminal_original = terminal
        .as_ref()
        .map(original_row)
        .transpose()?
        .unwrap_or(Value::Null);
    if let Some(fields) = terminal_original.as_object_mut() {
        for delivery_field in [
            "lease_owner",
            "lease_expires_at",
            "attempts",
            "projected_at",
            "statistics_outcome",
        ] {
            fields.remove(delivery_field);
        }
    }
    originals.insert("terminal".into(), terminal_original);
    for (name, sql) in [
        (
            "statistics",
            "SELECT * FROM request_stats_facts WHERE request_id=$1",
        ),
        (
            "egress",
            "SELECT * FROM request_egress_attempts WHERE request_id=$1 ORDER BY observed_at,attempt_id",
        ),
        (
            "transport",
            "SELECT * FROM request_upstream_transport_diagnostics WHERE request_id=$1 ORDER BY observed_at,route_id,upstream_account_id",
        ),
        (
            "observations",
            "SELECT * FROM conversation_observations WHERE request_id=$1 ORDER BY cluster_id,key_id",
        ),
    ] {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(request_id)
            .fetch_all(&mut **transaction)
            .await?;
        originals.insert(
            name.into(),
            Value::Array(rows.iter().map(original_row).collect::<Result<_, _>>()?),
        );
    }
    let tenant: String = identity.try_get("tenant_id")?;
    let key: String = identity.try_get("key_id")?;
    let history_identity = crate::request_history::HistoryIdentity {
        source: "native".into(),
        request_id: parse_uuid(request_id.to_owned())?,
        tenant_id: parse_uuid(tenant.clone())?,
        key_id: parse_uuid(key.clone())?,
        created_at: identity.try_get("created_at")?,
    };
    let payload =
        Database::history_original_view_in_transaction(transaction, &history_identity).await?;
    let archive =
        Database::history_native_archive_in_transaction(transaction, &history_identity).await?;
    let payload_json = serde_json::to_value(&payload)
        .map_err(|_| AppError::storage("history_envelope_serialization"))?;
    let statistics =
        Database::history_statistics_in_transaction(transaction, &history_identity, &payload_json)
            .await?;
    let fact =
        Database::history_analysis_fact_in_transaction(transaction, &history_identity).await?;
    let egress =
        Database::history_egress_in_transaction(transaction, history_identity.request_id).await?;
    let transport =
        Database::history_transport_in_transaction(transaction, history_identity.request_id)
            .await?;
    let observation = sqlx::query("SELECT * FROM conversation_observations WHERE request_id=$1 AND key_id=$2 AND cluster_id=$3")
        .bind(request_id).bind(&key).bind(payload_json["session_context"]["session_id"].as_str())
        .fetch_optional(&mut **transaction).await?;
    let execution = observation
        .as_ref()
        .map(super::requests::conversations::execution_metadata_from_row)
        .transpose()?
        .flatten();
    let structure = observation
        .as_ref()
        .map(super::requests::conversations::execution_structure_from_row)
        .transpose()?
        .flatten();
    let upstream_name: Option<String> = if let Some(upstream) = fact
        .get("upstream_account_id")
        .unwrap_or(&payload_json["upstream_account_id"])
        .as_str()
        .filter(|value| !value.is_empty())
    {
        sqlx::query_scalar("SELECT name FROM upstream_accounts WHERE id=$1")
            .bind(upstream)
            .fetch_optional(&mut **transaction)
            .await?
    } else {
        None
    };
    let filter_cost_micros: Option<i64> =
        if request.try_get::<Option<i64>, _>("completed_at")?.is_some() {
            request.try_get("cost_micros")?
        } else {
            None
        };
    let read_model = serde_json::json!({"archive":archive,"fact":fact,"upstream_name":upstream_name,"execution":execution,"structure":structure,"egress_attempts":egress,"transport_diagnostics":transport});
    let envelope = serde_json::json!({
        "version": 1, "source": "native", "revision": revision,
        "tenant_id": tenant, "key_id": key, "request_id": request_id,
        "created_at": identity.try_get::<i64,_>("created_at")?,
        // This exact immutable archive identity survives source-table retirement.
        "archive_identity": {"tenant_id": tenant, "request_id": request_id},
        "payload": payload, "filter_cost_micros": filter_cost_micros, "statistics": statistics, "read_model": read_model, "originals": originals,
    });
    Ok((history_identity, envelope))
}

#[tokio::test]
async fn postgres_native_capture_reuse_is_exactly_equal() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Database::connect_with_max(fixture.url(), 4).await.unwrap();
    db.migrate().await.unwrap();
    let request = Uuid::now_v7().to_string();
    let tenant = Uuid::now_v7().to_string();
    let key = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO request_records(id,tenant_id,key_id,created_at,protocol,model,input_tokens,output_tokens,cost_micros,request_object,reservation_id,currency) VALUES($1,$2,$3,1000,'openai','fixture',1,2,3,'gap://fixture/request',$4,'USD')").bind(&request).bind(&tenant).bind(&key).bind(Uuid::now_v7().to_string()).execute(&db.pool).await.unwrap();
    for observed in [2000_i64, 1000] {
        sqlx::query("INSERT INTO request_upstream_transport_diagnostics(request_id,route_id,upstream_account_id,credential_generation,transport_revision,failure_kind,failure_stage,gateway_pod,gateway_node,failure_domain,observed_at) VALUES($1,$2,$3,1,1,'transport','connect','fixture',NULL,'fixture',$4)")
            .bind(&request).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(observed).execute(&db.pool).await.unwrap();
    }
    let mut tx = db.begin_write_transaction().await.unwrap();
    let identity=sqlx::query("SELECT CAST(1 AS BIGINT) revision,CAST($1 AS TEXT) tenant_id,CAST($2 AS TEXT) key_id,CAST(1000 AS BIGINT) created_at").bind(&tenant).bind(&key).fetch_one(&mut *tx).await.unwrap();
    let before = original_native_envelope(&mut tx, &request, &identity)
        .await
        .unwrap();
    let after = source_native_envelope_in_transaction(&mut tx, &request, &identity)
        .await
        .unwrap();
    assert_eq!(before.0, after.0);
    assert_eq!(
        after.1["read_model"]["transport_diagnostics"][0]["observed_at"],
        1000
    );
    assert_eq!(
        after.1["read_model"]["transport_diagnostics"][1]["observed_at"],
        2000
    );
    assert_eq!(
        before.1, after.1,
        "all original/read-model fields and None values remain equal"
    );
    tx.rollback().await.unwrap();
}
