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

## Generation settings for Codex accounts

Codex accounts connected through OAuth accept text Chat Completions and Responses requests. MTC translates the request format, but Codex does not support every generation parameter from these APIs.

### Choose how unsupported parameters are handled

Administrators can configure chat controls and output limits separately in the Codex account's edit page. When using the configuration API, the corresponding fields are `transport_policy.chat_controls` and `transport_policy.responses_output_limits`.

| Setting | How requests are handled | When to use it |
| --- | --- | --- |
| Use upstream defaults (`provider_default`, the default) | MTC validates parameter format, removes sampling controls and output-limit hints that Codex cannot apply, and lets Codex choose the generation behavior. | Your client includes these parameters, but your application does not require them to take effect. |
| Strict validation (`strict`) | Chat controls accept only values that preserve default generation behavior; output-limit hints are rejected. Existing strict settings are not automatically changed to the default. | You want the client to receive an error for unsupported settings rather than continue with upstream defaults. |

Using upstream defaults **does not guarantee** your requested `temperature`, `top_p`, penalties, `seed`, `stop`, or output length. If your application depends on these controls, choose an authorized model route that supports them.

If a request reports an unsupported parameter, have the client omit that parameter. If the application requires it, switch to a route that supports it.

### Output limits and cost

`max_tokens`, `max_completion_tokens`, and `max_output_tokens` are not sent to Codex. The default setting accepts one positive integer hint within the supported range, but **does not cap output at that value**. For example, setting 16 does not restrict generation to 16 tokens. Do not send conflicting limit fields, null or invalid values, or your own reservation metadata.

A smaller limit hint does not reduce the quota MTC reserves using the operator's model bound. Final charges use observed upstream usage; cancelling a request does not guarantee that upstream generation stops or that incurred charges disappear. Unknown usage is not treated as zero. If you need an enforced output limit, including for cost control, choose a route that supports one instead of relying on Codex compatibility hints.
