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

impl Database {
    /// Resolve immutable object identities only after the producer committed.
    /// Once resolved, delivery bytes are frozen for retries and exact ACK.
    pub(crate) async fn history_immutable_pending(
        &self,
        page_size: i64,
    ) -> Result<Vec<crate::request_history::HistorySnapshot>, crate::request_history::HistoryError> {
        use crate::request_history::{HistoryError, HistoryIdentity, HistorySnapshot};
        let rows = sqlx::query("SELECT revision,source,request_id,tenant_id,key_id,created_at,snapshot,delivery_snapshot FROM request_history_immutable_outbox ORDER BY revision LIMIT $1")
            .bind(page_size).fetch_all(&self.pool).await?;
        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            let revision: i64 = row.try_get("revision")?;
            let stored_identity = HistoryIdentity {
                source: row.try_get("source")?, request_id: parse_uuid(row.try_get("request_id")?)?,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?, key_id: parse_uuid(row.try_get("key_id")?)?,
                created_at: row.try_get("created_at")?,
            };
            if let Some(frozen) = row.try_get::<Option<String>,_>("delivery_snapshot")? {
                let snapshot: HistorySnapshot = serde_json::from_str(&frozen)?;
                if snapshot.identity != stored_identity || snapshot.revision != revision {
                    return Err(HistoryError::Invalid("immutable delivery identity"));
                }
                snapshots.push(snapshot);
                continue;
            }
            let original: String = row.try_get("snapshot")?;
            let envelope: Value = serde_json::from_str(&original)?;
            if envelope["version"].as_i64() != Some(1) || envelope["source"].as_str() != Some("native") {
                return Err(HistoryError::Invalid("immutable history envelope version or source"));
            }
            let identity: HistoryIdentity = serde_json::from_value(envelope.clone())?;
            if identity != stored_identity || envelope["revision"].as_i64() != Some(revision) {
                return Err(HistoryError::Invalid("immutable history envelope revision"));
            }
            // A previously frozen legacy delivery remains authoritative for
            // this exact revision during the incremental producer rollout.
            let existing: Option<String> = sqlx::query_scalar("SELECT snapshot FROM request_history_export WHERE revision=$1 AND source=$2 AND request_id=$3")
                .bind(revision).bind(&identity.source).bind(identity.request_id.to_string())
                .fetch_optional(&self.pool).await?.flatten();
            let frozen = if let Some(existing) = existing {
                serde_json::to_string(&serde_json::from_str::<HistorySnapshot>(&existing)?)?
            } else {
                let payload: Option<Value> = serde_json::from_value(envelope["payload"].clone())?;
                let mut read_model = envelope["read_model"].clone();
                let mut archive: crate::model::RequestArchiveRefs = serde_json::from_value(read_model["archive"].clone())?;
                archive.view = serde_json::from_value(payload.clone().ok_or(HistoryError::Invalid("immutable native payload"))?)?;
                let archive = self.resolve_raw_archive_roots(identity.tenant_id, archive).await?;
                let payload = Some(serde_json::to_value(&archive.view)?);
                read_model["archive"] = serde_json::to_value(archive)?;
                serde_json::to_string(&HistorySnapshot {
                    identity, revision, payload, read_model,
                    filter_cost_micros: serde_json::from_value(envelope["filter_cost_micros"].clone())?,
                    statistics: serde_json::from_value(envelope["statistics"].clone())?,
                })?
            };
            let stored: Option<String> = sqlx::query_scalar("UPDATE request_history_immutable_outbox SET delivery_snapshot=COALESCE(delivery_snapshot,$1) WHERE revision=$2 AND snapshot=$3 RETURNING delivery_snapshot")
                .bind(&frozen).bind(revision).bind(&original).fetch_optional(&self.pool).await?;
            if let Some(stored) = stored {
                snapshots.push(serde_json::from_str(&stored)?);
            }
        }
        Ok(snapshots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request_history::{HistoryIdentity, HistorySnapshot};

    #[tokio::test]
    async fn immutable_history_delivery_survives_source_absence_restart_and_stale_ack() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", directory.path().join("history.sqlite").display());
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        let identity = HistoryIdentity { source: "native".into(), request_id: Uuid::now_v7(), tenant_id: Uuid::now_v7(), key_id: Uuid::now_v7(), created_at: 42 };
        let first = HistorySnapshot { identity: identity.clone(), revision: 1, filter_cost_micros: None, payload: None, statistics: None, read_model: Value::Null };
        let second = HistorySnapshot { revision: 2, ..first.clone() };
        for snapshot in [&first, &second] {
            sqlx::query("INSERT INTO request_history_immutable_outbox (revision,source,request_id,tenant_id,key_id,created_at,snapshot,delivery_snapshot) VALUES ($1,$2,$3,$4,$5,$6,'{}',$7)")
                .bind(snapshot.revision).bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at).bind(serde_json::to_string(snapshot).unwrap())
                .execute(&db.pool).await.unwrap();
        }
        // No original request, key or source journal is present.
        let pending = db.history_pending(1).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].revision, 1);
        let mut wrong_key = first.clone();
        wrong_key.identity.key_id = Uuid::now_v7();
        db.history_acknowledge(&[wrong_key]).await.unwrap();
        let mut wrong_payload = first.clone();
        wrong_payload.filter_cost_micros = Some(0);
        db.history_acknowledge(&[wrong_payload]).await.unwrap();
        assert_eq!(db.history_pending_count().await.unwrap(), 2);
        db.history_acknowledge(&[first.clone()]).await.unwrap();
        db.history_acknowledge(&[first]).await.unwrap();
        assert_eq!(db.history_pending_count().await.unwrap(), 1);
        db.pool.close().await;
        let reopened = Database::connect(&url).await.unwrap();
        let pending = reopened.history_pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].revision, 2);
        assert_eq!(pending[0].identity, identity);
        assert_eq!(pending[0].filter_cost_micros, None);
        reopened.history_acknowledge(&pending).await.unwrap();
        assert_eq!(reopened.history_pending_count().await.unwrap(), 0);
    }
}
