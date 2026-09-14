# Installed plugin hot revisions

The service image includes `experimental-plugin-revisions`. Configure
`MTC_PLUGIN_DIR` with the installed baseline and `MTC_PLUGIN_INVENTORY_FILE` with
an absolute path to host-provisioned JSON. Every Control/Gateway/Worker process
must have the same inventory and read-only revision directories before publishing.
The file maps opaque inventory IDs to `{"root":"/absolute/revision/root",
"grants":{"plugin-id":[{"version":"1.0.1","capabilities":[],
"manifest_digest":"<approved manifest digest>","identity":{
"component_sha256":"<approved Wasm digest>","provenance":{
"format_version":1,"source":"<approved OCI repository>",
"digest":"sha256:<approved artifact digest>",
"signature_policy":"cosign-public-key"}}}]}}`.
Grants are independently approved host input, or derived after a global
administrator approves the exact signed-install review through the
[Operator installation workflow](operator-plugin-installation.md). Management
requests never supply filesystem roots, trust keys or raw grant identities.
The inventory is re-read on discovery, staging and revision pinning.
Atomically append new IDs to this file after installation on every replica;
neither new contributions nor publication/rollback require a rebuild or restart.
Observed IDs, grants and roots cannot be changed or removed. The file is bounded
to 4 MiB and malformed updates fail closed without altering the database head.
The authority caches its last validated file identity, length and modification
metadata (including inode/device/change time on Unix). Requests inspect the open
file's metadata; an unchanged inventory is neither reread nor reparsed. Atomic
replacement is detected even for a same-length file.

Absent configuration preserves the startup runtime. An empty `{}` inventory is
valid and exposes an empty management status. Before the first publication,
requests pin the startup runtime. After publication, failures never fall back to
it. A configured malformed inventory fails startup; binaries built without the
feature reject the inventory option rather than silently ignoring it.
Installation uses the trusted host-configured installer, either from the CLI or
the global Operator/API workflow. It is not a browser upload or arbitrary
filesystem/URL execution API.

## Authority and request ownership

Migration 83 creates global candidate, immutable revision, singleton head, and
idempotency operation tables. A successful operation claims its idempotency key,
inserts the next revision, performs `expected_revision` CAS, and records its result
in one transaction. A failed CAS rolls back the operation and revision. Exact
replay returns the original revision receipt; a different request with the same
key conflicts. Rollback selects a historical inventory and publishes a strictly
new revision, never rewinding head. Identity and contract digests are internal
metadata and are not included in the API receipt.
Committed publish/rollback replay checks the durable operation/hash first and
does not require package files or compilation capacity to reproduce its receipt.

