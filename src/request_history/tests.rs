use super::*;
use serde_json::json;
use uuid::Uuid;

#[test]
fn client_session_filter_reads_request_metadata_and_keeps_tenant_scope() {
    let tenant_id = Uuid::new_v4();
    let query = HistoryQuery {
        scope: HistoryScope::Tenant(tenant_id),
        from_created_at: Some(1),
        to_created_at: Some(100),
        cursor: None,
        limit: 10,
        predicates: vec![HistoryPredicate {
            dimension: Dimension::ClientSessionId,
            comparison: Comparison::Equals,
            value: PredicateValue::Text("client-thread' OR 1=1".into()),
        }],
    };
    let sql = query.list_sql().unwrap();
    assert!(sql.sql.contains("FINAL WHERE tenant_id"));
    assert!(
        sql.sql
            .contains("JSONExtractString(snapshot, 'read_model', 'structure', 'session_id')")
    );
    assert!(!sql.sql.contains("OR 1=1"));
    assert!(
        sql.parameters
            .iter()
            .any(|(_, value)| value == "client-thread' OR 1=1")
    );
}

#[test]
fn latest_state_filters_follow_version_replacement_and_bind_values() {
    let query = HistoryQuery {
        scope: HistoryScope::Key {
            tenant_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
        },
        from_created_at: Some(1),
        to_created_at: Some(i64::MAX),
        cursor: None,
        limit: 500,
        predicates: vec![HistoryPredicate {
            dimension: Dimension::Model,
            comparison: Comparison::Equals,
            value: PredicateValue::Text("x' OR 1=1".into()),
        }],
    };
    let sql = query.list_sql().unwrap();
    assert!(sql.sql.contains("FINAL WHERE tenant_id"));
    assert!(sql.sql.contains(") WHERE deleted = 0 AND (model ="));
    assert!(!sql.sql.contains("OR 1=1"));
    assert!(sql.parameters.iter().any(|(_, value)| value == "x' OR 1=1"));
    assert!(query.daily_sql().is_ok());
}

#[test]
fn financial_unknown_and_operational_failure_remain_independent() {
    let snapshot = HistorySnapshot {
        identity: HistoryIdentity {
            tenant_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            created_at: 1,
            source: "native".into(),
        },
        revision: 1,
        filter_cost_micros: None,
        payload: Some(json!({"lifecycle_state": "failed", "status_code": 200,
            "operational_terminal": {"financial_state": "unknown"}})),
        statistics: None,
        read_model: serde_json::Value::Null,
    };
    let row = snapshot.row().unwrap();
    assert_eq!(row["outcome"], "failed");
    assert_eq!(row["settlement_state"], "unknown");
    assert!(row["cost_micros"].is_null());
    assert!(row["input_tokens"].is_null());
}

#[test]
fn session_association_uses_identity_nullness_with_equal_and_not_equal() {
    for (value, comparison, suffix) in [
        ("unlinked", Comparison::Equals, "IS NULL"),
        ("confirmed", Comparison::Equals, "IS NOT NULL"),
        ("unlinked", Comparison::NotEquals, "IS NOT NULL"),
        ("confirmed", Comparison::NotEquals, "IS NULL"),
    ] {
        let query = HistoryQuery {
            scope: HistoryScope::GlobalOperator,
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit: 1,
            predicates: vec![HistoryPredicate {
                dimension: Dimension::SessionAssociation,
                comparison,
                value: PredicateValue::Text(value.into()),
            }],
        };
        let sql = query.latest().unwrap();
        assert!(sql.sql.ends_with(&format!("AND session_id {suffix}")));
        assert!(!sql.sql.contains("session_name"));
    }
}

#[tokio::test]
async fn baseline_page_reads_use_two_http_calls_and_validate_identity_hash() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    fn stored(physical: &HistorySnapshot, payload: &HistorySnapshot) -> serde_json::Value {
        json!({"tenant_id":physical.identity.tenant_id,"key_id":physical.identity.key_id,
            "request_id":physical.identity.request_id,"source":physical.identity.source,
            "created_at":physical.identity.created_at.to_string(),
            "snapshot":serde_json::to_string(payload).unwrap(),"snapshot_hash":canonical_hash(payload).unwrap()})
    }
    let rows = (0..64)
        .map(|_| HistorySnapshot {
            identity: HistoryIdentity {
                source: "event".into(),
                request_id: Uuid::now_v7(),
                tenant_id: Uuid::now_v7(),
                key_id: Uuid::now_v7(),
                created_at: 42,
            },
            revision: 7,
            payload: None,
            read_model: serde_json::Value::Null,
            statistics: None,
            filter_cost_micros: None,
        })
        .collect::<Vec<_>>();
    let encoded = rows
        .iter()
        .map(|row| stored(row, row).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let reply = Arc::new(Mutex::new(encoded));
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_reply = reply.clone();
    let handler_calls = calls.clone();
    let router = axum::Router::new().route(
        "/",
        axum::routing::post(move || {
            let body = handler_reply.lock().unwrap().clone();
            handler_calls.fetch_add(1, Ordering::SeqCst);
            async move { body }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = ClickHouse::new(
        reqwest::Client::new(),
        &format!("http://{address}"),
        "fixture".into(),
        "default".into(),
        String::new(),
    )
    .unwrap();
    let ids = rows
        .iter()
        .map(|row| row.identity.clone())
        .collect::<Vec<_>>();
    for _ in 0..2 {
        let actual = client.baseline_snapshots(&ids).await.unwrap();
        assert_eq!(actual.len(), 64);
        for (expected, actual) in rows.iter().zip(actual) {
            assert_eq!(
                canonical_hash(expected).unwrap(),
                canonical_hash(&actual.unwrap()).unwrap()
            );
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let one = stored(&rows[0], &rows[0]).to_string();
    *reply.lock().unwrap() = format!("{one}\n{one}");
    assert!(client.baseline_snapshots(&ids).await.is_err());
    *reply.lock().unwrap() = one;
    assert!(client.baseline_snapshots(&ids[1..]).await.is_err());
    let mut bad = stored(&rows[0], &rows[0]);
    bad["snapshot_hash"] = json!("bad");
    *reply.lock().unwrap() = bad.to_string();
    assert!(client.baseline_snapshots(&ids).await.is_err());
    *reply.lock().unwrap() = format!(
        "{}\n{}",
        stored(&rows[0], &rows[1]),
        stored(&rows[1], &rows[0])
    );
    assert!(client.baseline_snapshots(&ids).await.is_err());
    server.abort();
}
