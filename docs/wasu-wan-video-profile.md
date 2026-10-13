# Wasu Wan native video profile

An HTTP JSON account can explicitly select `video_api: "wasu-wan-v1"`, `video_models: ["wan3.0-video"]`, its existing `/v1` base URL and operator-approved exact `result_origins`. This source change does not configure any live account. Other models retain their normal protocols.

The existing `POST /v1/videos/generations` accepts `{ "model": "<authorized public model>", "input": { "input": { "prompt": "a cat running on grass", "media": [{ "type": "reference_image", "url": "https://example.test/reference.png" }] }, "parameters": { "resolution": "720P", "ratio": "16:9", "duration": 5, "prompt_extend": false, "watermark": false } } }`. Use a legitimately usable reference image and a stable `Idempotency-Key`. This is the verified five-second profile, not the supplier's global minimum or complete parameter space. The published generation schema describes the whole MTC input; the current portal parameters form does not construct this nested input, so use an API client.

The worker supplies the authorized upstream model, replaces `/v1` with `/api/v1/services/aigc/video-generation/video-synthesis`, sends `X-DashScope-Async: enable`, reads `output.task_id`, and queries `GET /api/v1/tasks/{task_id}` while verifying the exact task ID. Client query and content remain `GET /self/v1/generations/{job_id}` and its `/assets/{asset_id}` endpoint.

Only `second` prices are accepted. Five seconds are reserved; successful provider usage must report one video and consistent positive duration/output_video_duration within the reservation. Missing usage is not invented. Supplier points, actual invoice costs, currency conversion and free coverage remain independent of MTC settlement prices.

Existing network restrictions, stable-key asset ownership, encrypted provider references and cross-origin credential isolation apply. Repeatable asset GET access requires the existing explicit `provider_asset_reads_repeatable` declaration; absent that guarantee the worker fails uncharged. Unknown ACKs retain the existing delivery quarantine/reservation and never cause a new submit, even if a provider contribution advertises generic idempotency. No supplier cancellation contract is assumed. H3 and other suppliers are outside this profile.

Wan submission uses the existing no-retry HTTP client, including its URL, DNS pinning and proxy boundary. Its single bounded asset-validation GET additionally requires one unambiguous `Content-Type: video/mp4` response header before success or settlement; missing, conflicting and non-MP4 MIME declarations fail uncharged. This verifies the provider response MIME, not a complete media decode or playback.

Wire evidence: October 12 platform sample and the existing single successful supplier task. GHA fixtures use synthetic mock credentials/assets; no live requests or production credentials are involved.
