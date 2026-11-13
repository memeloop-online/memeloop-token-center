//! Native business-row provenance. Delivery leases are operational PG state,
//! never historical facts. Original object locators are provenance, not GC roots.
//!
//! Reversible columns: identity gives id/request_id, tenant_id, key_id and
//! created_at; canonical payload gives lifecycle/status/timing/model/token
//! scalars; route_id maps model_route_id, session_context.session_id maps the
//! native conversation_cluster_id or terminal session_id; billing supplies
//! cost_micros/currency; generation usage supplies billed_units/billing_unit;
//! statistics supplies the nullable known-cache counters. Existing archive
//! locators supply object columns only when exactly equal to the original.
//! reservation/account/service-tier/routing/submission and future columns have
//! no equivalent canonical value and remain explicit scalar deltas. Raw zero,
//! NULL, empty string and original status/currency differences never collapse.
//! This codec covers request/terminal business rows, not the other four native
//! originals collections or ongoing operational/financial PG responsibilities.
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Map, Value, json};

use super::{HistoryError, HistoryIdentity, HistoryStatistics, canonical_hash};

const REQUEST_OPERATIONAL: &[&str] = &["proxy_owner_token", "proxy_owner_expires_at"];
const TERMINAL_OPERATIONAL: &[&str] = &[
    "lease_owner",
    "lease_expires_at",
    "attempts",
    "projected_at",
    "account_projected_at",
    "statistics_outcome",
];

fn canonical_row(
    identity: &HistoryIdentity,
    payload: &Value,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    terminal: bool,
) -> Result<Map<String, Value>, HistoryError> {
    let mut row = Map::new();
    row.insert(
        if terminal { "request_id" } else { "id" }.into(),
        json!(identity.request_id),
    );
    row.insert("tenant_id".into(), json!(identity.tenant_id));
    row.insert("key_id".into(), json!(identity.key_id));
    row.insert("created_at".into(), json!(identity.created_at));
    for name in [
        "completed_at",
        "protocol",
        "model",
        "upstream_model",
        "status_code",
        "duration_ms",
        "first_output_ms",
        "generation_duration_ms",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "error_code",
        "upstream_account_id",
        "usage_basis",
        "terminal_cause_code",
        "in_flight_stage",
    ] {
        row.insert(name.into(), payload[name].clone());
    }
    row.insert("model_route_id".into(), payload["route_id"].clone());
    row.insert(
        if terminal {
            "session_id"
        } else {
            "conversation_cluster_id"
        }
        .into(),
        payload["session_context"]["session_id"].clone(),
    );
    row.insert(
        "request_object".into(),
        model["archive"]["request_object"].clone(),
    );
    row.insert(
        "response_object".into(),
        model["archive"]["response_object"].clone(),
    );
    row.insert("currency".into(), payload["billing"]["currency"].clone());
    let cost = payload["billing"]["cost"]
        .as_str()
        .map(str::parse::<Decimal>)
        .transpose()
        .map_err(|_| HistoryError::Invalid("native original cost"))?
        .map(|cost| {
            cost.checked_mul(Decimal::from(1_000_000))
                .and_then(|cost| cost.to_i64())
                .ok_or(HistoryError::Invalid("native original cost overflow"))
        })
        .transpose()?;
    row.insert("cost_micros".into(), json!(cost));
    for (name, path) in [
        ("billed_units", "billed_units"),
        ("generation_units", "billed_units"),
        ("billing_unit", "billing_unit"),
    ] {
        row.insert(name.into(), payload["usage"]["generation"][path].clone());
    }
    for name in ["cache_known_read_tokens", "cache_known_input_tokens"] {
        let value = statistics.and_then(|statistics| match name {
            "cache_known_read_tokens" => statistics.cache_known_read_tokens,
            "cache_known_input_tokens" => statistics.cache_known_input_tokens,
            _ => unreachable!("fixed cache provenance fields"),
        });
        row.insert(name.into(), json!(value));
    }
    Ok(row)
}

