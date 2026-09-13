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
    // Match normal Kimi model traffic and OAuth refresh: the fixed public
    // destination may use direct pinned DNS or the account proxy policy.
    let http = crate::network::client_for_config_url(
        &state.http,
        USAGE_URL,
        &json!({"network_scope":"public"}),
        credential.proxy(),
        false,
    )
    .await
    .map_err(|_| "quota_destination_invalid")?;
    let payload = get_usage(&http, credential, USAGE_URL).await?;
    let now = unix_millis();
    snapshot.windows = windows(&payload, now)?;
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(now);
    snapshot.stale_after = Some(now + FRESH_MS);
    Ok(snapshot)
}

// Production caller supplies only USAGE_URL. URL injection remains private to
// this module so contract tests can use a local mock without a supplier probe.
async fn get_usage(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    url: &str,
) -> Result<Value, &'static str> {
    let request = crate::oauth::managed::kimi::apply_headers(
        http.get(url)
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
    decode_response(response).await
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse::<i64>().ok())
        .filter(|value| *value >= 0)
}

fn absolute_millis(value: &Value) -> Option<i64> {
    normalize::timestamp(value).or_else(|| {
        let value = integer(value)?;
        // Contemporary epoch milliseconds are >= 10^12; seconds remain < 10^11.
        if value >= 100_000_000_000 {
            Some(value)
        } else {
            value.checked_mul(1000)
        }
    })
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
        .find_map(|key| absolute_millis(&detail[*key]));
    let relative = ["reset_in", "resetIn", "ttl"].iter().find_map(|key| {
        integer(&detail[*key])
            .and_then(|value| value.checked_mul(1000))
            .and_then(|value| now.checked_add(value))
    });
    let window = &item["window"];
    let metadata = [window, item, detail];
    let unit = metadata
        .iter()
        .find_map(|value| value["timeUnit"].as_str())
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
    let period_seconds = metadata
        .iter()
        .find_map(|value| integer(&value["duration"]))
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
        unit: None,
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

    #[tokio::test]
    async fn native_usage_mock_is_one_get_and_sanitizes_supplier_errors() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };
        let server = MockServer::start().await;
        let credential = crate::oauth::managed::kimi::credential_from_native_import(&json!({
            "type":"kimi", "access_token":"fixture-access", "refresh_token":"fixture-refresh",
            "token_type":"bearer", "device_id":"fixture-device"
        }))
        .unwrap();
        Mock::given(method("GET"))
            .and(path("/coding/v1/usages"))
            .and(header("authorization", "Bearer fixture-access"))
            .and(header("x-msh-device-id", "fixture-device"))
            .respond_with(ResponseTemplate::new(429).set_body_string("supplier-secret"))
            .expect(1)
            .mount(&server)
            .await;
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        assert_eq!(
            get_usage(
                &http,
                &credential,
                &format!("{}/coding/v1/usages", server.uri())
            )
            .await
            .unwrap_err(),
            "quota_rate_limited"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method.as_str(), "GET");
    }

    #[test]
    fn timestamps_and_window_metadata_preserve_exact_milliseconds() {
        for reset in [
            json!(1_800_000_123_i64),
            json!("1800000123"),
            json!(1_800_000_123_000_i64),
            json!("1800000123000"),
        ] {
            let rows = windows(
                &json!({"limits":[{"window":{}, "duration":"5",
                "detail":{"limit":10,"used":3,"resetAt":reset,"timeUnit":"TIME_UNIT_HOUR"}}]}),
                1000,
            )
            .unwrap();
            assert_eq!(rows[0].reset_at, Some(1_800_000_123_000));
            assert_eq!(rows[0].period_seconds, Some(18000));
            assert!(!rows[0].reset_is_estimated);
        }
        for seconds in [json!(123), json!("123")] {
            let rows = windows(&json!({"usage":{"resetIn":seconds,"used":0}}), 1001).unwrap();
            assert_eq!(rows[0].reset_at, Some(124001));
            assert!(rows[0].reset_is_estimated);
        }
        let rows = windows(
            &json!({"limits":[{"window":{"duration":2,"timeUnit":"HOUR"},
            "duration":9,"timeUnit":"DAY","detail":{"duration":10,"timeUnit":"WEEK"}}]}),
            0,
        )
        .unwrap();
        assert_eq!(rows[0].period_seconds, Some(7200));
    }

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
        assert!(codex.plan && !kimi.plan);
        assert!(!codex.workspace && !kimi.workspace);
        assert!(!codex.window_amount_unit && !kimi.window_amount_unit);
        assert!(codex.supplier_read_only && kimi.supplier_read_only);
        assert!(!codex.refreshes_credentials && !kimi.refreshes_credentials);
        assert!(!codex.consumes_reset_credit && !kimi.consumes_reset_credit);
        assert!(!QuotaCapabilities::for_provider("unknown").read);
        let rows = windows(&json!({"usage":{"used":"NaN","limit":"Infinity"}}), 0).unwrap();
        assert!(rows[0].used.is_none() && rows[0].limit.is_none());
        assert!(rows[0].unit.is_none());
    }
}
