# Operator form pass — 2026-09-10

Source baseline: `c7d1e0bf8d0ca6e1b16105b8b4832c3203abb644`.
This is an implementation and baseline-observation receipt, not release acceptance.

## Actual Chromium observations

- Canonical `https://token-operator.k3s.onetwo.website/operator` returned 403 for
  routes, client credentials and service credentials. No network, proxy,
  allowlist or credential configuration was changed.
- The same three deployed screens returned 200 through an explicitly authorized
  localhost port-forward to the existing control service. These observations
  **do not establish canonical-host acceptance**.
- Only create disclosures were opened. No real objects were created or edited.
  The browser denied all mutations except the pre-authorized read-query paths;
  no denied mutation was attempted. No model calls, reset, OAuth refresh,
  rotation, preparation or consumption occurred.
- Client credential field order was routes, route groups, alias, currency,
  initial credit, policy fields, then principal. Identity fields were separated
  by unrelated configuration.
- Client credentials overflowed at 320/390/768/1024/1440/1920/2560 in light and
  dark. DOM geometry traced this to schema fieldsets' intrinsic minimum width.
- Routes overflowed at 1024 in light and dark: nested group editors retained
  180px + 280px columns inside a narrower card.
- Service credentials used a native multi-select for permission scopes.

Only field labels, control attributes and geometry were recorded. No secret
values, request/message content, traces or storage exports were retained.

## Implemented scope

- Shared grouped fieldsets and localized hints: credential identity → model
  access → balance/currency → policy; route identity → sources/model → access.
- Preserve server schema defaults and constraints. Route grant UUID arrays are
  owned by the typed selectors and no longer have duplicate raw-array editors.
- Schema forms use unique ID namespaces, first-error focus, synchronous
  duplicate-submit locks, disabled controls and an announced pending state.
- Client/service creation ignores stale-scope success and error responses.
- Route create/edit share a synchronous guarded submission path with scope
  checks, integer/range and candidate protocol checks. Route names are trimmed.
- Account autocomplete groups using authoritative provider metadata and can
  search provider, account name and stable account ID. Service scopes are
  searchable, keyboard-accessible multi-selection without granting defaults.
- Autocomplete Enter never implicitly submits its parent form; Escape and
  blur dismissal no longer race a delayed timer. Keyboard movement scrolls
  active options into view.
- Schema minimum sizing and group editor container queries address the two
  observed overflow causes.

## Verification and remaining gates

- `git diff --check` performed. No local product build, typecheck, or test run.
- Added CI-only Chromium mock coverage for both themes/all seven widths,
  grouping, validation focus, default preservation, double-submit prevention,
  stale-scope secrets, permission selection and route priority rejection.
  Updated existing multimodal credential selectors for unique form IDs.
- GitHub Actions must compile/typecheck/run all contracts; then deploy the exact
  verified release and repeat real-browser acceptance. No push or deployment
  occurred as part of this pass.
- Not covered here: full client policy/rename/routing edit ergonomics, source
  catalog query/value separation in `UpstreamModelCombobox`, pagination of
  authoritative model choices, all empty/error/retry and focus-return paths,
  canonical-host availability, complete site-wide navigation/layout acceptance.
- No price changes occurred. Any real price write still needs a separately
  approved authoritative-source/units/currency plan and exact endpoint scope.
