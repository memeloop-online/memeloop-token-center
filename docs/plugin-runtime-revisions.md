# Plugin runtime revision boundary

`plugin::lifecycle::RuntimeRevisions` is an experimental library primitive,
compiled only with the non-default `experimental-plugin-revisions` feature.
The service image includes that feature; installed-version application integration
and host inventory configuration are described in
[the application contract](plugin-runtime-application-draft.md).
Identity hashing, receipt parsing, manifest-ID precedence and epoch-task ownership
are enabled only by that explicit feature. Default builds retain the original
directory-order execution and component-file loading path, ignore installer
receipts, and keep the original detached epoch timer. CI runs focused loader
compatibility contracts without default features as well as with all features.
Without host inventory the application retains its startup runtime. With host
inventory, application requests pin the database-authoritative runtime/catalog
pair and execute every hook for that request on that snapshot.

Candidates are loaded from the read-only package directory populated by the
existing digest-pinned, signature-verified OCI installer. The revision manager
does not fetch, install or execute native libraries, scripts or browser code.
Host-owned grants pin plugin versions, capability allowlists and the complete
manifest digest (including provider, data endpoint and UI contributions), actual
Wasm SHA-256 and installer source/artifact receipt. The loader hashes and compiles
the same bounded byte buffer. An unchanged manifest or copied receipt cannot
authorize different Wasm bytes. Missing or mismatched provenance is rejected by
the revision primitive.

Grants must be independently provisioned by a trusted host/operator authority,
never generated from the candidate's self-reported identity. The receipt is
trusted only as installer output on a protected read-only package mount; it is
not independently signature-verified by this primitive. The OCI installer owns
signature verification. An untrusted-writable mount does not provide trusted
provenance and is not supported, even if it contains a receipt.

The grant map is a complete required inventory. A reload cannot silently omit
plugins or remove existing policy/rewrite declarations, including through
rollback. There is no disable authorization interface in this primitive.
Each inventory ID may have several independently approved exact versions and
identities for staged upgrade/rollback. The grant set is immutable for the
manager lifetime; granting or revoking versions requires an explicit host
reinitialization, not a guest configuration update. HTTP capabilities use an
origin subset check, while each version's complete manifest stays exactly pinned.

Compilation and manifest validation finish before publication. Replacement
uses an expected-revision compare-and-swap; rollback publishes a new monotonic
revision from at most two retained prior runtimes. Existing requests retain
their pinned snapshot. Epoch timers are aborted when the last runtime clone is
dropped, including failed candidate loads. Configuration retains the separate
durable database CAS/idempotency contract and bounded cache behavior; a runtime
revision does not promise a transactional snapshot across multiple database
configuration rows or replicas.

Policies execute in manifest-ID order. The snapshot executor isolates failures
per plugin and runtime revision, opens a circuit after three failures for 30
seconds, and permits one half-open probe. A failed/open security policy denies
the request with a host-owned reason, rather than permitting a request by
skipping policy. Guest text, configuration, request content and credentials are
not added to revision or circuit audit events. Existing WIT validation, fuel,
memory and epoch deadlines remain in force.

Plugin model/account outputs remain untrusted preferences. Core authorization
must revalidate model/route access and restrict account hints to authorized
candidates; this manager neither computes candidates nor grants tenant,
credential or route permissions. The application integration now publishes
runtime and provider catalog together, as described in
[Installed plugin hot revisions](plugin-runtime-application-draft.md). The
primitive library contract alone is still not deployment acceptance.

## Application integration and host deployment obligations

Application publication is implemented behind the feature and explicit host
inventory opt-in. The signed OCI installer can append reviewed new inventories
at runtime; Control supports discovery, staging, CAS publication and rollback.
New provider/OAuth contracts and supported declarative UI contributions need no
restart. The [Operator workflow](operator-plugin-installation.md) adds signed
reference installation, exact approval, revision history and audit UI; arbitrary
browser uploads remain unsupported. The following remain required boundaries:

- A separately provisioned, host-authorized grant document and verification
  keys, mounted independently of candidate packages. The installer must verify
  the signed artifact against that authority and bind its receipt to the exact
  installed bytes. Parsing `.mtc-oci-install.json` is not signature verification;
  constructing grants from `PluginRuntime` identities is never authorization.
- One application snapshot containing runtime and provider catalog, pinned
  before configuration lookup and retained through provider dispatch and every
  hook. Replacing only `AppState.plugins` leaves catalog and request consumers
  inconsistent. All existing core tenant, route, model and candidate checks must
  remain authoritative both before and after plugin output is applied.
- Authenticated, scoped management CAS/rollback operations with an explicit
  cross-replica publication contract. Required inventory cannot be removed by
  any management operation. Failed verification or publication must leave the
  previous snapshot usable; no automatic unverified fallback is permitted.
- Application-level tests for rejected grants/tampered packages, stale CAS,
  in-flight snapshot retention, tenant isolation, unauthorized model/account
  hints, and failure of a required policy. Library-only tests do not establish
  any of these endpoint or deployment properties.

Circuit admission generations retire on recovery as well as opening. Slow
completions from retired generations cannot reopen a recovered circuit. Within
one closed generation concurrent failures still count, and a success admitted
before a newer failure cannot clear that failure. These ordering contracts are
tested without sleeps or nondeterministic scheduling.

## Availability integration boundary

This change rebases the experimental runtime primitives from PR #55. It is a
prerequisite for a future availability-policy ABI, not an account-selection or
failover implementation. Existing work remains separate:

| Boundary | Owner and remaining work |
| --- | --- |
| Host-authorized account preference | PR #82 extracts the resolver ordering boundary. Candidate enumeration and dispatch-time eligibility remain host-owned. |
| Hook execution diagnostics | PR #87 bounds and classifies traffic-hook execution. Revision publication diagnostics here describe lifecycle changes, not hook execution or delivery. |
| Transport timeouts | PR #75 concerns transport-policy behavior, not plugin inventory or authorization. |
| Configuration consistency | PR #59 follows these primitives; this change does not freeze multiple database configuration rows atomically. |
| Application publication | Implemented by the feature-gated application authority, migration 83, host inventory and Control CAS endpoints. Runtime and catalog share one request pin; new inventories are discovered without restart. |

The versioned [group-routing contract](plugin-group-routing-v1.md) exposes
only the host-authorized candidate set and credential-free, generation-bound
observations. Plugin output may not add candidates, clear host breakers, widen
attempt/deadline limits, or authorize a second dispatch. A policy runtime circuit
in this module is distinct from an upstream account's health/cooldown state.
An HTTP 503 after dispatch is not evidence of non-execution and must not permit
blind replay. No request dispatch or account health logic changes in this slice.

Lifecycle rejection events have schema version 1, a host-owned operation and
stage, expected revision, and a closed error category. Publication events carry
only schema version, monotonic revision and host-owned reason. No candidate
path, package identity, provenance source, grant document, request, guest text,
credential or raw error is logged by these events. They are diagnostics, not a
durable management audit receipt or evidence of cross-replica convergence.

Deterministic unit contracts cover simultaneous CAS contenders, pinned snapshot
identity after rejection, bounded rollback history, unchanged rollback history
after stale CAS, and exact diagnostic fields/redaction. These supplement the
existing grant/provenance, required-inventory and circuit-generation contracts.
Compilation and tests run only in clean CI runners with all features; local
formatting and diff checks do not establish behavioral acceptance.
