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
