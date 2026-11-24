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
    /// Verify a consistent source baseline; this does not authorize cutover.
    BaselineSource {
        #[arg(long)]
        owner: uuid::Uuid,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(i64).range(1..=64))]
        page_size: i64,
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
        max_seconds: u64,
    },
    /// Abort a lost source snapshot and release only its exact delivery fence.
    AbortBaseline {
        #[arg(long)]
        owner: uuid::Uuid,
        #[arg(long)]
        epoch: i64,
    },
    /// Cap subsequent delivery revisions; capture continues, quiescence is not proved.
    FenceDelivery {
        #[arg(long)]
        owner: uuid::Uuid,
    },
    /// Resume exactly the inspected maintenance fence, preserving all queued work.
    ResumeDelivery {
        #[arg(long)]
        owner: uuid::Uuid,
        #[arg(long)]
        epoch: i64,
    },
    /// Install only free source tables while capture remains off; retry pending tables later.
    Bootstrap,
    /// Validate every installed definition, then atomically enable capture without DDL.
    Activate,
    /// Upgrade known off-mode functions without reinstalling source triggers.
    PrepareActivation,
    /// Upgrade only the known account-update capture function, preserving capture state.
    UpgradeUpstreamCapture,
    UpgradeTerminalCapture,
    /// Inspect the post-commit old-transaction cohort; Changed means still draining.
    CaptureHorizon,
    /// Re-export every retained source after upgrading the read-model contract.
    Refresh,
    Backfill {
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(i64).range(1..))]
        page_size: i64,
    },
    /// Queue one source page; delivery remains with the asynchronous exporter.
    BackfillPage {
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(i64).range(1..=64))]
        page_size: i64,
    },
    /// Export one bounded page through the ordinary lease and exact ACK path.
    ExportPage {
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(i64).range(1..=500))]
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
            "delivery_fence": database.history_delivery_status().await?,
        }));
    }
    match &command {
        HistoryCommand::UpgradeTerminalCapture => {
            return database.history_upgrade_terminal_capture().await;
        }
        HistoryCommand::UpgradeUpstreamCapture => {
            return database.history_upgrade_upstream_capture().await;
        }
        HistoryCommand::BackfillPage { page_size } => {
            let queued = database.history_backfill_page(*page_size).await?;
            return Ok(json!({
                "queued": queued,
                "backfill_done": database.history_backfill_done().await?,
            }));
        }
        HistoryCommand::PrepareActivation => {
            database.history_prepare_activation().await?;
            return Ok(json!({"mode":"off","functions_prepared":true}));
        }
        HistoryCommand::CaptureHorizon => {
            let ready = database.history_capture_horizon_ready().await?;
            return Ok(json!({"capture_horizon_ready":ready,"cutover_verified":false}));
        }
        HistoryCommand::AbortBaseline { owner, epoch } => {
            database
                .history_abort_source_snapshot(*owner, *epoch)
                .await?;
            return database.history_delivery_status().await;
        }
        HistoryCommand::FenceDelivery { owner } => {
            return database.history_fence_delivery(*owner).await;
        }
        HistoryCommand::ResumeDelivery { owner, epoch } => {
            database.history_resume_delivery(*owner, *epoch).await?;
            return database.history_delivery_status().await;
        }
        _ => {}
    }
    let database = if matches!(
        &command,
        HistoryCommand::Backfill { .. }
            | HistoryCommand::BaselineSource { .. }
            | HistoryCommand::Drain { .. }
            | HistoryCommand::ExportPage { .. }
            | HistoryCommand::Verify
            | HistoryCommand::Switch
            | HistoryCommand::Refresh
    ) {
        database.with_archive(crate::archive::ArchiveStore::from_config(config).await?)
    } else {
        database
    };
    database.refresh_http_settings().await?;
    database.start_http_settings_refresh();
    let role = match command {
        HistoryCommand::Start
        | HistoryCommand::Bootstrap
        | HistoryCommand::Activate
        | HistoryCommand::PrepareActivation
        | HistoryCommand::Refresh => "ADMIN",
        HistoryCommand::Verify | HistoryCommand::Switch => "READER",
        _ => "WRITER",
    };
    let client = ClickHouse::from_env_role(
        database
            .http_clients
            .handle(crate::http_settings::HttpPurpose::General),
        role,
    )?
    .ok_or(HistoryError::Invalid("MTC_REQUEST_HISTORY_URL is required"))?;
    match command {
        HistoryCommand::BaselineSource {
            owner,
            page_size,
            max_seconds,
        } => {
            let reader = ClickHouse::reader_from_env(
                database
                    .http_clients
                    .handle(crate::http_settings::HttpPurpose::General),
            )?
            .ok_or(HistoryError::Invalid("request history reader is required"))?;
            let source_database = database
                .clone()
                .with_auxiliary_pool_max_connections(1)
                .independent_persistence_pool();
            let mut source = database
                .history_begin_source_snapshot(&source_database, owner)
                .await?;
            let epoch = source.receipt["epoch"]
                .as_i64()
                .ok_or(HistoryError::Mismatch)?;
            let result = tokio::time::timeout(std::time::Duration::from_secs(max_seconds), async {
                while source
                    .verify_next_page(&database, &client, &reader, page_size)
                    .await?
                    != 0
                {}
                source.finish(&database).await
            })
            .await
            .unwrap_or(Err(HistoryError::Changed));
            match result {
                Ok(receipt) => return Ok(receipt),
                Err(error) => {
                    // No cursor is resumed with a replacement RR snapshot.
                    // All queued delta survives this exact-epoch release.
                    database.history_abort_source_snapshot(owner, epoch).await?;
                    return Err(error);
                }
            }
        }
        HistoryCommand::Start => {
            client.initialize().await?;
            database.history_start().await?;
        }
        HistoryCommand::Bootstrap => {
            client.initialize().await?;
            return database.history_bootstrap_capture().await;
        }
        HistoryCommand::PrepareActivation => {
            database.history_prepare_activation().await?;
            return Ok(json!({"mode":"off","functions_prepared":true}));
        }
        HistoryCommand::CaptureHorizon => {
            let ready = database.history_capture_horizon_ready().await?;
            return Ok(json!({"capture_horizon_ready":ready,"cutover_verified":false}));
        }
        HistoryCommand::Activate => {
            client.initialize().await?;
            database.history_activate_capture().await?;
        }
        HistoryCommand::Refresh => {
            client.initialize().await?;
            database.history_start().await?;
            // Frozen snapshots must be acknowledged before checkpoints replay
            // every source with the upgraded read model.
            drain(&database, &client, 500).await?;
            database.history_refresh().await?;
        }
        HistoryCommand::Backfill { page_size } => {
            let lease_database = database
                .clone()
                .with_auxiliary_pool_max_connections(1)
                .independent_persistence_pool();
            while !database.history_backfill_done().await? {
                database.history_backfill_page(page_size).await?;
                if export_page_with_lease(&database, &lease_database, &client, page_size).await?
                    == 0
                    && !database.history_delivery_status().await?["ceiling"].is_null()
                {
                    return Err(HistoryError::Changed);
                }
            }
            drain(&database, &client, page_size).await?;
        }
        HistoryCommand::ExportPage { page_size } => {
            let lease_database = database
                .clone()
                .with_auxiliary_pool_max_connections(1)
                .independent_persistence_pool();
            return Ok(
                match export_batch(&database, &lease_database, &client, page_size).await {
                    Ok(report) => {
                        json!({"status":"completed","attempted":report.attempted,"acked":report.acked,"deferred":report.attempted.saturating_sub(report.acked),"operation_failed":false})
                    }
                    Err(failure) => {
                        json!({"status":"failed","attempted":failure.attempted,"acked":failure.acked,"deferred":failure.acked.map(|acked| failure.attempted.saturating_sub(acked)),"operation_failed":true,"stage":failure.stage,"batch_first_source":failure.source,"batch_first_revision":failure.revision,"diagnostic":failure.error.safe_diagnostic()})
                    }
                },
            );
        }
        HistoryCommand::Drain { page_size } => drain(&database, &client, page_size).await?,
        HistoryCommand::Verify => return verify(&database, &client).await,
        HistoryCommand::Switch => {
            verify(&database, &client).await?;
            database.history_switch(true).await?;
        }
        HistoryCommand::Rollback
        | HistoryCommand::Status
        | HistoryCommand::UpgradeTerminalCapture
        | HistoryCommand::UpgradeUpstreamCapture
        | HistoryCommand::BackfillPage { .. }
        | HistoryCommand::FenceDelivery { .. }
        | HistoryCommand::ResumeDelivery { .. }
        | HistoryCommand::AbortBaseline { .. } => unreachable!(),
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
    let lease_database = database
        .clone()
        .with_auxiliary_pool_max_connections(1)
        .independent_persistence_pool();
    export_page_with_lease(database, &lease_database, client, page_size).await
}

