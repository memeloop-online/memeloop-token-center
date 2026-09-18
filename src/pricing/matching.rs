use std::collections::HashMap;

use super::{MAX_CANDIDATES_PER_MODEL, RemotePrice, SyncCandidate};

/// Normalize each source identity once. Buckets keep the exact cardinality and
/// only a bounded sample, preserving ambiguity semantics without rescanning a
/// source (or allocating normalized source strings) for every requested model.
pub(super) struct IndexedPrices {
    prices: Vec<RemotePrice>,
    exact: HashMap<String, MatchBucket>,
    tails: HashMap<String, MatchBucket>,
}

#[derive(Default)]
struct MatchBucket {
    count: usize,
    sample: Vec<usize>,
}

impl MatchBucket {
    fn insert(&mut self, index: usize) {
        self.count += 1;
        if self.sample.len() < MAX_CANDIDATES_PER_MODEL {
            self.sample.push(index);
        }
    }
}

impl IndexedPrices {
    pub(super) fn new(prices: Vec<RemotePrice>) -> Self {
        let mut exact = HashMap::<String, MatchBucket>::new();
        let mut tails = HashMap::<String, MatchBucket>::new();
        for (index, price) in prices.iter().enumerate() {
            let identity = normalize_identity(&price.source_model_id);
            tails
                .entry(model_tail(&identity).to_owned())
                .or_default()
                .insert(index);
            exact.entry(identity).or_default().insert(index);
        }
        Self {
            prices,
            exact,
            tails,
        }
    }

    pub(super) fn match_price(&self, requested: &str) -> (Option<RemotePrice>, Vec<SyncCandidate>) {
        let requested = normalize_identity(requested);
        let (bucket, reason) = if let Some(bucket) = self.exact.get(&requested) {
            (bucket, "ambiguous exact identity")
        } else if let Some(bucket) = self.tails.get(model_tail(&requested)) {
            (bucket, "provider prefix is ambiguous")
        } else {
            return (None, Vec::new());
        };
        if bucket.count == 1 {
            return (Some(self.prices[bucket.sample[0]].clone()), Vec::new());
        }
        (
            None,
            candidates(
                bucket
                    .sample
                    .iter()
                    .map(|&index| self.prices[index].clone())
                    .collect(),
                reason,
            ),
        )
    }
}

#[cfg(test)]
pub(super) fn match_price(
    requested: &str,
    prices: &[RemotePrice],
) -> (Option<RemotePrice>, Vec<SyncCandidate>) {
    IndexedPrices::new(prices.to_vec()).match_price(requested)
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
        .filter(|model| !model.is_empty() && model.len() <= 500)
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
