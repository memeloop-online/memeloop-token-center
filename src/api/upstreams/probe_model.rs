//! Choose the model a real upstream probe will call.
//!
//! An explicit `probe_model` on the account wins. Otherwise the account's own
//! catalog is used: the cheapest priced model, then the lowest numeric version,
//! then the first model in the supplied catalog order. The filter assistant's
//! model is never a candidate.

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogModel {
    pub(crate) model_id: String,
    pub(crate) protocol: String,
    pub(crate) input_price: Option<i64>,
}

pub(crate) fn select_probe_model(
    configured: Option<&str>,
    models: &[CatalogModel],
) -> Option<String> {
    if let Some(model) = configured.map(str::trim).filter(|model| !model.is_empty()) {
        return Some(model.to_owned());
    }
    let text: Vec<&CatalogModel> = models
        .iter()
        .filter(|model| is_text(&model.protocol))
        .collect();
    let pool: Vec<&CatalogModel> = if text.is_empty() {
        models.iter().collect()
    } else {
        text
    };
    if pool.is_empty() {
        return None;
    }
    if pool.iter().any(|model| model.input_price.is_some()) {
        return pool
            .iter()
            .filter(|model| model.input_price.is_some())
            .min_by(|left, right| {
                left.input_price
                    .cmp(&right.input_price)
                    .then_with(|| left.model_id.cmp(&right.model_id))
            })
            .map(|model| model.model_id.clone());
    }
    let mut versioned: Vec<(Vec<u64>, &CatalogModel)> = pool
        .iter()
        .filter_map(|model| version_numbers(&model.model_id).map(|version| (version, *model)))
        .collect();
    if !versioned.is_empty() {
        versioned.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.model_id.cmp(&right.1.model_id))
        });
        return Some(versioned[0].1.model_id.clone());
    }
    Some(pool[0].model_id.clone())
}

fn is_text(protocol: &str) -> bool {
    matches!(protocol, "openai" | "anthropic" | "any")
}

fn version_numbers(model: &str) -> Option<Vec<u64>> {
    let mut numbers = Vec::new();
    let mut current: Option<u64> = None;
    for character in model.chars() {
        if let Some(digit) = character.to_digit(10) {
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit)),
            );
        } else if let Some(value) = current.take() {
            numbers.push(value);
        }
    }
    if let Some(value) = current.take() {
        numbers.push(value);
    }
    if numbers.is_empty() {
        None
    } else {
        Some(pad_version(numbers))
    }
}

fn pad_version(mut numbers: Vec<u64>) -> Vec<u64> {
    while numbers.len() < 4 {
        numbers.push(0);
    }
    numbers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, protocol: &str, price: Option<i64>) -> CatalogModel {
        CatalogModel {
            model_id: id.to_owned(),
            protocol: protocol.to_owned(),
            input_price: price,
        }
    }

    #[test]
    fn configured_model_overrides_price_and_version() {
        let models = vec![model("gpt-4.1", "openai", Some(1))];
        assert_eq!(
            select_probe_model(Some(" gpt-5.4-mini "), &models).as_deref(),
            Some("gpt-5.4-mini")
        );
    }

    #[test]
    fn cheapest_priced_text_model_wins() {
        let models = vec![
            model("gpt-5.4", "openai", Some(30)),
            model("gpt-4.1-nano", "openai", Some(5)),
            model("whisper-1", "openai-audio", Some(1)),
        ];
        assert_eq!(
            select_probe_model(None, &models).as_deref(),
            Some("gpt-4.1-nano")
        );
    }

    #[test]
    fn missing_prices_use_the_lowest_numeric_version() {
        let models = vec![
            model("kimi-k3-256k", "openai", None),
            model("kimi-k2.6", "openai", None),
            model("kimi-latest", "openai", None),
        ];
        assert_eq!(
            select_probe_model(None, &models).as_deref(),
            Some("kimi-k2.6")
        );
    }

    #[test]
    fn models_without_versions_keep_catalog_order() {
        let models = vec![
            model("codex-mini", "openai", None),
            model("codex-astra", "openai", None),
        ];
        assert_eq!(
            select_probe_model(None, &models).as_deref(),
            Some("codex-mini")
        );
    }
}
