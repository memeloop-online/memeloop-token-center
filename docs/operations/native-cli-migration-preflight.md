# Copilot/Cursor native state migration preflight

Status: offline preflight and sealed-import dry-run contracts implemented; no
production state captured or changed. This procedure does not log in,
authorize an account, call a supplier, refresh/reset quota, enable a target
account, change a route, or retire the old service.

## Reviewed source fence

The immutable source implementation
`linonetwo/cpa-copilot-cursor@324c84e669c9959ce297358b0242ee74b8689529`
stores a non-secret account pointer (`provider`, random handle, label and login)
separately from the actual official-CLI home at
`/data/{copilot|cursor}/<handle>/home`. The reviewed GitOps source mounts PVC
`cliproxyapi/cpa-copilot-cursor-data` into image
`sha256:1bc600741c7266a23a1567a2fc19b3708e0b4476eacbe9b5df1637e19d7c10e5`.
The ordinary CPA auth PVC therefore cannot prove a usable Copilot or Cursor
migration by itself.

Never inventory the live writable directory and call that a consistent
capture. Record the workload UID, PVC UID, exact GitOps revision and image
digest; create a storage snapshot; wait for its bound/ready UID; mount only the
restored snapshot read-only in a dedicated capture job. The source workload,
PVC, routes and old endpoint remain available until every retirement receipt
below is complete. Do not scale down, delete, prune or mutate them during
preflight.

## Metadata-only preflight

The capture job must inspect metadata without printing file names or contents.
It must use directory-relative no-follow operations, reject traversal,
symlinks, devices, sockets and regular files with `nlink != 1`, and enforce
owner, file-count and byte limits. Run the metadata inventory twice against
the same immutable snapshot. Every file must be classified by a
provider/version-specific, reviewed authentication-state allowlist; a wildcard
home-directory copy is forbidden. Caches, settings, hooks, plugins, startup
files and executables are excluded.

Represent account, principal, pointer, allowlist and metadata identities only
as domain-separated SHA-256 digests. The preflight document contains no
handle, login, path, token, cookie, auth JSON or state bytes. Validate it with:

```text
node ops/verify-native-cli-migration-preflight.ts <owner-only-preflight.json>
```

The validator rejects unknown fields, secret-shaped input, changing or unsafe
metadata, incomplete classification, duplicate/mismatched identities, a
non-native target identity, enabled accounts/routes, supplier/reset activity,
incomplete encryption AAD, missing CAS/single-writer fences, or an incomplete
rollback hold. Its stdout is a safe aggregate receipt with only counts and an
evidence digest. `preflight_ready_for_sealed_capture` is not an activation or
migration-complete receipt.

## Sealed-capture import dry-run

After a separately reviewed capture owner has produced an owner-only sealed
capture receipt, generate the import plan with:

```text
node ops/plan-native-cli-migration-import.ts --dry-run <owner-only-sealed-capture.json>
```

This command accepts metadata and digests only. It deliberately has no apply
mode, cannot enumerate state files, cannot read ciphertext, cannot contact a
provider or cluster, and writes only a redacted aggregate receipt to stdout.
The sealed-capture input binds each package to the preflight evidence,
immutable snapshot UID, capture lease, allowlist, target tenant/account,
provider, credential generation and state generation. It requires the
`mtc-upstream-credential-v2` carrier with ChaCha20-Poly1305 and exact AAD fields
`tenant_external_id`, `account_id`, `provider`, `state_generation`; actual
ciphertext, nonce, key material and filesystem paths are forbidden in the
dry-run document. Credential generation is a separate account-row CAS fence;
it is not silently substituted for the encrypted state generation.

The second input is a read-only target inventory embedded in that document.
Its domain-separated digest is a compare-and-swap fence. A missing target
plans creation of a disabled native account plus state installation; a
disabled account without state plans installation; the exact already-installed
package at the same state generation becomes `no_op`. An enabled/routed target,
changed credential/state generation,
different installed package, stale inventory digest or non-native name rejects
the entire plan. Per-account idempotency keys and the plan digest are derived
from the preflight digest, target identity, both generations and package digest, but
only aggregate digests and counts are emitted.

The future apply owner must re-read and re-hash target state inside one
single-writer lease, use both recorded generation CAS conditions, persist the per-account
receipt atomically with the encrypted state, and return the prior receipt for
an identical idempotency key. This dry-run is not permission or code to apply
it. It never creates names containing old-service or bridge terminology: the
only public identities accepted are `Copilot` / `copilot-cli` and `Cursor` /
`cursor-cli`.

