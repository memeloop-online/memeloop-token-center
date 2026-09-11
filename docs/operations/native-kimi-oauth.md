# Native Kimi OAuth import

The builtin `kimi-oauth` driver accepts CPA managed imports with `source_type:
kimi`. The existing global `imports:cpa:write` endpoint, explicit tenant,
HMAC source provenance, immutable replay rules and encrypted credential storage
remain mandatory. This is not an API-key conversion or a bridge connection.

The reviewed source contract is
[CLIProxyAPI v7.2.128-onetwo.1](https://github.com/linonetwo/CLIProxyAPI/tree/v7.2.128-onetwo.1/internal/auth/kimi).
Imports retain access and refresh tokens, optional expiry, device ID, OAuth scope,
token type and disabled state. Unknown fields fail closed. Optional private SOCKS
proxies retain their original URL and require the existing global-operator
outbound authorization; they are never copied into public account configuration.
Import normalization is entirely local and performs no DNS or provider call.
The two-account migration uses the advertised `atomic_kimi_cohort_v1` endpoint,
which holds one tenant-scoped lock and database transaction across both stable
source identities. Account names contain only a server-keyed neutral source
suffix. The atomic cohort rejects an expired/disabled new credential before
writing while preserving time-independent exact replay; the compatible
single-import path creates expired credentials disabled and they never enter
the automatic refresh candidate set.

Refresh uses only `https://auth.kimi.com/api/oauth/token`, the fixed public client
ID and account-specific device headers. Response size and time are bounded.
Omitted or empty replacement refresh tokens retain the existing refresh token.
Omitted scope/type/expiry retain existing values. Credential rotation and
concurrent refresh arbitration use existing database leases and generation CAS.
Provider response bodies and tokens are not included in errors.

The native coding base is fixed to `https://api.kimi.com/coding`. Transport,
model aliases, trusted catalog limits and both source OpenAI/Anthropic routing
must be verified before enabling imported routes. The coarse provider protocol
metadata is not evidence of Responses, embeddings or generation support.
Import itself neither discovers account entitlements nor invents prices or
reservation bounds. No device login, source credential mutation or automatic
route creation is performed by this adapter.
