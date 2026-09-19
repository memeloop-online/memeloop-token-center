# Plugins overview

MTC plugins are versioned WebAssembly components (Component Model) for extending routing, provider adapters, OAuth flows, and request policies within explicit boundaries. Each package contains a `plugin.json` manifest, an optional `.wasm` component, a README, and an icon.

Authoritative contracts:

- [WIT interface definition: `token-center.wit`](https://github.com/memeloop-online/memeloop-token-center/blob/master/wit/token-center.wit)
- [`plugin.json` manifest schema](https://github.com/memeloop-online/memeloop-token-center/blob/master/schemas/plugin-manifest.schema.json)

## Capability model

The manifest's `capabilities` determine what a plugin can do:

| Capability | Meaning |
| --- | --- |
| `log` | Emit bounded host log events |
| `kv` | Use key/value storage isolated to the plugin namespace |
| `http` | Access only HTTPS origins listed in the manifest |

The host bounds fuel, memory, and execution time for every call. A plugin cannot see upstream credentials or bypass model permissions, balances, rate limits, or audit boundaries.

## Extension points

- Traffic policy: make bounded decisions or rewrites after authentication.
- Provider components: supply model catalogs, request preparation, and response normalization for a provider.
- OAuth adapters: declare provider-specific authorization-code or device-code flows.
- Group routing: order already authorized candidates and provide bounded health advice; see [group routing](routing.md).

See [plugin development](development.md) for implementation, manifest, and OCI package details.

## Security boundary

Plugin requests use identities and permissions produced by the core; sensitive credentials are used only between the core and a provider. When a plugin fails, the core falls back to native behavior, and in-flight requests keep the plugin snapshot captured at admission.

Plugins extend product capabilities without changing client-visible authorization. Installation, approval, and activation are handled by deployment administrators according to their release policy.
