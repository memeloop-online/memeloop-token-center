# Plugin runtime resources

Traffic hooks do not have a fixed byte limit for request JSON, configuration JSON,
or rewritten request JSON. The host does not configure a Wasmtime linear-memory
byte ceiling on traffic, wire-shim, provider, group-routing, or service-data stores.
HTTP bodies fetched by request-path plugins have no host byte ceiling. Service-data
endpoints retain their explicitly declared `max_body_bytes` and `timeout_millis`.

These components use the wasm32 canonical ABI. Each 32-bit linear memory has an
address space of at most 4GiB (65,536 pages of 64KiB); a guest may declare a smaller
maximum. Input strings, allocator state and guest data share that address space.
Allocation can also fail because the host cannot supply memory. These are runtime
allocation failures, not an application traffic-input limit or a reason to report
`plugin_input_too_large`. Wasmtime's default linear-memory policy is documented in
[StoreLimitsBuilder](https://docs.wasmtime.dev/api/wasmtime/struct.StoreLimitsBuilder.html#method.memory_size),
and the address-space bound in the [WebAssembly specification](https://webassembly.github.io/spec/core/valid/types.html#memory-types).

The existing `MTC_PROXY_MEMORY_BUDGET_BYTES` service option controls aggregate
gateway memory admission. Request-path plugin serialization and rewrite parsing
participate in that shared capacity; exhausted capacity still reports a service
capacity error. This setting is separate from a per-plugin or per-document byte
ceiling. This change does not increase the service setting or deployment resources.
Provider adapters also retain their declared response-body contracts.

Epoch deadlines still interrupt runaway guests. Raw runtime calls retain sanitized
execution and validation errors. The optional lifecycle layer skips unavailable
hooks, continues the remaining chain, and honors explicit policy denials. Capability
grants, outbound destination validation, package identity checks, model/route
authorization, and store instance/table counts remain enforced.

The regression suite synthesizes large inputs and configuration in memory and
large rewrites inside a small Wasm module. Its memory-growth probe touches only the
last byte beyond the old 512MiB ceiling. These checks establish that old application
thresholds are gone; real workload memory peaks require runtimeowner measurements.
