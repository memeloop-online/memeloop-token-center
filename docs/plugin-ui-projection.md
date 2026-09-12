# Plugin UI projection v1 (integration gate)

The new `PluginUiSlot` is an opt-in, core-owned renderer. It is deliberately not
mounted by production pages yet. The existing typed-data endpoint is not by itself
evidence that arbitrary plugin output is safe to expose through this contract.

## Server adapter required before mounting

The authenticated service-data handler may wrap a v1 projection in its existing
response envelope. Its adapter must enforce the following in order:

1. Resolve the current principal and tenant from authenticated context. Require
   the registered plugin, contribution, endpoint, and read capability; do not
   trust tenant/principal identifiers supplied by plugin output.
2. Execute only the registered endpoint under existing runtime budgets. Bound
   serialized projection responses to 256 KiB **before** JSON parsing, including
   on the browser loader. Apply a 10-second maximum read deadline.
3. Validate `schemas/plugin-ui-projection.schema.json`, exact requested plugin
   and slot identity, and HTTPS link origins against a core-owned exact-origin
   allowlist. Reject userinfo, whitespace, backslashes, malformed URLs, and
   control/bidi characters. No wildcard origins, arbitrary fetch URLs, HTML,
   Markdown, CSS, scripts, frames, action handlers, or asset downloads exist.
4. Project only fields the authenticated principal may read. Remove components
   with unauthorized data before serialization; omit denied slots altogether.
   There is no client-supplied permission field. Neither hidden DOM nor browser
   validation is authorization. Validate the final projected response again.
5. Audit request identity, plugin/slot/version, decision, bounded reason code,
   duration, and correlation ID. Never log component values, tokens, or raw
   plugin errors. Audit failures must follow the host's existing audit policy.
6. Use private/no-store responses or cache keys including tenant, principal,
   authorization revision, plugin revision, slot and locale. Recheck authorization
   on every read. Never share authorization-sensitive results across tenants.

## Frontend integration

Pass `load(signal)` from core code using an authenticated, fixed registered route;
the plugin never provides this callback or a request URL. The callback unwraps
only the validated projection. Supply localized `title`, loading/error/empty and
status messages, and a core-owned `allowedLinkOrigins` array (empty by default).
Metric values and plugin labels are bounded plain strings prepared by the server
for the requested locale, not browser-executed format expressions.

`scopeKey` must change on tenant, principal, credential/authorization revision,
or plugin revision. Scope and link-policy changes synchronously unmount old data,
abort in-flight loads and ignore late results. No raw credential belongs in the
projection, DOM, audit, or a shared cache. Revocations must invalidate the host's
scope key; the slot does not independently discover authorization changes.

Slots load once when visible, not when a catalog is listed. Each has a 10-second
deadline and independent error UI; exceptions and invalid responses do not take
down siblings. The host must bound registered slots per page and concurrency;
this module is not a global scheduling system. Unmounting aborts pending reads.

The closed 32-component grammar creates no remote executable or remote asset
surface. React renders text as text; links open with noopener/noreferrer and no
referrer. Semantic headings, descriptions, status text, visible keyboard focus,
inherited theme colors and wrapping accommodate mobile and assistive technology.
The browser contracts exercise the renderer independently of requests/overview.

## Remaining integration acceptance

Do not enable a production contribution until server tests prove cross-tenant
denial, field redaction, payload/deadline bounds, allowlist enforcement, audit
redaction, and revocation/cache behavior. Add endpoint/OpenAPI wiring only with
the service-data owner; this foundation does not claim those server checks exist.
