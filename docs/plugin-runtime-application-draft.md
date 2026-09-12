# Draft application plugin revision integration

Stacked on **#59** (`f1b5eab3c73ff240233aa2cf35886f9c2572372d`),
which is stacked on #55 (`ea4c937a11b3178319970125c04c79064756c828`).
**Not production enabled. Do not mix this change with #64/#65/#66 hotfix releases.**

The `experimental-plugin-revisions` feature exposes a host-only
`AppState::with_application_plugin_inventory` opt-in. The production executable
does not call it. There is deliberately no new environment variable, UI toggle,
remote installer API, or plugin-provided activation mechanism.

## Authority and request ownership

Migration 82 creates global candidate, immutable revision, singleton head, and
idempotency operation tables. A successful operation claims its idempotency key,
inserts the next revision, performs `expected_revision` CAS, and records its result
in one transaction. A failed CAS rolls back the operation and revision. Exact
replay returns the original revision receipt; a different request with the same
key conflicts. Rollback selects a historical inventory and publishes a strictly
new revision, never rewinding head. Identity and contract digests are internal
metadata and are not included in the API receipt.

Each authenticated proxy, synchronous image, or asynchronous generation request
pins the primary database head once at entry. The resulting request-owned
`ApplicationPluginSnapshot` contains both runtime and provider catalog; the
request's AppState clone carries that pair through traffic policy, provider
prepare, candidate retries, and provider normalize. A second pin on the same
request state retains the first snapshot. There is no notification dependency:
another AppState/replica reads the database on its next request, even if it has
missed every notification. Database failures, missing local inventory, identity
mismatches, schema/provider-contract changes, and package/configuration validation
failures stop admission; no startup-runtime fallback is permitted.

## Trusted inventory and management boundary

Inventory is an independently provisioned host map from a bounded opaque ID to
an absolute, read-only revision root and exact approved grants. Grants bind the
manifest, capabilities, component digest, and signed-install provenance. The
complete plugin set and all contribution contracts must match the original
application baseline. Database publication additionally compares the candidate
contract with the expected historical revision, preventing an incompatible
replica baseline from widening the contract. Versions and approved executable
identities may change; configuration schema, provider contract, capabilities,
policy set, and other contributions may not.

With the feature and host opt-in, the following control endpoints require a
**global** service credential with `plugins:write`:

| POST endpoint | Exact JSON body |
| --- | --- |
| `/internal/v1/plugin-runtime/candidates` | `{"inventory_id":"approved-a"}` |
| `/internal/v1/plugin-runtime/publish` | `{"inventory_id":"approved-a","expected_revision":0}` |
| `/internal/v1/plugin-runtime/rollback` | `{"target_revision":1,"expected_revision":2}` |

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

## Verification and remaining deployment gate

All heavy builds/tests run in the existing GitHub `cargo test --all-targets
--all-features` job, including its PostgreSQL service. Local work is limited to
formatting and diff checks. Tests use oneshot channels and barriers, not sleeps
or randomized scheduling, to check A-policy / switch-B / A-prepare-normalize and
new-request B; two independent AppStates cover CAS winners, concurrent exact
replay, lost-notification behavior, restart, monotonic rollback, migration replay,
missing packages, database failure, tampered input, and management authorization.
PostgreSQL schema UUIDs only isolate test data; they do not drive scheduling.

This first draft intentionally reloads and compiles the selected immutable local
inventory during admission rather than introducing an unproven stale cache.
It therefore has **not** passed a production performance or rollout gate. The
feature remains off by default and host opt-in is not wired into the executable.
It does not implement arbitrary schema/provider-contract changes, strict
configuration revocation, UI changes, or health/archive changes.
