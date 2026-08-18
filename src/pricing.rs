use std::{collections::HashMap, str::FromStr, time::Duration};

use bytes::BytesMut;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::Config,
    db::Database,
    error::AppError,
    model::ModelPriceView,
    network::{self, OutboundScope},
};
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone, Debug)]
struct RemotePrice {
    source: &'static str,
    source_model_id: String,
    input_per_million: Decimal,
    cached_input_per_million: Option<Decimal>,
    cache_write_per_million: Option<Decimal>,
    output_per_million: Decimal,
    service_tier: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSourceResult {
    pub source: String,
    pub models: usize,
    pub skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCandidate {
    pub source_model_id: String,
    pub source: String,
    pub reason: String,
    pub input_per_million: String,
    pub output_per_million: String,
    pub service_tier: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncCandidateSet {
    pub model: String,
    pub candidates: Vec<SyncCandidate>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPriceSyncResult {
    pub source: String,
    pub sources: Vec<String>,
    pub imported: usize,
    pub matched: Vec<String>,
    pub candidates: Vec<SyncCandidateSet>,
    pub unmatched: Vec<String>,
    pub preserved: Vec<String>,
    pub source_results: Vec<SyncSourceResult>,
    pub prices: Vec<ModelPriceView>,
}

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
    mut models: Vec<String>,
    currency: &str,
    source_specs: &[(&'static str, &str)],
    allow_test_loopback: bool,
) -> Result<ModelPriceSyncResult, AppError> {
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

    let existing = db
        .list_model_prices(currency)
        .await?
        .into_iter()
        .map(|price| (price.model.clone(), price))
        .collect::<HashMap<_, _>>();
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
        db.upsert_synced_model_price_tier(
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
        imported += 1;
        matched.push(model.clone());
    }

    let mut candidates = candidate_sets
        .into_iter()
        .filter(|(model, _)| !matched.contains(model) && !preserved.contains(model))
        .map(|(model, mut candidates)| {
            candidates.truncate(8);
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

async fn fetch_source(
    http: &reqwest::Client,
    source: &'static str,
    url: &str,
    allow_test_loopback: bool,
) -> Result<(Vec<RemotePrice>, usize), AppError> {
    let task = async {
        let outbound =
            network::client_for_url(http, url, OutboundScope::Public, allow_test_loopback).await?;
        let response = outbound.get(url).send().await?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "{source} returned HTTP {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SOURCE_BYTES as u64)
        {
            return Err(AppError::Upstream(format!(
                "{source} response exceeded the size limit"
            )));
        }
        let mut bytes = BytesMut::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_SOURCE_BYTES {
                return Err(AppError::Upstream(format!(
                    "{source} response exceeded the size limit"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        let document: Value = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::Upstream(format!("{source} returned invalid JSON")))?;
        match source {
            "models.dev" => parse_models_dev(&document),
            "litellm" => parse_litellm(&document),
            "openrouter" => parse_openrouter(&document),
            _ => Err(AppError::Internal),
        }
    };
    tokio::time::timeout(SOURCE_TIMEOUT, task)
        .await
        .map_err(|_| AppError::Upstream(format!("{source} synchronization timed out")))?
}

fn parse_models_dev(document: &Value) -> Result<(Vec<RemotePrice>, usize), AppError> {
    let providers = match document.get("providers") {
        Some(providers) => providers.as_object(),
        None => document.as_object(),
    }
    .ok_or_else(|| AppError::Upstream("models.dev returned an invalid catalog".into()))?;
    let mut prices = Vec::new();
    let mut skipped = 0;
    for (provider_id, provider) in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            skipped += 1;
            continue;
        };
        for (model_id, model) in models {
            let Some(cost) = model.get("cost") else {
                skipped += 1;
                continue;
            };
            let (Some(input), Some(output)) = (
                decimal_value(cost.get("input")),
                decimal_value(cost.get("output")),
            ) else {
                skipped += 1;
                continue;
            };
            prices.push(RemotePrice {
                source: "models.dev",
                source_model_id: format!("{provider_id}/{model_id}"),
                input_per_million: input,
                cached_input_per_million: decimal_value(
                    cost.get("cache_read")
                        .or_else(|| cost.get("input_cache_read")),
                ),
                cache_write_per_million: decimal_value(
                    cost.get("cache_write")
                        .or_else(|| cost.get("input_cache_write")),
                ),
                output_per_million: output,
                service_tier: "default".to_owned(),
            });
        }
    }
    ensure_prices("models.dev", prices, skipped)
}

fn parse_litellm(document: &Value) -> Result<(Vec<RemotePrice>, usize), AppError> {
    let entries = document
        .as_object()
        .ok_or_else(|| AppError::Upstream("litellm returned an invalid catalog".into()))?;
    let million = Decimal::from(1_000_000_u64);
    let mut prices = Vec::new();
    let mut skipped = 0;
    for (model_id, model) in entries {
        let (Some(input), Some(output)) = (
            decimal_value(model.get("input_cost_per_token")),
            decimal_value(model.get("output_cost_per_token")),
        ) else {
            skipped += 1;
            continue;
        };
        prices.push(RemotePrice {
            source: "litellm",
            source_model_id: model_id.clone(),
            input_per_million: input * million,
            cached_input_per_million: decimal_value(model.get("cache_read_input_token_cost"))
                .map(|price| price * million),
            cache_write_per_million: decimal_value(model.get("cache_creation_input_token_cost"))
                .map(|price| price * million),
            output_per_million: output * million,
            service_tier: model
                .get("service_tier")
                .and_then(Value::as_str)
                .filter(|tier| valid_remote_service_tier(tier))
                .unwrap_or("default")
                .to_owned(),
        });
    }
    ensure_prices("litellm", prices, skipped)
}

fn parse_openrouter(document: &Value) -> Result<(Vec<RemotePrice>, usize), AppError> {
    let entries = document
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Upstream("openrouter returned an invalid catalog".into()))?;
    let million = Decimal::from(1_000_000_u64);
    let mut prices = Vec::new();
    let mut skipped = 0;
    for model in entries {
        let (Some(model_id), Some(input), Some(output)) = (
            model.get("id").and_then(Value::as_str),
            decimal_value(
                model
                    .get("pricing")
                    .and_then(|pricing| pricing.get("prompt")),
            ),
            decimal_value(
                model
                    .get("pricing")
                    .and_then(|pricing| pricing.get("completion")),
            ),
        ) else {
            skipped += 1;
            continue;
        };
        prices.push(RemotePrice {
            source: "openrouter",
            source_model_id: model_id.to_owned(),
            input_per_million: input * million,
            cached_input_per_million: decimal_value(
                model
                    .get("pricing")
                    .and_then(|pricing| pricing.get("input_cache_read")),
            )
            .map(|price| price * million),
            cache_write_per_million: decimal_value(
                model
                    .get("pricing")
                    .and_then(|pricing| pricing.get("input_cache_write")),
            )
            .map(|price| price * million),
            output_per_million: output * million,
            service_tier: "default".to_owned(),
        });
    }
    ensure_prices("openrouter", prices, skipped)
}

fn ensure_prices(
    source: &str,
    prices: Vec<RemotePrice>,
    skipped: usize,
) -> Result<(Vec<RemotePrice>, usize), AppError> {
    if prices.is_empty() {
        Err(AppError::Upstream(format!(
            "{source} catalog contained no usable token prices"
        )))
    } else {
        Ok((prices, skipped))
    }
}

fn decimal_value(value: Option<&Value>) -> Option<Decimal> {
    let value = value?;
    let text = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    let decimal = Decimal::from_str(&text).ok()?;
    (decimal >= Decimal::ZERO).then_some(decimal)
}

fn valid_remote_service_tier(value: &str) -> bool {
    matches!(
        value,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    )
}

fn match_price(
    requested: &str,
    prices: &[RemotePrice],
) -> (Option<RemotePrice>, Vec<SyncCandidate>) {
    let requested = normalize_identity(requested);
    let exact = prices
        .iter()
        .filter(|price| normalize_identity(&price.source_model_id) == requested)
        .cloned()
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return (exact.into_iter().next(), Vec::new());
    }
    if exact.len() > 1 {
        return (None, candidates(exact, "ambiguous exact identity"));
    }
    let requested_tail = model_tail(&requested);
    let tail = prices
        .iter()
        .filter(|price| model_tail(&normalize_identity(&price.source_model_id)) == requested_tail)
        .cloned()
        .collect::<Vec<_>>();
    if tail.len() == 1 {
        return (tail.into_iter().next(), Vec::new());
    }
    if tail.len() > 1 {
        return (None, candidates(tail, "provider prefix is ambiguous"));
    }
    (None, Vec::new())
}

fn candidates(prices: Vec<RemotePrice>, reason: &str) -> Vec<SyncCandidate> {
    prices
        .into_iter()
        .take(8)
        .map(|price| SyncCandidate {
            source_model_id: price.source_model_id,
            source: price.source.to_owned(),
            reason: reason.to_owned(),
            input_per_million: price.input_per_million.normalize().to_string(),
            output_per_million: price.output_per_million.normalize().to_string(),
            service_tier: price.service_tier,
        })
        .collect()
}

fn normalized_models(models: Vec<String>) -> Vec<String> {
    let mut models = models
        .into_iter()
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty() && model.len() <= 300)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models.truncate(500);
    models
}

fn normalize_identity(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn model_tail(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

fn source_priority(source: &str) -> usize {
    match source {
        "models.dev" => 0,
        "litellm" => 1,
        "openrouter" => 2,
        "manual" => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

    use super::*;

    fn price(source: &'static str, id: &str) -> RemotePrice {
        RemotePrice {
            source,
            source_model_id: id.to_owned(),
            input_per_million: Decimal::ONE,
            cached_input_per_million: None,
            cache_write_per_million: None,
            output_per_million: Decimal::TWO,
            service_tier: "default".to_owned(),
        }
    }

    async fn test_database() -> (TempDir, Database) {
        let directory = tempfile::tempdir().expect("pricing test directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("pricing.db").display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("pricing test database");
        database.migrate().await.expect("pricing test migrations");
        (directory, database)
    }

    async fn mount_json(server: &MockServer, request_path: &str, fixture: &'static str) {
        Mock::given(path(request_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_raw(fixture, "application/json"),
            )
            .mount(server)
            .await;
    }

    async fn fixture_sources(server: &MockServer) -> Vec<(&'static str, String)> {
        mount_json(
            server,
            "/models-dev",
            include_str!("../tests/fixtures/pricing/models-dev.json"),
        )
        .await;
        mount_json(
            server,
            "/litellm",
            include_str!("../tests/fixtures/pricing/litellm.json"),
        )
        .await;
        mount_json(
            server,
            "/openrouter",
            include_str!("../tests/fixtures/pricing/openrouter.json"),
        )
        .await;
        vec![
            ("models.dev", format!("{}/models-dev", server.uri())),
            ("litellm", format!("{}/litellm", server.uri())),
            ("openrouter", format!("{}/openrouter", server.uri())),
        ]
    }

    fn borrowed_sources<'a>(sources: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
        sources
            .iter()
            .map(|(source, url)| (*source, url.as_str()))
            .collect()
    }

    #[test]
    fn exact_match_wins_before_ambiguous_provider_tails() {
        let prices = vec![
            price("models.dev", "openai/gpt-5"),
            price("models.dev", "azure/gpt-5"),
        ];
        let (matched, candidates) = match_price("openai/gpt-5", &prices);
        assert_eq!(matched.unwrap().source_model_id, "openai/gpt-5");
        assert!(candidates.is_empty());
    }

    #[test]
    fn provider_tail_must_be_unique_for_automatic_sync() {
        let prices = vec![
            price("models.dev", "openai/gpt-5"),
            price("models.dev", "azure/gpt-5"),
        ];
        let (matched, candidates) = match_price("gpt-5", &prices);
        assert!(matched.is_none());
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn openrouter_token_prices_are_scaled_to_per_million() {
        let document = serde_json::json!({"data": [{
            "id": "openai/gpt-test",
            "pricing": {"prompt": "0.000001", "completion": "0.000002"}
        }]});
        let (prices, skipped) = parse_openrouter(&document).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(prices[0].input_per_million, Decimal::ONE);
        assert_eq!(prices[0].output_per_million, Decimal::TWO);
    }

    #[tokio::test]
    async fn sync_is_deterministic_across_priority_conflicts_and_missing_fields() {
        let (_directory, database) = test_database().await;
        let server = MockServer::start().await;
        let owned_sources = fixture_sources(&server).await;
        let sources = borrowed_sources(&owned_sources);

        let result = sync_model_prices_with_sources(
            &database,
            &reqwest::Client::new(),
            vec![
                "openai/gpt-openrouter".into(),
                "gpt-conflict".into(),
                "openai/gpt-fallback".into(),
                "openai/gpt-priority".into(),
                "gpt-missing-output".into(),
                "openai/gpt-priority".into(),
            ],
            "usd",
            &sources,
            true,
        )
        .await
        .expect("offline price synchronization");

        assert_eq!(
            result.matched,
            vec![
                "openai/gpt-fallback",
                "openai/gpt-openrouter",
                "openai/gpt-priority"
            ]
        );
        assert_eq!(result.imported, 3);
        assert_eq!(result.sources, vec!["models.dev", "litellm", "openrouter"]);
        assert_eq!(
            result
                .source_results
                .iter()
                .map(|source| (source.source.as_str(), source.skipped))
                .collect::<Vec<_>>(),
            vec![("models.dev", 1), ("litellm", 1), ("openrouter", 1)]
        );
        assert_eq!(result.unmatched, vec!["gpt-missing-output"]);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].model, "gpt-conflict");
        assert_eq!(result.candidates[0].candidates.len(), 2);
        assert!(
            result.candidates[0]
                .candidates
                .iter()
                .all(|candidate| candidate.source == "models.dev"
                    && candidate.reason == "provider prefix is ambiguous")
        );

        let priority = database
            .model_price_view("openai/gpt-priority", "USD")
            .await
            .expect("preferred price");
        assert_eq!(priority.source, "models.dev");
        assert_eq!(priority.input_per_million, "1.25");
        assert_eq!(priority.output_per_million, "2.5");
        assert_eq!(priority.tiers[0].cached_input_per_million, "0.25");
        assert_eq!(priority.tiers[0].cache_write_per_million, "1.5");
        assert!(!priority.tiers[0].cache_price_estimated);
        let fallback = database
            .model_price_view("openai/gpt-fallback", "USD")
            .await
            .expect("fallback price");
        assert_eq!(fallback.source, "litellm");
        assert_eq!(fallback.input_per_million, "5");
        assert_eq!(fallback.tiers[0].cached_input_per_million, "0.5");
        assert_eq!(fallback.tiers[0].cache_write_per_million, "7");
        assert!(!fallback.tiers[0].cache_price_estimated);
        let openrouter = database
            .model_price_view("openai/gpt-openrouter", "USD")
            .await
            .expect("last source price");
        assert_eq!(openrouter.source, "openrouter");
        assert_eq!(openrouter.input_per_million, "13");
        assert_eq!(openrouter.tiers[0].cached_input_per_million, "1.3");
        assert_eq!(openrouter.tiers[0].cache_write_per_million, "15");
        assert!(!openrouter.tiers[0].cache_price_estimated);
    }

    #[tokio::test]
    async fn sync_preserves_manual_and_last_known_preferred_prices() {
        let (_directory, database) = test_database().await;
        database
            .upsert_model_price(
                "openai/gpt-manual",
                "USD",
                Decimal::from(100),
                Decimal::from(200),
            )
            .await
            .expect("manual price");
        database
            .upsert_synced_model_price(
                "openai/gpt-priority",
                "USD",
                Decimal::ONE,
                Decimal::TWO,
                "models.dev",
            )
            .await
            .expect("last-known preferred price");

        let server = MockServer::start().await;
        Mock::given(path("/models-dev"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        mount_json(
            &server,
            "/litellm",
            include_str!("../tests/fixtures/pricing/litellm.json"),
        )
        .await;
        mount_json(
            &server,
            "/openrouter",
            include_str!("../tests/fixtures/pricing/openrouter.json"),
        )
        .await;
        let owned_sources = vec![
            ("models.dev", format!("{}/models-dev", server.uri())),
            ("litellm", format!("{}/litellm", server.uri())),
            ("openrouter", format!("{}/openrouter", server.uri())),
        ];
        let sources = borrowed_sources(&owned_sources);

        let result = sync_model_prices_with_sources(
            &database,
            &reqwest::Client::new(),
            vec!["openai/gpt-manual".into(), "openai/gpt-priority".into()],
            "USD",
            &sources,
            true,
        )
        .await
        .expect("partial-source synchronization");

        assert_eq!(
            result.preserved,
            vec!["openai/gpt-manual", "openai/gpt-priority"]
        );
        assert_eq!(result.imported, 0);
        assert_eq!(result.sources, vec!["litellm", "openrouter"]);
        assert_eq!(result.source_results[0].source, "models.dev");
        assert_eq!(
            result.source_results[0].error.as_deref(),
            Some("source unavailable; last known prices were retained")
        );
        let manual = database
            .model_price_view("openai/gpt-manual", "USD")
            .await
            .expect("preserved manual price");
        assert_eq!(manual.source, "manual");
        assert_eq!(manual.input_per_million, "100");
        let last_known = database
            .model_price_view("openai/gpt-priority", "USD")
            .await
            .expect("preserved preferred price");
        assert_eq!(last_known.source, "models.dev");
        assert_eq!(last_known.input_per_million, "1");
    }

    #[tokio::test]
    async fn all_source_failures_do_not_modify_existing_prices() {
        let (_directory, database) = test_database().await;
        database
            .upsert_model_price(
                "openai/gpt-manual",
                "USD",
                Decimal::from(100),
                Decimal::from(200),
            )
            .await
            .expect("manual price");
        let server = MockServer::start().await;
        let owned_sources = vec![
            ("models.dev", format!("{}/missing-models-dev", server.uri())),
            ("litellm", format!("{}/missing-litellm", server.uri())),
            ("openrouter", format!("{}/missing-openrouter", server.uri())),
        ];
        let sources = borrowed_sources(&owned_sources);

        let error = sync_model_prices_with_sources(
            &database,
            &reqwest::Client::new(),
            vec!["openai/gpt-manual".into()],
            "USD",
            &sources,
            true,
        )
        .await
        .expect_err("all unavailable sources must fail closed");
        assert!(matches!(error, AppError::Upstream(_)));
        let price = database
            .model_price_view("openai/gpt-manual", "USD")
            .await
            .expect("unchanged price");
        assert_eq!(price.source, "manual");
        assert_eq!(price.input_per_million, "100");
    }

    #[tokio::test]
    async fn generation_prices_validate_units_and_updates() {
        let (_directory, database) = test_database().await;
        let inserted = database
            .upsert_generation_price("image-model", "usd", "image", Decimal::new(25, 2))
            .await
            .expect("generation price");
        assert_eq!(inserted.currency, "USD");
        assert_eq!(inserted.billing_unit, "image");
        assert_eq!(inserted.price_per_unit, "0.25");

        let updated = database
            .upsert_generation_price("image-model", "USD", "megapixel", Decimal::new(75, 2))
            .await
            .expect("updated generation price");
        assert_eq!(updated.billing_unit, "megapixel");
        assert_eq!(updated.price_per_unit, "0.75");
        assert_eq!(
            database.list_generation_prices("USD").await.unwrap().len(),
            1
        );

        let invalid = database
            .upsert_generation_price("image-model", "USD", "token", Decimal::ONE)
            .await
            .expect_err("generation billing unit allow-list");
        assert!(matches!(invalid, AppError::BadRequest(_)));
        let negative = database
            .upsert_generation_price("image-model", "USD", "image", -Decimal::ONE)
            .await
            .expect_err("negative generation price");
        assert!(matches!(negative, AppError::BadRequest(_)));
    }
}
