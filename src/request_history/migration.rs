use std::collections::BTreeMap;

use clap::Subcommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{ClickHouse, DailyBucket, HistoryError, HistoryQuery, HistoryScope};
use crate::{
    AppState,
    config::Config,
    db::{Database, RequestListFilter},
};

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    Start,
    Backfill {
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(i64).range(1..))]
        page_size: i64,
    },
    Drain {
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(i64).range(1..))]
        page_size: i64,
    },
    Verify,
    Switch,
    Rollback,
    Status,
}

pub async fn command(config: &Config, command: HistoryCommand) -> Result<Value, HistoryError> {
    let database =
        Database::connect_for_migration(&config.database_url, config.database_max_connections)
            .await?;
    if matches!(command, HistoryCommand::Rollback) {
        database.history_switch(false).await?;
        return Ok(json!({"mode": "shadow"}));
    }
    if matches!(command, HistoryCommand::Status) {
        return Ok(json!({
            "mode": database.history_mode().await?,
            "pending": database.history_pending_count().await?,
            "backfill_done": database.history_backfill_done().await?,
            "revision": database.history_revision().await?,
        }));
    }
    database.refresh_http_settings().await?;
    database.start_http_settings_refresh();
    let client = ClickHouse::from_env(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
    )?
    .ok_or(HistoryError::Invalid("MTC_REQUEST_HISTORY_URL is required"))?;
    match command {
        HistoryCommand::Start => {
            client.initialize().await?;
            database.history_start().await?;
        }
        HistoryCommand::Backfill { page_size } => {
            while !database.history_backfill_done().await? {
                database.history_backfill_page(page_size).await?;
                export_page(&database, &client, page_size).await?;
            }
            drain(&database, &client, page_size).await?;
        }
        HistoryCommand::Drain { page_size } => drain(&database, &client, page_size).await?,
        HistoryCommand::Verify => return verify(&database, &client).await,
        HistoryCommand::Switch => {
            verify(&database, &client).await?;
            database.history_switch(true).await?;
        }
        HistoryCommand::Rollback | HistoryCommand::Status => unreachable!(),
    }
    Ok(
        json!({"mode": database.history_mode().await?, "pending": database.history_pending_count().await?}),
    )
}

pub async fn export_page(
    database: &Database,
    client: &ClickHouse,
    page_size: i64,
) -> Result<usize, HistoryError> {
    let snapshots = database.history_pending(page_size).await?;
    client.append(&snapshots).await?;
    database.history_acknowledge(&snapshots).await?;
    Ok(snapshots.len())
}

pub async fn drain(
    database: &Database,
    client: &ClickHouse,
    page_size: i64,
) -> Result<(), HistoryError> {
    while database.history_pending_count().await? != 0 {
        export_page(database, client, page_size).await?;
    }
    Ok(())
}

pub(crate) async fn run_worker(state: AppState, mut shutdown: watch::Receiver<bool>) {
    let client = match ClickHouse::from_env(state.http.clone()) {
        Ok(Some(client)) => Some(client),
        Ok(None) => None,
        Err(_) => {
            tracing::error!("request history configuration invalid; forwarding remains available");
            None
        }
    };
    let database = state.db.independent_persistence_pool();
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = async {
                if let Some(client) = &client {
                    let result = async {
                        if database.history_mode().await? == "off" {
                            return Ok::<_, HistoryError>(());
                        }
                        export_page(&database, client, 500).await?;
                        Ok(())
                    }.await;
                    if result.is_err() {
                        tracing::warn!("request history export deferred; durable queue retained");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            } => {}
        }
    }
}

