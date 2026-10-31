//! Request-history half of PR699's analysis interface. The session owner keeps
//! identity authorization, relationship expansion and timezone validation.
use super::*;
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Default, Serialize)]
struct TaskUsage {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    request_duration_sum_ms: i64,
    unknown_cost_requests: i64,
    costs: BTreeMap<String, i64>,
}
impl TaskUsage {
    fn add(&mut self, snapshot: &HistorySnapshot) {
        let fact = &snapshot.read_model["fact"];
        let payload = snapshot.payload.as_ref().unwrap_or(&Value::Null);
        let source = if fact.is_object() { fact } else { payload };
        self.requests += 1;
        self.input_tokens += source["input_tokens"].as_i64().unwrap_or_default();
        self.output_tokens += source["output_tokens"].as_i64().unwrap_or_default();
        self.cached_input_tokens += source["cached_input_tokens"].as_i64().unwrap_or_default();
        self.cache_write_tokens += source["cache_write_tokens"].as_i64().unwrap_or_default();
        self.request_duration_sum_ms += source["duration_ms"].as_i64().unwrap_or_default();
        if let Some((currency, cost)) = fact["currency"]
            .as_str()
            .filter(|c| !c.is_empty())
            .zip(fact["cost_micros"].as_i64())
        {
            *self.costs.entry(currency.into()).or_default() += cost;
        } else {
            self.unknown_cost_requests += 1;
        }
    }
}

impl ClickHouse {
    /// Input is the exact, already-authorized union produced by PR699. Empty
    /// selection stays empty. There is no credential-wide unlinked fallback.
    pub async fn session_task_activity(
        &self,
        tenant: Uuid,
        selected: &[(Uuid, Uuid)],
        timezone: &str,
        offset_minutes: Option<i32>,
    ) -> Result<Value, HistoryError> {
        if tenant.is_nil() || offset_minutes.is_some_and(|v| !(-840..=840).contains(&v)) {
            return Err(HistoryError::Invalid("session task identity or offset"));
        }
        if timezone.is_empty()
            || (offset_minutes.is_some() && !matches!(timezone, "UTC" | "Etc/UTC"))
        {
            return Err(HistoryError::Invalid("session task timezone"));
        }
        let selected = selected.iter().copied().collect::<BTreeSet<_>>();
        let mut total = TaskUsage::default();
        let mut daily = BTreeMap::<i64, (i64, TaskUsage)>::new();
        let mut first = None::<i64>;
        let mut last = None::<i64>;
        for (key_id, session_id) in selected {
            let mut query = HistoryQuery {
                scope: HistoryScope::Key {
                    tenant_id: tenant,
                    key_id,
                },
                from_created_at: None,
                to_created_at: None,
                cursor: None,
                limit: 500,
                predicates: vec![
                    HistoryPredicate {
                        dimension: Dimension::SessionId,
                        comparison: Comparison::Equals,
                        value: PredicateValue::Text(session_id.to_string()),
                    },
                    HistoryPredicate {
                        dimension: Dimension::Source,
                        comparison: Comparison::NotEquals,
                        value: PredicateValue::Text("generation".into()),
                    },
                ],
            };
            loop {
                #[derive(serde::Deserialize)]
                struct Zoned {
                    snapshot: String,
                    snapshot_hash: String,
                    local_date: String,
                    day_start_at: i64,
                }
                let mut sql = query.list_sql()?;
                let zone = sql.bind(timezone, "String");
                let (date, start) = if let Some(offset) = offset_minutes {
                    let offset = sql.bind(offset, "Int32");
                    let shifted =
                        format!("addMinutes(fromUnixTimestamp64Milli(created_at,'UTC'),{offset})");
                    (
                        format!("formatDateTime({shifted},'%F','UTC')"),
                        format!(
                            "toUnixTimestamp64Milli(subtractMinutes(toDateTime64(toStartOfDay({shifted}),3,'UTC'),{offset}))"
                        ),
                    )
                } else {
                    (
                        format!(
                            "formatDateTime(fromUnixTimestamp64Milli(created_at), '%F', {zone})"
                        ),
                        format!(
                            "toUnixTimestamp64Milli(toDateTime64(toStartOfDay(fromUnixTimestamp64Milli(created_at),{zone}),3,{zone}))"
                        ),
                    )
                };
                sql.sql=sql.sql.replacen("SELECT snapshot, snapshot_hash",&format!("SELECT snapshot, snapshot_hash, {date} AS local_date, {start} AS day_start_at"),1);
                let mut page = self.rows::<Zoned>(sql).await?;
                page.truncate(query.limit);
                if page.is_empty() {
                    break;
                }
                for row in page {
                    let snapshot: HistorySnapshot = serde_json::from_str(&row.snapshot)?;
                    if canonical_hash(&snapshot)? != row.snapshot_hash
                        || snapshot.identity.tenant_id != tenant
                        || snapshot.identity.key_id != key_id
                    {
                        return Err(HistoryError::Mismatch);
                    }
                    query.cursor = Some(HistoryCursor {
                        before_created_at: snapshot.identity.created_at,
                        before_id: snapshot.identity.request_id,
                    });
                    if !snapshot.read_model["fact"].is_object()
                        && snapshot.identity.source != "archive"
                    {
                        continue;
                    }
                    let date = NaiveDate::parse_from_str(&row.local_date, "%Y-%m-%d")
                        .map_err(|_| HistoryError::Mismatch)?;
                    let day = date
                        .signed_duration_since(
                            NaiveDate::from_ymd_opt(1970, 1, 1).ok_or(HistoryError::Mismatch)?,
                        )
                        .num_days();
                    let at = snapshot.identity.created_at;
                    first = Some(first.map_or(at, |v| v.min(at)));
                    last = Some(last.map_or(at, |v| v.max(at)));
                    total.add(&snapshot);
                    daily
                        .entry(day)
                        .or_insert_with(|| (row.day_start_at, TaskUsage::default()))
                        .1
                        .add(&snapshot);
                }
            }
        }
        let span = daily
            .first_key_value()
            .zip(daily.last_key_value())
            .map_or(0, |((a, _), (b, _))| b - a + 1);
        Ok(
            json!({"first_request_at":first,"last_request_at":last,"calendar_span_days":span,"active_days":daily.len(),"totals":total,
            "daily":daily.into_iter().map(|(day,(start,usage))|json!({"day_bucket":day,"day_start_at":start,"usage":usage})).collect::<Vec<_>>(),
            "coverage":"completed_request_facts_and_archive_only"}),
        )
    }
}
