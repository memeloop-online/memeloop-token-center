# Plugin UI projection v1

Installed contributions opt in with `renderer: "typed_data_v1"` and
`presentation: "projection_v1"`. Existing `operator.overview.card` and
`operator.sidebar.tab` registrations render through `PluginUiSlot` inside the
real Operator page. Health-intelligence and generic JSON presentations remain
compatible. No second navigation registry or data endpoint is introduced.

## Data contract

The existing authenticated
`/internal/v1/plugins/{plugin_id}/data/{endpoint_id}` route checks `plugins:read`,
the endpoint's required scope and management tenant before reading service data.
It retains the existing bounded fetch, endpoint schema, cache and fallback rules.
Projection feeds additionally validate the bundled projection schema, installed
plugin/contribution identity and link origins against the manifest's approved
HTTP capabilities. Invalid projections produce a generic upstream error.
Responses are private/no-store, with authorization rechecked on each read.

`slot_id` is the contribution's `id`, not its placement name. A feed contains:

```json
{"schema_version":1,"plugin_id":"dashboard","slot_id":"summary","components":[{"kind":"metric","label":"Requests","value":"42"}]}
```

Components may be text, metric, status or HTTPS link, with at most 32 per slot.
The browser renders bounded plain text and local components. Link origins come
from the installed manifest, never from the projection itself. The authenticated
loader unwraps only `response.data`; it does not execute a guest URL or script.

## Scope and rendering

Credential, tenant, manifest revision and endpoint changes synchronously replace
the renderer boundary. Pending reads are aborted; late results cannot restore the
previous scope. Slots fetch on visibility and have a 10-second read deadline with
independent loading/error states. A denied tenant never retains prior-tenant data.
The existing server caps registered contributions and response body size.
This adds no installation, live revision activation or production toggle.

## Verification

CI runs backend projection shape/identity/link-grant checks, existing service-data
authorization tests, registry page/card checks, isolated mobile-theme renderer
checks and a real Operator plugin-route browser test. The latter follows the
actual remembered service credential, manifest registry, authenticated data route
and tenant selector, then checks a denied-tenant transition. No live supplier,
production or OAuth requests are used. Local checks are formatting/diff only.
