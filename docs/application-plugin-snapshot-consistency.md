# Application plugin snapshot lifecycle

Business request entry clones a single in-memory `Arc<ApplicationPluginSnapshot>`
containing the validated runtime, provider catalog and immutable revision receipt.
It does not consult SQL, inventory files, revision-load locks or compilation.
Requests already pinned keep their original receipt through completion.

Initialization creates the authority without compiling its published head. After
installing the normal router, every serving role immediately warms the head in
its background process lifecycle, then refreshes every five seconds. Liveness
stays healthy; readiness reports unavailable and business pins fail immediately
until the first verified snapshot is available. The 35/45-second load operation
is outside the AppState constructor and does not trigger initialization retry.
Shutdown drops the refresh future with the existing worker lifecycle. No new
service, queue, database schema, dependency or deployment resource is required.

Publish, rollback and uninstall retain the existing durable primary-database CAS
and idempotency receipts. A successful operation acknowledges that durable commit,
not simultaneous activation on every process. Processes normally observe it on
their next five-second refresh. While a replacement loads and validates, requests
continue using the previously verified snapshot. Runtime, catalog and receipt
switch together only after validation and a second primary-head comparison.
A concurrent publication discards the candidate activation and is retried by the
next lifecycle refresh, without clearing the serving snapshot.
A missing head is a refresh error, never a request to restore the baseline.
An already loaded process retains its verified snapshot; a restarted process
with revision history but no head remains uninitialized.
Explicitly publishing a reviewed empty inventory retains its existing semantics.

The serving snapshot has no wall-clock expiry. Refresh failure, cancellation,
timeout, an invalid candidate or a head race preserves it for new and existing
requests. The existing 45-second deadline bounds background work only; it is not
an authorization lease or a request admission deadline. Plugin snapshot readiness
is unavailable only until the first valid state is obtained. Existing database
and archive health checks retain their separate semantics.
Readiness exposes the serving revision and refresh error, and logs record the
failure. A successful subsequent refresh clears the error and atomically replaces
the snapshot. Request authentication, explicit credential/grant revocation and
plugin signature/host-grant validation retain their existing mechanisms; refresh
failure or network loss is not interpreted as an authorization revocation.

Historical durable work continues through `pin_historical`, with the original
authoritative stored receipt and identity/contract checks. It never substitutes
the current in-memory head. Administrative `pin` and `pin_if_published` remain
explicit authoritative database operations; business entry does not call them.

The TEST observation establishes 12.072 seconds inside application snapshot
pinning, with no route stage. It does not distinguish SQL acquisition/query,
inventory access, load-lock waiting or cold compilation. This change removes all
of those waits from business entry without claiming one was individually timed.
