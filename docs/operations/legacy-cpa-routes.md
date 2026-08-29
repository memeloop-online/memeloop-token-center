# Legacy CPA provider-exact route convergence

Status: implemented as an owner-reviewed TypeScript planner/importer; checked-in Kubernetes mode is live `dry-run`. This procedure does not grant any credential and does not make the legacy-credential continuity gate pass by itself.

## Why routes stay provider-specific

The old CPA inventory contains 23 provider/model mappings (DeepSeek 8, GLM 6, Qwen 5, Claude 2, other 2) and 61 exact policy grant shapes. MTC currently has fewer routes than those source mappings. A shared public model name is not permission to combine providers: each reviewed route binds exactly one source pattern to exactly one target upstream account and its pinned `source_stable_id`. Provider groups, route groups, credential grants, aliases, and family inference are forbidden.

MTC uniquely identifies a route choice by `(tenant, public_model, protocol, priority)`. When two source providers expose the same public model, the owner manifest must assign distinct priorities and explicitly review the selection semantics for credentials that receive both exact routes. The importer never invents the ordering and never combines the two managed Codex accounts.

Image/video generation routes are outside this migration. In particular, Qwen-shaped or otherwise similar old text mappings cannot be redirected to the existing SiliconFlow image/video routes. The live inventory parser accepts unrelated `generation` routes so a full target listing remains readable, while reviewed legacy targets allow only the exact source `openai` or `anthropic` protocol.

## Inputs and pins

All four input files, including the service token, must be regular files with one hard link and mode `0600`:

1. `source-inventory.json`: version 1 exact source mappings plus explicit `reauthorization_required` and `anomalies` worklists.
2. `upstream-inventory.json`: version 1 tenant and owner-reviewed `(upstream_account_id, source_stable_id)` bindings, with the expected live driver, active status, and `updated_at`.
3. `reviewed-manifest.json`: version 1, exact target control base URL, both preceding SHA-256 pins, and a one-to-one route specification for every source mapping.
4. The private control-plane service bearer token.

Every manifest item repeats the complete source pattern (`provider`, `model`, nullable `group`, nullable `upstream_prefix`, `protocol`) and the exact target account/stable-source pair, public model, upstream model, protocol, priority, and expected existing state. Target protocol must equal source protocol. Unknown fields, duplicate mappings, duplicate route choices, stale upstream bindings, priority outside `[-1000000,1000000]`, missing source mappings, and provider expansion all fail closed.

The known malformed legacy shape `{provider: codex, model: classify:csil, group: gpt-5.6-sol}` belongs in `anomalies`; it remains unmapped and blocks apply. Copilot and Cursor belong in `reauthorization_required`. That worklist is report-only and does not start OAuth. It does not block creation of otherwise fully reviewed routes, but any nonzero count means end-user continuity is still incomplete and production replacement remains blocked.

## Dry-run, apply, and recovery

Build/run `/usr/local/bin/import-cpa-model-routes` from the immutable importer digest. The checked-in [Job template](../../ops/kubernetes/legacy-route-import-job.yaml) stages every file to memory as owner-only and runs a live dry-run:

```text
import-cpa-model-routes --source-inventory-file /runtime/source-inventory.json --upstream-inventory-file /runtime/upstream-inventory.json --reviewed-manifest-file /runtime/reviewed-manifest.json --target-api-base-url EXACT_REVIEWED_URL --service-token-file /runtime/service-token --checkpoint-file /checkpoint/checkpoint.json
```

The output is count-only plus the three input digests and intent digest; it contains no URL, header, credential, source hash, UUID, account name, or email. Inspect at minimum `source_mapping_count`, `matched_mapping_count`, `unmatched_mapping_count`, `create_count`, `replay_count`, `update_count`, `conflict_count`, `reauthorization_required_count`, and `anomaly_count`.

`--apply` is explicit and refuses to run without a persistent owner-only checkpoint. An identical existing route is a zero-write replay. A missing route is created with one direct exact candidate, no groups and no credentials, then its returned topology is verified. A lost create response is safe to retry because MTC returns the exact equivalent route; the stable intent digest keeps the checkpoint valid. Resume revalidates every completed ordinal as an exact live replay before skipping it. A dry-run never overwrites an apply checkpoint.

An update needs exact route ID, `updated_at`, `grant_revision`, `history_and_references_reviewed: true`, and a separate owner-reviewed evidence SHA-256. Even then, the importer refuses to update a route with any existing credential grant, route-group relation, or provider-group relation. CAS conflicts and partial failures write only counts/digests to the checkpoint and stop.

HTTPS is mandatory by default. Cluster-internal HTTP requires all three conditions: the exact URL is stored in the digest-pinned owner manifest, the command uses that identical URL, and the operator explicitly adds `--allow-http-target`. This flag does not permit a URL different from the reviewed manifest.

The current implementation deliberately fails if either live routes or upstream accounts reaches the 100-row single-page boundary; it never treats a truncated page as complete. Extend it with tested keyset pagination before using it in a tenant at or above that boundary.

## Release gate

Do not run apply until the 23-entry source inventory, target upstream inventory, priority choices, anomaly disposition, rollback point, and dry-run counts have an owner approval reference. Route convergence must be followed by the separate exact key-policy import, legacy credential `/v1/models` checks, real text/multimodal requests, and history/charge verification. Copilot/Cursor reauthorization and any missing Claude/Codex/upstream account remain explicit blockers; this tool never claims continuity merely because some routes were created.
