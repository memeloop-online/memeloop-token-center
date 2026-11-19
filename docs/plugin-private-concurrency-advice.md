# Private concurrency observations

The installed `mtc-health-intelligence` operator page can read the existing
example-plugins Node concurrency companion through a private host adapter.
The public three-source health snapshot and its Wasm collector are unchanged.
This adapter does not run experiments, generate inference requests, or change
account limits or routing.

Configure the control service through its existing secret/config injection:

| Setting | Value |
| --- | --- |
| `MTC_PLUGIN_ADVICE_ORIGIN` | Companion HTTPS origin, without a path, query, fragment or URL credentials. HTTP is supported only for a loopback sidecar. |
| `MTC_PLUGIN_ADVICE_TENANT` | Exact tenant configured by the companion's `MTC_TENANT_EXTERNAL_ID`. |
| `MTC_PLUGIN_ADVICE_READ_TOKEN` | Companion's dedicated `MTC_ADVICE_READ_TOKEN`, never its core service token. |

No setting is accepted from browser input, plugin configuration or the signed
public manifest. The host configuration serializer omits all three fields,
and its debug output omits the token. Use the companion's existing deployment
contract for its separate core read credential. No database pool is created.

`GET /internal/v1/plugins/mtc-health-intelligence/private-data/concurrency-advice`
accepts only optional `tenant_external_id`. The host resolves the caller's
authorized tenant, checks the active installed plugin, and requires
`plugins:read`, `providers:read` and `requests:read` before fetching private
data. A global operator must select a tenant. The authorized tenant must
exactly match the host configuration; a mismatch never contacts the companion.
Other tenants cannot discover the configured tenant. Responses are
`private, no-store` and never enter the global plugin service-data cache.

The host sends only the dedicated read token to the fixed companion path.
Redirects and automatic retries are disabled. The response must identify the
same tenant, the `health_concurrency_advice_v1` contract, manual-only application,
and disabled experiments. Upstream bodies and transport errors are not exposed
on failure. Normal states distinguish selecting a tenant, missing permission,
an unconfigured integration, and temporary unavailability without breaking
the public health page.

The read has a four-second deadline and an 8 MiB encoded JSON body ceiling,
consistent with the companion's existing bounded read contract. These limits
apply only to private operator snapshots, never customer inference requests.
A separate 64 MiB process-local memory domain admits actual retained chunks
and JSON expansion. Its lease follows the response bytes until downstream
consumption or cancellation; it does not consume the inference memory pool.
Oversize, saturated or invalid reads show temporary unavailability.

The page shows sample and observation-window progress, current and sampled
peak load, and observed ranges only for fresh manual-review results. Evidence
details retain cohorts, success/latency/TPS estimates and bounds, coverage,
blockers and limitations. Missing numbers stay unknown. Changing the selected
tenant immediately hides the old tenant's data, including pending reads.
Refresh only reads the companion cache.

Deployment requires the separately reviewed example-plugins PR19 companion,
its tenant-scoped core credential, and this host configuration. A merged host
adapter alone does not prove companion deployment, actual installed collection,
or sufficient production samples for a recommendation.
