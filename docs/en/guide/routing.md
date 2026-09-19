# Model routing

Model routes map a client-visible public model name to one or more upstream models. Deployment administrators configure routes and credential permissions; clients use the public name without needing to know provider names or account selection.

![Credentials receive authorization through routes or route groups; routes select accounts and provider groups, then health and model-compatibility checks produce dispatchable candidates.](/diagrams/routing.svg)

## What clients see

The `model` field in a request uses a public model name such as `example-chat`. When a request enters MTC, the current permissions select available upstream candidates, and the deployment can try another candidate when one fails.

Routes can provide:

- One public name backed by models from multiple providers;
- Different model ranges for different applications or credentials;
- Common checks for model compatibility, health, and request budgets;
- Bounded failover when a candidate is unavailable.

## How routing affects requests

Routing and health state are snapshotted when a request enters the gateway. An in-flight request keeps its own snapshot; later configuration changes do not add candidates or extend its send deadline. A response stream that has been admitted is not cut short by a later route change.

Clients can use `/v1/models` to list the public models available to the current credential. A model list does not guarantee that every candidate is available for every request; upstream health and request constraints still apply.

## Plugin routing

Plugins can provide bounded ordering or health suggestions for already authorized candidates. A plugin cannot read upstream credentials, expand model permissions, or bypass balance, rate-limit, and audit boundaries. See [plugin group routing](../plugins/routing.md) for the protocol.
