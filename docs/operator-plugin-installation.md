# Operator plugin installation

The Operator **Plugins** page now runs the complete signed-package workflow:
install → inspect → approve → publish. It also exposes version history, rollback
and actor audit. Runtime recovery controls remain usable when the active plugin
catalog cannot load. Plugin configuration editing remains a separate tenant
operation; installation and activation always require global plugin authority.

## Host enablement

Service and standalone installer images include the pinned, security-patched
Cosign verifier and the same `install-plugin-oci` executable. The service binary
does not download or execute arbitrary installers. No native plugin, shell
command, browser script, local path or registry credential is accepted by the
installation API.

Set `MTC_PLUGIN_INVENTORY_FILE` to an absolute inventory JSON path and
`MTC_PLUGIN_INSTALL_POLICY_FILE` to an absolute host-owned policy path. Initialize
the inventory as `{}` when there are no preinstalled candidates. A policy is:

```json
{
  "plugin_root": "/var/lib/token-center/plugin-inventories",
  "allowed_sources": ["ghcr.io/example/plugin"],
  "cosign_public_keys": ["/run/plugin-trust/publisher.pem"],
  "registry_username_file": null,
  "registry_password_file": null,
  "registry_bearer_token_file": null
}
```

Allowed sources are exact registry/repository names, not individual plugin
versions or a fixed installation plan. Global administrators can install a new
digest and new contract from those repositories at runtime. Signing keys and
optional registry-credential files remain host-owned and read-only; their bytes
and paths never appear in management responses, audit records or child-process
output. Basic authentication requires both files; bearer authentication is
mutually exclusive with Basic authentication. Do not put credentials in OCI
references.

The Control process (UID 10001 in the image) needs writable inventory storage,
the inventory file's parent directory for atomic replacement, and temporary
storage for the verifier. Every Control/Gateway/Worker must see the same complete
immutable revision roots and inventory updates. Use a shared directory mount,
not a file-only/subPath mount that retains the old inode after atomic rename.
Gateway and Worker mounts can remain read-only. This API does not replicate
local disks or change deployment mounts; configure shared storage before use.
Keep old roots for durable work and rollback. A missing historical root fails
closed and is never replaced with a newer runtime.

## Workflow

1. Enter a fresh inventory ID and one to sixteen OCI references ending in
   `@sha256:<64 hexadecimal characters>`. Include the **complete** desired
   plugin set, retaining policies/providers needed by existing accounts.
2. **Install for review** creates a durable task and invokes the trusted
   installer. The request may disconnect without cancelling the owned task.
   Artifacts must pass repository, signature, descriptor, manifest and byte
   checks. Existing published or staged inventories cannot be modified.
3. Open the manifest review. Inspect all capabilities, outbound origins,
   provider/OAuth schemas and declarative UI contributions. The response contains
   a review digest binding every manifest, executable identity and the current
   signing-key/source trust policy. Installation alone creates no runtime grant.
4. Explicitly approve that exact digest. The host rechecks bytes, trust policy
   and persisted configuration, derives the approved grants, atomically appends
   the inventory, and validates the candidate. Changed review bytes or signing
   trust require a fresh inventory rather than silently widening approval.
5. Confirm global activation and publish. Publication uses the current expected
   revision and an idempotency key. Runtime and provider catalog become one
   request-owned snapshot; older in-flight requests keep their original pin.
6. To revert, confirm global activation and select an older version. Rollback
   creates a new monotonic revision and does not undo database schema/account
   changes. The current catalog is refreshed after either activation action.

Installation tasks are globally serialized with a database lease and a local
permit. Each task has a 240-second execution deadline, with a 270-second durable
lease. Restart/lease expiry produces an `interrupted` state; operators can retry
the existing task without retaining a browser-generated idempotency key. Retry
re-verifies signatures and exactly matches existing files; it never overwrites a
different artifact. Root ownership markers and attempt IDs prevent an unrelated
directory or a late completion from impersonating the current attempt.

Reviews are capped at 4 MiB and fetched individually rather than included in
every status poll. History and audit pages contain at most 100 records, with
strict revision/UUID cursors for older pages. Raw verifier output and arbitrary
plugin errors are suppressed. Installation state, approval, and publication
events retain a service identity or `bootstrap`/`host` marker; never a token.
Publication and its actor event commit atomically with the revision CAS.

## Control routes

All reads require global `plugins:read`; all writes require global
`plugins:write`. Tenant-scoped credentials cannot install or activate plugins.

| Method and path | Purpose |
| --- | --- |
| `GET /internal/v1/plugin-runtime/history` | Latest tasks, versions, audit and installation capability; `before_revision` and `before_audit_id` page older records. |
| `POST /internal/v1/plugin-runtime/installations` | Body `{inventory_id, packages}` and `Idempotency-Key`; returns a task with HTTP 202. |
| `GET /internal/v1/plugin-runtime/installations/{id}` | Fetch one task's bounded manifest review. |
| `POST /internal/v1/plugin-runtime/installations/{id}/approve` | Body `{review_digest}`; register the exact reviewed inventory without activating it. |
| `POST /internal/v1/plugin-runtime/installations/{id}/retry` | Retry an interrupted/failed task under current trust. |
| `POST /internal/v1/plugin-runtime/publish` | Existing `{inventory_id, expected_revision}` CAS plus `Idempotency-Key`. |
| `POST /internal/v1/plugin-runtime/rollback` | Existing `{target_revision, expected_revision}` CAS plus `Idempotency-Key`. |

The standalone CLI remains available for host-driven installation and reviewed
inventory registration. No production installation or deployment mutation is
performed as part of this implementation's tests.
