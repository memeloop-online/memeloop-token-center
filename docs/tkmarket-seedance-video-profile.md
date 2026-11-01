# tkmarket Seedance video-task adapter

This adapter implements tkmarket's native video-task wire dialect for the exact current catalogue IDs `seedance-2.0-idle-ts` and `seedance-2.5-idle-ts`. It does not equate them with historical `ts-seedance-*` aliases or select the Ark/Volcengine dialect from their names.

The adapter uses the existing asynchronous generation protocol registry and generation worker. Selection requires the existing HTTP JSON account's explicit `video_api: "tkmarket-seedance-v1"` and an exact entry in `video_models`. Other models retain their existing protocols. No account, route, price, grant or credential is changed by this source contribution.

The initial text-only input is `{ "prompt": "A blue cube slowly rotates on a white table.", "duration": 5, "resolution": "720p", "aspect_ratio": "16:9" }`. It rejects extra fields, model injection, conflicting duration spellings and reference inputs. This is the requested evaluation profile, not a claim about all provider capabilities. The public request remains `POST /v1/videos/generations` with the authorized public model and this object under `input`.

The worker injects the authorized exact upstream model, sends `POST /v1/video/generations`, validates the returned `id`, `model` and queued acknowledgement, and polls `GET /v1/video/generations/{id}`. It verifies the task identity in the `code: success` / `data` envelope and recognizes `IN_PROGRESS`, `SUCCESS`, and `FAILURE`. Success requires positive integer `total_tokens`, nonnegative integer `quota`, duration 5, ratio `16:9`, an HTTPS result URL, and an upstream `model_name` retained as provenance rather than an alias. Expiry is derived with checked arithmetic from `finish_time + execution_expires_after` when provided.

Submission uses the common no-retry HTTP client, common credential injection, durable unknown-delivery quarantine, ACK persistence and lease-fenced polling. It does not assume provider idempotency or cancellation. It never retries with another path after a possibly accepted POST. Asset origin checks use the existing common generation helper; capture, storage and audio contracts remain with PR707.

## Customer billing and supplier audit

Configure an explicit MTC generation price using the existing management API:
`POST /internal/v1/generation-prices/{currency}/{public_model}` with
`{"billing_unit":"million_tokens","price_per_unit":"<approved customer price per million tokens>"}`.
The price is in the key account's currency. This is an operator-controlled customer
price; the adapter does not install or derive one from supplier catalogue fees.
The existing job's `billing_unit_snapshot` and `micros_per_unit_snapshot` freeze
that price at admission. Later price changes cannot change the job's settlement.
`billed_units` and usage aggregates count native tokens, as pixel counts do for
megapixel pricing; `million_tokens` describes the price denominator. The existing
billing arithmetic rounds the final customer charge up to one currency micro.

This profile requires an explicitly authorized `metered_unlimited` key. The
supplier publishes an estimator (`fps: 24`, `extra_frames: 1`), not a guaranteed
upper bound, so `estimated_units: 0` means unbounded metered usage, with no prepaid
hold. The existing policy intentionally does not enforce prepaid budget/rate
limits. Prepaid keys are rejected before submission; no guessed hard token cap is
introduced. Exact terminal `total_tokens` settles through the existing usage
reservation, ledger, projection outbox and financial settlement feed.

Before result validation or asset probing, the worker persists a task-bound,
immutable terminal receipt in the existing job result. `result.provider_usage`
contains `task_id`, `status`, native `total_tokens`, supplier `quota`, checked
`supplier_usd_micros` (`quota * 2`), and implementation `provider_model_name`.
Missing or invalid evidence is `null` (unknown), never an invented zero. This
receipt is supplier-reported charge evidence, not an invoice. Running quota is a
hold and is not recorded as final cost. `result.customer_price` exposes the frozen
price denominator, micros and native usage unit. The supplier receipt survives
malformed terminal output, unavailable assets and uncharged failure. Receipt
replay must match the same job/task and values; a different receipt conflicts.

Success requires valid usage and an obtainable MP4 asset, then atomically commits
customer settlement and result metadata through the common idempotent terminal
transaction. Failure charges the customer zero while retaining any known supplier
cost independently. Unknown delivery remains quarantined with one submit identity.
Signed asset URLs stay encrypted in the existing provider reference and never
appear in public job JSON. Reads use the existing owned asset endpoint.

## HTTP account configuration

The existing HTTP JSON schema exposes `video_api: "tkmarket-seedance-v1"`.
Configuration requires a base URL ending exactly in `/v1`, a nonempty unique
`video_models` subset of `seedance-2.0-idle-ts` and `seedance-2.5-idle-ts`, nonempty
exact `result_origins`, and `provider_asset_reads_repeatable: true`. Other models
on the account keep their existing protocols. Configure an authorized generation
route and grant for each public model. The model catalogue publishes the closed
text-only input schema shown above. No live account, credential, route, grant or
price is changed by this source contribution.

## Primary evidence and verification boundary

The provider's [documentation application](https://www.tkmarket.ai/docs.html) at immutable asset [index-BFnvIM08.js](https://www.tkmarket.ai/assets/index-BFnvIM08.js) documents the singular paths, envelope, token count and quota conversion. Its [pricing catalogue](https://api.tkmarket.ai/api/pricing) identifies the two exact IDs and PER_TOKEN tiers; catalogue pricing is not an account invoice or MTC customer price. On 2026-10-31, an authenticated GET with an intentionally nonexistent task ID reached the singular query handler (`task_not_exist`), while the plural query path returned HTTP 404. This supports the documented query path; no paid POST was used to probe either path.

Existing GHA Cucumber fixtures exercise the actual MTC HTTP APIs, account validation,
route/grant, customer price, metered key, idempotent create, provider submit/ACK,
pending/terminal poll, usage/financial readback and repeatable output bytes for
both exact IDs. They change the price after admission, replay terminal settlement,
and reject different usage. Failure cases retain nonzero or unknown supplier
cost, missing tokens/output, wrong task identity and unknown ACK. Fixtures use
synthetic keys and bytes; they do not prove playable video. No paid video or
production deployment is performed by this source contribution. Real minimal
720p/5s acceptance follows source/CI acceptance in the parent's runtime window.

For MiniMax H3, the [official V2 create](https://platform.minimax.io/docs/api-reference/video-generation-v2-create.md) and [query](https://platform.minimax.io/docs/api-reference/video-generation-v2-query.md) contracts use `MiniMax-H3`, native `768P`, duration 5, `POST /v2/video_generation` and `GET /v2/query/video_generation/{task_id}`. Domestic `hailuo` is not a verified H3 mapping. The existing account review found no configured official MiniMax origin credential; the domestic model lists did not expose an exact H3 ID. This source change therefore does not register an H3 adapter under a guessed alias.
