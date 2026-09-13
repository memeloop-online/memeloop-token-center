# Native upstream quota projection

The read endpoint is on demand, not a worker poll. Cache freshness is 30 seconds,
failed reads are backed off 10 seconds, stale evidence expires after 5 minutes.
At most four accounts refresh concurrently, one refresh per account generation,
128 cache entries, 8 seconds per refresh, and 1 MiB per response. Cache identity
includes tenant UUID, authorized canonical external tenant ID, account ID,
credential generation and account update time; renames cannot reuse stale labels.
Reads do not
refresh OAuth credentials, synchronize models, or execute quota reset workflows.
Existing Codex conclusive quota evidence may clear an exhausted-account cooldown;
it is not a supplier mutation or a generic assertion that the account is healthy.

## Supported fields

| Native driver | Read source | Amounts | Reset windows | Credit expiry | Subscription expiry |
| --- | --- | --- | --- | --- | --- |
| openai-codex | WHAM usage and reset-credit GET endpoints | Percent used only; absolute amounts null | Supplier instant or marked estimate | Per Codex credit status/granted/expiry, with no identifier or secret | Null, capability false |
| kimi-oauth | Kimi coding v1 usages GET | Total/used/remaining when supplied or safely derived | Supplier instant or marked relative estimate | Unsupported | Unsupported |
| Other drivers | No network call | Unsupported | Unsupported | Unsupported | Unsupported |

Capabilities describe implemented fields, not current sample availability. Missing
values remain null. `freshness` is `unobserved`, `fresh`, or `stale`; clients must
also compare `stale_after` with current time. `ready` means a parsed observation,
not routing health. Kimi never supplies inferred `allowed` or `limit_reached`.
Missing or unrecognized Kimi duration units remain unknown. All timestamps are
milliseconds. Reset-credit rows intentionally exclude supplier credit IDs.

Both adapters require the account's private IP-literal socks5h transport. Both
use dedicated remote-DNS-only clients; supplier names are resolved by the proxy,
not the application. Kimi transport construction uses synchronous validation,
disables retries/redirects/environment proxy inheritance, and performs no DNS.
No direct-provider fallback is available. No proxy address is serialized.

Kimi absolute numeric reset timestamps below 100 billion are epoch seconds;
larger values are epoch milliseconds. Relative durations always use seconds.
Integer JSON numbers and integer strings use identical parsing. Window duration
and units fall back in order from window metadata to item metadata to detail.

## Reference and differences

Reference: [CPA management UI](https://github.com/router-for-me/Cli-Proxy-API-Management-Center),
revision `f4b304365142bc9dd151e79539a409b9a05bfa70`, specifically
`src/features/quota/providers/{codex,kimi}/data.ts` and
`src/utils/quota/{constants,builders,resolvers}.ts`.

This adapter independently maps the same supplier fields; it does not depend on
CPA, generic management api-call, or CPA naming. Unlike the reference Kimi builder,
missing used/total do not become zero and missing units do not become minutes.
Codex absolute amounts cannot be reconstructed from percentages. CPA subscription
expiry comes from stored auth-file metadata or ID-token claims, not WHAM usage;
the native credential schema does not currently preserve that verified metadata.
OAuth access-token expiry must not be substituted for subscription expiry.
Implementing that requires a separately reviewed native metadata/import contract.

Reset controls retain the existing explicit prepare/confirmation contract. This
read adapter never invokes reset, prepare, reconcile, model traffic, or catalog
sync. Do not validate the feature by consuming a real reset credit.

Validation in this change: formatting/static diff checks only locally. Focused
normalizer and transport mock tests are for CI; no local Rust build was run.
