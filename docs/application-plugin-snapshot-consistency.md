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
shutdown drops the refresh future with the existing worker lifecycle. No new
service, queue, database schema, dependency or deployment resource is required.

Publish, rollback and uninstall retain the existing durable primary-database CAS
and idempotency receipts. A successful operation acknowledges that durable commit,
not simultaneous activation on every process. Processes normally observe it on
their next five-second refresh. Once a process observes a different receipt, new
requests stop using its previous authorization while the replacement validates.
Runtime, catalog and receipt then switch together, after a second primary-head
comparison. A concurrent publication is retried by the next lifecycle refresh.
Deleting a previously published head fails closed, including after restart when
revision history exists; it never restores the baseline.
Explicitly publishing a reviewed empty inventory retains its existing semantics.

The last primary confirmation is valid for at most the existing 45-second pin
deadline, measured from the start of refresh, including SQL and loading time.
This bounds stale authorization during failed or stalled refreshes. A refresh
failure retains the last validated object for existing pins and diagnosis; it
does not renew confirmation. After expiration new requests receive the existing
overloaded error and readiness is unavailable. Observing a changed, missing or
invalid head invalidates confirmation immediately. Readiness exposes the retained
revision, readiness and refresh error, and logs record the actual failure. A
successful subsequent refresh clears the error and restores readiness.

Historical durable work continues through `pin_historical`, with the original
authoritative stored receipt and identity/contract checks. It never substitutes
the current in-memory head. Administrative `pin` and `pin_if_published` remain
explicit authoritative database operations; business entry does not call them.

The TEST observation establishes 12.072 seconds inside application snapshot
pinning, with no route stage. It does not distinguish SQL acquisition/query,
inventory access, load-lock waiting or cold compilation. This change removes all
of those waits from business entry without claiming one was individually timed.
