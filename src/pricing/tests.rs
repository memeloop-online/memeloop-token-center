use rust_decimal::Decimal;
use tempfile::TempDir;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

use super::sources::parse_openrouter;
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
        include_str!("../../tests/fixtures/pricing/models-dev.json"),
    )
    .await;
    mount_json(
        server,
        "/litellm",
        include_str!("../../tests/fixtures/pricing/litellm.json"),
    )
    .await;
    mount_json(
        server,
        "/openrouter",
        include_str!("../../tests/fixtures/pricing/openrouter.json"),
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
fn ambiguous_matches_retain_only_a_bounded_candidate_sample() {
    let prices = (0..(MAX_CANDIDATES_PER_MODEL + 5))
        .map(|index| price("models.dev", &format!("provider-{index}/gpt-5")))
        .collect::<Vec<_>>();
    let (matched, candidates) = match_price("gpt-5", &prices);
    assert!(matched.is_none());
    assert_eq!(candidates.len(), MAX_CANDIDATES_PER_MODEL);
}

#[tokio::test]
async fn sync_rejects_model_lists_over_the_hard_limit_before_fetching() {
    let (_directory, database) = test_database().await;
    let models = (0..=MAX_SYNC_MODELS)
        .map(|index| format!("model-{index}"))
        .collect();
    let error = sync_model_prices_with_sources(
        &database,
        &reqwest::Client::new(),
        models,
        "USD",
        &[],
        true,
    )
    .await
    .expect_err("oversized explicit model list must fail closed");
    assert!(matches!(error, AppError::BadRequest(_)));
    assert!(error.to_string().contains("at most 500 models"));
}

#[tokio::test]
async fn model_price_listing_pages_models_with_their_tiers() {
    let (_directory, database) = test_database().await;
    for model in ["model-a", "model-b"] {
        database
            .upsert_model_price(model, "USD", Decimal::ONE, Decimal::TWO)
            .await
            .expect("model price fixture");
    }

    let page = database
        .list_model_prices_page("USD", 1, 1)
        .await
        .expect("bounded model price page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].model, "model-b");
    assert_eq!(page[0].tiers.len(), 1);
    assert_eq!(page[0].tiers[0].service_tier, "default");

    let first = database
        .list_model_prices_after("USD", 1, "")
        .await
        .expect("first keyset model price page");
    let second = database
        .list_model_prices_after("USD", 1, &first[0].model)
        .await
        .expect("second keyset model price page");
    assert_eq!(first[0].model, "model-a");
    assert_eq!(second[0].model, "model-b");
    assert_eq!(second[0].tiers[0].service_tier, "default");
    assert!(
        database
            .list_model_prices_after("USD", 1, &second[0].model)
            .await
            .expect("exhausted keyset page")
            .is_empty()
    );

    let error = database
        .list_model_prices_page("USD", 1_001, 0)
        .await
        .expect_err("oversized model price pages must fail closed");
    assert!(matches!(error, AppError::BadRequest(_)));
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
async fn sync_falls_back_to_input_price_when_upstream_omits_cache_prices() {
    let (_directory, database) = test_database().await;
    let server = MockServer::start().await;
    mount_json(
        &server,
        "/models-dev-without-cache-prices",
        r#"{
            "providers": {
                "openai": {
                    "models": {
                        "gpt-no-cache-prices": {
                            "cost": {"input": "4.25", "output": "8.5"}
                        }
                    }
                }
            }
        }"#,
    )
    .await;

    let source_url = format!("{}/models-dev-without-cache-prices", server.uri());
    let result = sync_model_prices_with_sources(
        &database,
        &reqwest::Client::new(),
        vec!["openai/gpt-no-cache-prices".to_owned()],
        "USD",
        &[("models.dev", source_url.as_str())],
        true,
    )
    .await
    .expect("price synchronization without upstream cache prices");

    assert_eq!(result.imported, 1);
    let price = database
        .model_price_view("openai/gpt-no-cache-prices", "USD")
        .await
        .expect("fallback cache price view");
    assert_eq!(price.tiers[0].input_per_million, "4.25");
    assert_eq!(price.tiers[0].cached_input_per_million, "4.25");
    assert_eq!(price.tiers[0].cache_write_per_million, "4.25");
    assert!(price.tiers[0].cache_price_estimated);
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
        include_str!("../../tests/fixtures/pricing/litellm.json"),
    )
    .await;
    mount_json(
        &server,
        "/openrouter",
        include_str!("../../tests/fixtures/pricing/openrouter.json"),
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
