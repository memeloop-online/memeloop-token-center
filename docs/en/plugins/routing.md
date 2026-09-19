# Group routing (`group-routing-v1`)

Group routing is a separate optional ABI: a plugin orders candidate accounts **already authorized** by the host and provides bounded health advice. It creates no authorization and sees no credentials or request body. The WIT definition is the `group-routing-plugin` world in [token-center.wit](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit):

```wit
interface group-routing-v1 {
  plan: func(input-json: string) -> result<string, string>;
  observe: func(input-json: string) -> result<string, string>;
}
```

## How the host uses a strategy

Group routing strategies process only candidates already authorized by the host. A deployment can choose a native or installed strategy for a group and provide a stable priority when multiple groups match; choosing a strategy never grants a new model or account permission.

If strategy code is missing, invalid, or traps, the host falls back to native handling. The request continues to use the core authorization, health, and budget boundaries.

## `plan`: planning

Example input:

```json
{
  "tenant_id": "tenant",
  "seed": 42,
  "remaining_deadline_ms": 1000,
  "config": {},
  "candidates": [
    {
      "tenant_id": "tenant",
      "route_id": "route",
      "account_id": "account",
      "generation": 1,
      "health": "transient"
    }
  ]
}
```

- `health` ∈ `healthy`, `transient`, `hard_quota`, `authentication`.
- `remaining_deadline_ms` is the remaining native core budget, not a renewable plugin timeout; `plan` cannot extend it.
- `seed` is stable within the host's selection scope for reproducible ordering; it is not an authorization token.

Example output:

```json
{
  "candidates": [
    {
      "tenant_id": "tenant",
      "route_id": "route",
      "account_id": "account",
      "generation": 1,
      "allow_transient_probe": true,
      "cooldown_ms": 1000,
      "recovery_wait_ms": 500,
      "recheck_ms": 100,
      "stickiness": false
    }
  ]
}
```

Output contract:

- The result must be an **exact permutation** of the input candidate identities (including tenant and credential generation); it cannot filter, duplicate, inject, or modify identities.
- `allow_transient_probe` may be true only for a `transient` candidate; `hard_quota` and `authentication` states cannot be revived by a plugin.
- `stickiness` puts a candidate into the host's stable ordering layer (session-seed rendezvous); zero cooldown or sticky alone does not grant admission.
- All fields are required and unknown fields are rejected. There may be at most 1024 candidates and 1 MiB of input/output; identifiers are non-empty and ≤128 bytes; delay fields are ≤300000 ms, and `recovery_wait_ms` must also fit within the remaining Deadline.

## `observe`: terminal observation

At request termination, the host calls the component again. The input replaces `candidates` with one `candidate` and an `outcome` (`success`, `transient_failure`, `hard_quota`, `authentication`, `cancelled`). The output is one instruction object shaped like a `plan` candidate, with identity matching the input. Observation cannot wait, probe, or replay; after a long stream, `remaining_deadline_ms` may be 0, in which case it must return `recovery_wait_ms: 0`.

## Native health mode

When the manifest declares `"health_policy": "native"`, plugin ordering still applies, but the host ignores plugin health instructions: admission, cooldown, probes, and recovery all use native paths, and `observe` is not called. This is suitable for strategy packages that only prefer candidates.

## Optional quota context

A signed manifest can opt in with `capabilities: [{ "kind": "group_routing_quota" }]` (which requires `health_policy: "native"`). After opting in, `plan` input gains `quota_context`:

```json
{
  "quota_context": {
    "version": "account-windows-v1",
    "now_ms": 1000,
    "accounts": [
      {
        "account_id": "authorized-account-id",
        "generation": 7,
        "provider": "example-oauth",
        "observed_at": 900,
        "valid_until": 1100,
        "windows": [
          {
            "id": "summary",
            "period_seconds": 604800,
            "reset_at": 2000,
            "reset_is_estimated": false,
            "remaining_fraction": 0.5,
            "exhausted": false
          }
        ]
      }
    ]
  }
}
```

Times are Unix milliseconds. A window's period, reset time, remaining ratio, and exhausted state may each be `null`—unknown stays unknown, and the host does not synthesize “available.” A plugin without this capability receives exactly the previous input, without a `quota_context` field.

## Isolation

Each call has independent fuel and memory and at most 100 ms of execution time. The routing component has no network or KV access, even if other contributions in the same package declare those capabilities. From planning through terminal observation, a request uses the same compiled component and manifest snapshot; upgrading a plugin mid-request does not change in-flight behavior.
