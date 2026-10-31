# tkmarket Seedance video-task adapter

This adapter implements tkmarket's native video-task wire dialect for the exact current catalogue IDs `seedance-2.0-idle-ts` and `seedance-2.5-idle-ts`. It does not equate them with historical `ts-seedance-*` aliases or select the Ark/Volcengine dialect from their names.

The adapter uses the existing asynchronous generation protocol registry and generation worker. Selection requires the existing HTTP JSON account's explicit `video_api: "tkmarket-seedance-v1"` and an exact entry in `video_models`. Other models retain their existing protocols. No account, route, price, grant or credential is changed by this source contribution.

The initial text-only input is `{ "prompt": "A blue cube slowly rotates on a white table.", "duration": 5, "resolution": "720p", "aspect_ratio": "16:9" }`. It rejects extra fields, model injection, conflicting duration spellings and reference inputs. This is the requested evaluation profile, not a claim about all provider capabilities. The public request remains `POST /v1/videos/generations` with the authorized public model and this object under `input`.

The worker injects the authorized exact upstream model, sends `POST /v1/video/generations`, validates the returned `id`, `model` and queued acknowledgement, and polls `GET /v1/video/generations/{id}`. It verifies the task identity in the `code: success` / `data` envelope and recognizes `IN_PROGRESS`, `SUCCESS`, and `FAILURE`. Success requires positive integer `total_tokens`, nonnegative integer `quota`, duration 5, ratio `16:9`, an HTTPS result URL, and an upstream `model_name` retained as provenance rather than an alias. Expiry is derived with checked arithmetic from `finish_time + execution_expires_after` when provided.

Submission uses the common no-retry HTTP client, common credential injection, durable unknown-delivery quarantine, ACK persistence and lease-fenced polling. It does not assume provider idempotency or cancellation. It never retries with another path after a possibly accepted POST. Asset origin checks use the existing common generation helper; capture, storage and audio contracts remain with PR707.

## Billing integration required before availability

The wire adapter is implemented, but admission deliberately rejects every existing generation price unit before reservation, archival or provider submission. The current MTC generation settlement takes a single billed-unit count and cannot durably preserve both native provider tokens and supplier quota alongside an independent customer price. This contribution does not mislabel tokens as seconds/jobs, invent a token estimate or install a synthetic price. It also leaves HTTP account configuration schema changes to that owner: the profile is not yet selectable in the operator form.

The billing owner needs to connect these exact adapter outputs:

- `ProviderUsage.total_tokens`: the terminal provider's native video billing token count; positive integer on success.
- `ProviderUsage.quota`: independent terminal supplier charge in quota units. Running responses are holds, not final charges. Zero is valid and does not imply zero tokens.
- `ProviderUsage.supplier_usd_micros`: checked `quota * 2`, because the provider documents 500,000 quota per USD. This is supplier cost evidence, never an MTC customer price or invoice.
- `CompletedVideo`: validated result URL, upstream implementation model name and optional expiry, associated with the already-persisted task ID.
- `QueryResult::Failed { quota }`: failed-task supplier charge evidence must survive separately from the MTC customer's terminal policy.

Required shared integration: native token reservation using a documented bound and immutable customer price; durable receipt persistence keyed by MTC job/provider task; atomic idempotent settlement with independent supplier costs; then connect the completed receipt to existing provider-asset success and remove the admission guard. Do not merely delete the guard or substitute `total_tokens` for `billed_units` in the current whole-unit price contract. Existing account credentials, prices and quotas remain unchanged until the parent coordinates release and actual tests.

The HTTP configuration owner needs to add this closed profile value, enforce `/v1`, exact allowed model IDs and explicit result origins. PR707 needs no new capture API for this adapter; after billing is connected, the existing provider-asset helper provides the deliverable. Repeatable reads, cross-origin credential isolation, bounded range validation and private signed references remain shared contracts.

## Primary evidence and verification boundary

The provider's [documentation application](https://www.tkmarket.ai/docs.html) at immutable asset [index-BFnvIM08.js](https://www.tkmarket.ai/assets/index-BFnvIM08.js) documents the singular paths, envelope, token count and quota conversion. Its [pricing catalogue](https://api.tkmarket.ai/api/pricing) identifies the two exact IDs and PER_TOKEN tiers; catalogue pricing is not an account invoice or MTC customer price. On 2026-10-31, an authenticated GET with an intentionally nonexistent task ID reached the singular query handler (`task_not_exist`), while the plural query path returned HTTP 404. This supports the documented query path; no paid POST was used to probe either path.

CI fixtures cover both exact model IDs through synthetic HTTP submit, queued ACK, in-progress poll, completed poll, output bytes and native token/quota evidence; negative contracts cover identity, input, usage, expiry and admission. The output bytes are synthetic and do not prove a playable video or MTC customer settlement. No production video has been submitted by this contribution.

For MiniMax H3, the [official V2 create](https://platform.minimax.io/docs/api-reference/video-generation-v2-create.md) and [query](https://platform.minimax.io/docs/api-reference/video-generation-v2-query.md) contracts use `MiniMax-H3`, native `768P`, duration 5, `POST /v2/video_generation` and `GET /v2/query/video_generation/{task_id}`. Domestic `hailuo` is not a verified H3 mapping. The existing account review found no configured official MiniMax origin credential; the domestic model lists did not expose an exact H3 ID. This source change therefore does not register an H3 adapter under a guessed alias.
