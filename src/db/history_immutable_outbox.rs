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

// Each event owns its original row under its own revision fence. Parent
// envelopes must not include the append-only collection of sibling events.
async fn original_event_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    event_id: Uuid,
) -> Result<Value, AppError> {
    let row = sqlx::query("SELECT * FROM request_events WHERE event_id=$1")
        .bind(event_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?;
    Ok(row
        .as_ref()
        .map(original_row)
        .transpose()?
        .unwrap_or(Value::Null))
}

pub(super) async fn capture_native_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    // An off/shadow deployment keeps the existing capture contract. The durable
    // revision already allocated by its triggers also fences this projection.
    let identity = sqlx::query("SELECT revision, tenant_id, key_id, created_at FROM request_history_export WHERE source='native' AND request_id=$1 AND EXISTS (SELECT 1 FROM request_history_state WHERE id=1 AND mode <> 'off')")
        .bind(request_id).fetch_optional(&mut **transaction).await?;
    let Some(identity) = identity else {
        return Ok(());
    };
    let revision: i64 = identity.try_get("revision")?;
    let terminal = sqlx::query("SELECT * FROM terminal_projection_outbox WHERE request_id=$1")
        .bind(request_id)
        .fetch_optional(&mut **transaction)
        .await?;
    let request =
        sqlx::query("SELECT * FROM request_records WHERE id=$1 AND tenant_id=$2 AND key_id=$3")
            .bind(request_id)
            .bind(identity.try_get::<String, _>("tenant_id")?)
            .bind(identity.try_get::<String, _>("key_id")?)
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
    insert_envelope_in_transaction(transaction, &history_identity, revision, &envelope).await?;
    let events = sqlx::query("SELECT x.revision,x.request_id,x.created_at FROM request_history_export x JOIN request_events e ON e.event_id=x.request_id AND e.tenant_id=x.tenant_id AND e.key_id=x.key_id WHERE x.source='event' AND e.request_id=$1 AND x.tenant_id=$2 AND x.key_id=$3 ORDER BY x.revision")
        .bind(request_id).bind(&tenant).bind(&key).fetch_all(&mut **transaction).await?;
    for event in events {
        let event_id = parse_uuid(event.try_get("request_id")?)?;
        let event_identity = crate::request_history::HistoryIdentity {
            source: "event".into(),
            request_id: event_id,
            created_at: event.try_get("created_at")?,
            ..history_identity.clone()
        };
        let event_revision: i64 = event.try_get("revision")?;
        let payload =
            Database::history_original_event_in_transaction(transaction, event_id).await?;
        let original_event = original_event_in_transaction(transaction, event_id).await?;
        let envelope = serde_json::json!({"originals":{"event":original_event},"version":1,"source":"event","revision":event_revision,"tenant_id":tenant,"key_id":key,"request_id":event_id,"created_at":event_identity.created_at,"payload":payload,"filter_cost_micros":null,"statistics":null,"read_model":null,"archive_identity":{"tenant_id":tenant,"request_id":request_id,"source":"native"}});
        insert_envelope_in_transaction(transaction, &event_identity, event_revision, &envelope)
            .await?;
    }
    Ok(())
}

