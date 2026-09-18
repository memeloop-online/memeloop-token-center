mod matching;
mod sources;
mod types;

use std::collections::HashMap;

use crate::{config::Config, db::Database, error::AppError};

use self::{
    matching::{match_price, normalized_models, source_priority},
    sources::fetch_source,
};

use types::RemotePrice;
pub use types::{ModelPriceSyncResult, SyncCandidate, SyncCandidateSet, SyncSourceResult};

pub const MAX_SYNC_MODELS: usize = 500;
pub(crate) static MODEL_PRICE_SYNC_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(2);
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_PRICES: usize = 20_000;
const MAX_CANDIDATES_PER_MODEL: usize = 8;

pub fn model_price_sources(config: &Config) -> [(&'static str, &str); 3] {
    [
        ("models.dev", config.pricing_models_dev_url.as_str()),
        ("litellm", config.pricing_litellm_url.as_str()),
        ("openrouter", config.pricing_openrouter_url.as_str()),
    ]
}

pub async fn sync_model_prices(
    db: &Database,
    http: &reqwest::Client,
    models: Vec<String>,
    currency: &str,
    source_specs: &[(&'static str, &str)],
    allow_test_loopback: bool,
) -> Result<ModelPriceSyncResult, AppError> {
    sync_model_prices_with_sources(
        db,
        http,
        models,
        currency,
        source_specs,
        allow_test_loopback,
    )
    .await
}

async fn sync_model_prices_with_sources(
    db: &Database,
    http: &reqwest::Client,
    models: Vec<String>,
    currency: &str,
    source_specs: &[(&'static str, &str)],
    allow_test_loopback: bool,
) -> Result<ModelPriceSyncResult, AppError> {
    run_price_sync(
        db,
        http,
        models,
        currency,
        source_specs,
        allow_test_loopback,
        MAX_SYNC_MODELS,
    )
    .await
}

/// Server-owned catalog synchronization accepts the full bounded catalog,
/// fetching each source once and chunking database reads, not network requests.
pub(crate) async fn sync_catalog_model_prices(
    db: &Database,
    http: &reqwest::Client,
    models: Vec<String>,
    source_specs: &[(&'static str, &str)],
    allow_test_loopback: bool,
) -> Result<ModelPriceSyncResult, AppError> {
    let _permit = MODEL_PRICE_SYNC_PERMITS
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    run_price_sync(
        db,
        http,
        models,
        "USD",
        source_specs,
        allow_test_loopback,
        10_000,
    )
    .await
}

async fn run_price_sync(
    db: &Database,
    http: &reqwest::Client,
    mut models: Vec<String>,
    currency: &str,
    source_specs: &[(&'static str, &str)],
    allow_test_loopback: bool,
    max_models: usize,
) -> Result<ModelPriceSyncResult, AppError> {
    if models.len() > max_models {
        return Err(AppError::BadRequest(format!(
            "model price sync accepts at most {max_models} models"
        )));
    }
    models = normalized_models(models);
    if models.is_empty() {
        return Err(AppError::BadRequest(
            "model price sync requires at least one model".into(),
        ));
    }
    if !currency.eq_ignore_ascii_case("USD") {
        return Err(AppError::BadRequest(
            "public price sources currently publish USD prices only".into(),
        ));
    }

    let mut fetched = Vec::new();
    let mut source_results = Vec::new();
    let mut successful_sources = Vec::new();
    let mut failed_sources = Vec::new();
    for &(source, url) in source_specs {
        match fetch_source(http, source, url, allow_test_loopback).await {
            Ok((prices, skipped)) => {
                source_results.push(SyncSourceResult {
                    source: source.to_owned(),
                    models: prices.len(),
                    skipped,
                    error: None,
                });
                successful_sources.push(source.to_owned());
                fetched.push((source, prices));
            }
            Err(error) => {
                tracing::warn!(source, %error, "model price source synchronization failed");
                failed_sources.push(source.to_owned());
                source_results.push(SyncSourceResult {
                    source: source.to_owned(),
                    models: 0,
                    skipped: 0,
                    error: Some("source unavailable; last known prices were retained".to_owned()),
                });
            }
        }
    }
    if fetched.is_empty() {
        return Err(AppError::Upstream(
            "all configured model price sources are unavailable".into(),
        ));
    }

    let mut existing = HashMap::new();
    for batch in models.chunks(MAX_SYNC_MODELS) {
        existing.extend(
            db.model_price_views_for_models(currency, batch)
                .await?
                .into_iter()
                .map(|price| (price.model.clone(), price)),
        );
    }
    let mut selected = HashMap::<String, RemotePrice>::new();
    let mut candidate_sets = HashMap::<String, Vec<SyncCandidate>>::new();

    for model in &models {
        for (_source, prices) in &fetched {
            let (matched, candidates) = match_price(model, prices);
            if let Some(price) = matched {
                selected.insert(model.clone(), price);
                break;
            }
            if !candidates.is_empty() {
                candidate_sets
                    .entry(model.clone())
                    .or_default()
                    .extend(candidates);
            }
        }
    }

    let mut imported = 0;
    let mut matched = Vec::new();
    let mut preserved = Vec::new();
    for model in &models {
        let Some(price) = selected.get(model) else {
            continue;
        };
        if let Some(current) = existing.get(model) {
            let current_tier_source = current
                .tiers
                .iter()
                .find(|tier| tier.service_tier == price.service_tier)
                .map(|tier| tier.source.as_str())
                .or_else(|| (price.service_tier == "default").then_some(current.source.as_str()));
            let preserve_manual = current_tier_source == Some("manual");
            let preserve_failed_preferred = failed_sources.iter().any(|source| {
                current_tier_source.is_some_and(|current_source| {
                    source == current_source
                        && source_priority(current_source) < source_priority(price.source)
                })
            });
            if preserve_manual || preserve_failed_preferred {
                preserved.push(model.clone());
                continue;
            }
        }
        let cached = price
            .cached_input_per_million
            .unwrap_or(price.input_per_million);
        let cache_write = price
            .cache_write_per_million
            .unwrap_or(price.input_per_million);
        let cache_estimated =
            price.cached_input_per_million.is_none() || price.cache_write_per_million.is_none();
        let written = db
            .upsert_synced_model_price_tier(
                model,
                currency,
                &price.service_tier,
                price.input_per_million,
                cached,
                cache_write,
                price.output_per_million,
                price.source,
                cache_estimated,
            )
            .await?;
        if written
            .tiers
            .iter()
            .any(|tier| tier.service_tier == price.service_tier && tier.source == "manual")
        {
            preserved.push(model.clone());
            continue;
        }
        imported += 1;
        matched.push(model.clone());
    }

    let mut candidates = candidate_sets
        .into_iter()
        .filter(|(model, _)| !matched.contains(model) && !preserved.contains(model))
        .map(|(model, mut candidates)| {
            candidates.truncate(MAX_CANDIDATES_PER_MODEL);
            SyncCandidateSet { model, candidates }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.model.cmp(&right.model));
    let unmatched = models
        .iter()
        .filter(|model| {
            !matched.contains(model)
                && !preserved.contains(model)
                && !candidates.iter().any(|set| &set.model == *model)
        })
        .cloned()
        .collect();
    let source = if successful_sources.len() == 1 {
        successful_sources[0].clone()
    } else {
        "multi".to_owned()
    };
    Ok(ModelPriceSyncResult {
        source,
        sources: successful_sources,
        imported,
        matched,
        candidates,
        unmatched,
        preserved,
        source_results,
        prices: db.list_model_prices(currency).await?,
    })
}

#[cfg(test)]
mod tests;
