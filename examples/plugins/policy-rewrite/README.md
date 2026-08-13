# Policy rewrite + OAuth provider example

This installable example demonstrates all contribution types in the `0.1`
contract:

- a post-auth traffic policy implemented by `plugin.wasm`;
- safe JSON request rewriting (`model` becomes `example-rewritten`);
- a declarative HTTP provider with API-credential and OAuth-credential schemas;
- an OAuth adapter whose tokens remain in Token Center's encrypted credential
  table.

For local development, mount `examples/plugins` read-only and set
`MTC_PLUGIN_DIR` to that mount. The checked-in `plugin.wasm` is generated from
the auditable `plugin.wat` using `wasm-tools` 1.252.0:

```sh
wasm-tools component embed wit/token-center.wit --world plugin \
  examples/plugins/policy-rewrite/plugin.wat \
  -o target/policy-rewrite.embedded.wasm
wasm-tools component new target/policy-rewrite.embedded.wasm \
  -o examples/plugins/policy-rewrite/plugin.wasm
wasm-tools validate --features component-model \
  examples/plugins/policy-rewrite/plugin.wasm
```

The WAT deliberately has no HTTP, KV, or logging capability. It can only
return its decision and rewrite. Production packages should request the
smallest exact-origin capability set they need.