## Identity and encryption

Each source account maps exactly once to a new target-owned UUID under the
correct tenant. Public identity is exactly `Copilot` / `copilot-cli` or
`Cursor` / `cursor-cli`; CPA/plugin/bridge names are provenance only. The
digest of the operator-reviewed source principal must equal the expected
target principal, but this assertion alone does not prove the restored CLI is
logged into that subject. Keep `target_enabled=false` and every route
candidate disabled until a target subject receipt exists.

The capture owner copies only reviewed authentication state into the existing
`mtc-upstream-credential-v2` encrypted carrier. Encryption occurs before
durable target persistence. AAD is exactly tenant external ID, target account
ID, provider and encrypted state generation. The independently recorded
credential generation fences the owning account row. The receipt records a digest of the
key-reference revision, ciphertext/package digest, allowlist digest, immutable
snapshot UID, counts and sizes—never a key, nonce, plaintext, token or cookie.
Plaintext may exist only in bounded capture/restore process memory and an
owner-only ephemeral filesystem. The capture receipt is valid only after that
filesystem is destroyed within the recorded bounded window following a
verified sealed write.

Restoration is a single-writer lease with credential- and state-generation
compare-and-swap. An
expired capture/restore lease cannot publish a generation; retries either
return the same receipt or allocate a new attempt and generation. Persist
official-runtime refresh changes as another encrypted generation using the
same fencing rules.

## Required receipt chain

| Receipt | Required proof | Traffic allowed |
| --- | --- | --- |
| Preflight | Immutable snapshot identity; stable classified metadata; one-to-one disabled mappings; rollback hold | None |
| Sealed capture | Ciphertext/package digest, envelope/AAD, source snapshot and allowlist digests, counts and bounded destruction record | None |
| Target restore | Target tenant/account/generation, exclusive lease/CAS result, owner-only state install, runtime/image hashes | None |
| Target subject | Official runtime reports the expected principal and usable auth without logging raw output | At most a separately authorized status/model check; no generation or reset |
| Route activation | Exact candidate/route revision, old and new health state, rollback revision, scoped rollout | Only the explicitly canaried account |
| History parity | Account/route/request/session/archive identities reconcile under the same cutoff/watermark | Canary may expand |
| Zero legacy fallback window | Bounded observation shows no CPA/plugin/bridge dependency or fallback and rollback remains tested | Native route only |
| Retirement | All preceding digests linked, user-approved retirement, final rollback/archive disposition | CPA resources may then be removed |

An error, timeout or unknown outcome never advances the receipt chain. It
leaves the old source and route intact and the target disabled. Rollback before
retirement is a route-revision CAS to the recorded old candidate set and exact
verified image digest; do not “repair” by creating bridge accounts or copying
state again without a new capture lease.

## Human interaction that cannot be automated

The state-first path should require no new authorization if the sealed state
is complete, current and its target subject receipt matches. Human interaction
is mandatory when any account has missing, expired, revoked or identity-
mismatched state:

1. If state-first restoration cannot prove the expected subject, the account
   owner—not an operator or agent—starts the official reauthorization from the
   disabled target account, opens the provider-generated OAuth/device URL,
   verifies the expected Copilot or Cursor identity and grants consent. An
   operator/agent must not impersonate this step, paste a user/device code into
   logs, approve consent, or reuse another account's state. Polling may resume
   only after the owner reports completion; timeout/cancellation leaves the
   target disabled.
2. A user separately authorizes the first provider status/model check and the
   later traffic canary. The dry-run, sealed capture and successful OAuth
   callback do not authorize generation traffic or quota actions.
3. The user explicitly approves final retirement after the account/state,
   history/archive and zero-legacy-fallback receipts are reviewable. This is a
   distinct decision from OAuth consent and canary approval.

Quota reset is not part of migration. Cursor has no verified reset capability;
Copilot/Cursor reset must never be inferred, probed or invoked. Switching the
working Codex client back to MTC is also outside preflight: perform it only
after independent natural-traffic stability evidence, keeping the known-good
CPA endpoint available until the user accepts the cutover.

## Current blocking evidence

No production snapshot UID, metadata inventories, reviewed authentication-file
allowlists, target account mappings, sealed packages or target subject
receipts were produced in this change. Consequently Copilot/Cursor native
migration and old-service retirement remain blocked at the capture gate. This
code makes the preflight and import-plan gates auditable; it does not claim the
data has moved.
