# Authenticated pre-admission failures

An empty authorized candidate query returns HTTP 503 with
`error.code = no_authorized_upstream_candidate`. It is distinct from
`service_overloaded` and preserves authorization, route, account and catalog
filters. The query emits the server request UUID, key UUID, tenant UUID,
bounded model name, routing protocol and candidate count. It never logs a
credential, request payload, configuration or upstream response body.

After authentication, failures on the text gateway routes are recorded even
when billing admission never succeeded. An unread or invalid model is the
empty string, not an invented model. These rows have
`terminal_cause_code = gateway_pre_admission`, `usage_basis = not_observed`,
`billing.billable = false`, zero usage/cost, no upstream account/route, and an
empty reservation identifier. No reservation, provider attempt or financial
settlement is created. Request bodies are not retained. Existing schema
columns suffice; no migration is required.

Before returning an early error, the gateway atomically journals only the
bounded terminal metadata under
`responses_request_spool_path/early-terminals/<database fingerprint>`.
Successful/admitted forwarding does not write or wait for this journal.
A separate database pool replays files into the request list, failure
statistics and a single finished event in one transaction. A request locator
claim is the idempotency fence: duplicate replay or an admission whose commit
was already successful never changes the original record or counts twice.
Database errors leave the file for retry. Commit followed by process failure
is safe to replay.

Each journal record is at most 4096 bytes, with a 16384-file admission bound;
the replay worker processes at most 64 files per pass. The existing spool
filesystem must be retained for replay across process/pod replacement. A
replacement that loses that filesystem also loses unprojected records.
Filesystem/full-journal failures are explicit: the error response includes
`x-mtc-request-recording: unavailable`, a safe error log is emitted and the
`memeloop_token_center_early_terminal_total{outcome="failed"}` counter grows.
They do not fabricate a successful persistence result. Metrics also expose
`journaled` and `replayed` outcomes.

Unauthenticated requests and control/self-service failures are excluded.
Normal admitted requests retain their existing terminal lifecycle and
supplier attribution.

## OAuth refresh/catalog continuity

Only a generation-checked, staged native Kimi/Codex refresh may carry its
current known catalog into the new generation, in the credential transaction.
Both native credentials must validate their identity metadata; identity/scope
must be unchanged and the new credential must not be expired. Manual rotation,
reauthorization and other drivers never inherit catalog evidence. No grants,
route exclusions, disabled models or credential-expiry checks are relaxed.

The copied snapshot retains the exact model set, bounds and original fetch
timestamp. It is marked stale and due for the existing asynchronous catalog
sync; it is not presented as fresh discovery. The old sync lease is invalidated
so an old-generation result cannot overwrite the refreshed generation. A real
replacement remains authoritative and removed models are not resurrected on
the next refresh. Kimi permits an empty catalog; Codex retains its existing
nonempty trusted-model guard, so its withdrawal case retains another known
model without restoring the withdrawn route. Refresh sync failure preserves
the existing stale-evidence semantics, rather than creating a generation gap.

SQLite and PostgreSQL CI cases recreate the old generation-only empty-eligible
interleaving, pause before new catalog discovery, reject stale sync publication,
and cover sync failure, withdrawal, identity/scope change, manual rotation,
missing grants and expired credentials. This establishes a reproducible source
bug, not the unknown model/key of the historical 09:09 request.
