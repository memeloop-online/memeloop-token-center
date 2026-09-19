# Getting started

Memeloop Token Center (MTC) provides one endpoint for text, image, audio, and video requests. This guide starts with a client credential that has already been issued and shows how to connect, send a request, and view your usage.

## Connect a client

Set the client's service URL to the MTC address provided by your deployment, such as `https://mtc.example.com`, and send the client credential in the `Authorization: Bearer` header. Keep credentials on the server or in a controlled local development environment; do not commit them to a repository.

MTC supports these client protocols:

- OpenAI-compatible: `/v1/models`, `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/audio/transcriptions`
- Anthropic-compatible: `/v1/messages`, `/v1/messages/count_tokens`
- Generation jobs: `/v1/images/generations`, `/v1/videos/generations`, `/v1/generations`

Available models and interfaces depend on the permissions on the credential and the upstream services connected to the deployment.

## Make your first request

The following example uses the OpenAI-compatible interface. The address, model name, and credential are placeholders:

```bash
curl -X POST https://mtc.example.com/v1/chat/completions \
  -H "Authorization: Bearer mtc_example_client_key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "example-chat",
    "messages": [{ "role": "user", "content": "Hello" }]
  }'
```

Use `/v1/models` to list models available to the current credential:

```bash
curl https://mtc.example.com/v1/models \
  -H "Authorization: Bearer mtc_example_client_key"
```

## View your requests and usage

Client credentials can query their own data through `/self/v1/*`:

```bash
curl "https://mtc.example.com/self/v1/requests?limit=20" \
  -H "Authorization: Bearer mtc_example_client_key"
```

Available views include requests, statistics, sessions, and conversations. Results contain only data visible to the current credential.

## Next steps

- [Client credentials](credentials.md): store, rotate, and use credentials safely
- [Model routing](routing.md): understand model names, permissions, and failover
- [Upstream accounts](upstreams.md): see how deployments connect model providers
- [Requests and usage](requests.md): read request records, sessions, and cost semantics
- [API overview](api.md): review public endpoint families and transport conventions
