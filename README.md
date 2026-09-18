# Memeloop Token Center

[简体中文](README.zh-CN.md) · English

Memeloop Token Center (MTC) is a multi-model AI gateway for teams. It brings upstream accounts, OAuth and API keys, model routing, client credentials, live requests, usage, and cost into one operational workspace.

[Read the documentation](https://memeloop-online.github.io/memeloop-token-center/en/) · [Get started](https://memeloop-online.github.io/memeloop-token-center/en/guide/getting-started) · [Build a plugin](https://memeloop-online.github.io/memeloop-token-center/en/plugins/)

## What MTC brings together

- **One model entry point:** OpenAI-compatible, Anthropic, audio transcription, image, video, and generation-job APIs.
- **Routing and access:** reusable routes connect public model names to authorized upstream accounts and client credentials.
- **Operational visibility:** live requests, sessions, token usage, local settlement, upstream quota, and availability share one interface.
- **Tenant boundaries:** credentials, accounts, routes, and usage stay organized by tenant.
- **Wasm extensions:** versioned plugins add routing policies, OAuth adapters, protocol support, and operator views.

## Product interface

These screenshots come from the running product. Identity details use example values, while the operational layout and statistics remain representative.

[![Operations overview with request trends, usage, cost, and upstream quota](docs/public/images/overview.png)](https://memeloop-online.github.io/memeloop-token-center/en/guide/upstreams)

[![Live request stream with model, credential, token usage, cost, and status](docs/public/images/requests.png)](https://memeloop-online.github.io/memeloop-token-center/en/guide/requests)

[![Upstream service catalog with account connections, models, and route relationships](docs/public/images/providers.png)](https://memeloop-online.github.io/memeloop-token-center/en/guide/routing)
