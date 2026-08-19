use super::super::*;
use super::money::parse_decimal;

static MODEL_PRICE_SYNC_PERMITS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Debug, Deserialize)]
pub(in crate::api) struct PriceRequest {
    input_per_million: String,
    output_per_million: String,
    #[serde(default = "default_service_tier")]
    service_tier: String,
    cached_input_per_million: Option<String>,
    cache_write_per_million: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ModelPricesQuery {
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ModelPriceListQuery {
    #[serde(default = "default_currency")]
    currency: String,
    #[serde(default = "default_model_price_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

pub(in crate::api) async fn list_model_prices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelPriceListQuery>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "requests:read").await?;
    Ok(Json(
        state
            .db
            .list_model_prices_page(&query.currency, query.limit, query.offset)
            .await?,
    ))
}

pub(in crate::api) async fn model_price_usage_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?;
    let filter = query.to_filter(true, None)?;
    let stats = match tenant {
        Some(tenant) => state.db.operator_stats_filtered(&tenant, filter).await?,
        None => state.db.global_operator_stats_filtered(filter).await?,
    };
    Ok(Json(json!({
        "models": stats.by_model.into_iter().map(|bucket| json!({
            "model": bucket.name,
            "calls": bucket.requests,
            "input_tokens": bucket.input_tokens,
            "output_tokens": bucket.output_tokens
        })).collect::<Vec<_>>()
    })))
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ModelPriceSyncRequest {
    #[serde(default)]
    models: Vec<String>,
    #[serde(default = "default_currency")]
    currency: String,
    tenant_external_id: Option<String>,
}

pub(in crate::api) async fn sync_model_prices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelPriceSyncRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "prices:write").await?;
    require_global_service(&service)?;
    let _permit = MODEL_PRICE_SYNC_PERMITS
        .try_acquire()
        .map_err(|_| AppError::LimitExceeded {
            reason: crate::error::LimitReason::ConcurrencyExhausted,
            retry_after_seconds: Some(1),
        })?;
    let tenant = management_tenant(&service, body.tenant_external_id)?;
    let models = if body.models.is_empty() {
        state.db.pricing_models(tenant.as_deref()).await?
    } else {
        body.models
    };
    if models.len() > crate::pricing::MAX_SYNC_MODELS {
        return Err(AppError::BadRequest(format!(
            "model price sync accepts at most {} models",
            crate::pricing::MAX_SYNC_MODELS
        )));
    }
    let sources = crate::pricing::model_price_sources(&state.config);
    Ok(Json(
        crate::pricing::sync_model_prices(
            &state.db,
            &state.http,
            models,
            &body.currency,
            &sources,
            state.config.allow_oauth_loopback,
        )
        .await?,
    ))
}

pub(in crate::api) async fn upsert_price(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((currency, model)): Path<(String, String)>,
    Json(body): Json<PriceRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "prices:write").await?;
    require_global_service(&service)?;
    let input = parse_decimal(&body.input_per_million, "input_per_million")?;
    let output = parse_decimal(&body.output_per_million, "output_per_million")?;
    let cached_input = body
        .cached_input_per_million
        .as_deref()
        .map(|value| parse_decimal(value, "cached_input_per_million"))
        .transpose()?
        .unwrap_or(input);
    let cache_write = body
        .cache_write_per_million
        .as_deref()
        .map(|value| parse_decimal(value, "cache_write_per_million"))
        .transpose()?
        .unwrap_or(input);
    let price = state
        .db
        .upsert_model_price_tier(
            &model,
            &currency,
            &body.service_tier,
            input,
            cached_input,
            cache_write,
            output,
            body.cached_input_per_million.is_none() || body.cache_write_per_million.is_none(),
        )
        .await?;
    let _ = price;
    Ok(Json(state.db.model_price_view(&model, &currency).await?))
}

fn default_service_tier() -> String {
    "default".to_owned()
}

fn default_model_price_limit() -> usize {
    1_000
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct GenerationPriceRequest {
    billing_unit: String,
    price_per_unit: String,
}

pub(in crate::api) async fn upsert_generation_price(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((currency, model)): Path<(String, String)>,
    Json(body): Json<GenerationPriceRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "prices:write").await?;
    require_global_service(&service)?;
    let price = Decimal::from_str(&body.price_per_unit)
        .map_err(|_| AppError::BadRequest("price_per_unit must be a decimal string".into()))?;
    Ok(Json(
        state
            .db
            .upsert_generation_price(&model, &currency, &body.billing_unit, price)
            .await?,
    ))
}

pub(in crate::api) async fn list_generation_prices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelPricesQuery>,
) -> Result<impl IntoResponse, AppError> {
    require_service(&headers, &state, "prices:read").await?;
    Ok(Json(
        state.db.list_generation_prices(&query.currency).await?,
    ))
}
