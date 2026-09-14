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
An acknowledged uncertainty publication lists the request for manual
reconciliation in the existing generation workspace. Its response is
`image_submission_uncertain` with `reconciliation_available: true`. A read-only
idempotency replay may know only that submission was armed; it returns the same
uncertainty code with `reconciliation_available: null` and asks the caller to
check request status, without claiming that publication has completed.

If arm confirmation or quarantine publication is unavailable, the response is
`image_submission_state_unavailable`, `retryable: false`, and
`reconciliation_available: false`. This is **not** a durable quarantine receipt
or proof that an operator action already exists. Funds remain held; callers
must not create another submission. Once storage recovers, the existing
reservation reaper handles expired owners: an actual submission marker is
published as uncertain and becomes listable while its reservation stays held;
an absent marker with no live owner permits safe zero-cost termination instead.
The reaper uses its existing 30-minute age threshold. Expired idempotency lookup
also detects an armed request and prevents takeover, but its read-only result
does not itself promise reconciliation publication. No automatic recovery path
resends the POST or invents a reconciliation receipt.

For a listed request, the operator must supply a tenant-scoped
persistent service identity, current revision, evidence digest, idempotency
key, and an explicit same-currency confirmed amount (zero for non-delivery).
Resolution atomically audits and settles the original reservation, never
resends the request and never fabricates a successful image. A ledger that
cannot apply the exact confirmed amount rejects the whole resolution.
This also applies to a successful provider HTTP response with unusable image
data: oversized bodies, empty signed-URL assets, aggregate asset-budget
failures, and invalid Responses image-tool payloads do not prove non-delivery
or zero provider cost. These return a non-retryable uncertainty response and
hold the reservation until evidence-backed confirmation. Acceptance scenarios
exercise the real tenant-scoped reconciliation API, repeated decision receipt,
zero additional upstream POSTs before and after resolution, and the original
privacy, unpublished-asset, and durable staging-cleanup checks. Only explicit
non-delivery confirmation releases these reservations at zero cost.
Both image and async-job resolutions recheck the exact authenticated service
credential generation under transaction locks, including on idempotent replay.
Rotation revokes an in-flight old-credential decision even if the replacement
has identical permissions; the audit retains the actual authorizing generation.
Confirmed amounts are nonnegative integer micro-units capped at
9,007,199,254,740,991 (the API/UI exact-integer bound); accounting additions
are checked before mutation, and any overflow or budget shortfall rolls back.

Async jobs retain their existing pre-send quarantine and fixed upstream job
identity: polling never reroutes or resubmits. Existing completed idempotency
replays keep their original durable receipt. Async request normalization still
precedes hash-based replay lookup, so its pure planning hook may run before an
existing job is found; that result never replaces the stored receipt.

Migration 90 adds nullable media routing receipts and synchronous submission
fences. Existing rows keep their existing durable identities. No production
configuration changes or paid provider requests are part of this change.
