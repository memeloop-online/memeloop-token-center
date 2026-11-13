# Conversation object contract v1

This topic wires conversation body writers and readers to the existing
`ArchiveStore`. It is not safe to deploy over unmigrated rows: readers reject
legacy plaintext instead of silently falling back. External migration,
verification, cutover and source retirement are deployment gates.

The long-lived archive retains exact raw JSON/SSE alongside conversation and
media objects. Raw request cache eviction is separate; it must never target
`tenants/*/cas/` or conversation roots. Raw captures in manifests reference
long-lived archive objects, not evictable cache objects.

`semantic_atoms.content_json` temporarily holds only a
`ConversationBodyReference` (CAS reference plus Unicode character count).
This avoids colliding with the coordinated accounting/transport migrations.
Renaming that metadata column and dropping retired body/spool columns belong
to a separately numbered, post-verification cleanup migration.

## Objects and identity

All new content uses tenant-local BLAKE3 CAS:
`tenants/{tenant_uuid}/cas/v1/blake3/{first_two_hex}/{digest}`.
`CasArchiveObject` contains `object_locator`, `blake3_digest` and `size_bytes`.
Size and digest describe the uncompressed original bytes, including when reading
the existing compressed CAS representation. ETags are not content hashes.
Conditional create is mandatory; an existing object is read back and verified,
never overwritten. A tenant mismatch, corrupt digest, wrong size or missing
object is an error, not an empty conversation or a database fallback.

A `ConversationManifest` v1 is an immutable object containing tenant, session,
branch, parent-branch/event identity, a prefix head, optional raw captures and
explicit gaps. Store only its reference, ownership, revision and retention
control metadata in CNPG. Do not put the manifest JSON or any event JSON there.
Publishing a new revision requires the control-plane root's compare-and-swap;
writing the immutable manifest alone does not publish it or confer access.
Authorization must establish tenant and stable-key/session ownership before
passing a manifest reference to these storage methods.

A prefix reference contains an immutable node reference and an exact event
count. Each node contains version, parent prefix and event object reference.
Extending a branch adds one event and one prefix node; identical extensions
reuse their CAS objects. A fork reuses its complete ancestral prefix.
There is no 1,024-event truncation, timestamp sorting or branch flattening.
Every edge must decrease the count by exactly one and terminate at zero.
Repeated event IDs and causal parents outside the preceding branch ancestry
are rejected. Subagent branches preserve their explicit agent relationships;
they are not concatenated into the main agent's dialogue.
Parent-branch/event declarations must be paired and match an ancestral event;
the final event must belong to the manifest branch.

## Fidelity and media

`ConversationEvent.document` is a `RawValue`: original JSON for that event,
including provider IDs, tool call IDs, arguments, opaque reasoning and unknown
fields. Serialization preserves even whitespace, number spelling and fields
that the old normalized UI projection strips. Origin, role, kind, event ID,
request ID, branch ID and agent relationships are carried separately.
Do not create these records from an already lossy `SemanticAtom` or
`TrainingUtterance` and label them lossless. Historical atoms are preserved as
historical source material; missing original information must remain a gap.

Projection must preserve request/response envelope fields as events as well as
messages, tool calls/results and ordered stream events. SSE raw captures retain
the exact byte stream and use `format: sse`, not `json`. Completion events do
not replace earlier deltas; an empty final `output` is not proof of no tools.
Original bytes can be retained even when parsing fails, with a corresponding
gap on the manifest. The storage contract does not itself parse protocols,
prove that all source events were projected, or discover missing URL-only
media. The source adapter owns that completeness assertion.

Media references contain an existing JSON pointer into the event, MIME type
and tenant CAS reference. Every referenced binary must pass GET/size/BLAKE3
verification; a locator, signed URL or MIME label alone is insufficient.
The stored media bytes must match the source bytes. Externalizing inline media
requires a reversible, documented replacement at the pointer; this first
storage delta does not rewrite inline JSON. If bytes cannot be captured,
record a gap and retain the source; do not assert a complete training export.

`export_conversation` returns the manifest and every original event in prefix
order with media references. It verifies raw captures still named by that
manifest, media bytes and the entire prefix, and rejects empty exports and any
declared gap.
It is the lossless export substrate, not a provider-specific role projection
or self-contained media tarball. An external exporter must package referenced
binaries and digests. Role filtering, opaque-field removal, redaction and
training-schema conversion produce separately versioned derived artifacts;
they never replace the sole retained original.

