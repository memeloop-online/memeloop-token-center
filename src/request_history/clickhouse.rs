use crate::http_settings::{ClientOptions, HttpClientSource};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

use super::{
    DailyBucket, HistoryCursor, HistoryError, HistoryPage, HistoryQuery, HistoryScope,
    HistorySnapshot, query::BoundQuery,
};

#[derive(Clone)]
pub struct ClickHouse {
    client: Arc<dyn HttpClientSource>,
    endpoint: reqwest::Url,
    database: String,
    user: String,
    password: String,
}

impl ClickHouse {
    pub fn from_env(client: impl HttpClientSource + 'static) -> Result<Option<Self>, HistoryError> {
        Self::from_env_role(client, "WRITER")
    }

    pub fn reader_from_env(
        client: impl HttpClientSource + 'static,
    ) -> Result<Option<Self>, HistoryError> {
        Self::from_env_role(client, "READER")
    }

    pub(crate) fn from_env_role(
        client: impl HttpClientSource + 'static,
        role: &str,
    ) -> Result<Option<Self>, HistoryError> {
        let Ok(endpoint) = std::env::var("MTC_REQUEST_HISTORY_URL") else {
            return Ok(None);
        };
        Self::new(
            client,
            &endpoint,
            std::env::var("MTC_REQUEST_HISTORY_DATABASE").unwrap_or_else(|_| "default".into()),
            std::env::var(format!("MTC_REQUEST_HISTORY_{role}_USER"))
                .or_else(|_| std::env::var("MTC_REQUEST_HISTORY_USER"))
                .unwrap_or_else(|_| "default".into()),
            std::env::var(format!("MTC_REQUEST_HISTORY_{role}_PASSWORD"))
                .or_else(|_| std::env::var("MTC_REQUEST_HISTORY_PASSWORD"))
                .unwrap_or_default(),
        )
        .map(Some)
    }

    pub fn new(
        client: impl HttpClientSource + 'static,
        endpoint: &str,
        database: String,
        user: String,
        password: String,
    ) -> Result<Self, HistoryError> {
        let endpoint =
            reqwest::Url::parse(endpoint).map_err(|_| HistoryError::Invalid("ClickHouse URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(HistoryError::Invalid("ClickHouse HTTP(S) URL"));
        }
        Ok(Self {
            client: Arc::new(client),
            endpoint,
            database,
            user,
            password,
        })
    }

    async fn execute(
        &self,
        query: BoundQuery,
        data: Option<String>,
    ) -> Result<reqwest::Response, HistoryError> {
        let mut parameters = query.parameters;
        parameters.extend([
            ("wait_end_of_query".into(), "1".into()),
            ("optimize_move_to_prewhere".into(), "0".into()),
            ("optimize_move_to_prewhere_if_final".into(), "0".into()),
            ("output_format_json_quote_64bit_integers".into(), "0".into()),
        ]);
        let body = match data {
            Some(data) => {
                parameters.extend([
                    ("query".into(), query.sql),
                    ("async_insert".into(), "0".into()),
                ]);
                data
            }
            None => query.sql,
        };
        let response = self
            .client
            .http_client(ClientOptions::default())
            .map_err(|_| HistoryError::Unavailable)?
            .post(self.endpoint.clone())
            .basic_auth(&self.user, Some(&self.password))
            .header("X-ClickHouse-Database", &self.database)
            .query(&parameters)
            .body(body)
            .send()
            .await
            .map_err(|_| HistoryError::Unavailable)?;
        if !response.status().is_success()
            || response
                .headers()
                .get("X-ClickHouse-Exception-Code")
                .is_some_and(|value| value.as_bytes() != b"0")
        {
            return Err(HistoryError::Rejected {
                status: response.status().as_u16(),
                code: response
                    .headers()
                    .get("X-ClickHouse-Exception-Code")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok()),
            });
        }
        Ok(response)
    }

