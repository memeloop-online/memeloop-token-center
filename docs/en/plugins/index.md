# Plugins overview

MTC plugins are versioned WebAssembly components (Component Model) for extending routing, provider adapters, OAuth flows, and request policies within explicit boundaries. Each package contains a `plugin.json` manifest, an optional `.wasm` component, a README, and an icon.

Authoritative contracts:

- [WIT interface definition: `token-center.wit`](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)
- [`plugin.json` manifest schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)

## Capability model

The manifest's `capabilities` determine what a plugin can do:

| Capability | Meaning |
| --- | --- |
| `log` | Emit bounded host log events |
| `kv` | Use key/value storage isolated to the plugin namespace |
| `http` | Access only HTTPS origins listed in the manifest |

The host bounds fuel, memory, and execution time for every call. A plugin cannot see upstream credentials or bypass model permissions, balances, rate limits, or audit boundaries.

## Extension points

- Traffic policy: make bounded decisions or rewrites after authentication.
- Provider components: supply model catalogs, request preparation, and response normalization for a provider.
- OAuth adapters: declare provider-specific authorization-code or device-code flows.
- Group routing: order already authorized candidates and provide bounded health advice; see [group routing](routing.md).

See [plugin development](development.md) for implementation, manifest, and OCI package details.

## Security boundary

Plugin requests use identities and permissions produced by the core; sensitive credentials are used only between the core and a provider. When a plugin fails, the core falls back to native behavior, and in-flight requests keep the plugin snapshot captured at admission.

Plugins extend product capabilities without changing client-visible authorization. Installation, approval, and activation are handled by deployment administrators according to their release policy.

## Published snapshot authority

Every new application plugin pin reads its current or requested historical receipt
from the primary database. A previously fully validated compiled snapshot can be
reused only when all receipt fields match: revision, inventory ID, reason, package
identity digest and contract digest. Its complete host grants, executable bytes,
provenance, manifests, capabilities, stored configurations and provider types were
validated before the snapshot entered the bounded process-local cache. Tenant
configuration resolution and hook policy checks continue on their existing paths.

An exact warm pin does not refresh the inventory file or inspect its root. Removing,
changing or losing access to those files does not revoke an already compiled
snapshot that the fresh database receipt still authorizes. This applies to current
requests and authenticated durable work using an exact historical receipt, including
OAuth consumed-session replay. Publishing a replacement changes authority for new
current pins; historical work still requires its own database receipt. Database
errors, absent receipts and mismatched receipts never authorize cached fallback.
Already pinned in-flight requests retain their existing snapshot.

New, cold or evicted revisions must validate the live inventory, root, complete
assets and grants before compilation and receipt validation can populate the cache.
A missing root fails a cold current or historical pin, even when another revision
is cached. Administrative staging and status retain live filesystem validation;
staging continues to reject tampered manifests or grants. Immutable inventory IDs
cannot be edited or removed through inventory refresh. Filesystem removal alone is
not a revocation mechanism for an exact warm snapshot.

## Protected inventory descriptor export

With `experimental-plugin-revisions`, the control-only endpoint
`POST /internal/v1/plugin-runtime/descriptor` requires existing **global**
`plugins:write` authority and a configured application inventory. Its request is:

```json
{"inventory_id":"release","expected_revision":0}
```

`inventory_id` is an optional **staged-candidate existence assertion**, not an
inventory filter or a publication selection. An unknown candidate returns 404.
Omitting it or supplying `null` removes only that assertion. Every successful
export includes the **entire host-owned inventory**, including retained historical
entries and any locally provisioned entries that have not been staged. Export does
not stage those entries, approve installations, add grants, or publish a revision.
One invalid inventory fails the whole export, even when another ID was selected.

`expected_revision` must match the observed head; use 0 for no published head.
Export reads complete candidate and revision history snapshots before and after
local asset validation, without holding a database transaction during that work.
A stale expected head or a changed authority snapshot returns 409. This is
**observation consistency**, not a CAS reservation: the head may change after the
final observation. Subsequent publication still requires its own existing CAS,
authorization and validation. Export success is not asset readiness, a gateway
acknowledgement, or permission to bypass publication checks.

Success returns only `descriptor`, `descriptor_digest` and `observed_head`, with
`Cache-Control: private, no-store`. An unconfigured application inventory returns
404; a configured empty map can export with `observed_head:null`; an explicitly
registered empty inventory has an entry with no packages or grants. No installation
row is fabricated for these cases. No head does not prove that the separate host
baseline has no plugins.

Destination roots are server-owned: the configured `MTC_PLUGIN_DIR`, or otherwise
the parent of `MTC_PLUGIN_INVENTORY_FILE`, plus `inventory-<inventory_id>`. Hidden
empty-registration directories are not exported verbatim. This mapping remains
stable for subsequent exports under the same server configuration; an incompatible
previous bundle still fails the importer's historical-retention checks. The API
does not accept roots, grants, approval flags, credentials or readiness assertions.

Source loading preserves existing application authority semantics: a root containing
`plugin.json` is one package, not a container whose children are also activated.
Otherwise the root is a package container. Exported packages must exactly satisfy
the host grants and, for staged entries, the database identity/contract receipts.
A child hidden by a root manifest cannot be silently omitted if host grants require
it. Import maps each exported package to a child of the destination inventory and
uses the strict container loader to recheck the complete package set and digests.
These are equivalent runtime inventories, not identical directory layouts.

Export validates local manifests, component bytes and installed provenance against
host grants; it compiles components but does not execute guest hooks. It does not
download artifacts or reverify signatures online. The OCI importer must still
perform its existing signature verification and complete runtime validation.
Export persists no distribution object and changes neither the publication head
nor request pinning. A failed export does not revoke an existing valid warm pin.
