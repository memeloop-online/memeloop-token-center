# Sticky Codex transport proxy groups

This backend slice supports native Codex generation, native Codex OAuth refresh,
and native Codex catalog reads. Other providers and accounts without a configured
group retain their existing single-proxy behavior. It does not deploy proxies or
change production accounts.

Set `MTC_TRANSPORT_PROXY_GROUPS` through private server configuration, identically
on gateway, control, and worker processes. Its default is `[]`. Each entry has
`account_id` (MTC upstream account UUID), positive integer `version`, and `proxies`
(one to four distinct private IP-literal `socks5h://` URLs). The whole JSON input
is limited to 256 KiB and 256 accounts. URLs may contain credentials; never put
this variable in public account config or logs. Startup rejects invalid input
without echoing it. Config serialization omits the variable.

An account participates only when its existing credential proxy is a group
member. Initial selection preserves that proxy. Selection is stored in a single
database row per account, shared across processes and restarts; only a keyed
configuration fingerprint and numeric indices are stored, never proxy URLs.
Healthy requests read the selected member without rotating, polling other
members, or issuing health probes. No concurrency settings are changed.

Only the generation transport's existing typed pre-delivery connection failure
classification permits a compare-and-swap to another member. The logical send
visits each member at most once and retains the existing connection-attempt
limit and absolute deadline. A failure can update selection for future requests
even when this request has no retry budget. Late failures cannot replace a newer
selection or cross a credential generation change. HTTP statuses, ambiguous
sends, HTTP/2 resets/GOAWAY, body failures, and already-output streams do not
trigger proxy fallback. Existing domain-specific HTTP 400 handling is unchanged.

OAuth refresh and catalog freeze the same durable account selection. This slice
does not add retries to these operations; in particular a refresh POST remains
one-shot behind the existing durable dispatch fence. Refresh persists the
original credential proxy binding, so transport selection cannot accidentally
rewrite operator configuration. Token generation updates invalidate client
snapshots while retaining the selected exit. Selection generations also enter
the bounded client-cache key, including an exit that later becomes selected
again. In-flight clients retain their immutable snapshots.

Apply migration 114 before enabling groups. Increment `version` when changing
group membership or ordering. A new version resets selection to the credential's
configured proxy; processes with older versions or conflicting same-version
membership fail closed for participating accounts. Removing a group restores
single-proxy behavior, so configuration changes must be coordinated across all
roles. Rotating an account's configured proxy outside its group also restores
single-proxy behavior. Updating configuration requires a process restart.

Validation belongs in GitHub Actions: the SQLite/PostgreSQL selection contracts,
Codex connection retry and HTTP/2 failure safety tests, OAuth unknown-outcome
tests, catalog tests, and connection-pool cache tests. No local build or test is
required or permitted by the workspace policy.
