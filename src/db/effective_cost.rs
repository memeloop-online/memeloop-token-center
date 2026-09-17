//! Shared request-cost presentation semantics.
//!
//! Gross request cost remains immutable for settlement and audit. User-facing
//! request and session projections use the effective cost: a terminal failed
//! request is displayed as zero unless the provider explicitly reported usage.
//! Terminal failure evidence includes a non-empty error code even when the HTTP
//! status is 2xx. Successful requests, provider-reported usage, and pending
//! requests retain their stored amount.

pub(crate) fn effective_displayed_cost_micros(
    cost_micros: i64,
    status_code: Option<i64>,
    error_code: Option<&str>,
    usage_basis: Option<&str>,
) -> i64 {
    let terminal_failure = status_code.is_some_and(|code| {
        !(200..400).contains(&code) || error_code.is_some_and(|value| !value.is_empty())
    });
    if terminal_failure && usage_basis != Some("provider_reported") {
        0
    } else {
        cost_micros
    }
}

#[cfg(test)]
mod tests {
    use super::effective_displayed_cost_micros;

    #[test]
    fn terminal_failures_require_provider_reported_usage_to_keep_cost() {
        for status_code in [499, 502, 503] {
            for usage_basis in [
                None,
                Some("provider_estimated"),
                Some("contract_ceiling"),
                Some("not_observed"),
            ] {
                assert_eq!(
                    effective_displayed_cost_micros(594, Some(status_code), None, usage_basis),
                    0,
                    "status {status_code} with {usage_basis:?}",
                );
            }
        }
        assert_eq!(
            effective_displayed_cost_micros(594, None, None, Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(200), None, Some("not_observed")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), None, Some("provider_reported")),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(503), None, None),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(
                594,
                Some(200),
                Some("client_cancelled"),
                Some("contract_ceiling"),
            ),
            0
        );
        assert_eq!(
            effective_displayed_cost_micros(
                594,
                Some(200),
                Some("client_cancelled"),
                Some("provider_reported"),
            ),
            594
        );
        assert_eq!(
            effective_displayed_cost_micros(594, Some(200), Some(""), Some("not_observed")),
            594
        );
    }
}