pub(super) async fn capture_generation_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    // An off/shadow deployment keeps the existing capture contract. The durable
    // revision already allocated by its triggers also fences this projection.
    let identity = sqlx::query("SELECT revision, tenant_id, key_id, created_at FROM request_history_export WHERE source='generation' AND request_id=$1 AND EXISTS (SELECT 1 FROM request_history_state WHERE id=1 AND mode <> 'off')")
        .bind(request_id).fetch_optional(&mut **transaction).await?;
    let Some(identity) = identity else {
        return Ok(());
    };
    let revision: i64 = identity.try_get("revision")?;
    let request =
        sqlx::query("SELECT * FROM generation_jobs WHERE id=$1 AND tenant_id=$2 AND key_id=$3")
            .bind(request_id)
            .bind(identity.try_get::<String, _>("tenant_id")?)
            .bind(identity.try_get::<String, _>("key_id")?)
            .fetch_one(&mut **transaction)
            .await?;
    let mut originals = serde_json::Map::new();
    originals.insert("request".into(), original_row(&request)?);
    for (name, sql) in [
        (
            "statistics",
            "SELECT * FROM generation_stats_facts WHERE job_id=$1",
        ),
        (
            "assets",
            "SELECT * FROM generation_assets WHERE job_id=$1 ORDER BY asset_index,id",
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
        source: "generation".into(),
        request_id: parse_uuid(request_id.to_owned())?,
        tenant_id: parse_uuid(tenant.clone())?,
        key_id: parse_uuid(key.clone())?,
        created_at: identity.try_get("created_at")?,
    };
    let payload =
        Database::history_original_view_in_transaction(transaction, &history_identity).await?;
    let archive = Database::history_generation_archive_in_transaction(
        transaction,
        history_identity.key_id,
        history_identity.request_id,
    )
    .await?;
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
        sqlx::query_scalar("SELECT cost_micros FROM generation_stats_facts WHERE job_id=$1")
            .bind(request_id)
            .fetch_optional(&mut **transaction)
            .await?
            .flatten();
    let read_model = serde_json::json!({"archive":archive,"fact":fact,"upstream_name":upstream_name,"execution":execution,"structure":structure,"egress_attempts":egress,"transport_diagnostics":transport});
    let envelope = serde_json::json!({
        "version": 1, "source": "generation", "revision": revision,
        "tenant_id": tenant, "key_id": key, "request_id": request_id,
        "created_at": identity.try_get::<i64,_>("created_at")?,
        // This exact immutable archive identity survives source-table retirement.
        "archive_identity": {"tenant_id": tenant, "request_id": request_id},
        "payload": payload, "filter_cost_micros": filter_cost_micros, "statistics": statistics, "read_model": read_model, "originals": originals,
    });
    insert_envelope_in_transaction(transaction, &history_identity, revision, &envelope).await?;
    let events = sqlx::query("SELECT x.revision,x.request_id,x.created_at FROM request_history_export x JOIN request_events e ON e.event_id=x.request_id AND e.tenant_id=x.tenant_id AND e.key_id=x.key_id WHERE x.source='event' AND e.request_id=$1 AND x.tenant_id=$2 AND x.key_id=$3 ORDER BY x.revision")
        .bind(request_id).bind(&tenant).bind(&key).fetch_all(&mut **transaction).await?;
    for event in events {
        let event_id = parse_uuid(event.try_get("request_id")?)?;
        let event_identity = crate::request_history::HistoryIdentity {
            source: "event".into(),
            request_id: event_id,
            created_at: event.try_get("created_at")?,
            ..history_identity.clone()
        };
        let event_revision: i64 = event.try_get("revision")?;
        let payload =
            Database::history_original_event_in_transaction(transaction, event_id).await?;
        let original_event = original_event_in_transaction(transaction, event_id).await?;
        let envelope = serde_json::json!({"originals":{"event":original_event},"version":1,"source":"event","revision":event_revision,"tenant_id":tenant,"key_id":key,"request_id":event_id,"created_at":event_identity.created_at,"payload":payload,"filter_cost_micros":null,"statistics":null,"read_model":null,"archive_identity":{"tenant_id":tenant,"request_id":request_id,"source":"generation"}});
        insert_envelope_in_transaction(transaction, &event_identity, event_revision, &envelope)
            .await?;
    }
    Ok(())
}

pub(super) async fn capture_archive_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    // An off/shadow deployment keeps the existing capture contract. The durable
    // revision already allocated by its triggers also fences this projection.
    let identity = sqlx::query("SELECT revision, tenant_id, key_id, created_at FROM request_history_export WHERE source='archive' AND request_id=$1 AND EXISTS (SELECT 1 FROM request_history_state WHERE id=1 AND mode <> 'off')")
        .bind(request_id).fetch_optional(&mut **transaction).await?;
    let Some(identity) = identity else {
        return Ok(());
    };
    let revision: i64 = identity.try_get("revision")?;
    let request =
        sqlx::query("SELECT * FROM session_archive_unlinked_requests WHERE archive_request_id=$1 AND tenant_id=$2 AND key_id=$3")
            .bind(request_id)
            .bind(identity.try_get::<String, _>("tenant_id")?)
            .bind(identity.try_get::<String, _>("key_id")?)
            .fetch_one(&mut **transaction)
            .await?;
    let mut originals = serde_json::Map::new();
    originals.insert("request".into(), original_row(&request)?);
    let tenant: String = identity.try_get("tenant_id")?;
    let key: String = identity.try_get("key_id")?;
    let history_identity = crate::request_history::HistoryIdentity {
        source: "archive".into(),
        request_id: parse_uuid(request_id.to_owned())?,
        tenant_id: parse_uuid(tenant.clone())?,
        key_id: parse_uuid(key.clone())?,
        created_at: identity.try_get("created_at")?,
    };
    let payload =
        Database::history_original_view_in_transaction(transaction, &history_identity).await?;
    let archive = Database::history_archive_source_in_transaction(
        transaction,
        history_identity.key_id,
        request_id,
    )
    .await?;
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
    let filter_cost_micros: Option<i64> = None;
    let read_model = serde_json::json!({"archive":archive,"fact":fact,"upstream_name":upstream_name,"execution":execution,"structure":structure,"egress_attempts":egress,"transport_diagnostics":transport});
    let envelope = serde_json::json!({
        "version": 1, "source": "archive", "revision": revision,
        "tenant_id": tenant, "key_id": key, "request_id": request_id,
        "created_at": identity.try_get::<i64,_>("created_at")?,
        // This exact immutable archive identity survives source-table retirement.
        "archive_identity": {"tenant_id": tenant, "request_id": request_id},
        "payload": payload, "filter_cost_micros": filter_cost_micros, "statistics": statistics, "read_model": read_model, "originals": originals,
    });
    insert_envelope_in_transaction(transaction, &history_identity, revision, &envelope).await?;
    Ok(())
}