async fn export_page_with_lease(
    database: &Database,
    lease_database: &Database,
    client: &ClickHouse,
    page_size: i64,
) -> Result<usize, HistoryError> {
    export_batch(database, lease_database, client, page_size)
        .await
        .map(|report| report.attempted)
        .map_err(|failure| failure.error)
}

struct ExportReport {
    attempted: usize,
    acked: usize,
}
struct ExportFailure {
    stage: &'static str,
    attempted: usize,
    acked: Option<usize>,
    source: Option<String>,
    revision: Option<i64>,
    error: HistoryError,
}

async fn export_batch(
    database: &Database,
    lease_database: &Database,
    client: &ClickHouse,
    page_size: i64,
) -> Result<ExportReport, ExportFailure> {
    let started = std::time::Instant::now();
    let failure = |stage, error| ExportFailure {
        stage,
        attempted: 0,
        acked: Some(0),
        source: None,
        revision: None,
        error,
    };
    let (mut lease, ceiling) = lease_database
        .history_delivery_lease()
        .await
        .map_err(|error| failure("lease", error))?;
    let lease_ms = started.elapsed().as_millis() as u64;
    let phase = std::time::Instant::now();
    let snapshots = database
        .history_pending_through(page_size, ceiling)
        .await
        .map_err(|error| failure("capture_materialize", error))?;
    let materialize_ms = phase.elapsed().as_millis() as u64;
    let failure = |stage, error| ExportFailure {
        stage,
        attempted: snapshots.len(),
        acked: Some(0),
        source: snapshots.first().map(|row| row.identity.source.clone()),
        revision: snapshots.first().map(|row| row.revision),
        error,
    };
    if let Some(lease) = lease.as_mut() {
        sqlx::query("SELECT 1")
            .execute(&mut **lease)
            .await
            .map_err(|error| failure("lease_validate", error.into()))?;
    }
    let phase = std::time::Instant::now();
    client
        .append(&snapshots)
        .await
        .map_err(|error| failure("append", error))?;
    let append_ms = phase.elapsed().as_millis() as u64;
    let phase = std::time::Instant::now();
    let acked = database
        .history_acknowledge_count(&snapshots)
        .await
        .map_err(|error| {
            let mut failure = failure("acknowledge", error);
            failure.acked = None;
            failure
        })?;
    let acknowledge_ms = phase.elapsed().as_millis() as u64;
    if let Some(lease) = lease {
        lease.rollback().await.map_err(|error| {
            let mut failure = failure("lease_release", error.into());
            failure.acked = Some(acked);
            failure
        })?;
    }
    if !snapshots.is_empty() {
        tracing::info!(
            attempted = snapshots.len(),
            acked,
            lease_ms,
            materialize_ms,
            append_ms,
            acknowledge_ms,
            total_ms = started.elapsed().as_millis() as u64,
            "request history export phase timings"
        );
    }
    Ok(ExportReport {
        attempted: snapshots.len(),
        acked,
    })
}

