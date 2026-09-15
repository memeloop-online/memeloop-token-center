//! Personal plan quota from first-party Cursor DashboardService protobuf.
//! No CLI, token refresh, inference, enterprise aggregation, on-demand limit
//! inference or reset operation. See docs/cursor-native-protocol.md.
use super::*;
use crate::cursor_native::{
    self, Method,
    proto::{Field, WireValue, fields},
};

pub(super) async fn read(
    state: &AppState,
    credential: &UpstreamCredential,
    mut snapshot: QuotaSnapshot,
) -> Result<QuotaSnapshot, &'static str> {
    // The existing quota cache supplies the overall eight-second deadline.
    // Optional plan metadata must not hold a successful quota read until that
    // deadline; failure leaves its name/reset fallback unknown.
    let (usage, plan_info) = tokio::join!(
        cursor_native::unary(state, credential, Method::GetCurrentPeriodUsage),
        tokio::time::timeout(
            Duration::from_secs(3),
            cursor_native::unary(state, credential, Method::GetPlanInfo)
        ),
    );
    let usage = usage.map_err(error_code)?;
    let metadata = plan_info
        .ok()
        .and_then(Result::ok)
        .and_then(|bytes| metadata(&bytes).ok())
        .unwrap_or_default();
    snapshot.windows = windows(&usage, metadata.reset_at)?;
    snapshot.plan_type = metadata.name;
    let now = unix_millis();
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(now);
    snapshot.stale_after = Some(now + FRESH_MS);
    Ok(snapshot)
}

fn error_code(error: &str) -> &'static str {
    match error {
        "credential_invalid" => "credential_invalid",
        "authentication_failed" => "quota_not_authorized",
        "rate_limited" => "quota_rate_limited",
        "connection_failed" => "quota_transport_failed",
        "destination_invalid" | "redirect_rejected" => "quota_destination_invalid",
        "response_too_large" => "quota_response_too_large",
        "invalid_response" => "quota_invalid_payload",
        _ => "quota_upstream_error",
    }
}

fn decode(bytes: &[u8]) -> Result<Vec<Field<'_>>, &'static str> {
    fields(bytes).map_err(error_code)
}

// Presence is preserved. Even non-optional proto3 scalars absent from the wire
// stay unknown here, rather than converting an absent supplier value into zero.
fn field<'a>(fields: &[Field<'a>], number: u32) -> Result<Option<WireValue<'a>>, &'static str> {
    let mut matching = fields.iter().filter(|field| field.number == number);
    let first = matching.next().map(|field| field.value);
    if matching.next().is_some() {
        return Err("quota_invalid_payload");
    }
    Ok(first)
}

fn message<'a>(fields: &[Field<'a>], number: u32) -> Result<Option<&'a [u8]>, &'static str> {
    match field(fields, number)? {
        None => Ok(None),
        Some(WireValue::Bytes(bytes)) => Ok(Some(bytes)),
        _ => Err("quota_invalid_payload"),
    }
}

