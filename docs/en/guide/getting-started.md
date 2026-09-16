# Getting started

Memeloop Token Center (MTC) is a unified gateway for AI text, image, audio, and video requests. Clients connect through open protocols while MTC handles credentials, model routing, quotas and balance accounting, request history, and archiving.

## Surfaces

The same service image runs as `serve --role gateway|control|worker|all` and exposes three logical surfaces. This guide uses `https://mtc.example.com` as an example; use your deployment's actual address.

| Surface | Path | Purpose | Credential |
| --- | --- | --- | --- |
| Gateway | `/v1/*`, `/self/v1/*`, `/portal` | Client AI requests and self-service queries | `mtc_…` client credential |
| Control plane | `/internal/v1/*`, `/operator`, `/version` | Operations management and the Operator console | `mts_…` service credential |
| Probes | `/livez`, `/readyz` | Process health and dependency readiness checks | None |

Supported client protocols:

- OpenAI-compatible: `/v1/models`, `/v1/chat/completions`, `/v1/responses` (HTTP and WebSocket), `/v1/embeddings`, `/v1/audio/transcriptions`
- Anthropic-compatible: `/v1/messages`, `/v1/messages/count_tokens`
- Generation: `/v1/images/generations`, `/v1/videos/generations`, `/v1/generations`

## Make your first request

The steps below use curl against the control-plane API. You can perform the same operations with forms in the Operator console (`https://mtc.example.com/operator`). You need a supported provider `base_url` and an available model name. Addresses, credentials, and IDs below are fictional; use the `id` returned when creating an account and route, and the `key_id` returned when creating a credential.

### 1. Prepare a service credential

Management operations require an `mts_…` service credential. Obtain one through the onboarding flow on first deployment, then create additional credentials on the **Service credentials** page of the Operator console. This flow needs read/write access to upstreams, routes, credentials, and prices; grant everyday users only the permissions required for their work.

### 2. Add an upstream account

```bash
curl -X POST https://mtc.example.com/internal/v1/upstreams \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -d '{
    "tenant_external_id": "default",
    "name": "Example Provider Account",
    "driver": "http-json",
    "config": { "base_url": "https://api.provider-example.com" },
    "credential": { "type": "api_key", "value": "sk-example-upstream-key" }
  }'
```

The response `id` identifies the account; later endpoints call it `account_id`. For OAuth providers, use the login flow described in [Upstream accounts](upstreams.md).

### 3. Sync the model catalog

```bash
curl -X POST "https://mtc.example.com/internal/v1/upstreams/0193f2ab-7c1e-7000-8000-0000000000a1/models/sync?tenant_external_id=default" \
  -H "Authorization: Bearer mts_example_service_token"
```

Sync may return an in-progress status such as `already-syncing`, meaning the refresh continues in the background. Before creating a route, use `GET /internal/v1/upstreams/{account_id}/models` to confirm that the target upstream model appears in the catalog. The completed catalog is used for model-compatibility checks during routing.

### 4. Set model pricing

On the **Pricing** page in the Operator console, create pricing for the public model `example-chat`: choose USD or CNY and enter input/output token unit prices. A model without pricing cannot pass pre-request reservation checks, so complete this step before the first request.

### 5. Create a model route

```bash
curl -X POST https://mtc.example.com/internal/v1/model-routes \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 8f2a1c4e-0000-4000-8000-example0002" \
  -d '{
    "tenant_external_id": "default",
    "public_model": "example-chat",
    "upstream_model": "provider-chat-v2",
    "protocol": "openai",
    "upstream_account_ids": ["0193f2ab-7c1e-7000-8000-0000000000a1"]
  }'
```

`public_model` is the model name visible to clients; `upstream_model` is the name sent to the provider.

### 6. Create a client credential and grant the route

```bash
curl -X POST https://mtc.example.com/internal/v1/keys \
  -H "Authorization: Bearer mts_example_service_token" \
  -H "Content-Type: application/json" \
  -H "Idempotency-Key: 8f2a1c4e-0000-4000-8000-example0003" \
  -d '{
    "tenant_external_id": "default",
    "principal_external_id": "user-alice",
    "alias": "alice-dev",
    "initial_balance": "10.00",
    "route_ids": ["0193f2ab-7c1e-7000-8000-0000000000b2"]
  }'
```

`initial_balance` is the initial prepaid balance (default `0`; a request is rejected when the balance is zero). The response `key` field is the plaintext `mtc_…` credential. If it is lost, retrieve the current credential through the authorized copy interface described in [Credential management](credentials.md).

### 7. Make a request

```bash
curl -X POST https://mtc.example.com/v1/chat/completions \
  -H "Authorization: Bearer mtc_example_client_key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "example-chat",
    "messages": [{ "role": "user", "content": "Hello" }]
  }'
```

### 8. View usage

Clients can query their own request history and statistics at any time:

```bash
curl "https://mtc.example.com/self/v1/requests?limit=20" \
  -H "Authorization: Bearer mtc_example_client_key"
```

You can also use the self-service portal at `https://mtc.example.com/portal`.

## Next steps

- [Credential management](credentials.md): rotation, quota policies, and service credential scopes
- [Model routing](routing.md): multi-account routing, health and failover, and transport policy
- [Upstream accounts](upstreams.md): OAuth login, proxies, quota windows, and refresh
- [Requests and usage](requests.md): history queries, session metadata, and cost semantics
- [API overview](api.md): version policy, idempotency, pagination, and error conventions
