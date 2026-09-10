# Operator routing information architecture

## Resource boundaries

1. **Provider → account → catalog model** describes the source. Provider identity
   comes from provider metadata, account identity from its stable ID, and model
   membership from that account's authoritative catalog. Names do not establish
   identity or compatibility.
2. **Model route** maps a public model/protocol to an upstream model and a
   candidate set. The candidate set is explicit accounts ∪ included provider
   group members − excluded provider group members. Exclusion takes precedence.
3. **Route group** collects routes; it does not grant access merely by existing.
   A credential must explicitly receive the group or a route. Empty groups
   grant no routes. A disabled route is not callable merely because it is granted.
4. **Credential access** must distinguish direct grants, group grants, and the
   server's effective route IDs. Effective membership is an authorization view,
   not proof of runtime health, quota, price availability or a successful call.

## Page responsibilities

| Surface | Primary decision | Required evidence |
| --- | --- | --- |
| Providers | Which account supplies the model? | Provider/account identity; catalog freshness; account status; traffic health separate from probes |
| Routes | Which sources can serve this public model? | Explicit/included/excluded provenance; protocol; route priority; disabled/missing candidates |
| Route groups | What access does group membership represent? | Saved membership vs unsaved draft; member IDs/names; zero-member warning; grant semantics |
| Client credential routing | What can this credential access? | Direct/group selection and authoritative effective route count; disabled/missing member warnings |

## Interaction rules

- Search text is not a selected model ID. Browsing by provider/account must not
  mutate the selected model or clear a valid selection's coverage confirmation.
- The exact-model entry remains available for known IDs; an unknown/stale
  catalog still requires explicit custom-model confirmation where allowed.
- List popovers use one non-modal top-layer pattern, keyboard active-option
  scrolling, Escape/outside dismissal and current-theme colors.
- Candidate order in a form is not fallback order. The resolver orders routes
  by ascending route priority; live eligibility also depends on authorization,
  account state, expiry, runtime health/cooldown, capability and quota. Without
  authoritative live eligibility, show it as unknown, never as healthy.
- Group edits must distinguish saved membership from the unsaved editor. This
  UI does not assume credentials have received a group and does not expand
  permissions automatically. Provider groups expose total/enabled referencing
  routes; route groups expose total/active credential grants. Saving a
  membership change that affects an enabled route or active credential requires
  an explicit impact confirmation.

## Verification boundary

Implementation and synthetic regression contracts are reviewed without local
product builds/tests. GitHub Actions and post-release real-browser checks remain
required. No production mutations, model calls, resets or price assumptions are
part of this pass.
