use super::*;

/// The exact event row prepared before taking the global commit-order cursor.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EventDraft {
    pub event_id: String,
    pub tenant_id: String,
    pub key_id: String,
    pub request_id: String,
    pub event_at: i64,
    pub event_kind: String,
    pub protocol: String,
    pub model: String,
    #[serde(default)]
    pub status_code: Option<i64>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
    #[serde(default)]
    pub error_code: Option<String>,
}

impl EventDraft {
    pub const VALUES_SQL: &'static str = "SELECT CAST($1 AS TEXT) AS event_id, CAST($2 AS TEXT) AS tenant_id, CAST($3 AS TEXT) AS key_id, CAST($4 AS TEXT) AS request_id, CAST($5 AS BIGINT) AS event_at, CAST($6 AS TEXT) AS event_kind, CAST($7 AS TEXT) AS protocol, CAST($8 AS TEXT) AS model, CAST($9 AS BIGINT) AS status_code, CAST($10 AS BIGINT) AS duration_ms, CAST($11 AS BIGINT) AS input_tokens, CAST($12 AS BIGINT) AS output_tokens, CAST($13 AS BIGINT) AS cost_micros, CAST($14 AS TEXT) AS error_code";

    pub fn from_row(row: &AnyRow) -> Result<Self, AppError> {
        serde_json::from_value(original_row(row)?)
            .map_err(|_| AppError::storage("history_envelope_serialization"))
    }

    pub fn bind<'q>(
        &'q self,
        query: sqlx::query::Query<'q, Any, sqlx::any::AnyArguments>,
    ) -> sqlx::query::Query<'q, Any, sqlx::any::AnyArguments> {
        query
            .bind(&self.event_id)
            .bind(&self.tenant_id)
            .bind(&self.key_id)
            .bind(&self.request_id)
            .bind(self.event_at)
            .bind(&self.event_kind)
            .bind(&self.protocol)
            .bind(&self.model)
            .bind(self.status_code)
            .bind(self.duration_ms)
            .bind(self.input_tokens)
            .bind(self.output_tokens)
            .bind(self.cost_micros)
            .bind(&self.error_code)
    }

    pub async fn prepare(
        self,
        tx: &mut Transaction<'_, Any>,
    ) -> Result<PreparedEventCapture, AppError> {
        let mode_sql = if tx.as_mut().backend_name() == "PostgreSQL" {
            "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE"
        } else {
            "SELECT mode FROM request_history_state WHERE id=1"
        };
        let mode: String = sqlx::query_scalar(sqlx::AssertSqlSafe(mode_sql))
            .fetch_one(&mut **tx)
            .await?;
        if mode == "off" {
            return Ok(PreparedEventCapture {
                draft: self,
                capture: None,
            });
        }
        let payload = Database::history_draft_event_in_transaction(tx, &self).await?;
        let generation: i64 = sqlx::query_scalar("SELECT CAST(CASE WHEN EXISTS(SELECT 1 FROM generation_jobs WHERE id=$1 AND tenant_id=$2 AND key_id=$3) THEN 1 ELSE 0 END AS BIGINT)")
            .bind(&self.request_id).bind(&self.tenant_id).bind(&self.key_id).fetch_one(&mut **tx).await?;
        let archive_identity = serde_json::json!({"tenant_id":self.tenant_id,"request_id":self.request_id,"source":if generation==1 {"generation"} else {"native"}});
        Ok(PreparedEventCapture {
            draft: self,
            capture: Some((
                serde_json::to_value(payload)
                    .map_err(|_| AppError::storage("history_envelope_serialization"))?,
                archive_identity,
            )),
        })
    }
}

pub(crate) struct PreparedEventCapture {
    draft: EventDraft,
    capture: Option<(Value, Value)>,
}

impl PreparedEventCapture {
    pub async fn publish(
        tx: &mut Transaction<'_, Any>,
        prepared: Option<Self>,
        now: i64,
    ) -> Result<u64, AppError> {
        let Some(prepared) = prepared else {
            return Ok(0);
        };
        let cursor = crate::db::allocate_request_event_cursor(
            tx,
            now,
            &prepared.draft.tenant_id,
            &prepared.draft.key_id,
            &prepared.draft.request_id,
        )
        .await?;
        prepared.insert(tx, &cursor).await?;
        Ok(1)
    }
    pub async fn insert(
        mut self,
        tx: &mut Transaction<'_, Any>,
        cursor: &super::super::RequestEventCursor,
    ) -> Result<(), AppError> {
        self.draft.event_id = cursor.event_id.to_string();
        self.draft.event_at = cursor.event_at;
        self.draft.bind(sqlx::query("INSERT INTO request_events (event_id,tenant_id,key_id,request_id,event_at,event_kind,protocol,model,status_code,duration_ms,input_tokens,output_tokens,cost_micros,error_code) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)"))
            .execute(&mut **tx).await?;
        let revision: Option<i64> = sqlx::query_scalar("SELECT revision FROM request_history_export WHERE source='event' AND request_id=$1 AND created_at=$2 AND EXISTS(SELECT 1 FROM request_history_state WHERE id=1 AND mode<>'off')")
            .bind(&self.draft.event_id).bind(self.draft.event_at).fetch_optional(&mut **tx).await?;
        let Some(revision) = revision else {
            return Ok(());
        };
        let Some((mut payload, archive_identity)) = self.capture else {
            return Err(AppError::storage("history_event_capture_mode_changed"));
        };
        if let Some(payload) = payload.as_object_mut() {
            payload.insert(
                "event_id".into(),
                Value::String(self.draft.event_id.clone()),
            );
            payload.insert("event_at".into(), self.draft.event_at.into());
        }
        let identity = crate::request_history::HistoryIdentity {
            source: "event".into(),
            request_id: parse_uuid(self.draft.event_id.clone())?,
            tenant_id: parse_uuid(self.draft.tenant_id.clone())?,
            key_id: parse_uuid(self.draft.key_id.clone())?,
            created_at: self.draft.event_at,
        };
        let original = serde_json::to_value(&self.draft)
            .map_err(|_| AppError::storage("history_envelope_serialization"))?;
        let envelope = serde_json::json!({"originals":{"event":original},"version":1,"source":"event","revision":revision,"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":identity.created_at,"payload":payload,"read_model":null,"statistics":null,"filter_cost_micros":null,"archive_identity":archive_identity});
        insert_envelope_in_transaction(tx, &identity, revision, &envelope).await
    }
}
