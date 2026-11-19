//! Lossless event-time facts without duplicating facts already in the canonical view.
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Map, Value, json};

use super::{HistoryError, HistoryIdentity, canonical_hash};

const COLUMNS: [&str; 14] = [
    "event_id",
    "tenant_id",
    "key_id",
    "request_id",
    "event_at",
    "event_kind",
    "protocol",
    "model",
    "status_code",
    "duration_ms",
    "input_tokens",
    "output_tokens",
    "cost_micros",
    "error_code",
];

fn base(identity: &HistoryIdentity, payload: &Value) -> Result<Map<String, Value>, HistoryError> {
    if identity.source != "event" || !payload.is_object() {
        return Err(HistoryError::Invalid("event original canonical identity"));
    }
    let cost = payload["billing"]["cost"]
        .as_str()
        .map(str::parse::<Decimal>)
        .transpose()
        .map_err(|_| HistoryError::Invalid("event original cost"))?
        .map(|cost| {
            cost.checked_mul(Decimal::from(1_000_000))
                .and_then(|value| value.to_i64())
                .ok_or(HistoryError::Invalid("event original cost overflow"))
        })
        .transpose()?;
    let mut values = Map::new();
    values.insert("event_id".into(), json!(identity.request_id));
    values.insert("tenant_id".into(), json!(identity.tenant_id));
    values.insert("key_id".into(), json!(identity.key_id));
    for name in [
        "request_id",
        "event_at",
        "event_kind",
        "protocol",
        "model",
        "status_code",
        "duration_ms",
        "input_tokens",
        "output_tokens",
        "error_code",
    ] {
        values.insert(name.into(), payload[name].clone());
    }
    values.insert("cost_micros".into(), json!(cost));
    Ok(values)
}

fn validate_identity(
    identity: &HistoryIdentity,
    original: &Map<String, Value>,
) -> Result<(), HistoryError> {
    for (name, expected) in [
        ("event_id", identity.request_id),
        ("tenant_id", identity.tenant_id),
        ("key_id", identity.key_id),
    ] {
        if original
            .get(name)
            .and_then(Value::as_str)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(expected)
        {
            return Err(HistoryError::Mismatch);
        }
    }
    if original.get("event_at").and_then(Value::as_i64) != Some(identity.created_at) {
        return Err(HistoryError::Mismatch);
    }
    Ok(())
}

pub(crate) fn read_model(
    identity: &HistoryIdentity,
    payload: &Value,
    original: &Value,
) -> Result<Value, HistoryError> {
    if payload.is_null() && original.is_null() {
        return Ok(Value::Null);
    }
    let original = original.as_object().ok_or(HistoryError::Changed)?;
    if COLUMNS.iter().any(|name| !original.contains_key(*name)) {
        return Err(HistoryError::Invalid("event original source columns"));
    }
    validate_identity(identity, original)?;
    let canonical = base(identity, payload)?;
    // Unknown future source columns are retained, never silently dropped.
    let delta: Map<String, Value> = original
        .iter()
        .filter(|(name, value)| canonical.get(*name) != Some(*value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let model = json!({"event_original": {
        "version": 1,
        "sha256": canonical_hash(original)?,
        "delta": delta,
    }});
    if reconstruct(identity, payload, &model)? != Value::Object(original.clone()) {
        return Err(HistoryError::Mismatch);
    }
    Ok(model)
}

pub(crate) fn reconstruct(
    identity: &HistoryIdentity,
    payload: &Value,
    read_model: &Value,
) -> Result<Value, HistoryError> {
    let fidelity = &read_model["event_original"];
    if fidelity["version"].as_i64() != Some(1) {
        return Err(HistoryError::Invalid("event original format"));
    }
    let delta = fidelity["delta"]
        .as_object()
        .ok_or(HistoryError::Mismatch)?;
    let mut original = base(identity, payload)?;
    original.extend(delta.clone());
    validate_identity(identity, &original)?;
    let digest = canonical_hash(&original)?;
    if fidelity["sha256"].as_str() != Some(digest.as_str()) {
        return Err(HistoryError::Mismatch);
    }
    Ok(Value::Object(original))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn request_history_event_original_delta_preserves_raw_facts_and_unknown_columns() {
        let identity = HistoryIdentity {
            source: "event".into(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            request_id: Uuid::now_v7(),
            created_at: 42,
        };
        let payload = json!({"request_id":Uuid::now_v7(),"event_at":42,"event_kind":"finished",
            "protocol":"openai","model":"fixture","status_code":200,"duration_ms":10,
            "input_tokens":null,"output_tokens":2,"error_code":null,"billing":{"cost":"0.000042"}});
        let mut original = base(&identity, &payload).unwrap();
        original.insert("status_code".into(), json!(202));
        original.insert("input_tokens".into(), json!(0));
        original.insert("cost_micros".into(), json!(17));
        original.insert("future_column".into(), Value::Null);
        let raw = Value::Object(original);
        let model = read_model(&identity, &payload, &raw).unwrap();
        assert_eq!(reconstruct(&identity, &payload, &model).unwrap(), raw);
        for already_canonical in [
            "event_id",
            "tenant_id",
            "key_id",
            "request_id",
            "event_at",
            "model",
        ] {
            assert!(
                model["event_original"]["delta"]
                    .get(already_canonical)
                    .is_none()
            );
        }
        assert_eq!(model["event_original"]["delta"]["input_tokens"], 0);
        let mut tampered = model.clone();
        tampered["event_original"]["delta"]["status_code"] = json!(500);
        assert!(reconstruct(&identity, &payload, &tampered).is_err());
        let mut changed_base = payload.clone();
        changed_base["model"] = json!("tampered");
        assert!(reconstruct(&identity, &changed_base, &model).is_err());
    }
}
