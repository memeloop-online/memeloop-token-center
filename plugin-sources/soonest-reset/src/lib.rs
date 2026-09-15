//! Read-only stable ordering over host-authorized candidate identities and a
//! bounded host-clock quota projection. No network, credentials or health writes.
wit_bindgen::generate!({ path: "../../wit/token-center.wit", world: "group-routing-plugin" });

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    target_provider: String,
    target_window_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    tenant_id: String,
    route_id: String,
    account_id: String,
    generation: u64,
    health: Health,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Health {
    Healthy,
    Transient,
    HardQuota,
    Authentication,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Window {
    id: String,
    period_seconds: Option<i64>,
    reset_at: Option<i64>,
    reset_is_estimated: bool,
    remaining_fraction: Option<f64>,
    exhausted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    account_id: String,
    generation: u64,
    provider: String,
    observed_at: i64,
    valid_until: i64,
    windows: Vec<Window>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuotaContext {
    version: String,
    now_ms: i64,
    accounts: Vec<Account>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tenant_id: String,
    seed: u64,
    remaining_deadline_ms: u64,
    config: Configuration,
    candidates: Vec<Candidate>,
    quota_context: QuotaContext,
}

#[derive(Serialize)]
struct Directive {
    tenant_id: String,
    route_id: String,
    account_id: String,
    generation: u64,
    allow_transient_probe: bool,
    cooldown_ms: u64,
    recovery_wait_ms: u64,
    recheck_ms: u64,
    stickiness: bool,
}

#[derive(Serialize)]
struct Plan {
    candidates: Vec<Directive>,
}

fn bounded(value: &str, max: usize) -> bool {
    value.len() <= max && !value.chars().any(char::is_control)
}

fn rank(candidate: &Candidate, input: &Input) -> Option<i64> {
    if candidate.health != Health::Healthy
        || input.config.target_provider.is_empty()
        || input.config.target_window_id.is_empty()
    {
        return None;
    }
    let context = &input.quota_context;
    let account = context.accounts.iter().find(|account| {
        account.account_id == candidate.account_id && account.generation == candidate.generation
    })?;
    if account.provider != input.config.target_provider
        || account.observed_at < 0
        || account.observed_at > context.now_ms
        || account.valid_until <= context.now_ms
        || account.windows.iter().any(|window| {
            window.exhausted == Some(true)
                || window
                    .remaining_fraction
                    .is_some_and(|fraction| fraction <= 0.0)
        })
    {
        return None;
    }
    let window = account
        .windows
        .iter()
        .find(|window| window.id == input.config.target_window_id)?;
    // Cadence is informational only. Never infer a weekly window from elapsed
    // time, a label or period; configuration chooses the exact provider/window.
    let _ = window.period_seconds;
    if window.reset_is_estimated
        || !window
            .remaining_fraction
            .is_some_and(|fraction| fraction > 0.0)
    {
        return None;
    }
    window.reset_at.filter(|reset| *reset > context.now_ms)
}

fn plan(input: &str) -> Result<String, String> {
    if input.len() > 1024 * 1024 {
        return Err("routing input exceeds its bound".into());
    }
    let mut input: Input = serde_json::from_str(input).map_err(|_| "invalid routing input")?;
    let context = &input.quota_context;
    if input.candidates.len() > 1024
        || context.accounts.len() > 1024
        || context.version != "account-windows-v1"
        || context.now_ms < 0
        || input.remaining_deadline_ms == 0
        || !bounded(&input.config.target_provider, 128)
        || !bounded(&input.config.target_window_id, 512)
        || input
            .candidates
            .iter()
            .any(|candidate| candidate.tenant_id != input.tenant_id)
        || context.accounts.iter().enumerate().any(|(index, account)| {
            account.windows.len() > 64
                || context.accounts[..index].iter().any(|other| {
                    other.account_id == account.account_id && other.generation == account.generation
                })
                || account.windows.iter().enumerate().any(|(index, window)| {
                    !bounded(&window.id, 512)
                        || window.id.is_empty()
                        || account.windows[..index]
                            .iter()
                            .any(|other| other.id == window.id)
                        || window.remaining_fraction.is_some_and(|fraction| {
                            !fraction.is_finite() || !(0.0..=1.0).contains(&fraction)
                        })
                })
        })
    {
        return Err("invalid routing input".into());
    }
    // The host already seeded native order. Only eligible slots are reordered;
    // unknown/stale/exhausted/unhealthy candidates keep their exact positions.
    // Stable ties retain native order and no candidate is removed or invented.
    let _ = input.seed;
    let mut eligible: Vec<_> = input
        .candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            rank(candidate, &input).map(|reset| (index, reset, candidate.clone()))
        })
        .collect();
    let slots: Vec<_> = eligible.iter().map(|(index, _, _)| *index).collect();
    eligible.sort_by_key(|(_, reset, _)| *reset);
    for (slot, (_, _, candidate)) in slots.into_iter().zip(eligible) {
        input.candidates[slot] = candidate;
    }
    let candidates = input
        .candidates
        .into_iter()
        .map(|candidate| Directive {
            tenant_id: candidate.tenant_id,
            route_id: candidate.route_id,
            account_id: candidate.account_id,
            generation: candidate.generation,
            allow_transient_probe: false,
            cooldown_ms: 0,
            recovery_wait_ms: 0,
            recheck_ms: 0,
            stickiness: false,
        })
        .collect();
    serde_json::to_string(&Plan { candidates }).map_err(|_| "invalid routing output".into())
}