    pub(crate) async fn rows<Row: DeserializeOwned>(
        &self,
        query: BoundQuery,
    ) -> Result<Vec<Row>, HistoryError> {
        let mut response = self.execute(query, None).await?;
        let mut pending = Vec::new();
        let mut rows = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| HistoryError::Unavailable)?
        {
            pending.extend_from_slice(&chunk);
            let mut consumed = 0;
            for (index, byte) in pending.iter().enumerate() {
                if *byte == b'\n' {
                    if index > consumed {
                        rows.push(serde_json::from_slice(&pending[consumed..index])?);
                    }
                    consumed = index + 1;
                }
            }
            pending.drain(..consumed);
        }
        if !pending.is_empty() {
            rows.push(serde_json::from_slice(&pending)?);
        }
        Ok(rows)
    }

    pub async fn initialize(&self) -> Result<(), HistoryError> {
        let response = self
            .execute(BoundQuery::new(super::SCHEMA), None)
            .await?
            .bytes()
            .await
            .map_err(|_| HistoryError::Unavailable)?;
        if !response.iter().all(u8::is_ascii_whitespace) {
            return Err(HistoryError::Unavailable);
        }
        self.execute(BoundQuery::new("ALTER TABLE request_history_v1 ADD COLUMN IF NOT EXISTS stats_filter_cost_micros Nullable(Int64)"), None).await?.error_for_status().map_err(|_|HistoryError::Unavailable)?;
        Ok(())
    }

    pub async fn append(&self, snapshots: &[HistorySnapshot]) -> Result<(), HistoryError> {
        if snapshots.is_empty() {
            return Ok(());
        }
        let mut body = String::new();
        let mut identities = std::collections::BTreeMap::new();
        for snapshot in snapshots {
            if snapshot.revision <= 0 {
                return Err(HistoryError::Invalid("durable revision is required"));
            }
            let identity = (
                snapshot.identity.source.clone(),
                snapshot.identity.request_id,
                snapshot.revision,
            );
            let hash = super::canonical_hash(snapshot)?;
            if identities
                .insert(identity, hash.clone())
                .is_some_and(|prior| prior != hash)
            {
                return Err(HistoryError::Mismatch);
            }
            body.push_str(&serde_json::to_string(&snapshot.row()?)?);
            body.push('\n');
        }
        let response = self
            .execute(
                BoundQuery::new("INSERT INTO request_history_v1 FORMAT JSONEachRow"),
                Some(body),
            )
            .await?
            .bytes()
            .await
            .map_err(|_| HistoryError::Unavailable)?;
        if !response.iter().all(u8::is_ascii_whitespace) {
            return Err(HistoryError::Unavailable);
        }
        Ok(())
    }

    pub async fn snapshots(
        &self,
        query: &HistoryQuery,
    ) -> Result<Vec<HistorySnapshot>, HistoryError> {
        self.read_snapshots(query, false).await
    }

    pub(crate) async fn read_snapshots(
        &self,
        query: &HistoryQuery,
        statistics: bool,
    ) -> Result<Vec<HistorySnapshot>, HistoryError> {
        #[derive(serde::Deserialize)]
        struct Stored {
            snapshot: String,
            snapshot_hash: String,
        }
        let mut sql = query.list_sql()?;
        if statistics {
            sql.sql = sql
                .sql
                .replace("filter_status", "stats_status")
                .replace("filter_cost_micros", "stats_filter_cost_micros");
        }
        self.rows::<Stored>(sql)
            .await?
            .into_iter()
            .map(|row| {
                let snapshot: HistorySnapshot = serde_json::from_str(&row.snapshot)?;
                let allowed = match query.scope {
                    HistoryScope::EmptyTenant => false,
                    HistoryScope::Key { tenant_id, key_id } => {
                        snapshot.identity.tenant_id == tenant_id
                            && snapshot.identity.key_id == key_id
                    }
                    HistoryScope::Tenant(tenant_id) => snapshot.identity.tenant_id == tenant_id,
                    HistoryScope::GlobalOperator => true,
                };
                if !allowed || super::canonical_hash(&snapshot)? != row.snapshot_hash {
                    return Err(HistoryError::Mismatch);
                }
                Ok(snapshot)
            })
            .collect()
    }

    pub async fn list(&self, query: &HistoryQuery) -> Result<HistoryPage, HistoryError> {
        let mut snapshots = self.snapshots(query).await?;
        let has_more = snapshots.len() > query.limit;
        snapshots.truncate(query.limit);
        let next_cursor = if has_more {
            snapshots.last().map(|snapshot| HistoryCursor {
                before_created_at: snapshot.identity.created_at,
                before_id: snapshot.identity.request_id,
            })
        } else {
            None
        };
        let requests = snapshots
            .into_iter()
            .filter_map(|snapshot| snapshot.payload)
            .map(|mut payload| {
                if matches!(query.scope, HistoryScope::Key { .. }) {
                    payload["credential_identity"] = Value::Null;
                }
                payload
            })
            .collect();
        Ok(HistoryPage {
            requests,
            next_cursor,
        })
    }

    pub(crate) async fn event_snapshots(
        &self,
        scope: HistoryScope,
        after: i64,
        after_id: Option<uuid::Uuid>,
        limit: usize,
    ) -> Result<Vec<HistorySnapshot>, HistoryError> {
        #[derive(serde::Deserialize)]
        struct Stored {
            snapshot: String,
            snapshot_hash: String,
        }
        let query = HistoryQuery {
            scope,
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit,
            predicates: vec![],
        };
        let mut sql = query.latest()?;
        let at = sql.bind(after, "Int64");
        let id = sql.bind(
            after_id.map(|v| v.to_string()).unwrap_or_default(),
            "String",
        );
        let size = sql.bind(limit, "UInt64");
        sql.sql = format!(
            "SELECT snapshot, snapshot_hash FROM ({}) WHERE source = 'event' AND (created_at, toString(request_id)) > ({at}, {id}) ORDER BY created_at, toString(request_id) LIMIT {size} FORMAT JSONEachRow",
            sql.sql
        );
        self.rows::<Stored>(sql)
            .await?
            .into_iter()
            .map(|row| {
                let snapshot: HistorySnapshot = serde_json::from_str(&row.snapshot)?;
                let allowed = match scope {
                    HistoryScope::EmptyTenant => false,
                    HistoryScope::Key { tenant_id, key_id } => {
                        snapshot.identity.tenant_id == tenant_id
                            && snapshot.identity.key_id == key_id
                    }
                    HistoryScope::Tenant(tenant_id) => snapshot.identity.tenant_id == tenant_id,
                    HistoryScope::GlobalOperator => true,
                };
                if !allowed
                    || snapshot.identity.source != "event"
                    || super::canonical_hash(&snapshot)? != row.snapshot_hash
                {
                    return Err(HistoryError::Mismatch);
                }
                Ok(snapshot)
            })
            .collect()
    }

    pub(crate) async fn events(
        &self,
        scope: HistoryScope,
        after: i64,
        after_id: Option<uuid::Uuid>,
        limit: usize,
    ) -> Result<Vec<crate::model::RequestEventView>, HistoryError> {
        self.event_snapshots(scope, after, after_id, limit)
            .await?
            .into_iter()
            .map(|snapshot| {
                Ok(serde_json::from_value(
                    snapshot.payload.ok_or(HistoryError::Mismatch)?,
                )?)
            })
            .collect()
    }

    pub(crate) async fn request_snapshot(
        &self,
        scope: HistoryScope,
        request_id: uuid::Uuid,
    ) -> Result<Option<HistorySnapshot>, HistoryError> {
        let query = HistoryQuery {
            scope,
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit: 1,
            predicates: vec![super::HistoryPredicate {
                dimension: super::Dimension::RequestId,
                comparison: super::Comparison::Equals,
                value: super::PredicateValue::Uuid(request_id),
            }],
        };
        Ok(self.snapshots(&query).await?.into_iter().next())
    }

    pub(crate) async fn egress_summary(
        &self,
        scope: HistoryScope,
        start: i64,
        end: i64,
    ) -> Result<Vec<crate::db::egress::RequestEgressSummary>, HistoryError> {
        use crate::db::egress::{RequestEgressAttempt, RequestEgressSummary};
        let query = HistoryQuery {
            scope,
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit: 500,
            predicates: vec![],
        };
        let mut groups = std::collections::BTreeMap::<String, RequestEgressSummary>::new();
        self.scan(query, |snapshot| {
            let attempts: Vec<RequestEgressAttempt> =
                serde_json::from_value(snapshot.read_model["egress_attempts"].clone())?;
            for attempt in attempts
                .into_iter()
                .filter(|a| a.observed_at >= start && a.observed_at < end)
            {
                let key = serde_json::to_string(&attempt.selected)?;
                let group = groups.entry(key).or_insert_with(|| RequestEgressSummary {
                    selected: attempt.selected.clone(),
                    recorded_attempts: 0,
                    completed_attempts: 0,
                    final_response_attempts: 0,
                    request_known_attempts: 0,
                    response_known_attempts: 0,
                    request_payload_bytes: None,
                    response_payload_bytes: None,
                    measurement_basis: "application_payload_estimate",
                    wire_bytes: None,
                    supplier_billed_bytes: None,
                });
                group.recorded_attempts += 1;
                group.completed_attempts += i64::from(attempt.completed);
                group.final_response_attempts += i64::from(attempt.final_response);
                if let Some(bytes) = attempt.request_payload_bytes {
                    group.request_known_attempts += 1;
                    group.request_payload_bytes =
                        Some(group.request_payload_bytes.unwrap_or_default() + bytes);
                }
                if let Some(bytes) = attempt.response_payload_bytes {
                    group.response_known_attempts += 1;
                    group.response_payload_bytes =
                        Some(group.response_payload_bytes.unwrap_or_default() + bytes);
                }
            }
            Ok(())
        })
        .await?;
        Ok(groups.into_values().collect())
    }

    pub async fn daily(&self, query: &HistoryQuery) -> Result<Vec<DailyBucket>, HistoryError> {
        self.rows(query.daily_sql()?).await
    }
}
