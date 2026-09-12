# Request routing policy

The gateway resolves a bounded set of tenant- and credential-authorized routes
before transport preparation. An account hint orders that set; it cannot add an
account, override a grant, or force an unavailable account. Health admission and
credential revalidation still apply to every candidate, including deferred
half-open recovery attempts. External quota reset does not reset gateway health.

Native Codex account `config.transport_policy` uses one typed, fail-closed parser
for validation and execution. Unsupported fields, versions and out-of-range
values reject configuration. Missing `version` means version 1 for compatibility.
Changes use the existing authorized `PUT /internal/v1/upstreams/{account_id}`
account-update CAS; the provider catalog schema declares these fields for both
API validation and the schema-driven editor. Read them back through the authorized
upstream list API. Accepted changes emit a secret-free structured audit event
with actor/account/tenant IDs, expected/accepted revisions and bounded policy
values. This is a log event, not a new transactional audit table.

| Version 1 field | Default | Allowed |
| --- | --- | --- |
| `connect_attempts` | 2 | 1–4 |
| `connect_retry_delay_millis` | 150 | 0–2000 |
| `shared_probe_attempts` | Service health setting | 0–4 |
| `candidate_attempts` | 3 | 1–8 |
| `failover_deadline_millis` | 300000 | 1000–300000 |

Candidate count and deadline are snapshotted from the first prepared account,
before request reservation/archive work. Standbys and live configuration changes
cannot replenish that request's budget. No new send starts after deadline expiry;
an in-flight send is canceled as ambiguous and never replayed. Database admission
and terminal settlement retain their own lifecycle bounds. The failover deadline
does not truncate a successfully admitted response stream. Non-Codex primary
routes retain their existing three-attempt and transport timeout behavior.

Health and replay decisions are separate. HTTP 429 rejection and definite
connection failure before delivery can advance to another authorized candidate.
HTTP 503 after dispatch cools the account but never proves non-delivery; neither
other 5xx responses, output-framing failures, visible output, nor ambiguous send
timeouts permit replay. Known same-account HTTP 400 recovery remains governed by
its separate bounded delivery contract.

Decision logs include request/route/account correlation, candidate and attempt
position, policy version and a fixed disposition code. They contain no raw
supplier body or transport credential. Existing admission and health events
provide the cooldown, preparation and quota portions of the reason chain.

This is a data-only policy boundary, not an executable plugin interface. Plugin
names, arbitrary hooks and retry-status overrides are rejected. No plugin may
add candidates or relax delivery evidence through this configuration. A future
plugin integration needs its own versioned, allowlisted and audited typed
contract restricted to already-authorized handles; it is not enabled here.
