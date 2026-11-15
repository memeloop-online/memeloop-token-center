//! Reversible native metadata collections. JSON text is preserved as text:
//! parsing/re-serializing it would discard historical byte-level distinctions.
use serde_json::{Map, Value, json};

use super::{HistoryError, HistoryIdentity, HistoryStatistics, canonical_hash};

pub(super) const NAMES: [&str; 4] = ["statistics", "egress", "transport", "observations"];

fn validate(
    identity: &HistoryIdentity,
    name: &str,
    row: &Map<String, Value>,
) -> Result<(), HistoryError> {
    if row
        .get("request_id")
        .and_then(Value::as_str)
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        != Some(identity.request_id)
    {
        return Err(HistoryError::Mismatch);
    }
    for (field, expected) in [
        ("tenant_id", identity.tenant_id),
        ("key_id", identity.key_id),
    ] {
        if (field != "key_id" || name == "statistics")
            && let Some(value) = row.get(field)
            && value.as_str().and_then(|id| uuid::Uuid::parse_str(id).ok()) != Some(expected)
        {
            return Err(HistoryError::Mismatch);
        }
    }
    Ok(())
}

fn base(
    identity: &HistoryIdentity,
    name: &str,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    row: &Value,
) -> Result<Map<String, Value>, HistoryError> {
    let mut base = match name {
        "statistics" => serde_json::to_value(statistics)?
            .as_object()
            .cloned()
            .unwrap_or_default(),
        "egress" | "transport" => {
            let (collection, keys): (&str, &[&str]) = if name == "egress" {
                ("egress_attempts", &["attempt_id"])
            } else {
                (
                    "transport_diagnostics",
                    &["route_id", "upstream_account_id"],
                )
            };
            model[collection]
                .as_array()
                .into_iter()
                .flatten()
                .find(|candidate| keys.iter().all(|key| candidate[*key] == row[*key]))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default()
        }
        "observations" => Map::new(),
        _ => return Err(HistoryError::Mismatch),
    };
    base.insert("request_id".into(), json!(identity.request_id));
    base.insert("tenant_id".into(), json!(identity.tenant_id));
    base.insert("key_id".into(), json!(identity.key_id));
    Ok(base)
}

pub(super) fn encode(
    identity: &HistoryIdentity,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    originals: &Value,
) -> Result<Value, HistoryError> {
    if identity.source != "native" {
        return Err(HistoryError::Mismatch);
    }
    let mut collections = Map::new();
    for name in NAMES {
        let mut encoded = Vec::new();
        for row in originals[name].as_array().ok_or(HistoryError::Mismatch)? {
            let original = row.as_object().ok_or(HistoryError::Mismatch)?;
            validate(identity, name, original)?;
            let base = base(identity, name, statistics, model, row)?;
            let delta: Map<String, Value> = original
                .iter()
                .filter(|(key, value)| base.get(*key) != Some(*value))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            // Stable lookup keys are included separately: their values can be
            // omitted from delta only if the canonical collection restores them.
            let lookup = match name {
                "egress" => json!({"attempt_id": row["attempt_id"]}),
                "transport" => {
                    json!({"route_id":row["route_id"],"upstream_account_id":row["upstream_account_id"]})
                }
                _ => Value::Null,
            };
            encoded.push(json!({"lookup":lookup,"columns":original.keys().collect::<Vec<_>>(),"delta":delta,"sha256":canonical_hash(original)?}));
        }
        collections.insert(name.into(), Value::Array(encoded));
    }
    let encoded = json!({"version":1,"collections":collections});
    if reconstruct(identity, statistics, model, &encoded)? != *originals {
        return Err(HistoryError::Mismatch);
    }
    Ok(encoded)
}

pub(super) fn reconstruct(
    identity: &HistoryIdentity,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    encoded: &Value,
) -> Result<Value, HistoryError> {
    if identity.source != "native" || encoded["version"] != 1 {
        return Err(HistoryError::Mismatch);
    }
    let mut collections = Map::new();
    for name in NAMES {
        let mut rows = Vec::new();
        for encoded in encoded["collections"][name]
            .as_array()
            .ok_or(HistoryError::Mismatch)?
        {
            let base = base(identity, name, statistics, model, &encoded["lookup"])?;
            let delta = encoded["delta"].as_object().ok_or(HistoryError::Mismatch)?;
            let mut row = Map::new();
            for column in encoded["columns"]
                .as_array()
                .ok_or(HistoryError::Mismatch)?
            {
                let column = column.as_str().ok_or(HistoryError::Mismatch)?;
                let value = delta
                    .get(column)
                    .or_else(|| base.get(column))
                    .ok_or(HistoryError::Mismatch)?;
                if row.insert(column.into(), value.clone()).is_some() {
                    return Err(HistoryError::Mismatch);
                }
            }
            if delta.keys().any(|key| !row.contains_key(key)) {
                return Err(HistoryError::Mismatch);
            }
            validate(identity, name, &row)?;
            if encoded["sha256"].as_str() != Some(canonical_hash(&row)?.as_str()) {
                return Err(HistoryError::Mismatch);
            }
            rows.push(Value::Object(row));
        }
        collections.insert(name.into(), Value::Array(rows));
    }
    Ok(Value::Object(collections))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn request_history_native_collections_keep_raw_json_null_zero_and_scope() {
        let identity = HistoryIdentity {
            source: "native".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let attempt = Uuid::now_v7();
        let model = json!({"egress_attempts":[{"request_id":identity.request_id,"attempt_id":attempt,"observed_at":42,"completed":false}],"transport_diagnostics":[]});
        let originals = json!({
            "statistics":[{"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"cost_micros":0,"currency":"EUR","future":null}],
            "egress":[{"request_id":identity.request_id,"attempt_id":attempt,"observed_at":42,"completed":0,"snapshot_json":"{ \"kind\" : \"unknown\" }","request_payload_bytes":0,"response_payload_bytes":null}],
            "transport":[],
            "observations":[{"request_id":identity.request_id,"key_id":identity.key_id,"atom_hashes_json":"[ ]","future":{"value":"x".repeat(16384)}}]
        });
        let encoded = encode(&identity, None, &model, &originals).unwrap();
        assert!(
            encoded["collections"]["egress"][0]["delta"]
                .get("observed_at")
                .is_none()
        );
        assert_eq!(encoded["collections"]["egress"][0]["delta"]["completed"], 0);
        assert_eq!(
            reconstruct(&identity, None, &model, &encoded).unwrap(),
            originals
        );
        let mut tampered = encoded.clone();
        tampered["collections"]["statistics"][0]["delta"]["cost_micros"] = json!(1);
        assert!(reconstruct(&identity, None, &model, &tampered).is_err());
        let mut wrong = identity.clone();
        wrong.tenant_id = Uuid::now_v7();
        assert!(reconstruct(&wrong, None, &model, &encoded).is_err());
        wrong = identity.clone();
        wrong.request_id = Uuid::now_v7();
        assert!(reconstruct(&wrong, None, &model, &encoded).is_err());
    }
}
