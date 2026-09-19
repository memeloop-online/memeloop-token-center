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