pub async fn drain(
    database: &Database,
    client: &ClickHouse,
    page_size: i64,
) -> Result<(), HistoryError> {
    let lease_database = database
        .clone()
        .with_auxiliary_pool_max_connections(1)
        .independent_persistence_pool();
    while database.history_pending_count().await? != 0 {
        if export_page_with_lease(database, &lease_database, client, page_size).await? == 0 {
            // A durable fence may leave newer work intentionally undelivered.
            // Never busy-spin or claim that the complete queue was drained.
            return Err(HistoryError::Changed);
        }
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
    let lease_database = database
        .clone()
        .with_auxiliary_pool_max_connections(1)
        .independent_persistence_pool();
    let mut failure_buckets = BTreeMap::new();
    loop {
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = async {
                if let Some(client) = &client {
                    let result = match database.history_mode().await {
                        Ok(mode) if mode == "off" => Ok(ExportReport { attempted:0, acked:0 }),
                        Ok(_) => export_batch(&database, &lease_database, client, 500).await,
                        Err(error) => Err(ExportFailure { stage:"mode",attempted:0,acked:Some(0),source:None,revision:None,error }),
                    };
                    {
                        match result {
                            Ok(_) => {}
                            Err(failure) => {
                                let diagnostic = failure.error.safe_diagnostic();
                                if let Some(occurrences) = failure_log_occurrences(&mut failure_buckets, failure.stage, &diagnostic, std::time::Instant::now()) {
                                    tracing::warn!(stage=failure.stage, diagnostic=%diagnostic, batch_first_source=?failure.source, batch_first_revision=?failure.revision, occurrences, "request history export deferred; durable queue retained");
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            } => {}
        }
    }
}

fn failure_log_occurrences(
    buckets: &mut BTreeMap<(&'static str, &'static str), (std::time::Instant, u64)>,
    stage: &'static str,
    diagnostic: &Value,
    now: std::time::Instant,
) -> Option<u64> {
    // The bucket space is the fixed export stages and fixed safe categories.
    // SQLSTATE/reason/identity values never create additional log buckets.
    let category = match diagnostic["category"].as_str() {
        Some("invalid") => "invalid",
        Some("verification") => "verification",
        Some("sql") => "sql",
        Some("clickhouse_rejected") => "clickhouse_rejected",
        Some("unavailable") => "unavailable",
        Some("changed") => "changed",
        Some("mismatch") => "mismatch",
        Some("database") => "database",
        Some("json") => "json",
        _ => "unknown",
    };
    match buckets.entry((stage, category)) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert((now, 0));
            Some(1)
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let (last, count) = entry.get_mut();
            *count += 1;
            if now.duration_since(*last) < std::time::Duration::from_secs(60) {
                return None;
            }
            let occurrences = *count;
            *last = now;
            *count = 0;
            Some(occurrences)
        }
    }
}

#[cfg(test)]
mod export_diagnostic_tests {
    use super::*;
    #[test]
    fn request_history_export_log_buckets_count_alternating_failures_independently() {
        let mut buckets = BTreeMap::new();
        let now = std::time::Instant::now();
        for category in ["sql", "database"] {
            assert_eq!(
                failure_log_occurrences(
                    &mut buckets,
                    "capture_materialize",
                    &json!({"category":category}),
                    now
                ),
                Some(1)
            );
        }
        for _ in 0..3 {
            for category in ["sql", "database"] {
                assert_eq!(
                    failure_log_occurrences(
                        &mut buckets,
                        "capture_materialize",
                        &json!({"category":category}),
                        now
                    ),
                    None
                );
            }
        }
        for category in ["sql", "database"] {
            assert_eq!(
                failure_log_occurrences(
                    &mut buckets,
                    "capture_materialize",
                    &json!({"category":category}),
                    now + std::time::Duration::from_secs(60)
                ),
                Some(4)
            );
        }
        assert_eq!(buckets.len(), 2);
    }
}

pub async fn verify(database: &Database, client: &ClickHouse) -> Result<Value, HistoryError> {
    if !database.history_activation_baseline_proved().await?
        || !database.history_backfill_done().await?
        || database.history_pending_count().await? != 0
    {
        return Err(HistoryError::Changed);
    }
    let revision = database.history_revision().await?;
    let mut filter = RequestListFilter {
        limit: 500,
        ..RequestListFilter::default()
    };
    let mut count = 0_u64;
    let mut tenants = BTreeMap::<String, u64>::new();
    let mut credentials = BTreeMap::<(uuid::Uuid, uuid::Uuid), (u64, Sha256)>::new();
    let mut hash = Sha256::new();
    let mut daily = BTreeMap::new();
    loop {
        let source = database.source_history_list(&filter).await?;
        let query = crate::request_history_api::query_from_request_filter(
            HistoryScope::GlobalOperator,
            &filter,
        )?;
        let mut target = client.snapshots(&query).await?;
        target.truncate(query.limit);
        if source.len() != target.len() {
            return Err(HistoryError::Verification("request page count"));
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
                || expected.read_model != snapshot.read_model
                || snapshot.revision <= 0
                || snapshot.revision > revision
                || expected.payload != Some(serde_json::to_value(view)?)
            {
                return Err(HistoryError::Verification(
                    "request snapshot and read model",
                ));
            }
            count += 1;
            *tenants
                .entry(expected.identity.tenant_id.to_string())
                .or_default() += 1;
            let scoped = credentials
                .entry((expected.identity.tenant_id, expected.identity.key_id))
                .or_insert_with(|| (0, Sha256::new()));
            scoped.0 += 1;
            scoped.1.update(super::canonical_hash(&snapshot)?);
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
        return Err(HistoryError::Verification("daily aggregates"));
    }
    for ((tenant_id, key_id), (expected_count, expected_hash)) in &credentials {
        let mut scoped = HistoryQuery {
            scope: HistoryScope::Key {
                tenant_id: *tenant_id,
                key_id: *key_id,
            },
            ..query.clone()
        };
        let mut actual_count = 0_u64;
        let mut actual_hash = Sha256::new();
        loop {
            let mut page = client.snapshots(&scoped).await?;
            page.truncate(scoped.limit);
            if page.is_empty() {
                break;
            }
            for snapshot in &page {
                actual_count += 1;
                actual_hash.update(super::canonical_hash(snapshot)?);
            }
            let last = page.last().ok_or(HistoryError::Mismatch)?;
            scoped.cursor = Some(super::HistoryCursor {
                before_created_at: last.identity.created_at,
                before_id: last.identity.request_id,
            });
        }
        if actual_count != *expected_count
            || actual_hash.finalize() != expected_hash.clone().finalize()
        {
            return Err(HistoryError::Verification("credential count and hash"));
        }
        let hidden = client
            .list(&HistoryQuery {
                limit: 1,
                cursor: None,
                ..scoped
            })
            .await?;
        if hidden
            .requests
            .iter()
            .any(|payload| !payload["credential_identity"].is_null())
        {
            return Err(HistoryError::Verification("credential redaction"));
        }
        let absent_key = HistoryQuery {
            scope: HistoryScope::Key {
                tenant_id: *tenant_id,
                key_id: uuid::Uuid::new_v4(),
            },
            limit: 1,
            ..query.clone()
        };
        if !client.snapshots(&absent_key).await?.is_empty() {
            return Err(HistoryError::Verification("unknown credential scope"));
        }
    }
    if !client
        .snapshots(&HistoryQuery {
            scope: HistoryScope::EmptyTenant,
            ..query.clone()
        })
        .await?
        .is_empty()
    {
        return Err(HistoryError::Verification("unknown tenant scope"));
    }
    let days: std::collections::BTreeSet<_> = daily.keys().map(|key| key.0).collect();
    for day in days {
        let from = day
            .checked_mul(86_400_000)
            .ok_or(HistoryError::Invalid("verification date"))?;
        let to = from.saturating_add(86_399_999);
        let filter = crate::db::StatsFilter {
            from_created_at: Some(from),
            to_created_at: Some(to),
            ..Default::default()
        };
        let expected = database.source_history_stats(None, &filter).await?;
        let actual = client
            .statistics(&HistoryQuery {
                from_created_at: Some(from),
                to_created_at: Some(to),
                ..query.clone()
            })
            .await?;
        if serde_json::to_value(expected)? != serde_json::to_value(actual)? {
            return Err(HistoryError::Verification("source statistics"));
        }
    }
    // Event receipts have their own ascending, stable cursor and are never
    // included in request or financial totals. Compare the full inventory.
    let mut event_at = 0;
    let mut event_id = None;
    let mut event_count = 0_u64;
    let mut event_hash = Sha256::new();
    loop {
        let source = database
            .history_source_event_page(event_at, event_id, 500)
            .await?;
        let target = client
            .event_snapshots(HistoryScope::GlobalOperator, event_at, event_id, 500)
            .await?;
        if source.len() != target.len() {
            return Err(HistoryError::Verification("event page count"));
        }
        for (identity, actual) in source.iter().zip(target) {
            let expected = database
                .history_snapshot(identity.clone(), actual.revision)
                .await?;
            if super::canonical_hash(&expected)? != super::canonical_hash(&actual)?
                || actual.revision <= 0
                || actual.revision > revision
            {
                return Err(HistoryError::Verification("event snapshot and revision"));
            }
            event_count += 1;
            event_hash.update(super::canonical_hash(&expected)?);
            event_at = expected.identity.created_at;
            event_id = Some(expected.identity.request_id);
        }
        if source.is_empty() {
            break;
        }
    }
    if revision != database.history_revision().await?
        || database.history_pending_count().await? != 0
    {
        return Err(HistoryError::Changed);
    }
    let report = json!({
        "read_model_version": super::READ_MODEL_VERSION,
        "revision": revision, "count": count, "sha256": format!("{:x}", hash.finalize()),
        "tenants": tenants, "daily": daily.values().collect::<Vec<_>>(),
        "request_list_payloads_equal": true, "statistics_equal": true,
        "credential_scopes_verified": credentials.len(), "tenant_key_negative_probes": true,
        "event_count":event_count,"events_sha256":format!("{:x}",event_hash.finalize()),
        "read_surfaces": ["requests", "stats", "detail", "events", "usage", "pricing", "sessions", "monitoring"],
        "private_read_models_equal":true,
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
