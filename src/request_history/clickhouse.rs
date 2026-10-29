use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    DailyBucket, HistoryCursor, HistoryError, HistoryPage, HistoryQuery, HistoryScope,
    HistorySnapshot, query::BoundQuery,
};

#[derive(Clone)]
pub struct ClickHouse {
    client: reqwest::Client,
    endpoint: reqwest::Url,
    database: String,
    user: String,
    password: String,
}

impl ClickHouse {
    pub fn from_env(client: reqwest::Client) -> Result<Option<Self>, HistoryError> {
        let Ok(endpoint) = std::env::var("MTC_REQUEST_HISTORY_URL") else {
            return Ok(None);
        };
        Self::new(
            client,
            &endpoint,
            std::env::var("MTC_REQUEST_HISTORY_DATABASE").unwrap_or_else(|_| "default".into()),
            std::env::var("MTC_REQUEST_HISTORY_USER").unwrap_or_else(|_| "default".into()),
            std::env::var("MTC_REQUEST_HISTORY_PASSWORD").unwrap_or_default(),
        )
        .map(Some)
    }

    pub fn new(
        client: reqwest::Client,
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
            client,
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
            return Err(HistoryError::Unavailable);
        }
        Ok(response)
    }

    async fn rows<Row: DeserializeOwned>(
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
        self.execute(BoundQuery::new(super::SCHEMA), None)
            .await?
            .bytes()
            .await
            .map_err(|_| HistoryError::Unavailable)?;
        Ok(())
    }

    pub async fn append(&self, snapshots: &[HistorySnapshot]) -> Result<(), HistoryError> {
        if snapshots.is_empty() {
            return Ok(());
        }
        let mut body = String::new();
        for snapshot in snapshots {
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
        #[derive(serde::Deserialize)]
        struct Stored {
            snapshot: String,
        }
        self.rows::<Stored>(query.list_sql()?)
            .await?
            .into_iter()
            .map(|row| serde_json::from_str(&row.snapshot).map_err(Into::into))
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

    pub async fn daily(&self, query: &HistoryQuery) -> Result<Vec<DailyBucket>, HistoryError> {
        self.rows(query.daily_sql()?).await
    }
}