fn integer(fields: &[Field<'_>], number: u32) -> Result<Option<u64>, &'static str> {
    match field(fields, number)? {
        None => Ok(None),
        Some(WireValue::Varint(value)) => Ok(Some(value)),
        _ => Err("quota_invalid_payload"),
    }
}

fn amount(fields: &[Field<'_>], number: u32) -> Result<Option<f64>, &'static str> {
    // Signed int32 plan fields are used only for the first-party percent ratio.
    // Their unit is unverified; never label them dollars/cents/tokens.
    Ok(integer(fields, number)?
        .and_then(|value| i32::try_from(value).ok())
        .map(f64::from))
}

fn instant(fields: &[Field<'_>], number: u32) -> Result<Option<i64>, &'static str> {
    Ok(integer(fields, number)?
        .and_then(|value| i64::try_from(value).ok())
        .filter(|value| *value > 0))
}

fn percent(fields: &[Field<'_>], number: u32) -> Result<Option<f64>, &'static str> {
    match field(fields, number)? {
        None => Ok(None),
        Some(WireValue::Fixed64(bits)) => {
            let value = f64::from_bits(bits);
            Ok((value.is_finite() && value >= 0.0).then_some(value))
        }
        _ => Err("quota_invalid_payload"),
    }
}

#[derive(Default)]
struct Metadata {
    name: Option<String>,
    reset_at: Option<i64>,
}

fn metadata(bytes: &[u8]) -> Result<Metadata, &'static str> {
    let outer = decode(bytes)?;
    let Some(inner) = message(&outer, 1)? else {
        return Ok(Metadata::default());
    };
    let inner = decode(inner)?;
    let name = message(&inner, 1)?
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|name| {
            !name.trim().is_empty() && name.len() <= 160 && !name.chars().any(char::is_control)
        })
        .map(str::to_owned);
    Ok(Metadata {
        name,
        reset_at: instant(&inner, 4)?,
    })
}

fn row(id: &str, label: &str, reset_at: Option<i64>, used_percent: Option<f64>) -> QuotaWindow {
    QuotaWindow {
        id: id.into(),
        label: label.into(),
        used_percent,
        used: None,
        remaining: None,
        limit: None,
        unit: None,
        reset_at,
        period_seconds: None,
        source: "cursor_period_usage",
        reset_is_estimated: false,
        allowed: None,
        limit_reached: None,
    }
}

fn windows(bytes: &[u8], fallback_reset: Option<i64>) -> Result<Vec<QuotaWindow>, &'static str> {
    let outer = decode(bytes)?;
    let Some(plan) = message(&outer, 3)? else {
        return Err("quota_usage_unavailable");
    };
    let plan = decode(plan)?;
    // First-party usage-data.ts prefers the current response's positive epoch
    // millisecond end, then PlanInfo. It never substitutes OAuth token expiry.
    let reset_at = instant(&outer, 2)?.or(fallback_reset);
    let used = amount(&plan, 2)?; // included_spend, deliberately not total_spend (1).
    let limit = amount(&plan, 5)?;
    let total_percent = percent(&plan, 14)?.or_else(|| {
        used.zip(limit)
            .filter(|(_, limit)| *limit > 0.0)
            .map(|(used, limit)| used / limit * 100.0)
    });
    let mut result = vec![row("included", "Included plan", reset_at, total_percent)];
    for (id, label, percent_field) in [("auto", "Auto usage", 12), ("api", "API usage", 13)] {
        let used_percent = percent(&plan, percent_field)?;
        if used_percent.is_some() {
            result.push(row(id, label, reset_at, used_percent));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independently encoded wire fixtures, not generated/vendored client code.
    fn varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        while value >= 128 {
            bytes.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        bytes.push(value as u8);
        bytes
    }
    fn number(field: u32, value: u64) -> Vec<u8> {
        [varint(u64::from(field) << 3), varint(value)].concat()
    }
    fn bytes(field: u32, value: &[u8]) -> Vec<u8> {
        [
            varint((u64::from(field) << 3) | 2),
            varint(value.len() as u64),
            value.to_vec(),
        ]
        .concat()
    }
    fn double(field: u32, value: f64) -> Vec<u8> {
        [
            varint((u64::from(field) << 3) | 1),
            value.to_le_bytes().to_vec(),
        ]
        .concat()
    }

    #[test]
    fn explicit_optional_percent_and_exact_reset_take_precedence() {
        let plan = [
            number(1, 999),
            number(2, 50),
            number(5, 200),
            double(14, 60.0),
            double(12, 0.0),
            double(13, 25.0),
        ]
        .concat();
        let current = [
            number(2, 1_800_000_123_456),
            bytes(3, &plan),
            bytes(99, b"ignored"),
        ]
        .concat();
        let rows = windows(&current, Some(1_900_000_000_000)).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].used_percent, Some(60.0));
        assert_eq!(rows[1].used_percent, Some(0.0));
        assert_eq!(rows[2].used_percent, Some(25.0));
        assert!(
            rows.iter()
                .all(|row| row.reset_at == Some(1_800_000_123_456)
                    && row.period_seconds.is_none()
                    && !row.reset_is_estimated
                    && row.used.is_none()
                    && row.remaining.is_none()
                    && row.limit.is_none()
                    && row.unit.is_none()
                    && row.allowed.is_none()
                    && row.limit_reached.is_none())
        );
        let fallback = windows(
            &bytes(3, &[number(1, 999), number(2, 50), number(5, 200)].concat()),
            None,
        )
        .unwrap();
        assert_eq!(fallback[0].used_percent, Some(25.0)); // Never total_spend / limit.
    }

    #[test]
    fn missing_zero_invalid_and_enterprise_evidence_remain_distinct() {
        let rows = windows(&bytes(3, &[]), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].used_percent.is_none() && rows[0].reset_at.is_none());
        for plan in [
            number(5, 200),
            number(2, 50),
            [number(2, 0), number(5, 0)].concat(),
            [number(2, u64::MAX), number(5, 200)].concat(),
            double(14, f64::NAN),
            double(14, f64::INFINITY),
            double(14, -1.0),
        ] {
            assert!(
                windows(&bytes(3, &plan), None).unwrap()[0]
                    .used_percent
                    .is_none()
            );
        }
        let zero = windows(&bytes(3, &[number(2, 0), number(5, 200)].concat()), None).unwrap();
        assert_eq!(zero[0].used_percent, Some(0.0));
        assert!(matches!(windows(&[], None), Err("quota_usage_unavailable")));
        // Spend-limit-only/team responses do not prove personal included quota.
        assert!(matches!(
            windows(&bytes(4, &number(5, 100)), None),
            Err("quota_usage_unavailable")
        ));
    }

    #[test]
    fn optional_plan_info_is_bounded_and_only_supplies_name_and_reset_fallback() {
        let info = bytes(
            1,
            &[
                bytes(1, b"Pro"),
                number(2, 2000),
                number(4, 1_800_000_123_456),
            ]
            .concat(),
        );
        let info = metadata(&info).unwrap();
        assert_eq!(info.name.as_deref(), Some("Pro"));
        let rows = windows(&bytes(3, &double(14, 10.0)), info.reset_at).unwrap();
        assert_eq!(rows[0].reset_at, Some(1_800_000_123_456));
        assert!(rows[0].limit.is_none()); // PlanInfo cents do not prove PlanUsage unit.
        for name in [b"bad\nname".to_vec(), vec![b'x'; 161], vec![255]] {
            assert!(
                metadata(&bytes(1, &bytes(1, &name)))
                    .unwrap()
                    .name
                    .is_none()
            );
        }
        for timestamp in [0, u64::MAX] {
            assert!(
                metadata(&bytes(1, &number(4, timestamp)))
                    .unwrap()
                    .reset_at
                    .is_none()
            );
        }
    }

    #[test]
    fn malformed_wire_and_error_codes_fail_closed_without_echoing_supplier_data() {
        for payload in [
            vec![0],
            number(3, 1),
            bytes(3, &number(14, 1)),
            [bytes(3, &[]), bytes(3, &[])].concat(),
            bytes(3, &[double(14, 1.0), double(14, 2.0)].concat()),
        ] {
            assert!(matches!(
                windows(&payload, None),
                Err("quota_invalid_payload")
            ));
        }
        assert_eq!(error_code("authentication_failed"), "quota_not_authorized");
        assert_eq!(error_code("rate_limited"), "quota_rate_limited");
        assert_eq!(error_code("response_too_large"), "quota_response_too_large");
        assert_eq!(error_code("supplier-secret"), "quota_upstream_error");
    }

    #[test]
    fn cursor_capabilities_do_not_claim_reset_money_units_or_subscription_expiry() {
        let capabilities = QuotaCapabilities::for_provider("cursor");
        assert!(capabilities.read && capabilities.plan && capabilities.window_percent);
        assert!(capabilities.supplier_read_only && !capabilities.refreshes_credentials);
        assert!(!capabilities.window_amounts && !capabilities.window_amount_unit);
        assert!(
            !capabilities.reset_credit_expiry
                && !capabilities.subscription_expiry
                && !capabilities.consumes_reset_credit
        );
        let account: UpstreamAccountView = serde_json::from_value(json!({
            "id":Uuid::from_u128(1),"tenant_id":Uuid::from_u128(2),"name":"fixture",
            "driver":"cursor","auth_kind":"oauth","connection_method":"native_oauth",
            "credential_generation":1,"status":"active","config":{},"can_refresh":false,
            "can_rotate":false,"can_reauthorize":false,"route_count":0,"created_at":0,"updated_at":10
        })).unwrap();
        let snapshot = QuotaSnapshot::empty(&account, "fixture", None);
        assert!(
            !snapshot.reset_capability.implementation_available
                && !snapshot.reset_capability.prepare_available
        );
        assert_eq!(
            snapshot.reset_capability.reason,
            "quota_reset_not_supported"
        );
        assert!(snapshot.credits.balance.is_none() && snapshot.subscription_active_until.is_none());
    }
}
