# Plugin development

This page covers the minimum end-to-end loop for an MTC plugin: implement WIT exports → write `plugin.json` → package and sign as OCI → have an operator install and publish it. The authoritative interface is [token-center.wit](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit) (current package version `memeloop:token-center@0.2.0`) and the [manifest schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json).

## WIT ABI

In the `plugin` world, a component imports host capabilities and exports extension points:

```wit
interface host {
  log: func(level: string, message: string);
  kv-get: func(key: string) -> result<option<list<u8>>, string>;
  kv-put: func(key: string, value: list<u8>) -> result<_, string>;
  http-request: func(method: string, url: string, headers-json: string, body: list<u8>) -> result<list<u8>, string>;
}
```

A component can export two interfaces (enabled by manifest declarations):

- `traffic-policy.post-auth(context, request-json) -> decision`: a bounded post-auth traffic-policy/request-rewrite hook.
- `upstream-provider`: `list-models`, `quote`, `prepare`, and `normalize` for a buffered upstream Provider adapter.

Key records:

```wit
record request-context {
  tenant-id: string, principal-id: string, key-id: string,
  protocol: string, model: string, config-json: string,
}
record decision {
  allow: bool, reason: option<string>, model: option<string>,
  upstream-account-id: option<string>, request-json: option<string>,
}
record metering {
  currency: string, amount: string,
  input-tokens: u64, output-tokens: u64, estimated: bool,
}
```

## Traffic-policy example

Example `decision` returned by `post-auth`:

```json
{
  "allow": true,
  "reason": "matched allowlist",
  "model": null,
  "upstream_account_id": null,
  "request_json": null
}
```

- A non-empty `model` means request rewriting; the rewritten model still goes through route-authorization checks again.
- `upstream_account_id` is only a preference ordering within the authorized set; it cannot add or override authorization.
- `reason` and `log` messages are untrusted metadata: they are not copied verbatim into logs or error responses. Do not use them to communicate with users, and never put them in prompts or credentials.

## Provider component (`buffered-v1`)

An executable Provider declares `component_adapter.api_version = "buffered-v1"` and `max_response_bytes` (≤ 4 MiB) in its manifest. The interaction order is:

1. The host gives `prepare` normalized, credential-free request JSON. The component returns a buffered envelope containing a safe method, same-origin relative path, non-sensitive headers, and a base64 body, and asserts `streaming=false`.
2. The host validates the envelope, pins the destination URL, applies SSRF/DNS protection, **injects** that stable account's API/OAuth credential only after the call returns, sets a timeout, and reads a bounded response.
3. The host gives `normalize` a credential-free response envelope. The component returns a public-protocol response and token usage; usage is used only for settlement against the core price table.

Components never see credentials. A `stream=true` declaration, cross-origin path, sensitive header, size violation, or component trap fails closed and does not fall back to a built-in driver.

## OAuth declaration

A Provider can declare `oauth_adapter` (`api_version: "oauth-adapter-v1"`, `flow_kind: "cursor_pkce"`, with `login_url`, `poll_url`, and `refresh_url`), which the control plane executes through the versioned PKCE protocol. It can also declare `authorization_code_pkce` for the generic authorization-code flow. In both cases, tokens enter only the core encrypted credential table; components and plugin KV never receive tokens.

## Minimal manifest

```json
{
  "id": "example-policy",
  "version": "1.0.0",
  "wit_version": "0.2.0",
  "capabilities": [
    { "kind": "log" },
    { "kind": "kv" },
    { "kind": "http", "allowed_origins": ["https://plugin-api.example.com"] }
  ],
  "contributions": {
    "traffic_policy": true,
    "request_rewrite": true,
    "configuration": {
      "schema": {
        "type": "object",
        "additionalProperties": false,
        "properties": {
          "mode": { "type": "string", "enum": ["safe", "fast"] }
        }
      },
      "default": { "mode": "safe" }
    },
    "providers": []
  }
}
```

Rules:

- `id` / provider IDs are 1–64 lowercase letters, digits, or hyphens; `version` is SemVer; `wit_version` must be compatible with `0.2.x`; unknown manifest fields are rejected.
- A declarative-only Provider/OAuth package can set `"wasm": null`; a package declaring `traffic_policy` must include a component.
- Configuration Schema uses a restricted declarative subset of Draft 2020-12: bounded local `#/*` references are allowed, remote/file references and `writeOnly` are forbidden, and size, depth, and node counts are bounded.
- `http` host calls allow only `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, and `DELETE`, and only declared origins; requests and responses are each bounded.

## OCI packaging and signing

Plugins are distributed as OCI artifacts with one file per layer (no tar layers):

| Content | Media type |
| --- | --- |
| config | `application/vnd.memeloop.token-center.plugin.config.v1+json` (`{"format_version":1}`) |
| `plugin.json` | `application/vnd.memeloop.token-center.plugin.manifest.v1+json` |
| Wasm (at most one) | `application/vnd.wasm.content.layer.v1+wasm` |
| README, icons, and so on | `application/vnd.memeloop.token-center.plugin.asset.v1` |

The artifact type is `application/vnd.memeloop.token-center.plugin.v1`. Publish and sign it with standard tools (the repository address is a fictional example):

```bash
printf '{"format_version":1}' > artifact-config.json
oras push --artifact-type application/vnd.memeloop.token-center.plugin.v1 \
  --config artifact-config.json:application/vnd.memeloop.token-center.plugin.config.v1+json \
  ghcr.io/example/token-center-plugins/example-policy:1.0.0 \
  plugin.json:application/vnd.memeloop.token-center.plugin.manifest.v1+json \
  plugin.wasm:application/vnd.wasm.content.layer.v1+wasm \
  README.md:application/vnd.memeloop.token-center.plugin.asset.v1
digest="$(oras resolve ghcr.io/example/token-center-plugins/example-policy:1.0.0)"
cosign sign --key cosign.key "ghcr.io/example/token-center-plugins/example-policy@${digest}"
```

Installation references must pin a digest (never a tag). The operator installation policy configures a trusted-repository allowlist and Cosign public keys; artifacts with a failed signature or an unallowlisted source cannot be installed.

## Boundaries

- The extension points listed here are the complete current ABI: streaming request hooks, arbitrary JavaScript UI, and plugin-created accounts/routes do not exist. Do not design around them as future features.
- Group routing uses the separate `group-routing-plugin` world; see [Group routing](routing.md). Operator interface extensions are described in [Operator UI](operator-ui.md).
