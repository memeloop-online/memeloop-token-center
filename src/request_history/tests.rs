use super::*;
use serde_json::json;
use uuid::Uuid;

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
    };
    let row = snapshot.row().unwrap();
    assert_eq!(row["outcome"], "failed");
    assert_eq!(row["settlement_state"], "unknown");
    assert!(row["cost_micros"].is_null());
    assert!(row["input_tokens"].is_null());
}
