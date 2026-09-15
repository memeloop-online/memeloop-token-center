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

| Native driver | Read source | Plan/workspace | Amounts and units | Reset windows | Credit expiry | Subscription expiry |
| --- | --- | --- | --- | --- | --- | --- |
| openai-codex | WHAM usage and reset-credit GET endpoints | Supplier plan; workspace null | Percent used only; absolute amounts and unit null | Supplier instant or marked estimate | Per Codex credit status/granted/expiry, source `codex_reset_credits`, with no identifier or secret | Null, capability false |
| kimi-oauth | Kimi coding v1 usages GET | Null; capability false | Total/used/remaining when supplied or safely derived; unit null because the supplier field has no verified unit contract | Supplier instant or marked relative estimate | Unsupported | Unsupported |
| google-antigravity | `retrieveUserQuotaSummary` POST, read-only project query | Null; capability false | Percent used from supplier remaining fraction; absolute amounts and unit null | Exact supplier RFC3339 instant; cadence only from explicit window metadata | Unsupported | Unsupported |
| Other drivers | No network call | Unsupported | Unsupported | Unsupported | Unsupported | Unsupported |

Capabilities describe implemented fields, not current sample availability. Missing
values remain null. `freshness` is `unobserved`, `fresh`, or `stale`; clients must
also compare `stale_after` with current time. `ready` means a parsed observation,
not routing health. Kimi never supplies inferred `allowed` or `limit_reached`.
Missing or unrecognized Kimi duration units remain unknown. All timestamps are
milliseconds. Reset-credit rows intentionally exclude supplier credit IDs.
Machine-readable capabilities distinguish implemented fields from missing sample
values and state that quota reads are supplier-read-only, do not refresh OAuth
credentials, and do not consume reset credits. The separate reset capability is
derived from the server-held driver contract; fresh credit evidence remains a
requirement before prepare or confirmation can dispatch anything.

Codex quota uses the same mandatory private IP-literal `socks5h` boundary as
native Codex traffic; supplier DNS is delegated to that proxy, with no direct
fallback. Kimi quota instead reuses normal native Kimi network policy: direct
traffic uses validated pinned public DNS, while an account proxy is validated
and applied through the shared configuration transport. Quota inspection adds
no Kimi-only proxy requirement. Environment proxy inheritance remains disabled,
redirects are rejected, and no proxy address is serialized.

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

### Antigravity quota contract

Reference: the official CPA management panel's
[endpoint constants](https://github.com/router-for-me/Cli-Proxy-API-Management-Center/blob/c12997e1a544374336ea385d5e9a9bbabe1e4767/src/utils/quota/constants.ts)
and [group/bucket parser](https://github.com/router-for-me/Cli-Proxy-API-Management-Center/blob/c12997e1a544374336ea385d5e9a9bbabe1e4767/src/utils/quota/builders.ts),
which passes remaining fractions to the [fraction normalizer](https://github.com/router-for-me/Cli-Proxy-API-Management-Center/blob/c12997e1a544374336ea385d5e9a9bbabe1e4767/src/utils/quota/parsers.ts).
The adapter sends one POST to the account's configured `base_url` plus
`/v1internal:retrieveUserQuotaSummary`, with only `{"project": project_id}` as
the JSON body. It uses the existing OAuth credential, validated native request
headers, and account proxy policy. No alternative origin is probed, no client
identity is fabricated, and no token refresh, project discovery/onboarding,
catalog synchronization, model request, or quota-reset operation occurs.
Missing project configuration returns `quota_project_required`; an expired
credential returns `credential_invalid` without refreshing it.

The `groups[].buckets[]` projection supports camelCase and snake_case fields.
Remaining fraction becomes percent used, never an invented token count.
Numeric fractions and numeric strings must be finite and within 0–1; explicit
percentage strings such as `25%` are divided by 100 and bounded to 0–100%.
Only explicit `5h`/`five-hour`/`five_hour` or `weekly`/`week` metadata establishes
a window period. A reset timestamp does not establish cadence; missing or
unrecognized dates and periods remain null. Group/bucket identifiers are scoped
and bounded, duplicate identifiers and malformed fractions fail closed.
When supplier group labels or bucket identifiers/window metadata are absent,
ordinal fallback IDs depend on response ordering; anonymous rows are not promised
stable identity across reordering.
This provider never supplies inferred routing health, subscription expiry or
reset credits. Quota reading is available; reset implementation and preparation
remain unavailable. Supplier-side reset support itself is unknown, not asserted
from local cooldown clearing behavior. All observations share the existing
tenant/generation-scoped cache and bounded stale fallback.

Reset controls retain the existing explicit prepare/confirmation contract. This
read adapter never invokes reset, prepare, reconcile, model traffic, or catalog
sync. Do not validate the feature by consuming a real reset credit.

Validation in this change: formatting/static diff checks only locally. Focused
normalizer and transport mock tests are for CI; no local Rust build was run.
