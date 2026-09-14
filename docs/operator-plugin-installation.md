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
the inventory as `{}` when there are no preinstalled candidates.

Without inventory configuration, authorized runtime-status reads return an empty
status and history reports `runtime_enabled: false`, allowing the Plugins page to
show disabled management without failed capability-probe requests. Write endpoints
still reject requests when runtime authority is absent.

The Plugins page first reads `/internal/v1/plugins/runtime-access`, a self-scope
capability lookup available to plugin readers. Tenant-bound credentials receive
false view/manage flags and never mount global runtime reads. Global readers can
inspect history; only global writers get enabled installation/publication actions.
Every backend operation still checks its own scope independently.

Host policy example:

```json
{
  "plugin_root": "/var/lib/token-center/plugin-inventories",
  "allowed_sources": ["ghcr.io/example/plugin"],
  "cosign_public_keys": ["/run/plugin-trust/publisher.pem"],
  "source_credentials": {
    "ghcr.io/example/plugin": {
      "registry_username_file": "/run/plugin-registry/username",
      "registry_password_file": "/run/plugin-registry/password",
      "registry_bearer_token_file": null
    }
  }
}
```

## Helm runtime inventory

`plugins.runtimeInventory` is separate from legacy `plugins.enabled`; the modes
are mutually exclusive so a startup directory cannot silently diverge from the
published complete inventory. The chart uses the service image's bundled installer
at `/usr/local/bin/install-plugin-oci` and verifier at `/usr/local/bin/cosign`.
No installer sidecar, shell image, additional Service or namespace is required.

```yaml
plugins:
  enabled: false
  runtimeInventory:
    enabled: true
    existingClaim: shared-plugin-inventories
    installationEnabled: true
    policyConfigMap: plugin-install-policy
    cosignPublicKeysSecret:
      name: plugin-publisher-trust
      keys: [publisher.pem]
    registrySecrets: [] # Public artifacts require no registry credential.
```

Alternatively, clear `existingClaim` and set
`persistence: {create: true, storageClass: <reviewed-RWX-class>, size: 1Gi}`.
This creates one dedicated same-namespace `ReadWriteMany`, Filesystem PVC and
retains it on Helm removal. The storage class must be explicitly chosen; the
chart does not assume a cluster's default class supports RWX. Increase capacity
for retained revisions: a package can consume 80 MiB plus verified staging space,
and a 16-package inventory plus history may exceed the initial 1 GiB allowance.
Do not reuse database, archive, migration-clone or MinIO data volumes.

All roles mount the entire PVC directory at
`/var/lib/memeloop-token-center/plugin-runtime` (never a file `subPath`). Control
and the combined `all` role mount it read-write. Gateway and worker mount it
read-only at both PVC and container boundaries. UID/GID/fsGroup are 10001; the
driver must actually provide that identity writable directory ownership, including
on root-squashed storage. No root/chown helper container is introduced.

The existing service image runs `prepare-plugin-inventory` as an init container.
Writers create a fully synced temporary `{}` file and atomically link it to
`inventory.json` without replacing an existing file; concurrent initializers
converge. Existing malformed files fail closed and are not emptied. Reader init
containers use `--read-only` and retry until Control has published a valid file.
`{}` is the actual inventory map schema, not a fake installed plugin; no revision
is published and no provider is added by initialization.

The named host ConfigMap must contain `policy.json` with these mounted paths:

```json
{
  "plugin_root": "/var/lib/memeloop-token-center/plugin-runtime/inventories",
  "allowed_sources": ["ghcr.io/your-org/reviewed-plugin"],
  "cosign_public_keys": ["/var/run/mtc-plugin-trust/publisher.pem"],
  "source_credentials": {}
}
```

The source above is a placeholder, not a published artifact or an implicitly
trusted publisher. Policy, public-key Secret and selected registry Secret items
are mounted only on Control/all when installation is enabled. For private sources,
each `registrySecrets` entry `{name, keys}` is mounted at
`/var/run/mtc-plugin-registry/<zero-based-index>/<key>`; refer to those files from
the policy's exact-source credential map. Secret bytes are never Helm values.
Disabling installation removes policy/trust/credential mounts while retaining
readable historical inventories and explicit revision publication/rollback.

Before formal enablement, the storage owner must verify the chosen RWX backend
supports coherent cross-client file locks, atomic no-replace rename/hard links,
fsync and same-path updates across nodes. RWX access mode alone does not prove
these semantics; object-store/FUSE mounts or NFS mounted with local-only locks
are not interchangeable. Database attempt fencing does not replace the filesystem
lock used to serialize inventory-file appends. Confirm these capabilities with
the actual CSI/storage configuration before permitting multiple Control replicas.

First-install acceptance requires a reviewed **plugin artifact** digest and its
approved signing trust (an existing Cosign public key, or optional exact GitHub
Actions keyless identity). A service/installer image digest is not a plugin
artifact. Service-image release workflows do not publish or sign the
example plugin packages. The policy-rewrite example changes requests and test
group-routing components are fixtures; neither should be silently selected as a
production smoke test. The first-party Model Guard package has no network or
rewrite capability and an empty default blocked-model list. Its separate manual
[signed release workflow and availability checks](first-party-plugin-release.md)
must succeed before it can be selected; checked-in source is not a release.
After artifact publishing identity and signing trust are explicitly approved,
verify in the browser: install for review, inspect capabilities,
approve the exact digest, publish a revision, inspect audit/history, and roll back
to the previous complete inventory. These steps need no paid upstream request.

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
permit. A 270-second durable lease renews every 30 seconds while an attempt is
alive; losing ownership cancels its subprocess. Each package has a three-hour
hard ceiling, covering 64 layers/config/manifest (each network operation is
bounded at 120 seconds), eight signing-key attempts and local validation. The
complete set has no shorter aggregate deadline that would starve later packages.
The three-hour bound covers the entire package, including checkpoint reuse,
subprocess verification, shared-volume hashing and database progress writes.
Preparation and final review each have a 60-second bound, as do individual
checkpoint reads. An outer attempt ceiling is the package count times three
hours plus two minutes. Any deadline ends renewal; a bounded terminal database
write releases the claim, or its last lease expires if the database is unavailable.
Blocking filesystem I/O cannot necessarily be canceled by Rust. Such work retains
one separate installation-storage/validation permit until the OS call returns;
it does not retain the installation database lease or the request/staging
compilation permit. Further installation I/O may return overload until storage
recovers. This explicitly bounds retained work instead of detaching unlimited
threads or pretending a timed-out join handle stopped an OS read.
Restart/lease expiry produces an `interrupted` state; operators can retry
the existing task without retaining a browser-generated idempotency key. Retry
uses database checkpoints for completed packages: unchanged current signing
trust and an exact bounded full-tree hash allow skipping their downloads. Changed
trust requires signature verification again; changed bytes never gain approval.
The UI reports committed package progress. Uncheckpointed packages are verified
and exactly compared by the installer; different artifacts are never overwritten.
Root ownership markers are fsynced in temporary directories before atomic
no-replace publication; a crash before publication cannot poison the inventory
ID. Attempt IDs fence progress and late completion. Abandoned hidden temporary
directories are not executable inventory roots and can be removed during host
storage maintenance after confirming no installation is running.

Credentials are bound to exact allowed source names through `source_credentials`.
Unmapped sources always use anonymous authentication, including another repository
on the same registry. Use `{}` for all-public packages. Unscoped top-level registry
credential fields are rejected, not silently forwarded to every allowed source.

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
