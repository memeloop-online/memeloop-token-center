# MemeLoop Cloud principal ensure — fixed-head review (2026-09-09)

This review records evidence against immutable commits. It does not review whatever
the pull-request branch may contain later.

| Repository | Pull request | Base | Reviewed head |
| --- | --- | --- | --- |
| `memeloop-online/memeloop-token-center` | [#4](https://github.com/memeloop-online/memeloop-token-center/pull/4) | `ff2981e509174266f56736bdb88cb03a002b0824` | `defd9b446fc1d4f86d20034df3cf017b4684a42e` |
| `memeloop-online/memeloop-cloud` | [#4](https://github.com/memeloop-online/memeloop-cloud/pull/4) | `173c60083119ad34410c07264fd4345172b2a315` | `0a3a4913b56bf8c9a266402dfab6e6f33cc70abc` |

The Cloud pull request is a broad release branch at this fixed head (1,137 files,
144,549 additions, and 22,767 deletions). This review therefore treats Cloud
release readiness separately from whether the narrow Token Center change is safe
to merge.

## Decision

- **Token Center PR merge: blocked.** Reject surrounding whitespace consistently
  at the subscription webhook boundary before hashing or provisioning. The ensure
  endpoint already rejects it, but the webhook does not. Add both webhook-first and
  ensure-first regressions plus a PostgreSQL ensure/subscription race.
- **Cloud principal-ensure release: blocked independently of the Token Center
  merge.** Cloud still needs a recoverable `key: null` path for the lost-first-
  response case, row-bound authenticated encryption for canonical user
  credentials, a Token Center trial-credit/routing workflow, and an executed green
  CI run for the exact Cloud head.
- The Cloud release blockers are not reasons to expand the narrow Token Center
  pull request. They belong in Cloud follow-up changes and deployment gates.

## Findings

### P1 — Token Center merge blocker: webhook and ensure disagree on whitespace

The ensure handler rejects a tenant or principal with surrounding whitespace
([`cloud_principals.rs:20-28`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/api/cloud_principals.rs#L20-L28)).
The signed subscription webhook parses the same identifiers and immediately
computes its canonical/event and provisioning digests without the same check
([`cloud_entitlements.rs:244-262`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/api/cloud_entitlements.rs#L244-L262)).
The database path then trims both identifiers when inserting or looking up tenant
and principal rows
([`keys.rs:101-165`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/db/credentials/keys.rs#L101-L165)).

Consequently, a webhook for `" tenant"` / `" user"` can create normalized database
identity rows under a provisioning digest derived from the padded strings. A
later clean ensure derives a different digest and can create a second credential
and account for the same normalized principal. The existing whitespace regression
only exercises ensure
([`principal_ensure.rs:248-262`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/tests/cloud_entitlements/principal_ensure.rs#L248-L262)).

Required fix: reject surrounding whitespace in the webhook after authenticated
JSON parsing and before all digests/validation/database calls. Test padded webhook
delivery both before and after a clean ensure, and verify no additional key,
account, entitlement, or event is created.

### P1 — Cloud release blocker: the lost-first-response `key: null` case is not recoverable

Token Center deliberately returns stable metadata with a null secret once replay
conditions no longer permit returning the initially issued secret
([`keys.rs:1070-1093`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/db/credentials/keys.rs#L1070-L1093)).
Cloud's canonical persistence returns `manual_review` when there is no local record
and the ensure response has `key: null`
([`principalCredentialCore.ts:63-78`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/tokenCenterPrincipalCredentials/principalCredentialCore.ts#L63-L78)).
The outbox handler turns that outcome into a durable manual-review failure
([`handler.ts:66-78`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/integrations/tokenCenterPrincipal/handler.ts#L66-L78)).

This occurs if Token Center committed the first ensure but Cloud lost that response
before persisting it. Subscription-side legacy recovery does not help a new
principal-ensure row in that failure window. Before release, define and test a
bounded authenticated recovery/rotation operation; do not turn a missing secret
into an unbounded automatic rotation.

### P1 — Cloud release blocker: signup trial value is local-only and has no model routes

Production registration does correctly enqueue principal ensure and makes signup
promotion depend on that event
([`productionPrincipalRegistration.ts:29-51`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/composition/productionPrincipalRegistration.ts#L29-L51)),
and production registers both handlers
([`productionOutbox.ts:125-154`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/composition/productionOutbox.ts#L125-L154)).
However, signup promotion only mutates Cloud's local ledger
([`productionSignupPromotions.ts:20-65`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/composition/productionSignupPromotions.ts#L20-L65)).
Token Center ensure creates a zero-balance/default-policy credential
([`keys.rs:91-100`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/db/credentials/keys.rs#L91-L100)),
and Cloud's own fixed-head integration note says free users still need explicit
route authorization
([`token-center-credit-integration.md:48-57`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/docs/product/token-center-credit-integration.md#L48-L57)).

Before calling the trial usable, atomically enqueue an idempotent Token Center
promotion entitlement and an explicit free-route grant/merge policy, then test
replay, expiry/cancellation, paid-plan coexistence, and out-of-order delivery.

### P1 — Cloud release blocker: canonical user ciphertext is not bound to its row

The canonical credential core passes tenant, owner, account, key, currency,
generation, and encryption-key version as cipher context
([`principalCredentialCore.ts:33-45`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/tokenCenterPrincipalCredentials/principalCredentialCore.ts#L33-L45)).
The user-table compatibility cipher ignores that context and calls the legacy
encrypt/decrypt functions with only the plaintext/ciphertext and encryption key
([`persistence.ts:111-115`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/tokenCenterPrincipalCredentials/persistence.ts#L111-L115)).

Authenticated ciphertext copied between compatible rows can therefore decrypt
under the destination row rather than fail its metadata binding. Migrate the
canonical user table to AAD that covers the full context, version the ciphertext
format, retain an explicit legacy read/migration path, and add ciphertext/nonce
swap tests across users, tenants, keys, generations, and key versions.

### P1 — Cloud release blocker: exact-head CI never executed

At Cloud head `0a3a4913b56bf8c9a266402dfab6e6f33cc70abc`, all five reported checks
(`build-test`, three application builds, and `miniapp-payment-contract`) have
failure conclusions, empty step lists, and no runner assignment. The check
annotation says the jobs were not started because recent account payments failed
or the spending limit needs to be increased. This is infrastructure evidence, not
evidence of a test failure or a passing build.

Billing is outside this code review. After it is repaired by an authorized owner,
rerun GitHub Actions for the exact Cloud head and require all release gates green.

### P2 — Remaining concurrency and integration coverage gaps

The Token Center PostgreSQL test races two ensure calls, then applies the
subscription sequentially
([`cloud_principal_ensure_postgres.rs:105-186`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/tests/cloud_principal_ensure_postgres.rs#L105-L186)).
It does not race the first ensure against the first subscription. Add a barriered
PostgreSQL test that starts those two HTTP requests together and asserts one
normalized principal, one account, one key, one entitlement, the subscription
balance/policy, and matching credential/account IDs.

No fixed-head test exercises the real current Cloud caller against the real current
Token Center service, including response loss followed by `key: null`, encrypted
persistence, and retry. Keep that as a Cloud release acceptance gate even after the
Token Center unit/integration change is green.

## Verified non-blocking properties

- Tenant-scoped `keys:write` authorization is enforced by the ensure handler before
  provisioning
  ([`cloud_principals.rs:19-31`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/api/cloud_principals.rs#L19-L31)).
- Paid subscription fulfillment defaults `principalExternalId` to the same Cloud
  `userId`
  ([`ordersAdapter.ts:36-52`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/memberships/ordersAdapter.ts#L36-L52)).
  Subscription credential persistence also checks tenant, principal/user, and
  currency bindings before writing
  ([`principalCredentialPersistence.ts:116-126`](https://github.com/memeloop-online/memeloop-cloud/blob/0a3a4913b56bf8c9a266402dfab6e6f33cc70abc/packages/memeloop-cloud/src/integrations/tokenCenterMembership/principalCredentialPersistence.ts#L116-L126)).
- PostgreSQL principal provisioning uses a transaction-scoped advisory lock before
  identity creation
  ([`keys.rs:101-109`](https://github.com/memeloop-online/memeloop-token-center/blob/defd9b446fc1d4f86d20034df3cf017b4684a42e/src/db/credentials/keys.rs#L101-L109)).
- Token Center GitHub Actions run
  [`34327108001`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/34327108001)
  completed successfully for Rust, web, packaging, dependency security,
  repository security, migration smoke, API contract, and the optimized memory
  acceptance harness. Release publishing jobs were skipped by pull-request policy.

## Verification policy

This review used GitHub metadata and source inspection only. No local product
build or test was run, no Cloud branch was modified, and no GitHub review,
approval, merge, push, rerun, or billing action was performed.
