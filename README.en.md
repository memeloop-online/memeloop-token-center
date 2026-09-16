# Memeloop Token Center

[简体中文](README.md) · English

Memeloop Token Center (MTC) is a team-oriented AI gateway: manage model services, client credentials, routing permissions, and usage through one entry point.

See the complete product documentation on the [documentation site](https://memeloop-online.github.io/memeloop-token-center/en/).

## One gateway for every model

Configure your clients' service URL and API key to use MTC, then call any model you have been authorized to access. Interfaces include:

- OpenAI-compatible: `/v1/models`, `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`
- Anthropic: `/v1/messages`, `/v1/messages/count_tokens`
- Audio transcription: `/v1/audio/transcriptions`
- Generation jobs: `/v1/images/generations`, `/v1/videos/generations`, `/v1/generations`

Available interface capabilities and models depend on the connected upstreams and route configuration. See the [product documentation](https://memeloop-online.github.io/memeloop-token-center/en/) for usage details.

## Routing and permissions

- Model routes map public model names to one or more upstream accounts, providing unified account selection and failover management.
- Routes can be granted individually to client credentials; clients can access only authorized models and their own `/self/v1/*` views.
- Upstream accounts support API key and OAuth connections.
- Tenant isolation: credentials, accounts, routes, and usage are all managed within tenant boundaries.

## Usage and requests

- Before a request is sent, MTC reserves quota, balance, and price limits; after completion, it settles against the usage actually reported.
- The management plane provides a live request stream, session views, usage analysis, upstream remaining-quota and availability observations.
- Clients can self-service their credential information, request records, statistics, and sessions.
- Session and text-request records can be viewed and replayed.

## Plugins

Extend model services, OAuth login, traffic policies, and request rewriting with WebAssembly plugins. See the [plugin development documentation](https://memeloop-online.github.io/memeloop-token-center/en/plugins/), which describes the interfaces, capabilities, and development workflow.

## Product interface

The screenshots below come from a real running management interface. Account names, email addresses, credential aliases, domains, IDs, and other identity information have been replaced with example text; the statistics remain real.

![Overview: an operational monitoring snapshot, upstream remaining quota, and account/model combinations](docs/public/images/overview.png)

![Requests: a live request stream with per-request usage, cost, and status](docs/public/images/requests.png)

![Upstream services: account connections, route counts, and recent availability](docs/public/images/providers.png)
