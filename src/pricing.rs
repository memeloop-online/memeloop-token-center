use std::{collections::HashMap, str::FromStr, time::Duration};

use bytes::BytesMut;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

use crate::{db::Database, error::AppError, model::ModelPriceView};

const MODELS_DEV_URL: &str = "https://models.dev/catalog.json";
const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const SOURCE_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Clone, Debug)]
struct RemotePrice {
    source: &'static str,
    source_model_id: String,
    input_per_million: Decimal,
    output_per_million: Decimal,
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

pub async fn sync_model_prices(
    db: &Database,
    http: &reqwest::Client,
    mut models: Vec<String>,
    currency: &str,
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

    let source_specs = [
        ("models.dev", MODELS_DEV_URL),
        ("litellm", LITELLM_URL),
        ("openrouter", OPENROUTER_URL),
    ];
    let mut fetched = Vec::new();
    let mut source_results = Vec::new();
    let mut successful_sources = Vec::new();
    let mut failed_sources = Vec::new();
    for (source, url) in source_specs {
        match fetch_source(http, source, url).await {
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
            let preserve_manual = current.source == "manual";
            let preserve_failed_preferred = failed_sources.iter().any(|source| {
                source == &current.source
                    && source_priority(&current.source) < source_priority(price.source)
            });
            if preserve_manual || preserve_failed_preferred {
                preserved.push(model.clone());
                continue;
            }
        }
        db.upsert_synced_model_price(
            model,
            currency,
            price.input_per_million,
            price.output_per_million,
            price.source,
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
    url: &'static str,
) -> Result<(Vec<RemotePrice>, usize), AppError> {
    let task = async {
        let response = http.get(url).send().await?;
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
                output_per_million: output,
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
            output_per_million: output * million,
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
            output_per_million: output * million,
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
    use super::*;

    fn price(source: &'static str, id: &str) -> RemotePrice {
        RemotePrice {
            source,
            source_model_id: id.to_owned(),
            input_per_million: Decimal::ONE,
            output_per_million: Decimal::TWO,
        }
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
}
