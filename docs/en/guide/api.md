# API overview

MTC provides model request interfaces and self-service queries scoped to the current client credential. See the repository's [OpenAPI specification](https://github.com/memeloop-online/memeloop-token-center/blob/master/openapi/openapi.yaml) for the complete public field and protocol definitions.

## Client interfaces

| Path family | Purpose |
| --- | --- |
| `/v1/*` | Model listing, text, image, audio, video, and other generation requests |
| `/self/v1/*` | Requests, statistics, sessions, and conversations for the current client credential |
| `/portal` | A deployment-provided self-service entry point, when enabled |

The public model interfaces follow common OpenAI and Anthropic protocols. Exact capabilities depend on the upstream services connected to the deployment and the routes granted to the credential.

## Versioning and errors

- Responses carry `X-MTC-API-Version: v1` to identify the API version.
- Errors use JSON and standard HTTP status codes and do not include provider credential material.
- New fields and endpoints can be added incrementally within v1; removing existing fields or behavior requires a public deprecation window.

## Retries and pagination

Write requests should follow the idempotency requirements in their endpoint documentation and reuse the same `Idempotency-Key` when retrying. List interfaces use the cursor returned by the service; clients should not infer the next page with an offset.

## Responses transport

`/v1/responses` supports HTTP and WebSocket transport. Clients should handle incremental events, normal terminal events, and error terminal events. Whether a disconnected request can be resumed depends on the client's idempotency strategy and upstream capabilities.

## Anthropic Messages and Claude Code gateways

MTC serves `POST /v1/messages` and `POST /v1/messages/count_tokens` through the same credential, route authorization, audit, billing, and upstream network policy used by the other model protocols. A Claude Code gateway can set `ANTHROPIC_BASE_URL` to the MTC endpoint and use an MTC client credential.

- `anthropic-version`, `anthropic-beta`, other `anthropic-*` headers, and `x-claude-code-*` session and agent identifiers remain available to an Anthropic-format upstream. Request fields stay open so newer tool, thinking, cache, and context-management fields can travel with their matching beta capabilities.
- Streamed Messages responses are relayed as `text/event-stream`, including Anthropic `event: ping` frames with `{"type":"ping"}` data. Ping frames are transport control events. `retry-after`, `retry-after-ms`, `x-should-retry`, and the `anthropic-ratelimit-*` response-header family remain available for retry and plan-limit handling.
- `GET /v1/models` supports Anthropic gateway discovery with `limit` from 1 through 1000 and an optional `before_id` or `after_id` cursor. Its response follows the Anthropic list shape and contains models with an active authorized Anthropic route for the current MTC credential.
- Error responses from an Anthropic-format upstream retain their status, response body, and retry/limit headers for the client. MTC stores the terminal request facts without retaining the upstream error body.

`/v1/messages/count_tokens` is available when the selected route's upstream supports the endpoint. Provider-specific request translation is supplied by the selected provider adapter; the core gateway keeps the Anthropic Messages envelope intact.