The HTTP training export streams NDJSON: a manifest record, event references
in descending ordinal order, then a verified end record. Consumers require
that end record and EOF, restore ascending ordinal order per branch, and fetch
the immutable event/media objects. Missing or corrupt objects terminate the
stream without a success trailer. No total event or aggregate-byte limit is
applied to this endpoint. Event verification skips opaque document strings
through a bounded chunk reader instead of collecting whole conversations.
Manifest publication similarly walks events without collecting their bodies.
Application hot reads retain the existing bounded memory
cache and otherwise read objects directly. The format does not add a DB body
read, a new cache, a new storage backend or a DB archive wait on the frontend.

## Migration tool ABI and ownership

CPA and operational migration orchestration belongs exclusively to
`memeloop-online/memeloop-token-center-migration-tools`. Product code owns the
format, `ArchiveStore` methods and validation invariants. Do not add a CPA
CLI, external source adapter or production cleanup script to this repository.
The following is a proposed checkpoint ABI for coordination, not a new
implemented command or an owner-acknowledged operational procedure:

| Field | Required meaning |
| --- | --- |
| `format_version`, `run_id`, `revision` | Versioned checkpoint and CAS revision |
| `scope` | Exact tenant/source range; never an implicit whole installation |
| `source_identity` | Source schema, snapshot/fence, bucket identity, adapter revision |
| `target_identity` | Independent instance/bucket identity, product format revision |
| `phase` | inventory, copy, verify, catch_up, cutover_ready, retired |
| `cursor`, `high_watermark` | Stable composite PK and closed source high watermark |
| `counts`, `bytes`, `inventory_digest` | Source/target exact counts and hash-chain receipt |
| `verification_receipt` | Object, branch, media and real-dialogue reconstruction proof |
| `lease_owner`, `lease_expires_at` | Single-owner resumable checkpoint advancement |

Read a stable source snapshot in bounded keyset batches. Persist byte-exact
source evidence, translate prefix identities explicitly (old semantic hashes
are not CAS byte hashes), upload once per unique tenant digest, verify the
target bytes, then atomically checkpoint. On restart, verify existing target
content before skipping it. A cursor advance or HEAD result alone is not a
verification receipt. Crashes before checkpoint may repeat reads but cannot
overwrite verified CAS content. The product APIs deliberately never delete
source evidence.

Inventory request/response objects, semantic payloads, atom bodies, prefix
nodes, projection snapshots, encrypted spool chunks (decode once using the
existing approved source adapter), media and import/quarantine sources.
Do not omit unresolved requests or silently turn historical gaps into empty
dialogue. Store explicit source-key-to-target mapping in the migration receipt
so different source rows deduplicating to one object still reconcile counts.

After the baseline, close the incremental watermark with a source-writer
fence and drain. Verify every retained root, exact source/target logical
counts, object hashes/sizes and real multi-branch agent/tool/media replay.
Only then publish new roots and switch all readers/writers together.
No indefinite dual write or read fallback is part of this contract.

Retirement includes old blobs, duplicate ciphertext spools, snapshot readers,
`session-body-rebuild`, replay compatibility and old worker production paths.
Schema deletion and code deletion are completion gates, not optional later
cleanup. Production deletion requires its separately approved exact inventory
and verification receipt. Do not run a bulk data move or network object I/O
inside an application-startup schema transaction.

## Retention and FIFO

This delta adds no deletion API. Existing CAS deletion protection stays on.
Raw captures still named by an active manifest are necessary references.
Before raw FIFO eligibility, publish a verified canonical revision that no
longer requires that raw object, and retire the old root under the approved
retention policy. Preserve every retained event, branch prefix, media object,
training export root, migration source and legal hold.

Use reachability from all live roots, not a raw key prefix or a single
manifest. Deduplication can make the same object both raw cache and sole
training evidence. FIFO orders only already eligible, expired, unreferenced
objects. Concurrent root publication needs an epoch/lease fence and a final
reference check before delete; stale reference counts are not proof.
Incomplete projection, failed verification or unknown ownership blocks GC.
Never enable a bucket-wide TTL that bypasses these rules.

## Validation

Synthetic contract tests live in `src/archive/conversation/tests.rs` and must
run in the existing GHA Rust suites. The memory backend tests do not establish
RustFS conditional-create, multipart, restart or range-read compatibility;
the independent operations owner must provide isolated TEST evidence for
those behaviors before endpoint cutover.