pub(super) async fn capture_request_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
) -> Result<(), AppError> {
    let sources:Vec<String>=sqlx::query_scalar("SELECT source FROM request_history_export WHERE request_id=$1 AND source IN ('native','generation','archive') AND EXISTS (SELECT 1 FROM request_history_state WHERE id=1 AND mode<>'off')")
        .bind(request_id).fetch_all(&mut **transaction).await?;
    for source in sources {
        match source.as_str() {
            "native" => capture_native_in_transaction(transaction, request_id).await?,
            "generation" => capture_generation_in_transaction(transaction, request_id).await?,
            "archive" => capture_archive_in_transaction(transaction, request_id).await?,
            _ => unreachable!("bounded source SQL"),
        }
    }
    Ok(())
}

async fn insert_envelope_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    identity: &crate::request_history::HistoryIdentity,
    revision: i64,
    envelope: &Value,
) -> Result<(), AppError> {
    let frozen = serde_json::to_string(envelope)
        .map_err(|_| AppError::storage("history_envelope_serialization"))?;
    let stored:String=sqlx::query_scalar("INSERT INTO request_history_immutable_outbox (revision,source,request_id,tenant_id,key_id,created_at,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(revision) DO UPDATE SET snapshot=request_history_immutable_outbox.snapshot RETURNING snapshot")
        .bind(revision).bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at).bind(&frozen).fetch_one(&mut **transaction).await?;
    if stored != frozen {
        return Err(AppError::storage("history_envelope_revision_conflict"));
    }
    Ok(())
}

