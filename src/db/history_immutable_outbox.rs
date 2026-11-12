//! SQL-only capture at the terminal projection commit boundary.
//! No ClickHouse or object-store operation belongs in this transaction.
use super::*;
use sqlx::{Column, ValueRef};

/// Preserve the original scalar types rather than coercing unknown money to zero.
fn original_row(row: &AnyRow) -> Result<Value, AppError> {
    let mut result = serde_json::Map::new();
    for column in row.columns() {
        let name = column.name();
        let value = if row.try_get_raw(name)?.is_null() {
            Value::Null
        } else if let Ok(value) = row.try_get::<i64, _>(name) {
            Value::from(value)
        } else if let Ok(value) = row.try_get::<String, _>(name) {
            Value::from(value)
        } else {
            return Err(AppError::storage("history_envelope_unsupported_scalar"));
        };
        result.insert(name.to_owned(), value);
    }
    Ok(Value::Object(result))
}

pub(super) async fn capture_native_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    // An off/shadow deployment keeps the existing capture contract. The durable
    // revision already allocated by its triggers also fences this projection.
    let identity = sqlx::query("SELECT revision, tenant_id, key_id, created_at FROM request_history_export WHERE source='native' AND request_id=$1")
        .bind(request_id).fetch_optional(&mut **transaction).await?;
    let Some(identity) = identity else { return Ok(()); };
    let revision: i64 = identity.try_get("revision")?;
    let terminal = sqlx::query("SELECT * FROM terminal_projection_outbox WHERE request_id=$1")
        .bind(request_id).fetch_one(&mut **transaction).await?;
    let request = sqlx::query("SELECT * FROM request_records WHERE id=$1 AND tenant_id=$2 AND key_id=$3")
        .bind(request_id).bind(identity.try_get::<String,_>("tenant_id")?).bind(identity.try_get::<String,_>("key_id")?)
        .fetch_one(&mut **transaction).await?;
    let mut originals = serde_json::Map::new();
    originals.insert("request".into(), original_row(&request)?);
    let mut terminal_original = original_row(&terminal)?;
    if let Some(fields) = terminal_original.as_object_mut() {
        for delivery_field in ["lease_owner", "lease_expires_at", "attempts", "projected_at", "statistics_outcome"] {
            fields.remove(delivery_field);
        }
    }
    originals.insert("terminal".into(), terminal_original);
    for (name, sql) in [
        ("events", "SELECT * FROM request_events WHERE request_id=$1 ORDER BY event_at,event_id"),
        ("statistics", "SELECT * FROM request_stats_facts WHERE request_id=$1"),
        ("egress", "SELECT * FROM request_egress_attempts WHERE request_id=$1 ORDER BY observed_at,attempt_id"),
        ("transport", "SELECT * FROM request_upstream_transport_diagnostics WHERE request_id=$1 ORDER BY observed_at,route_id,upstream_account_id"),
        ("observations", "SELECT * FROM conversation_observations WHERE request_id=$1 ORDER BY cluster_id,key_id"),
    ] {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(request_id).fetch_all(&mut **transaction).await?;
        originals.insert(name.into(), Value::Array(rows.iter().map(original_row).collect::<Result<_,_>>()?));
    }
    let tenant: String = identity.try_get("tenant_id")?;
    let key: String = identity.try_get("key_id")?;
    let history_identity = crate::request_history::HistoryIdentity {
        source: "native".into(), request_id: parse_uuid(request_id.to_owned())?,
        tenant_id: parse_uuid(tenant.clone())?, key_id: parse_uuid(key.clone())?,
        created_at: identity.try_get("created_at")?,
    };
    let payload = Database::history_original_view_in_transaction(transaction, &history_identity).await?;
    let archive = Database::history_native_archive_in_transaction(transaction, &history_identity).await?;
    let payload_json = serde_json::to_value(&payload).map_err(|_| AppError::storage("history_envelope_serialization"))?;
    let statistics = Database::history_statistics_in_transaction(transaction, &history_identity, &payload_json).await?;
    let fact = Database::history_analysis_fact_in_transaction(transaction, &history_identity).await?;
    let egress = Database::history_egress_in_transaction(transaction, history_identity.request_id).await?;
    let transport = Database::history_transport_in_transaction(transaction, history_identity.request_id).await?;
    let observation = sqlx::query("SELECT * FROM conversation_observations WHERE request_id=$1 AND key_id=$2 AND cluster_id=$3")
        .bind(request_id).bind(&key).bind(payload_json["session_context"]["session_id"].as_str())
        .fetch_optional(&mut **transaction).await?;
    let execution = observation.as_ref().map(super::requests::conversations::execution_metadata_from_row).transpose()?.flatten();
    let structure = observation.as_ref().map(super::requests::conversations::execution_structure_from_row).transpose()?.flatten();
    let upstream_name: Option<String> = if let Some(upstream) = fact.get("upstream_account_id").unwrap_or(&payload_json["upstream_account_id"]).as_str().filter(|value| !value.is_empty()) {
        sqlx::query_scalar("SELECT name FROM upstream_accounts WHERE id=$1").bind(upstream).fetch_optional(&mut **transaction).await?
    } else { None };
    let filter_cost_micros: Option<i64> = if request.try_get::<Option<i64>,_>("completed_at")?.is_some() { request.try_get("cost_micros")? } else { None };
    let read_model = serde_json::json!({"archive":archive,"fact":fact,"upstream_name":upstream_name,"execution":execution,"structure":structure,"egress_attempts":egress,"transport_diagnostics":transport});
    let envelope = serde_json::json!({
        "version": 1, "source": "native", "revision": revision,
        "tenant_id": tenant, "key_id": key, "request_id": request_id,
        "created_at": identity.try_get::<i64,_>("created_at")?,
        // This exact immutable archive identity survives source-table retirement.
        "archive_identity": {"tenant_id": tenant, "request_id": request_id},
        "payload": payload, "filter_cost_micros": filter_cost_micros, "statistics": statistics, "read_model": read_model, "originals": originals,
    });
    let frozen = serde_json::to_string(&envelope).map_err(|_| AppError::storage("history_envelope_serialization"))?;
    let stored: String = sqlx::query_scalar("INSERT INTO request_history_immutable_outbox (revision,source,request_id,tenant_id,key_id,created_at,snapshot) VALUES ($1,'native',$2,$3,$4,$5,$6) ON CONFLICT(revision) DO UPDATE SET snapshot=request_history_immutable_outbox.snapshot RETURNING snapshot")
        .bind(revision).bind(request_id).bind(&tenant).bind(&key).bind(identity.try_get::<i64,_>("created_at")?).bind(&frozen)
        .fetch_one(&mut **transaction).await?;
    if stored != frozen {
        return Err(AppError::storage("history_envelope_revision_conflict"));
    }
    Ok(())
}