fn business_row(row: &Value, terminal: bool) -> Result<Value, HistoryError> {
    if row.is_null() {
        return Ok(Value::Null);
    }
    let mut row = row.as_object().ok_or(HistoryError::Mismatch)?.clone();
    for name in if terminal {
        TERMINAL_OPERATIONAL
    } else {
        REQUEST_OPERATIONAL
    } {
        row.remove(*name);
    }
    Ok(Value::Object(row))
}

fn validate(
    identity: &HistoryIdentity,
    row: &Map<String, Value>,
    terminal: bool,
) -> Result<(), HistoryError> {
    for (name, expected) in [
        (
            if terminal { "request_id" } else { "id" },
            identity.request_id,
        ),
        ("tenant_id", identity.tenant_id),
        ("key_id", identity.key_id),
    ] {
        if row
            .get(name)
            .and_then(Value::as_str)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(expected)
        {
            return Err(HistoryError::Mismatch);
        }
    }
    if row.get("created_at").and_then(Value::as_i64) != Some(identity.created_at) {
        return Err(HistoryError::Mismatch);
    }
    Ok(())
}

/// Encode only business differences. The async materializer selects inline
/// metadata or rooted CAS; both preserve exact future columns and NULL values.
pub(crate) fn read_model(
    identity: &HistoryIdentity,
    payload: &Value,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    originals: &Value,
) -> Result<Value, HistoryError> {
    if identity.source != "native" || !payload.is_object() {
        return Err(HistoryError::Mismatch);
    }
    let mut rows = Map::new();
    for (name, terminal) in [("request", false), ("terminal", true)] {
        let raw = business_row(&originals[name], terminal)?;
        if raw.is_null() {
            if !terminal {
                return Err(HistoryError::Mismatch);
            }
            rows.insert(name.into(), Value::Null);
            continue;
        }
        let original = raw.as_object().ok_or(HistoryError::Mismatch)?;
        validate(identity, original, terminal)?;
        let base = canonical_row(identity, payload, statistics, model, terminal)?;
        let delta: Map<String, Value> = original
            .iter()
            .filter(|(name, value)| base.get(*name) != Some(*value))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        rows.insert(name.into(), json!({"columns":original.keys().collect::<Vec<_>>(), "sha256":canonical_hash(original)?, "delta":delta}));
    }
    let fidelity = json!({"version":1,"rows":rows});
    let restored = reconstruct(identity, payload, statistics, model, &fidelity)?;
    for (name, terminal) in [("request", false), ("terminal", true)] {
        if restored[name] != business_row(&originals[name], terminal)? {
            return Err(HistoryError::Mismatch);
        }
    }
    Ok(fidelity)
}

/// Materialization-only: large provenance is kept once in tenant CAS, with
/// an independently discoverable owner published before delivery can be ACKed.
/// The threshold selects representation; it never rejects a history value.
pub(crate) async fn read_model_with_archive(
    versioned_identity: (&HistoryIdentity, i64),
    payload: &Value,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    originals: &Value,
    archive: Option<&crate::archive::ArchiveStore>,
) -> Result<Value, HistoryError> {
    let (identity, revision) = versioned_identity;
    let fidelity = read_model(identity, payload, statistics, model, originals)?;
    let encoded = serde_json::to_vec(&fidelity)?;
    if encoded.len() <= 4096 {
        return Ok(fidelity);
    }
    let archive = archive.ok_or(HistoryError::Unavailable)?;
    let object = archive
        .put_tenant_content(identity.tenant_id, bytes::Bytes::from(encoded))
        .await?;
    let root = crate::archive::HistoryArchiveRoot {
        version: 1,
        tenant_id: identity.tenant_id,
        key_id: identity.key_id,
        source: identity.source.clone(),
        request_id: identity.request_id,
        created_at: identity.created_at,
        revision,
        object,
    };
    let location = archive.publish_history_root(&root).await?;
    let encoded = json!({"version":2,"root_location":location,"root":root});
    let restored = reconstruct_with_archive(
        (identity, revision),
        payload,
        statistics,
        model,
        &encoded,
        archive,
    )
    .await?;
    for (name, terminal) in [("request", false), ("terminal", true)] {
        if restored[name] != business_row(&originals[name], terminal)? {
            return Err(HistoryError::Mismatch);
        }
    }
    Ok(encoded)
}

