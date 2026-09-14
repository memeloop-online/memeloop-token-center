# Preferred Account

An opt-in, credential-free `group-routing-v1` plugin for explicitly preferring
already-authorized accounts. It is **not** an automatic quota/reset scheduler.
It has no access to quota observations, reset times, a clock, network or KV.

Configure the installed strategy on the existing provider or route group:

```json
{"preferred_account_ids": ["<preferred-account-id>"]}
```

The array is ordered, unique and bounded to 128 accounts. Its default is empty,
preserving the incoming native order. IDs outside the actual candidate set do
nothing; configuration never grants membership, a route or tenant access.
Only healthy preferred candidates move forward. All other candidates and
same-account route ties retain their input order. Every candidate identity and
credential generation remains present exactly once. `stickiness: false` keeps
the host from replacing the explicit preference with its session-seeded sticky
tier. Within a group the plugin does not promise native weighting across
preferred accounts; the configured priority is intentional.

The signed manifest sets `health_policy: "native"`. Health fields required by
the v1 wire protocol are placeholders, not zero-cooldown overrides. The host
does not create guest health policies or call observe, so authentication,
hard quota, transient recovery, exclusive probe leases, original deadlines and
safe failover all remain native. An exhausted preferred account is skipped for
an eligible remaining candidate. A received/uncertain request is never replayed
merely to satisfy this preference. There is no hard-coded extra cooldown.

Use one existing Kimi provider group with both members and the existing routes;
do not duplicate routes or mutate SQL weights. After compatible signed
installation, select this strategy and its explicit account preference through
the normal version-fenced group UI/API. No accounts are automatically bound by
installation. Remove the preference or select native to stop preferring it.

## Build and acceptance

The independent locked Rust crate builds `mtc_preferred_account.wasm` for
`wasm32-unknown-unknown`. Encode it with pinned `wasm-tools component new` into
the package's `plugin.wasm`; the source binds the repository's
`group-routing-plugin` WIT world. CI builds this actual guest, exercises stable
identity/order tests, runs `first_party_preferred_account`, then the real
mock-upstream gateway fixture `preferred_account_real_gateway`. The latter
checks final selection, native-health policy absence, durable restoration and
known-exhausted fallback. No production request is needed for acceptance.

Publication must reuse the fixed first-party package allowlist, exact reviewed
installer image, GitHub OIDC signature and real OCI installer acceptance. The
older Model Guard installer pin does not support this manifest addition;
obtain a newly reviewed compatible image before publishing/installing this
package. This source PR does not publish, install or change a group.

## Separate follow-up: earliest-reset preference

Automatic scheduling needs a version-negotiated, bounded host projection of
tenant/account/generation-bound quota observations, host time, freshness and
the chosen weekly reset/remaining evidence. Current quota reads are an on-demand
Control-process cache, not a cross-Pod data-plane feed. Build a shared read-only
observation path outside request scheduling; do not fetch suppliers per request
or give the guest credentials/network. Ignore expired/unknown/stale evidence,
preserve exact candidate and final-health boundaries, and fall back to native
order. This is explicitly not implemented by Preferred Account.
