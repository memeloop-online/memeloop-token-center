# Media group routing

Media uses the same `group-routing-v1` plan and observe contract as text. It
does not introduce another strategy selector, inventory, authorization system,
or retry mechanism. Native ordering remains the input order. Planning only
sees the core-authorized tenant/route/account/generation candidates.

Admission freezes the native recovery deadline before planning. The selected
group ID, monotonically increasing configuration version, configuration,
candidate identity and directive, seed, absolute deadline, application revision,
and exact loaded manifest/Wasm fingerprints are stored with the reservation.
Async claims restore this receipt without replanning or reading current group
configuration. Historical revision loading uses the existing application
inventory authority. Missing historical artifacts or changed code never run
replacement code against old configuration: the selected account retains core
native health admission and emits a fixed revision-unavailable diagnostic.
Credential rotation likewise cannot apply an old-generation directive or
observation to the replacement credential.

The existing cross-Pod health lease and bounded recovery wait are reused.
Async submit and polling each own a health attempt; a valid pending poll is
evidence of a working transport, not completion of the generation job. Job
ownership fencing remains separate from account health fencing. Losing job
ownership releases the account probe without running an observation.
Transport, authentication, quota and invalid-response outcomes are typed;
local storage/configuration failures are inconclusive.

Synchronous image requests durably arm submission before their sole POST.
After that fence, cancellation, timeout, malformed response or process loss
cannot authorize another POST or automatic refund. Idempotency lookup returns
an explicit uncertain outcome, and the generic reservation reaper excludes
these requests. A confirmed successful response still commits the existing
atomic usage/archive terminal transaction. An upstream idempotency header is
not treated as proof that a provider can safely replay a request.

Confirmed HTTP client rejection (excluding 408/425/429) retains the existing
zero-charge failure settlement; authentication remains hard health evidence.
Unknown outcomes are explicitly listed for manual reconciliation in the
existing generation workspace. The operator must supply a tenant-scoped
persistent service identity, current revision, evidence digest, idempotency
key, and an explicit same-currency confirmed amount (zero for non-delivery).
Resolution atomically audits and settles the original reservation, never
resends the request and never fabricates a successful image. A ledger that
cannot apply the exact confirmed amount rejects the whole resolution.

Async jobs retain their existing pre-send quarantine and fixed upstream job
identity: polling never reroutes or resubmits. Existing completed idempotency
replays keep their original durable receipt. Async request normalization still
precedes hash-based replay lookup, so its pure planning hook may run before an
existing job is found; that result never replaces the stored receipt.

Migration 90 adds nullable media routing receipts and synchronous submission
fences. Existing rows keep their existing durable identities. No production
configuration changes or paid provider requests are part of this change.
