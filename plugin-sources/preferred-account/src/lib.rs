//! Pure ordering of host-authorized identities. No clock, quota guesses, host
//! calls, credentials, retries or health policy. The signed manifest opts into
//! native health handling; directive health fields are ABI placeholders only.
wit_bindgen::generate!({ path: "../../wit/token-center.wit", world: "group-routing-plugin" });

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    preferred_account_ids: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    tenant_id: String,
    route_id: String,
    account_id: String,
    generation: u64,
    health: Health,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Health {
    Healthy,
    Transient,
    HardQuota,
    Authentication,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tenant_id: String,
    seed: u64,
    remaining_deadline_ms: u64,
    config: Configuration,
    candidates: Vec<Candidate>,
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

fn plan(input: &str) -> Result<String, String> {
    if input.len() > 1024 * 1024 {
        return Err("routing input exceeds its bound".into());
    }
    let mut input: Input = serde_json::from_str(input).map_err(|_| "invalid routing input")?;
    let preferences = &input.config.preferred_account_ids;
    if input.candidates.len() > 1024
        || preferences.len() > 128
        || preferences
            .iter()
            .any(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
        || preferences
            .iter()
            .enumerate()
            .any(|(index, id)| preferences[..index].contains(id))
        || input
            .candidates
            .iter()
            .any(|candidate| candidate.tenant_id != input.tenant_id)
        || input.remaining_deadline_ms == 0
    {
        return Err("invalid routing input".into());
    }
    // The host has already seeded native order. Stable sorting retains that
    // order for unconfigured, unhealthy and same-account route candidates.
    let _ = input.seed;
    input.candidates.sort_by_key(|candidate| {
        if candidate.health == Health::Healthy {
            preferences
                .iter()
                .position(|id| id == &candidate.account_id)
                .unwrap_or(usize::MAX)
        } else {
            usize::MAX
        }
    });
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

struct PreferredAccount;

impl exports::memeloop::token_center::group_routing_v1::Guest for PreferredAccount {
    fn plan(input_json: String) -> Result<String, String> {
        plan(&input_json)
    }
    fn observe(_input_json: String) -> Result<String, String> {
        // Native-health manifests are never observed by the scheduler. Refuse
        // direct use rather than inventing a cooldown or recovery directive.
        Err("native health policy has no guest observation".into())
    }
}

// Canonical ABI export names contain ':'/'#' and are not ELF symbols. Native
// tests exercise the same policy without emitting a Wasm export table.
#[cfg(target_arch = "wasm32")]
export!(PreferredAccount);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn preference_is_an_exact_stable_permutation_and_never_promotes_unhealthy_accounts() {
        let candidate = |route, account, health| {
            json!({"tenant_id":"tenant", "route_id":route,
            "account_id":account, "generation":7, "health":health})
        };
        let original = vec![
            candidate("r0", "other", "healthy"),
            candidate("r1", "preferred", "healthy"),
            candidate("r2", "preferred", "healthy"),
            candidate("r3", "blocked", "hard_quota"),
        ];
        let request = |preferences| {
            json!({"tenant_id":"tenant","seed":42,"remaining_deadline_ms":1000,
            "config":{"preferred_account_ids":preferences},"candidates":original})
        };
        let output = |input: Value| -> Value {
            serde_json::from_str(&plan(&input.to_string()).unwrap()).unwrap()
        };
        let result = output(request(json!(["absent", "blocked", "preferred"])));
        assert_eq!(
            result["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["route_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["r1", "r2", "r0", "r3"]
        );
        for directive in result["candidates"].as_array().unwrap() {
            let source = original
                .iter()
                .find(|v| v["route_id"] == directive["route_id"])
                .unwrap();
            for field in ["tenant_id", "route_id", "account_id", "generation"] {
                assert_eq!(directive[field], source[field]);
            }
            assert_eq!(directive["stickiness"], false);
            assert_eq!(directive["allow_transient_probe"], false);
        }
        let defaults = output(request(json!([])));
        assert_eq!(
            defaults["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| &v["route_id"])
                .collect::<Vec<_>>(),
            original.iter().map(|v| &v["route_id"]).collect::<Vec<_>>()
        );
        assert!(plan(&request(json!(["preferred", "preferred"])).to_string()).is_err());
    }
}
