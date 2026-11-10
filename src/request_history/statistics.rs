use std::collections::{BTreeMap, BTreeSet};

use super::{ClickHouse, HistoryCursor, HistoryError, HistoryQuery, HistorySnapshot};
use crate::model::{CacheStats, OperatorStats, StatsBucket, StatsSummary, UsageAnalysisCost};

#[derive(Default)]
struct Accumulator {
    requests: i64,
    succeeded: i64,
    failed: i64,
    input: i64,
    output: i64,
    cache: CacheStats,
    costs: BTreeMap<String, i64>,
    unknown_cost: bool,
    unknown_currencies: BTreeSet<String>,
}

fn add(total: &mut i64, value: i64) -> Result<(), HistoryError> {
    *total = total
        .checked_add(value)
        .ok_or(HistoryError::Invalid("statistics overflow"))?;
    Ok(())
}

impl Accumulator {
    fn accumulate(&mut self, snapshot: &HistorySnapshot) -> Result<(), HistoryError> {
        let Some(stats) = &snapshot.statistics else {
            return Ok(());
        };
        add(&mut self.requests, 1)?;
        add(&mut self.succeeded, i64::from(stats.status == "success"))?;
        add(&mut self.failed, i64::from(stats.status == "error"))?;
        add(&mut self.input, stats.input_tokens)?;
        add(&mut self.output, stats.output_tokens)?;
        if let Some(read) = stats.cache_known_read_tokens {
            add(&mut self.cache.reported_read_tokens, read)?;
            add(&mut self.cache.reported_requests, 1)?;
        }
        if let Some((read, input)) = stats
            .cache_known_read_tokens
            .zip(stats.cache_known_input_tokens)
        {
            add(&mut self.cache.known_read_tokens, read)?;
            add(&mut self.cache.known_input_tokens, input)?;
            add(&mut self.cache.eligible_requests, 1)?;
        } else {
            add(&mut self.cache.unknown_requests, 1)?;
        }
        if let Some((cost, currency)) = stats
            .cost_micros
            .zip(stats.currency.as_ref())
            .filter(|(_, currency)| !currency.is_empty())
        {
            add(self.costs.entry(currency.clone()).or_default(), cost)?;
        } else {
            self.unknown_cost = true;
            if let Some(currency) = &stats.currency {
                self.unknown_currencies.insert(currency.clone());
            }
        }
        Ok(())
    }

    fn costs(&self) -> (Option<String>, Vec<UsageAnalysisCost>) {
        let costs: Vec<_> = self
            .costs
            .iter()
            .filter(|(currency, _)| !self.unknown_currencies.contains(*currency))
            .map(|(currency, micros)| UsageAnalysisCost {
                currency: currency.clone(),
                cost: rust_decimal::Decimal::from_i128_with_scale(i128::from(*micros), 6)
                    .normalize()
                    .to_string(),
            })
            .collect();
        let compatible = if self.unknown_cost || costs.len() > 1 {
            None
        } else {
            Some(
                costs
                    .first()
                    .map(|cost| cost.cost.clone())
                    .unwrap_or_else(|| "0".into()),
            )
        };
        (compatible, costs)
    }

    fn summary(mut self) -> StatsSummary {
        let (total_cost, costs) = self.costs();
        self.cache.hit_rate = (self.cache.known_input_tokens > 0)
            .then(|| self.cache.known_read_tokens as f64 / self.cache.known_input_tokens as f64);
        StatsSummary {
            total_requests: self.requests,
            successful_requests: self.succeeded,
            failed_requests: self.failed,
            pending_requests: self.requests - self.succeeded - self.failed,
            input_tokens: self.input,
            output_tokens: self.output,
            cache_usage: self.cache,
            total_cost,
            costs,
        }
    }

    fn bucket(self, name: String) -> StatsBucket {
        let (cost, costs) = self.costs();
        StatsBucket {
            name,
            requests: self.requests,
            input_tokens: self.input,
            output_tokens: self.output,
            cost,
            costs,
        }
    }
}

impl ClickHouse {
    pub async fn statistics(&self, query: &HistoryQuery) -> Result<OperatorStats, HistoryError> {
        let mut query = query.clone();
        query.cursor = None;
        let mut summary = Accumulator::default();
        let mut models = BTreeMap::<String, Accumulator>::new();
        let mut days = BTreeMap::<String, Accumulator>::new();
        let mut errors = BTreeMap::<String, Accumulator>::new();
        loop {
            let mut page = self.read_snapshots(&query, true).await?;
            page.truncate(query.limit);
            if page.is_empty() {
                break;
            }
            for snapshot in &page {
                if snapshot.statistics.is_none() {
                    continue;
                }
                let payload = snapshot.payload.as_ref().ok_or(HistoryError::Mismatch)?;
                summary.accumulate(snapshot)?;
                models
                    .entry(payload["model"].as_str().unwrap_or_default().into())
                    .or_default()
                    .accumulate(snapshot)?;
                let day = chrono::DateTime::from_timestamp_millis(snapshot.identity.created_at)
                    .ok_or(HistoryError::Invalid("statistics date"))?
                    .format("%Y-%m-%d")
                    .to_string();
                days.entry(day).or_default().accumulate(snapshot)?;
                if let Some(error) = payload["error_code"]
                    .as_str()
                    .filter(|error| !error.is_empty())
                {
                    errors
                        .entry(error.into())
                        .or_default()
                        .accumulate(snapshot)?;
                }
            }
            let last = page.last().ok_or(HistoryError::Mismatch)?;
            query.cursor = Some(HistoryCursor {
                before_created_at: last.identity.created_at,
                before_id: last.identity.request_id,
            });
        }
        fn buckets(values: BTreeMap<String, Accumulator>, rank: bool) -> Vec<StatsBucket> {
            let mut values: Vec<_> = values
                .into_iter()
                .map(|(name, value)| value.bucket(name))
                .collect();
            if rank {
                values.sort_by(|left, right| {
                    right
                        .requests
                        .cmp(&left.requests)
                        .then_with(|| left.name.cmp(&right.name))
                });
                values.truncate(100);
            }
            values
        }
        Ok(OperatorStats {
            summary: summary.summary(),
            by_model: buckets(models, true),
            by_day: buckets(days, false),
            errors: buckets(errors, true),
        })
    }
}