pub async fn verify(database: &Database, client: &ClickHouse) -> Result<Value, HistoryError> {
    if !database.history_backfill_done().await? || database.history_pending_count().await? != 0 {
        return Err(HistoryError::Changed);
    }
    let revision = database.history_revision().await?;
    let mut filter = RequestListFilter {
        limit: 500,
        ..RequestListFilter::default()
    };
    let mut count = 0_u64;
    let mut tenants = BTreeMap::<String, u64>::new();
    let mut hash = Sha256::new();
    let mut daily = BTreeMap::new();
    loop {
        let source = database
            .list_global_requests_filtered(filter.clone())
            .await?;
        let query = crate::request_history_api::query_from_request_filter(
            HistoryScope::GlobalOperator,
            &filter,
        )?;
        let mut target = client.snapshots(&query).await?;
        target.truncate(query.limit);
        if source.len() != target.len() {
            return Err(HistoryError::Mismatch);
        }
        for (view, snapshot) in source.iter().zip(target) {
            let identity = database.history_identity_for_view(view.request_id).await?;
            let expected = database
                .history_snapshot(identity, snapshot.revision)
                .await?;
            if expected.identity != snapshot.identity
                || expected.payload != snapshot.payload
                || expected.filter_cost_micros != snapshot.filter_cost_micros
                || expected.statistics != snapshot.statistics
                || snapshot.revision <= 0
                || snapshot.revision > revision
                || expected.payload != Some(serde_json::to_value(view)?)
            {
                return Err(HistoryError::Mismatch);
            }
            count += 1;
            *tenants
                .entry(expected.identity.tenant_id.to_string())
                .or_default() += 1;
            hash.update(super::canonical_hash(&expected.identity)?);
            hash.update(super::canonical_hash(&expected.payload)?);
            hash.update(super::canonical_hash(&expected.filter_cost_micros)?);
            hash.update(super::canonical_hash(&expected.statistics)?);
            hash.update(snapshot.revision.to_be_bytes());
            add_daily(&mut daily, &expected.row()?)?;
        }
        let Some(last) = source.last() else { break };
        filter.before_created_at = Some(last.created_at);
        filter.before_id = Some(last.request_id);
    }
    let query = HistoryQuery {
        scope: HistoryScope::GlobalOperator,
        from_created_at: None,
        to_created_at: None,
        cursor: None,
        limit: 500,
        predicates: Vec::new(),
    };
    let actual = client
        .daily(&query)
        .await?
        .into_iter()
        .map(|bucket| (daily_key(&bucket), bucket))
        .collect::<BTreeMap<_, _>>();
    if daily != actual {
        return Err(HistoryError::Mismatch);
    }
    if revision != database.history_revision().await?
        || database.history_pending_count().await? != 0
    {
        return Err(HistoryError::Changed);
    }
    let report = json!({
        "revision": revision, "count": count, "sha256": format!("{:x}", hash.finalize()),
        "tenants": tenants, "daily": daily.values().collect::<Vec<_>>(),
        "request_list_payloads_equal": true,
    });
    database
        .history_save_verification(revision, &report)
        .await?;
    Ok(report)
}

type DailyKey = (i64, String, Option<String>, Option<String>);

fn daily_key(bucket: &DailyBucket) -> DailyKey {
    (
        bucket.day_bucket,
        bucket.outcome.clone(),
        bucket.currency.clone(),
        bucket.billing_unit.clone(),
    )
}

fn add_daily(
    buckets: &mut BTreeMap<DailyKey, DailyBucket>,
    row: &Value,
) -> Result<(), HistoryError> {
    if row["source"] == "archive" {
        return Ok(());
    }
    let bucket = DailyBucket {
        day_bucket: row["created_at"].as_i64().unwrap_or_default() / 86_400_000,
        outcome: row["outcome"].as_str().unwrap_or_default().into(),
        currency: row["currency"].as_str().map(str::to_owned),
        billing_unit: row["billing_unit"].as_str().map(str::to_owned),
        requests: 0,
        input_tokens: None,
        input_known_requests: 0,
        output_tokens: None,
        output_known_requests: 0,
        cached_input_tokens: None,
        cache_write_tokens: None,
        generation_units: None,
        cost_micros: None,
        cost_known_requests: 0,
    };
    let bucket = buckets.entry(daily_key(&bucket)).or_insert(bucket);
    bucket.requests += 1;
    for (field, sum) in [
        ("input_tokens", &mut bucket.input_tokens),
        ("output_tokens", &mut bucket.output_tokens),
        ("cached_input_tokens", &mut bucket.cached_input_tokens),
        ("cache_write_tokens", &mut bucket.cache_write_tokens),
        ("generation_units", &mut bucket.generation_units),
        ("cost_micros", &mut bucket.cost_micros),
    ] {
        if let Some(value) = row[field].as_i64() {
            *sum = Some(
                sum.unwrap_or_default()
                    .checked_add(value)
                    .ok_or(HistoryError::Invalid("daily aggregate overflow"))?,
            );
        }
    }
    bucket.input_known_requests += u64::from(row["input_tokens"].is_i64());
    bucket.output_known_requests += u64::from(row["output_tokens"].is_i64());
    bucket.cost_known_requests += u64::from(row["cost_micros"].is_i64());
    Ok(())
}