struct SoonestReset;
impl exports::memeloop::token_center::group_routing_v1::Guest for SoonestReset {
    fn plan(input_json: String) -> Result<String, String> {
        plan(&input_json)
    }
    fn observe(_input_json: String) -> Result<String, String> {
        Err("native health policy has no guest observation".into())
    }
}
#[cfg(target_arch = "wasm32")]
export!(SoonestReset);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/soonest-reset-v1.json"
        ))
        .unwrap()
    }
    fn output(input: &Value) -> Value {
        serde_json::from_str(&plan(&input.to_string()).unwrap()).unwrap()
    }
    fn ids(value: &Value) -> Vec<Value> {
        value["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["route_id"].clone())
            .collect()
    }

    #[test]
    fn exact_fresh_window_sort_is_stable_and_preserves_all_identities_and_unknown_slots() {
        let fixture = fixture();
        let input = &fixture["input"];
        let result = output(input);
        assert_eq!(json!(ids(&result)), fixture["expected_route_ids"]);
        for candidate in result["candidates"].as_array().unwrap() {
            let original = input["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["route_id"] == candidate["route_id"])
                .unwrap();
            for key in ["tenant_id", "route_id", "account_id", "generation"] {
                assert_eq!(candidate[key], original[key]);
            }
            assert_eq!(candidate["stickiness"], false);
            assert_eq!(candidate["allow_transient_probe"], false);
            for field in ["cooldown_ms", "recovery_wait_ms", "recheck_ms"] {
                assert_eq!(candidate[field], 0);
            }
        }
    }

    #[test]
    fn default_or_wrong_provider_window_never_guesses_cadence() {
        for config in [
            json!({"target_provider":"","target_window_id":""}),
            json!({"target_provider":"unknown","target_window_id":"summary"}),
            json!({"target_provider":"kimi-oauth","target_window_id":"weekly"}),
        ] {
            let mut input = fixture()["input"].clone();
            input["config"] = config;
            assert_eq!(ids(&output(&input)), ids(&input));
        }
    }

    #[test]
    fn unusable_evidence_and_native_health_keep_candidate_in_place() {
        for (field, value) in [
            ("reset_at", json!(1000000)),
            ("reset_at", Value::Null),
            ("remaining_fraction", Value::Null),
            ("remaining_fraction", json!(0)),
            ("reset_is_estimated", json!(true)),
            ("exhausted", json!(true)),
        ] {
            let mut input = fixture()["input"].clone();
            input["quota_context"]["accounts"][2]["windows"][0][field] = value;
            assert_eq!(output(&input)["candidates"][2]["route_id"], "early");
        }
        for (field, value) in [
            ("valid_until", json!(1000000)),
            ("observed_at", json!(1000001)),
            ("generation", json!(8)),
        ] {
            let mut input = fixture()["input"].clone();
            input["quota_context"]["accounts"][2][field] = value;
            assert_eq!(output(&input)["candidates"][2]["route_id"], "early");
        }
        for health in ["transient", "hard_quota", "authentication"] {
            let mut input = fixture()["input"].clone();
            input["candidates"][2]["health"] = json!(health);
            assert_eq!(output(&input)["candidates"][2]["route_id"], "early");
        }
        let mut input = fixture()["input"].clone();
        input["quota_context"]["accounts"][2]["windows"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "id":"short", "period_seconds":18000, "reset_at":1010000,
                "reset_is_estimated":false, "remaining_fraction":0, "exhausted":true
            }));
        assert_eq!(output(&input)["candidates"][2]["route_id"], "early");
    }

    #[test]
    fn ambiguous_or_out_of_bounds_input_fails_closed() {
        let mut input = fixture()["input"].clone();
        let duplicate = input["quota_context"]["accounts"][0].clone();
        input["quota_context"]["accounts"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(plan(&input.to_string()).is_err());
        for fraction in [json!(-0.1), json!(1.1), json!("NaN")] {
            let mut input = fixture()["input"].clone();
            input["quota_context"]["accounts"][0]["windows"][0]["remaining_fraction"] = fraction;
            assert!(plan(&input.to_string()).is_err());
        }
        let mut input = fixture()["input"].clone();
        input["quota_context"]["version"] = json!("future-contract");
        assert!(plan(&input.to_string()).is_err());
        input = fixture()["input"].clone();
        input["candidates"][0]["tenant_id"] = json!("foreign");
        assert!(plan(&input.to_string()).is_err());
        input = fixture()["input"].clone();
        input["quota_context"]["accounts"][0]["windows"] = json!(vec![json!({}); 65]);
        assert!(plan(&input.to_string()).is_err());
    }
}
