# Plugin runtime revision boundary

`plugin::lifecycle::RuntimeRevisions` is an experimental library primitive,
compiled only with the non-default `experimental-plugin-revisions` feature.
It is not a completed application hot-reload feature.
The default application state still holds `PluginRuntime` directly;
there is no network reload endpoint or cross-replica revision publication in
this change. Integrators must pin one snapshot before resolving configuration
and execute every hook for that request on that snapshot.

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
credential or route permissions. Provider-catalog replacement and management
authorization need an application-level atomic integration before exposing any
reload endpoint. No deployment or runtime acceptance is claimed by this
contract alone.
