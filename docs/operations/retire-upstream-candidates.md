# Retire upstream accounts without deleting route authorization

Use the product control API, never SQL force/cascade or a broad prune.
Retirement deliberately makes affected models unavailable when no remaining
eligible native candidate exists; it does not substitute a different model.

1. Retain a private, no-secret inventory of the exact account UUIDs, revisions,
   route candidates, all route/key/group grants, and request/generation identity
   counts. Confirm the intended account names and tenant immediately before
   every change.
2. Disable each affected route with its current `expected_updated_at`. Retain
   its identity, model, authorization and historical associations.
3. Disable the precisely selected upstream accounts using their existing
   optimistic-concurrency status API. Do not rotate credentials or change
   config, proxy, tenant, balances or key policy.
4. For each already-disabled route call
   `POST /internal/v1/model-routes/{route_id}/retire-upstreams` with
   `tenant_external_id`, the explicit `upstream_account_ids` to remove, and
   fresh `expected_updated_at` and `expected_grant_revision`.
   This requires both `routes:write` and `providers:write`.
5. Verify every removed account is absent from explicit candidates, every
   retained candidate and grant is unchanged, the route is still disabled,
   and the route UUID/model still exists. A zero-candidate route is valid only
   as this disabled retained identity; ordinary create/enable remains strict.
6. Refresh per-account deletion readiness. Only zero-route, non-import-pinned,
   disabled accounts can be deleted through the existing product DELETE with
   fresh CAS. That transaction preserves `deleted_upstream_account_snapshots`
   before removing the live account and its credential material.
7. Reconcile the immutable request/generation identity counts and every route
   grant against the pre-retirement receipt.

The retirement endpoint holds the tenant routing write lock, checks both route
and grant revisions, requires disabled accounts, removes only explicitly
named direct candidate edges, and updates the compatibility account UUID to
the first remaining direct candidate or nil. It never touches customer
credentials, grants, route groups, provider-group rules or request history.
Provider-group candidates are rejected for separate exact membership review.
Retiring a missing candidate, changing dependencies, or using a stale revision
fails atomically. On uncertain delivery, read the authoritative route before
resuming; do not blindly replay against a new revision.

The endpoint does not authorize deleting the preserved development ingress,
source data, PVCs, or PostgreSQL volumes. It does not implement quota reset.
