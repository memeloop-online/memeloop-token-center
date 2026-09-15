# Soonest Reset

An independent, opt-in first-party group-routing guest. It does not modify the
published Preferred Account plugin. It ranks only already-authorized healthy
candidates using the host's read-only, credential-free `account-windows-v1`
quota projection. It cannot fetch quota, refresh credentials, reset a supplier
window, add a cooldown, delete a candidate or bypass native health decisions.

The default configuration disables preference and retains native order:

```json
{"target_provider":"", "target_window_id":""}
```

To target the native Kimi quota adapter's explicit weekly usage window:

```json
{"target_provider":"kimi-oauth", "target_window_id":"summary"}
```

Both values are exact, case-sensitive IDs, not display labels. No supplier window
is guessed from its duration or remaining time. A candidate is eligible only if
its account and credential generation match the host observation, the observation
is fresh at host-provided time, the target window has a strictly future,
non-estimated reset instant and explicitly positive remaining fraction, and no
observed window explicitly reports exhaustion or zero remaining fraction.
Other providers, missing windows, unknown amounts and stale/expired observations
cannot establish eligibility.

Eligible candidates are stably sorted by earliest reset instant **only within
their existing positions**. Unknown, exhausted and unhealthy candidates retain
their exact native positions, not merely relative order. Ties and multiple routes
for the same account preserve input order. Every authorized candidate is returned
exactly once with its original tenant, route, account and credential generation.
This is a partial preference, not a guarantee that an eligible account always
precedes an unknown candidate. Setting either target value empty disables it.

The manifest requests only `group_routing_quota` and sets `health_policy: native`.
The host preserves authentication/quota/transient recovery, exclusive probe
leases, deadlines and failover safety. Guest health directive values are unused
ABI placeholders. `stickiness: false` prevents sticky preference from replacing
the explicit order. The guest rejects observe calls; it never invents health
or retry state. Missing/incomplete host quota context must fall back to native
scheduling, never trigger a supplier request on the request path.

## Build and acceptance

The independent locked crate emits `mtc_soonest_reset.wasm`; CI wraps it using the
existing pinned `wasm-tools` into `plugin.wasm`. Native algorithm tests and the
real Wasm host fixture share `tests/fixtures/soonest-reset-v1.json`, including
multiple routes for one account, ties, unknown and exhausted windows. Additional
cases exercise expiry, estimates, bad data, non-target windows and native health.
The host fixture requires `MTC_SOONEST_RESET_PACKAGE` and runs as
`cargo test --all-features --test first_party_soonest_reset -- --ignored` in CI.
No supplier account or paid model request is needed for these checks.

Installation requires a runtime/installer supporting `group_routing_quota` and
the shared quota observation path. Do not reuse an older reviewed installer pin
that predates this capability. Source acceptance alone is not signed publication
or production installation: use the reviewed fixed-package keyless publication
and real OCI-install verification before the normal group configuration workflow.
This source does not automatically install the package or bind any group.
