//! Read-only native Kimi quota adapter. Never refreshes credentials or sends model traffic.
use super::*;

const USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

pub(super) async fn read(
    state: &AppState,
    credential: &UpstreamCredential,
    mut snapshot: QuotaSnapshot,
) -> Result<QuotaSnapshot, &'static str> {
    credential
        .validate(unix_millis())
        .map_err(|_| "credential_invalid")?;
    let proxy = credential.proxy().ok_or("quota_proxy_required")?;
    let parsed = reqwest::Url::parse(proxy.0).map_err(|_| "quota_destination_invalid")?;
    if parsed.scheme() != "socks5h" {
        return Err("quota_proxy_required");
    }
    let http = crate::network::client_for_config_url(
        &state.http,
        USAGE_URL,
        &json!({"network_scope":"public"}),
        Some(proxy),
        false,
    )
    .await
    .map_err(|_| "quota_destination_invalid")?;
    let request = crate::oauth::managed::kimi::apply_headers(
        http.get(USAGE_URL)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(6)),
        credential,
    )
    .map_err(|_| "credential_invalid")?;
    let response = credential
        .apply(request, unix_millis())
        .map_err(|_| "credential_invalid")?
        .send()
        .await
        .map_err(|_| "quota_transport_failed")?;
    let payload = decode_response(response).await?;
    let now = unix_millis();
    snapshot.windows = windows(&payload, now)?;
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(now);
    snapshot.stale_after = Some(now + FRESH_MS);
    Ok(snapshot)
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn label(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| {
            !value.is_empty() && value.len() <= 160 && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
}

fn row(id: String, item: &Value, now: i64) -> Result<QuotaWindow, &'static str> {
    let detail = item.get("detail").unwrap_or(item);
    if !detail.is_object() {
        return Err("quota_invalid_payload");
    }
    let limit = amount(&detail["limit"]);
    let mut used = amount(&detail["used"]);
    let mut remaining = amount(&detail["remaining"]);
    if used.is_none() {
        used = limit
            .zip(remaining)
            .filter(|(limit, remaining)| remaining <= limit)
            .map(|(limit, remaining)| limit - remaining);
    }
    if remaining.is_none() {
        remaining = limit.zip(used).map(|(limit, used)| (limit - used).max(0.0));
    }
    let used_percent = used
        .zip(limit)
        .filter(|(_, limit)| *limit > 0.0)
        .map(|(used, limit)| used / limit * 100.0)
        .filter(|value| value.is_finite());
    let absolute = ["reset_at", "resetAt", "reset_time", "resetTime"]
        .iter()
        .find_map(|key| {
            normalize::timestamp(&detail[*key]).or_else(|| {
                detail[*key]
                    .as_i64()
                    .filter(|value| *value >= 0)
                    .and_then(|value| value.checked_mul(1000))
            })
        });
    let relative = ["reset_in", "resetIn", "ttl"].iter().find_map(|key| {
        detail[*key]
            .as_i64()
            .filter(|value| *value >= 0)
            .and_then(|value| value.checked_mul(1000))
            .and_then(|value| now.checked_add(value))
    });
    let window = item.get("window").unwrap_or(item);
    let unit = window["timeUnit"]
        .as_str()
        .unwrap_or("")
        .to_ascii_uppercase();
    let multiplier = match unit.trim_start_matches("TIME_UNIT_") {
        "SECOND" | "SECONDS" => Some(1),
        "MINUTE" | "MINUTES" => Some(60),
        "HOUR" | "HOURS" => Some(3600),
        "DAY" | "DAYS" => Some(86400),
        "WEEK" | "WEEKS" => Some(604800),
        _ => None,
    };
    let period_seconds = window["duration"]
        .as_i64()
        .filter(|value| *value > 0)
        .zip(multiplier)
        .and_then(|(duration, multiplier)| duration.checked_mul(multiplier));
    Ok(QuotaWindow {
        label: label(&item["name"])
            .or_else(|| label(&detail["name"]))
            .or_else(|| label(&item["title"]))
            .or_else(|| label(&item["scope"]))
            .unwrap_or_else(|| id.clone()),
        id,
        used_percent,
        used,
        remaining,
        limit,
        reset_at: absolute.or(relative),
        period_seconds,
        source: "kimi_usage",
        reset_is_estimated: absolute.is_none() && relative.is_some(),
        allowed: None,
        limit_reached: None,
    })
}

fn windows(payload: &Value, now: i64) -> Result<Vec<QuotaWindow>, &'static str> {
    let mut rows = Vec::new();
    if let Some(limits) = payload.get("limits") {
        let limits = limits.as_array().ok_or("quota_invalid_payload")?;
        if limits.len() > 32 {
            return Err("quota_too_many_windows");
        }
        for (index, item) in limits.iter().enumerate() {
            rows.push(row(format!("limit-{index}"), item, now)?);
        }
    }
    if let Some(usage) = payload.get("usage") {
        rows.push(row("summary".into(), usage, now)?);
    }
    if rows.is_empty() {
        return Err("quota_invalid_payload");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_amounts_do_not_invent_missing_values_or_health() {
        let rows = windows(
            &json!({"limits":[
                {"detail":{"limit":100,"remaining":25,"resetAt":"2026-09-14T00:00:00Z"},
                 "window":{"duration":5,"timeUnit":"TIME_UNIT_HOUR"}},
                {"detail":{"used":0}}, {"detail":{}}
            ]}),
            1000,
        )
        .unwrap();
        assert_eq!(rows[0].used, Some(75.0));
        assert_eq!(rows[0].period_seconds, Some(18000));
        assert!(rows[0].reset_at.is_some());
        assert_eq!(rows[1].used, Some(0.0));
        assert!(rows[1].limit.is_none());
        assert!(rows[2].used.is_none());
        assert!(rows.iter().all(|row| row.allowed.is_none()));
    }

    #[test]
    fn invalid_and_unbounded_payloads_fail_closed() {
        assert!(windows(&json!({"error":"no quota"}), 0).is_err());
        assert!(windows(&json!({"limits":vec![json!({});33]}), 0).is_err());
        assert!(windows(&json!({"usage":null}), 0).is_err());
    }

    #[test]
    fn capabilities_distinguish_native_read_from_unsupported_metadata() {
        let codex = QuotaCapabilities::for_provider("openai-codex");
        let kimi = QuotaCapabilities::for_provider("kimi-oauth");
        assert!(codex.read && codex.reset_credit_expiry && !codex.window_amounts);
        assert!(kimi.read && kimi.window_amounts && !kimi.reset_credit_expiry);
        assert!(!codex.subscription_expiry && !kimi.subscription_expiry);
        assert!(!QuotaCapabilities::for_provider("unknown").read);
        let rows = windows(&json!({"usage":{"used":"NaN","limit":"Infinity"}}), 0).unwrap();
        assert!(rows[0].used.is_none() && rows[0].limit.is_none());
    }
}