pub(crate) async fn reconstruct_with_archive(
    versioned_identity: (&HistoryIdentity, i64),
    payload: &Value,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    fidelity: &Value,
    archive: &crate::archive::ArchiveStore,
) -> Result<Value, HistoryError> {
    let (identity, revision) = versioned_identity;
    if fidelity["version"].as_i64() == Some(1) {
        return reconstruct(identity, payload, statistics, model, fidelity);
    }
    if fidelity["version"].as_i64() != Some(2) {
        return Err(HistoryError::Mismatch);
    }
    let root: crate::archive::HistoryArchiveRoot =
        serde_json::from_value(fidelity["root"].clone())?;
    if root.tenant_id != identity.tenant_id
        || root.key_id != identity.key_id
        || root.source != identity.source
        || root.request_id != identity.request_id
        || root.created_at != identity.created_at
        || root.revision != revision
        || fidelity["root_location"].as_str() != Some(root.location()?.as_str())
    {
        return Err(HistoryError::Mismatch);
    }
    let location = root.location()?;
    let expected = serde_json::to_vec(&root)?;
    let actual = archive.get_bounded(&location, expected.len()).await?;
    if crate::archive::ArchiveStore::read_history_root_bytes(&location, &actual)? != root {
        return Err(HistoryError::Mismatch);
    }
    let maximum = usize::try_from(root.object.size_bytes).map_err(|_| HistoryError::Mismatch)?;
    let bytes = archive
        .get_tenant_content(identity.tenant_id, &root.object, maximum)
        .await?;
    let decoded = serde_json::from_slice(&bytes)?;
    reconstruct(identity, payload, statistics, model, &decoded)
}