impl Database {
    /// Transfer a bounded journal page to immutable envelopes while locking the
    /// existing revision fence. Source triggers cannot commit a newer revision
    /// halfway through this SQL-only capture.
    pub(crate) async fn history_capture_pending(&self, page_size: i64) -> Result<(), AppError> {
        if self.history_mode().await.map_err(AppError::from)? == "off" {
            return Ok(());
        }
        let mut transaction = self.pool.begin().await?;
        let rows=sqlx::query("SELECT revision,source,request_id,tenant_id,key_id,created_at FROM request_history_export WHERE snapshot IS NULL ORDER BY revision LIMIT $1")
            .bind(page_size.clamp(1, 64)).fetch_all(&mut *transaction).await?;
        for row in rows {
            let revision: i64 = row.try_get("revision")?;
            let identity = crate::request_history::HistoryIdentity {
                source: row.try_get("source")?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                created_at: row.try_get("created_at")?,
            };
            let locked=sqlx::query("UPDATE request_history_export SET snapshot=snapshot WHERE revision=$1 AND source=$2 AND request_id=$3 AND snapshot IS NULL")
                .bind(revision).bind(&identity.source).bind(identity.request_id.to_string()).execute(&mut *transaction).await?;
            if locked.rows_affected() == 0 {
                continue;
            }
            let request_id = identity.request_id.to_string();
            if identity.source == "event" {
                let payload = Self::history_original_event_in_transaction(
                    &mut transaction,
                    identity.request_id,
                )
                .await?;
                let archive_identity=sqlx::query("SELECT e.request_id,CASE WHEN EXISTS(SELECT 1 FROM generation_jobs g WHERE g.id=e.request_id AND g.tenant_id=e.tenant_id AND g.key_id=e.key_id) THEN 'generation' ELSE 'native' END AS source FROM request_events e WHERE e.event_id=$1 AND e.tenant_id=$2 AND e.key_id=$3")
                    .bind(&request_id).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).fetch_optional(&mut *transaction).await?;
                let archive_identity=archive_identity.map(|row| -> Result<Value,AppError> {Ok(serde_json::json!({"tenant_id":identity.tenant_id,"request_id":row.try_get::<String,_>("request_id")?,"source":row.try_get::<String,_>("source")?}))}).transpose()?;
                let original_event =
                    original_event_in_transaction(&mut transaction, identity.request_id).await?;
                let envelope = serde_json::json!({"originals":{"event":original_event},"version":1,"source":"event","revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":payload,"read_model":null,"statistics":null,"filter_cost_micros":null,"archive_identity":archive_identity});
                insert_envelope_in_transaction(&mut transaction, &identity, revision, &envelope)
                    .await?;
                continue;
            }
            if Self::history_original_view_in_transaction(&mut transaction, &identity)
                .await?
                .is_none()
            {
                let envelope = serde_json::json!({"version":1,"source":identity.source,"revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":null,"read_model":null,"statistics":null,"filter_cost_micros":null});
                insert_envelope_in_transaction(&mut transaction, &identity, revision, &envelope)
                    .await?;
                continue;
            }
            match identity.source.as_str() {
                "native" => capture_native_in_transaction(&mut transaction, &request_id).await?,
                "generation" => {
                    capture_generation_in_transaction(&mut transaction, &request_id).await?
                }
                "archive" => capture_archive_in_transaction(&mut transaction, &request_id).await?,
                _ => return Err(AppError::storage("immutable_history_unknown_source")),
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Resolve immutable object identities only after the producer committed.
    /// Once resolved, delivery bytes are frozen for retries and exact ACK.
    pub(crate) async fn history_immutable_pending(
        &self,
        page_size: i64,
    ) -> Result<Vec<crate::request_history::HistorySnapshot>, crate::request_history::HistoryError>
    {
        use crate::request_history::{HistoryError, HistoryIdentity, HistorySnapshot};
        let rows = sqlx::query("SELECT revision,source,request_id,tenant_id,key_id,created_at,snapshot,delivery_snapshot FROM request_history_immutable_outbox ORDER BY revision LIMIT $1")
            .bind(page_size).fetch_all(&self.pool).await?;
        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            let revision: i64 = row.try_get("revision")?;
            let stored_identity = HistoryIdentity {
                source: row.try_get("source")?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                created_at: row.try_get("created_at")?,
            };
            if let Some(frozen) = row.try_get::<Option<String>, _>("delivery_snapshot")? {
                let snapshot: HistorySnapshot = serde_json::from_str(&frozen)?;
                if snapshot.identity != stored_identity || snapshot.revision != revision {
                    return Err(HistoryError::Invalid("immutable delivery identity"));
                }
                snapshots.push(snapshot);
                continue;
            }
            let original: String = row.try_get("snapshot")?;
            let envelope: Value = serde_json::from_str(&original)?;
            if envelope["version"].as_i64() != Some(1)
                || !matches!(
                    envelope["source"].as_str(),
                    Some("native" | "generation" | "archive" | "event")
                )
            {
                return Err(HistoryError::Invalid(
                    "immutable history envelope version or source",
                ));
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
                let mut payload: Option<Value> =
                    serde_json::from_value(envelope["payload"].clone())?;
                let mut read_model = envelope["read_model"].clone();
                if identity.source != "event" && payload.is_some() {
                    let mut archive: crate::model::RequestArchiveRefs =
                        serde_json::from_value(read_model["archive"].clone())?;
                    archive.view = serde_json::from_value(
                        payload
                            .clone()
                            .ok_or(HistoryError::Invalid("immutable native payload"))?,
                    )?;
                    let mut archive = if identity.source == "native" {
                        self.resolve_raw_archive_roots(identity.tenant_id, archive)
                            .await?
                    } else {
                        archive
                    };
                    // Match source history_snapshot: raw-root availability
                    // enriches private archive refs, not the SQL canonical
                    // archive_state. Public list enrichment adds only a
                    // supplier summary for errors.
                    let mut canonical: crate::model::RequestView =
                        serde_json::from_value(payload.clone().ok_or(HistoryError::Mismatch)?)?;
                    if identity.source == "native"
                        && canonical.status_code.is_some_and(|status| status >= 400)
                    {
                        canonical.supplier_error = archive.view.supplier_error.clone();
                    }
                    archive.view = canonical;
                    payload = Some(serde_json::to_value(&archive.view)?);
                    read_model["archive"] = serde_json::to_value(archive)?;
                } else if let Some(payload) = &mut payload
                    && envelope["archive_identity"]["source"].as_str() != Some("generation")
                {
                    let native_request = envelope["archive_identity"]["request_id"]
                        .as_str()
                        .ok_or(HistoryError::Invalid("immutable event archive identity"))?;
                    if let Some(summary) = self
                        .body_archive()?
                        .supplier_summary(
                            identity.tenant_id,
                            parse_uuid(native_request.to_owned())?,
                        )
                        .await?
                    {
                        payload["supplier_error"] = serde_json::to_value(summary)?;
                    }
                }
                serde_json::to_string(&HistorySnapshot {
                    identity,
                    revision,
                    payload,
                    read_model,
                    filter_cost_micros: serde_json::from_value(
                        envelope["filter_cost_micros"].clone(),
                    )?,
                    statistics: serde_json::from_value(envelope["statistics"].clone())?,
                })?
            };
            let stored: Option<String> = sqlx::query_scalar("UPDATE request_history_immutable_outbox SET delivery_snapshot=COALESCE(delivery_snapshot,$1) WHERE revision=$2 AND snapshot=$3 RETURNING delivery_snapshot")
                .bind(&frozen).bind(revision).bind(&original).fetch_optional(&self.pool).await?;
            if let Some(stored) = stored {
                let snapshot: HistorySnapshot = serde_json::from_str(&stored)?;
                if snapshot.identity != stored_identity || snapshot.revision != revision {
                    return Err(HistoryError::Invalid("immutable frozen delivery identity"));
                }
                snapshots.push(snapshot);
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
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("history.sqlite").display()
        );
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        let identity = HistoryIdentity {
            source: "native".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let first = HistorySnapshot {
            identity: identity.clone(),
            revision: 1,
            filter_cost_micros: None,
            payload: None,
            statistics: None,
            read_model: Value::Null,
        };
        let second = HistorySnapshot {
            revision: 2,
            ..first.clone()
        };
        for snapshot in [&first, &second] {
            sqlx::query("INSERT INTO request_history_immutable_outbox (revision,source,request_id,tenant_id,key_id,created_at,snapshot,delivery_snapshot) VALUES ($1,$2,$3,$4,$5,$6,'{}',$7)")
                .bind(snapshot.revision).bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at).bind(serde_json::to_string(snapshot).unwrap())
                .execute(&db.pool).await.unwrap();
        }
        sqlx::query("INSERT INTO request_history_export (revision,source,request_id,tenant_id,key_id,created_at,snapshot) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(first.revision).bind(&identity.source).bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at).bind(serde_json::to_string(&first).unwrap()).execute(&db.pool).await.unwrap();
        // The mixed queues count the same global revision only once. No
        // original request or key exists to reconstruct either delivery.
        assert_eq!(db.history_pending_count().await.unwrap(), 2);
        let pending = db.history_pending(1).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].revision, 1);
        let mut wrong_tenant = first.clone();
        wrong_tenant.identity.tenant_id = Uuid::now_v7();
        db.history_acknowledge(&[wrong_tenant]).await.unwrap();
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
        // Only immutable revision 2 remains. All completion gates must see it.
        sqlx::query("UPDATE request_history_backfill SET done=1")
            .execute(&reopened.pool)
            .await
            .unwrap();
        reopened.history_save_verification(
            reopened.history_revision().await.unwrap(),
            &serde_json::json!({"read_model_version":crate::request_history::READ_MODEL_VERSION}),
        ).await.unwrap();
        assert!(reopened.history_refresh().await.is_err());
        assert!(reopened.history_switch(true).await.is_err());
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = writes.clone();
        let router = axum::Router::new().route(
            "/",
            axum::routing::post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    let row: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(row["revision"], 2);
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ""
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = crate::request_history::ClickHouse::new(
            reopened
                .http_clients
                .handle(crate::http_settings::HttpPurpose::General),
            &endpoint,
            "default".into(),
            "default".into(),
            String::new(),
        )
        .unwrap();
        assert!(matches!(
            crate::request_history::migration::verify(&reopened, &client).await,
            Err(crate::request_history::HistoryError::Changed)
        ));
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        crate::request_history::migration::drain(&reopened, &client, 1)
            .await
            .unwrap();
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reopened.history_pending_count().await.unwrap(), 0);
        server.abort();
    }
}
