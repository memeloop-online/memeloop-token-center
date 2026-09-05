# Model-route create idempotency

Use a fresh visible-ASCII `Idempotency-Key` for each logical route creation.
Retry an uncertain create with the same request and key. A successful first
write returns HTTP `201` and `X-MTC-Route-Create-Disposition: created`; an
exact replay returns HTTP `200` and `...: reused`, with the same route ID.
The optional `enabled` field defaults to `true`; set it to `false` to prepare
a route without making it eligible for traffic. It is part of the canonical
create payload, so a retry cannot change that choice.

The key is opaque control metadata, not a credential: use a bounded operation
identifier (for example, a UUID), never an API key, bearer token, request
content, or personal data. Generate one key per logical create and reuse it
only for its retries; generating another key for each retry defeats the
ownership and storage bounds.

The key is scoped to the tenant and has 30 days of replay authority. Reusing it
for any different canonical route payload returns HTTP `409`. The server
stores only a keyed HMAC digest, never the supplied key. Expired claims are
lazily pruned in bounded batches on later route creates through the indexed
expiry column. Before each lookup, the requested operation's expired claim is
removed directly, so a global cleanup backlog cannot extend this 30-day
window.

A route with an unexpired create claim cannot be deleted, even while disabled.
That deletion fence preserves the promised replay result for the whole replay
window; it does not change normal route disablement or historical retention.

An exact owned replay resolves its recorded route before checking mutable
upstream membership, provider registration, or route-group names. Therefore
an operator's later route edit or upstream retirement cannot turn a valid
in-window retry into a new create or an authorization error; it still returns
the recorded route ID and `reused`. The returned route representation is the
current representation of that stable resource.

Header-less creates retain their historical semantic-equality behavior for
existing integrations. They return `201` and either `created` or
`equivalent_unowned`. The latter means an equal pre-existing route was found;
it is not evidence that the current caller created or owns the route. Release,
canary, or cleanup automation must fail closed unless the disposition is
`created` or `reused`.
