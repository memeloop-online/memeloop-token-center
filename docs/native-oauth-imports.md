# Native OAuth cohort imports

Native OAuth import is a global-operator control-plane operation for moving an
explicitly approved credential cohort into stable upstream accounts. It is not
an interactive OAuth flow and never contacts, resolves, or refreshes the
provider while importing.

The caller must use a global service credential with
`upstreams:import:write`. Before writing, it must require the exact capability
values returned by `GET /internal/v1/native-oauth-imports/capabilities`,
including `atomic_kimi_cohort_v2` and the v2 credential-envelope contract.

`POST /internal/v1/native-oauth-imports/kimi-cohort` accepts exactly two Kimi
documents. Each source identity is a lowercase 64-character HMAC-SHA-256 made
with an operator-held key; the key and original identity are never sent. The
source-document digest is SHA-256 over canonical JSON (object keys recursively
sorted, arrays kept in input order, and compact JSON encoding). The relative
path is validated as metadata but is not stored or reflected in a response.

Every item carries an all-null or all-present current-state CAS triple:
`expected_current_account_id`, `expected_current_document_sha256`, and
`expected_current_credential_generation`. The approval contains two canonical
request-order array digests:

- Current entries contain `source_identity_hash`, `account_id`,
  `source_document_sha256`, and `credential_generation`.
- New entries contain `source_identity_hash` and `source_document_sha256`.

Under one tenant lock and database transaction, the target permits only these
outcomes:

- No existing cohort accounts: create both (`created`, HTTP 201).
- One exact, active, unexpired cohort account: create the missing account
  (`converged`, HTTP 200).
- Two exact cohort accounts: return them unchanged (`replayed`, HTTP 200).
- Two changed accounts with matching CAS and approval: rotate both sealed
  credentials and increment both generations (`rotated`, HTTP 200).

Any foreign, legacy, disabled, expired, routed, differently configured, or
mixed-rotation state returns HTTP 409 and writes nothing. Device identity must
also remain unchanged during rotation. Accounts are returned in request order;
their stable display names are `Kimi OAuth 1` and `Kimi OAuth 2`, assigned by
sorted source identity hash rather than path, email, or credential material.

The complete request and response schema is maintained in
[`openapi/openapi.yaml`](../openapi/openapi.yaml).