Migration 81 is reserved for OAuth authority (#103), and 82 for conversation
query indexes. This stack must retain both migrations when integrated with master;
it must not reuse either version or replace their schema contracts.

Each authenticated proxy, synchronous image, or asynchronous generation request
pins the primary database head once at entry. The resulting request-owned
`ApplicationPluginSnapshot` contains both runtime and provider catalog; the
request's AppState clone carries that pair through traffic policy, provider
prepare, candidate retries, and provider normalize. A second pin on the same
request state retains the first snapshot. There is no notification dependency:
another AppState/replica reads the database on its next request, even if it has
missed every notification. Database failures, missing local inventory, identity
mismatches and package/configuration validation
failures stop admission once a head exists; no startup-runtime fallback is permitted.
Control plugin manifests, provider types, service-data and configuration handlers
pin the same runtime/catalog pair as execution, after authenticating the caller.
Provider account management, OAuth adapter start/poll, model discovery and worker
refresh also pin it. Durable work can pin an exact historical revision without
substituting the latest catalog when that historical inventory is missing.
Cursor/provider-adapter login tokens and encrypted database state/ready payloads
retain the application revision from start. Poll verifies tenant/operator/expiry
before pinning that exact historical catalog through completion and consumed
replay. Sessions predating publication explicitly use startup state, not the new
head. Missing historical authority fails before outbound polling or consumption.

## Trusted inventory and management boundary

Inventory is an independently provisioned host map from a bounded opaque ID to
an absolute, read-only revision root and exact approved grants. Grants bind the
manifest, capabilities, component digest, and signed-install provenance. The
complete candidate set must match its independently approved grants. New package
IDs, provider/OAuth contracts and supported declarative UI slots are permitted;
there is no equality requirement with the startup catalog. Existing persisted
configuration is validated before staging. Contract digests still bind each
immutable candidate and historical receipt. Operators must retain providers and
policies needed by their active accounts/workflows in each complete inventory;
rollback does not migrate account data or configuration to an older schema.

With the feature and host opt-in, the following control endpoints require a
**global** service credential with `plugins:write`:

| POST endpoint | Exact JSON body |
| --- | --- |
| `/internal/v1/plugin-runtime/candidates` | `{"inventory_id":"approved-a"}` |
| `/internal/v1/plugin-runtime/publish` | `{"inventory_id":"approved-a","expected_revision":0}` |
| `/internal/v1/plugin-runtime/rollback` | `{"target_revision":1,"expected_revision":2}` |

`GET /internal/v1/plugin-runtime` (also `GET .../candidates`) requires a global
`plugins:read` credential. It returns `current` (null before publication) and
`candidates`, each with `inventory_id`, `staged`, and host-approved plugin versions.
It never returns roots, grant digests, executable bytes or provenance. Discover
the inventory, stage an ID, publish with `expected_revision` equal to current
revision (or 0), then verify current and the plugin catalog on another replica.

Publish and rollback require `Idempotency-Key`. Unknown fields are rejected,
including URL, path, Wasm, grant and tenant overrides. Candidate IDs cannot encode
paths or URLs. Candidate staging validates local bytes before persisting an
immutable identity; publication also validates, so a direct publish is safe.
Unknown inventory is forbidden. Tenant-scoped credentials cannot publish even
with `plugins:write`. No remote package source, key material, or local root is
returned in receipts. The gateway role does not expose these control endpoints.

The installer accepts `--inventory-id ID` to install under `--plugin-dir/ID`.
Each new approved package set gets a new root. Existing package IDs are never
overwritten; signed OCI verification and atomic no-replace install remain in
force. Provision the complete inventory before mounting its root read-only on
every replica. Do not modify that root after publication. Keep historical roots
available for in-flight requests, restart, and rollback.

With both `plugin-distribution` and `experimental-plugin-revisions` enabled, the
installer additionally accepts `--inventory-file /absolute/inventory.json` and
`--inventory-entry-file /absolute/reviewed-entry.json` together with
`--inventory-id ID`. The entry file is one reviewed `PreinstalledInventory`
object, including the complete root and independent grants. Initialize the
inventory file as `{}` first. On successful signed installation, the installer
checks the installed source/digest/version against the reviewed entry and
atomically appends that ID under an exclusive sibling file lock. It preserves
file permissions and fsyncs publication. For a multi-package set, use these flags
only on the final package install. Staging then verifies every package's manifest,
component and provenance before publication; registration alone never activates
code. Failed registration leaves the verified package installed but inactive.
Retry re-verifies the signature and compares the complete installed package
against fresh verified artifact bytes (manifest, component, assets and receipt);
only exact matches can continue to registration. An existing identical inventory
entry is a successful replay; differing bytes or grants never get overwritten.
The Operator Plugins page also exposes signed-reference installation, explicit
review approval, current/candidate versions, publication, rollback and actor
audit. Browser file uploads remain unsupported.

## Verification and remaining deployment gate

All heavy builds/tests run in the existing GitHub `cargo test --all-targets
--all-features` job, including its PostgreSQL service. Local work is limited to
formatting and diff checks. Tests use oneshot channels and barriers, not sleeps
or randomized scheduling, to check A-policy / switch-B / A-prepare-normalize and
new-request B; two independent AppStates cover CAS winners, concurrent exact
replay, lost-notification behavior, restart, monotonic rollback, migration replay,
missing packages, database failure, tampered input, and management authorization.
PostgreSQL schema UUIDs only isolate test data; they do not drive scheduling.

Each authority retains at most two compiled revision snapshots. Every request
still reads the primary database head, checks the local inventory root and validates
the exact cached receipt; database/identity/missing-root failures never fall back
to a cached older revision. Concurrent cold pins join one manager-owned loading
task and publication through a shared completion channel. The first caller owns
neither the task nor its result: cancellation cannot discard the compiled revision
or force followers to restart it. A short-held mutex bounds in-flight bookkeeping
to one load per authority. A process-wide owned permit admits one compilation at a time,
including staging; an abandoned blocking task retains its permit until it finishes.
Permit waits are capped at five seconds, compilation waits at 35 seconds and
shared pin/loading waits at 45 seconds. Immutable
compiled bytes remain valid if the package files later change; staging and fresh
loads still revalidate those bytes. Inventory roots must remain read-only.

Eviction drops only cache ownership, never an in-flight request pin. The retained
runtime also retains its revision circuit state across requests. CI extends the
existing SQLite/PostgreSQL authority exercise with concurrent Arc identity,
single-compilation after leader cancellation and bounded-history checks; warm DB/missing-root failures remain
covered. This has **not** passed a production performance or rollout gate. The
service image includes the feature and host inventory is wired into startup.
It does not implement arbitrary schema/provider-contract changes, strict
configuration revocation, UI changes, or health/archive changes.
