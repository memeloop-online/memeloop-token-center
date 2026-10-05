# Empty plugin inventory recovery

With `experimental-plugin-revisions` and a writable host inventory file enabled,
`POST /internal/v1/plugin-runtime/empty-inventories` registers and stages a named
empty inventory. It requires the existing **global** `plugins:write` authority
and an `Idempotency-Key`. The complete body is:

```json
{"inventory_id":"empty-recovery"}
```

Success is HTTP 204. This operation does **not** publish or change the current
revision. The host creates its own empty directory beside the configured
inventory file and grants no capabilities or packages. Caller-supplied paths,
grants and packages are rejected. Normal OCI installations still require one to
sixteen signed, source-approved packages and their existing review process.

Registration shares the installation lock and the atomic inventory-file writer.
Candidate loading completes before acquiring the database write transaction;
waiting for Wasmtime admission does not reserve the SQLite writer. Ordinary
installation rechecks staged IDs under the same installation lock, so a stale
preflight cannot consume an already staged recovery ID.
The staged candidate and `register_empty` audit receipt commit together. Reusing
the same key, inventory ID and actor is idempotent; changing the ID or actor is a
conflict. Inventory IDs remain immutable. Interrupted filesystem/DB work may
leave an unactivated host-owned empty entry; retrying the same request validates
it and completes registration. There is no distributed filesystem/DB transaction
and no automatic publication. History never includes caller keys or host paths.

Before first plugin activation, register the recovery inventory and verify its
candidate reports `staged:true` and `plugins:{}` on the shared host inventory.
For an explicit rollback target, publish that inventory with expected revision
0, producing revision 1; then publish the reviewed plugin inventory with expected
revision 1. A rollback to revision 1 from revision 2 creates **revision 3**, not
revision 0. Existing positive-revision CAS and publication idempotency apply.
If the empty inventory was staged but never published, disabling plugins instead
requires publishing it as a new positive revision with the current expected head.
Wait for gateway request pins to converge; already pinned requests retain their
original immutable runtime.

The existing `ci` Rust job installs the exact official signed Claude
artifact using an exact keyless identity/issuer and digest, verifies the component
hash, and reuses the same files for its existing host ABI test and the additional
real TCP gateway matrix. There is no second artifact download or plugin release.
The installer runs from the already accepted d72 host image, pinned by digest
and checked against its source-revision label; the older generic installer pin
predates wire-shim manifests. No deployed image is changed. Its test-only account
fixture points to an in-process loopback capture server; no production provider
schema, destination rule, OAuth flow or endpoint override is added. Synthetic
native Messages and Responses bridge requests verify final wire content, semantic
preservation, unchanged-body bytes, actual finalize observations and rollback to
empty across gateway restart. The capture retains synthetic bodies and header
names only, never authorization values. The fixed fixture grants derive from the
verified artifact; this GHA test is not a claim that cluster installation approval
or cluster traffic has occurred.

Cluster Wasmtime review/stage evidence and GHA gateway/finalize evidence are
separate. Neither permits production publication or substitutes for a user's
own OAuth authorization. All build/test execution belongs in GHA.