pub(crate) fn reconstruct(
    identity: &HistoryIdentity,
    payload: &Value,
    statistics: Option<&HistoryStatistics>,
    model: &Value,
    fidelity: &Value,
) -> Result<Value, HistoryError> {
    if identity.source != "native" || fidelity["version"].as_i64() != Some(1) {
        return Err(HistoryError::Mismatch);
    }
    let mut rows = Map::new();
    for (name, terminal) in [("request", false), ("terminal", true)] {
        let encoded = &fidelity["rows"][name];
        if encoded.is_null() {
            if !terminal {
                return Err(HistoryError::Mismatch);
            }
            rows.insert(name.into(), Value::Null);
            continue;
        }
        let base = canonical_row(identity, payload, statistics, model, terminal)?;
        let delta = encoded["delta"].as_object().ok_or(HistoryError::Mismatch)?;
        let mut original = Map::new();
        for name in encoded["columns"]
            .as_array()
            .ok_or(HistoryError::Mismatch)?
        {
            let name = name.as_str().ok_or(HistoryError::Mismatch)?;
            let value = delta
                .get(name)
                .or_else(|| base.get(name))
                .ok_or(HistoryError::Mismatch)?;
            if original.insert(name.into(), value.clone()).is_some() {
                return Err(HistoryError::Mismatch);
            }
        }
        if delta.keys().any(|key| !original.contains_key(key))
            || (if terminal {
                TERMINAL_OPERATIONAL
            } else {
                REQUEST_OPERATIONAL
            })
            .iter()
            .any(|name| original.contains_key(*name))
        {
            return Err(HistoryError::Mismatch);
        }
        validate(identity, &original, terminal)?;
        if encoded["sha256"].as_str() != Some(canonical_hash(&original)?.as_str()) {
            return Err(HistoryError::Mismatch);
        }
        rows.insert(name.into(), Value::Object(original));
    }
    Ok(Value::Object(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn request_history_native_large_delta_uses_durable_cas_without_a_value_limit() {
        let identity = HistoryIdentity {
            source: "native".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let payload = json!({"model":"fixture","status_code":200});
        let model = json!({"archive":{}});
        let originals = json!({"request":{"id":identity.request_id,"tenant_id":identity.tenant_id,
            "key_id":identity.key_id,"created_at":42,"status_code":202,
            "future":{"nested":[null,0,"x".repeat(16384)]}},"terminal":null});
        let archive = crate::archive::ArchiveStore::memory_for_test();
        assert!(
            read_model_with_archive((&identity, 7), &payload, None, &model, &originals, None)
                .await
                .is_err()
        );
        let encoded = read_model_with_archive(
            (&identity, 7),
            &payload,
            None,
            &model,
            &originals,
            Some(&archive),
        )
        .await
        .unwrap();
        assert_eq!(encoded["version"], 2);
        let replay = read_model_with_archive(
            (&identity, 7),
            &payload,
            None,
            &model,
            &originals,
            Some(&archive),
        )
        .await
        .unwrap();
        assert_eq!(encoded, replay);
        assert_eq!(
            reconstruct_with_archive((&identity, 7), &payload, None, &model, &encoded, &archive)
                .await
                .unwrap(),
            originals
        );
        assert!(
            reconstruct_with_archive((&identity, 8), &payload, None, &model, &encoded, &archive)
                .await
                .is_err()
        );
        let mut wrong = identity.clone();
        wrong.key_id = Uuid::now_v7();
        assert!(
            reconstruct_with_archive((&wrong, 7), &payload, None, &model, &encoded, &archive)
                .await
                .is_err()
        );
    }

    #[test]
    fn request_history_native_business_delta_preserves_raw_money_status_and_unknown() {
        let identity = HistoryIdentity {
            source: "native".into(),
            request_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            created_at: 42,
        };
        let payload = json!({"request_id":identity.request_id,"created_at":42,"protocol":"openai","model":"fixture","status_code":200,"input_tokens":null,"output_tokens":2,"billing":{"cost":"0.000042","currency":"USD"}});
        let model = json!({"archive":{"request_object":"archive-object:v1:fixture-existing-reference","response_object":null}});
        let originals = json!({"request":{"id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":42,"protocol":"openai","model":"fixture","status_code":202,"input_tokens":0,"output_tokens":2,"cost_micros":17,"currency":"EUR","request_object":"archive-object:v1:fixture-existing-reference","future_column":null,"proxy_owner_token":"not-a-historical-fact","proxy_owner_expires_at":99},"terminal":{"request_id":identity.request_id,"tenant_id":identity.tenant_id,"key_id":identity.key_id,"created_at":42,"status_code":202,"account_projected_at":99}});
        let encoded = read_model(&identity, &payload, None, &model, &originals).unwrap();
        let restored = reconstruct(&identity, &payload, None, &model, &encoded).unwrap();
        assert_eq!(restored["request"]["status_code"], 202);
        assert_eq!(restored["request"]["input_tokens"], 0);
        assert_eq!(restored["request"]["cost_micros"], 17);
        assert_eq!(restored["request"]["currency"], "EUR");
        assert!(restored["request"].get("future_column").unwrap().is_null());
        assert!(restored["request"].get("proxy_owner_token").is_none());
        assert!(restored["terminal"].get("account_projected_at").is_none());
        for shared in [
            "id",
            "tenant_id",
            "key_id",
            "created_at",
            "protocol",
            "model",
            "request_object",
        ] {
            assert!(encoded["rows"]["request"]["delta"].get(shared).is_none());
        }
        let mut delivery_only = originals.clone();
        delivery_only["terminal"]["account_projected_at"] = json!(100);
        delivery_only["request"]["proxy_owner_expires_at"] = json!(100);
        assert_eq!(
            read_model(&identity, &payload, None, &model, &delivery_only).unwrap(),
            encoded
        );
        let mut tampered = encoded.clone();
        tampered["rows"]["request"]["delta"]["cost_micros"] = json!(0);
        assert!(reconstruct(&identity, &payload, None, &model, &tampered).is_err());
        let mut wrong_identity = identity.clone();
        wrong_identity.tenant_id = Uuid::now_v7();
        assert!(reconstruct(&wrong_identity, &payload, None, &model, &encoded).is_err());
        let mut large = originals.clone();
        large["request"]["future_blob"] = json!("x".repeat(4097));
        let large_encoded = read_model(&identity, &payload, None, &model, &large).unwrap();
        assert_eq!(
            reconstruct(&identity, &payload, None, &model, &large_encoded).unwrap()["request"]["future_blob"],
            large["request"]["future_blob"]
        );
    }
}
