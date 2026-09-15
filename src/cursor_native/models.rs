use super::proto::{WireValue, fields};
use crate::db::DiscoveredUpstreamModel;
use std::collections::BTreeSet;

/// Deliberately project only the public model ID. The current catalog has no
/// display-name fields. Never retain raw ModelDetails: it can contain BYOK
/// credentials. This schema provides no context/output token limit.
pub(crate) fn decode(body: &[u8]) -> Result<Vec<DiscoveredUpstreamModel>, &'static str> {
    let mut ids = BTreeSet::new();
    let mut count = 0;
    for field in fields(body)? {
        if field.number != 1 {
            continue;
        }
        count += 1;
        if count > 10_000 {
            return Err("invalid_response");
        }
        let WireValue::Bytes(model) = field.value else {
            return Err("invalid_response");
        };
        let mut id = None;
        for field in fields(model)? {
            if field.number != 1 {
                continue;
            }
            let WireValue::Bytes(value) = field.value else {
                return Err("invalid_response");
            };
            let value = std::str::from_utf8(value).map_err(|_| "invalid_response")?;
            if id.replace(value).is_some() {
                return Err("invalid_response");
            }
        }
        let id = id.ok_or("invalid_response")?;
        if id.is_empty() || id.len() > 500 || id.trim() != id || id.chars().any(char::is_control) {
            return Err("invalid_response");
        }
        ids.insert(id.to_owned());
    }
    Ok(ids
        .into_iter()
        .map(|model_id| DiscoveredUpstreamModel {
            model_id,
            // Discovery is not a claim that OpenAI generation is implemented.
            protocol: "cursor_agent".into(),
            context_window: None,
            reservation_token_bound: None,
            reservation_bound_source: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_keep_only_ids_without_inventing_limits() {
        // Synthetic ModelDetails: model_id=x, display_name=y, API credentials=secret.
        let body = [
            10, 14, 10, 1, b'x', 34, 1, b'y', 66, 6, b's', b'e', b'c', b'r', b'e', b't',
        ];
        let models = decode(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "x");
        assert_eq!(models[0].protocol, "cursor_agent");
        assert!(models[0].context_window.is_none());
        assert!(models[0].reservation_token_bound.is_none());
        assert!(models[0].reservation_bound_source.is_none());
        assert!(decode(&[]).unwrap().is_empty());
        for body in [
            &[10, 0][..],
            &[10, 2, 8, 1],
            &[10, 3, 10, 1, 255],
            &[10, 6, 10, 1, b'x', 10, 1, b'y'],
        ] {
            assert_eq!(decode(body).err(), Some("invalid_response"));
        }
    }
}
