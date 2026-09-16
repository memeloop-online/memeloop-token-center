use std::{str::FromStr, time::Duration};

use bytes::BytesMut;
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::{
    error::AppError,
    network::{self, OutboundScope},
};

use super::{MAX_SOURCE_BYTES, MAX_SOURCE_PRICES, RemotePrice};

const SOURCE_TIMEOUT: Duration = Duration::from_secs(12);

pub(super) async fn fetch_source(
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
            ensure_price_count("models.dev", prices.len())?;
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
        ensure_price_count("litellm", prices.len())?;
    }
    ensure_prices("litellm", prices, skipped)
}

pub(super) fn parse_openrouter(document: &Value) -> Result<(Vec<RemotePrice>, usize), AppError> {
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
        ensure_price_count("openrouter", prices.len())?;
    }
    ensure_prices("openrouter", prices, skipped)
}

fn ensure_price_count(source: &str, count: usize) -> Result<(), AppError> {
    if count > MAX_SOURCE_PRICES {
        Err(AppError::Upstream(format!(
            "{source} catalog exceeded the model limit"
        )))
    } else {
        Ok(())
    }
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
