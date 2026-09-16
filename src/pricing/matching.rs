use super::{MAX_CANDIDATES_PER_MODEL, RemotePrice, SyncCandidate};

pub(super) fn match_price(
    requested: &str,
    prices: &[RemotePrice],
) -> (Option<RemotePrice>, Vec<SyncCandidate>) {
    let requested = normalize_identity(requested);
    let (exact_count, exact) = bounded_matches(prices, |price| {
        normalize_identity(&price.source_model_id) == requested
    });
    if exact_count == 1 {
        return (exact.into_iter().next(), Vec::new());
    }
    if exact_count > 1 {
        return (None, candidates(exact, "ambiguous exact identity"));
    }
    let requested_tail = model_tail(&requested);
    let (tail_count, tail) = bounded_matches(prices, |price| {
        model_tail(&normalize_identity(&price.source_model_id)) == requested_tail
    });
    if tail_count == 1 {
        return (tail.into_iter().next(), Vec::new());
    }
    if tail_count > 1 {
        return (None, candidates(tail, "provider prefix is ambiguous"));
    }
    (None, Vec::new())
}

fn bounded_matches(
    prices: &[RemotePrice],
    mut predicate: impl FnMut(&RemotePrice) -> bool,
) -> (usize, Vec<RemotePrice>) {
    let mut count = 0_usize;
    let mut matches = Vec::with_capacity(MAX_CANDIDATES_PER_MODEL);
    for price in prices.iter().filter(|price| predicate(price)) {
        count = count.saturating_add(1);
        if matches.len() < MAX_CANDIDATES_PER_MODEL {
            matches.push(price.clone());
        }
    }
    (count, matches)
}

fn candidates(prices: Vec<RemotePrice>, reason: &str) -> Vec<SyncCandidate> {
    prices
        .into_iter()
        .take(MAX_CANDIDATES_PER_MODEL)
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

pub(super) fn normalized_models(models: Vec<String>) -> Vec<String> {
    let mut models = models
        .into_iter()
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty() && model.len() <= 300)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models
}

pub(super) fn source_priority(source: &str) -> usize {
    match source {
        "models.dev" => 0,
        "litellm" => 1,
        "openrouter" => 2,
        "manual" => 3,
        _ => 4,
    }
}

fn normalize_identity(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn model_tail(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}
