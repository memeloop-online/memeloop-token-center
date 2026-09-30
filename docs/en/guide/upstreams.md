# Upstream accounts

![Upstream accounts with routes, availability, and quota refresh controls. Identity details are replaced; the interface is shown in Chinese.](/images/providers.png)

An upstream account is the configuration unit MTC uses to connect to a model provider. Deployment administrators handle connections and permissions; clients use the capabilities exposed through public model names.

## Connection model

An upstream can use an API key, OAuth, or another provider-supported connection method. MTC tracks connection state and maintains a model catalog for route compatibility checks.

Public account information includes connection state, available models, route relationships, and bounded quota observations. Credential material does not appear in client requests or public pages.

## Model catalogs and routes

The model catalog confirms compatibility between public model names and provider models. After an administrator completes synchronization and routing, clients use `/v1/models` and the public models granted to their credential.

When an upstream is unavailable, routing can select another authorized candidate based on health state. An in-flight request keeps the candidate snapshot captured when it entered the gateway.

## Quota and availability

Quota observations follow the windows and timestamps supplied by each provider. Unknown quantities remain unknown; they are not displayed as zero or full. Reading an observation is read-only and does not refresh credentials or send a model request.

Providers expose different data ranges. Clients should use request results and the current state shown by their deployment as the source of truth.

## Codex OAuth output-limit compatibility

The Codex OAuth route translates text Chat Completions to the Codex Responses transport. The native Codex request does not send `max_tokens`, `max_completion_tokens`, or `max_output_tokens` to that transport. These fields cannot be treated as enforced generation caps.

By default, `transport_policy.chat_controls: strict` rejects Chat requests with any of those output-limit fields. `transport_policy.responses_output_limits: strict` does the same for Responses. The error identifies the unsupported hard-limit semantics. If an application requires a hard cap, use an authorized route that supports and enforces it; do not enable compatibility merely to suppress the error.

An administrator may explicitly set `chat_controls: provider_default` for Chat and/or `responses_output_limits: provider_default` for Responses on the Codex account. In this mode MTC validates exactly one positive bounded integer limit, removes it from the upstream request, and lets Codex choose the actual output length. Null, invalid, conflicting, or client-supplied reservation metadata is rejected. This is a compatibility hint, **not** a 16-token (or other) hard limit. Authorization and quota admission still apply. MTC reserves using the operator's trusted model bound, never the smaller client hint, and settles once against observed upstream usage; unknown usage and client cancellation retain their existing conservative settlement behavior.

After enabling the policy, verify a synthetic authorized request to `/v1/responses` with `max_output_tokens: 16` and a synthetic Chat request with `max_tokens: 16`. Confirm an upstream success, a single settled reservation, provider-reported usage, and that neither field is sent on the Codex wire. This is not a replay of any private WorkBuddy request.
