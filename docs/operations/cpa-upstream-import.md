# CPA upstream account import

`ops/cpa-upstreams/import-cpa-upstreams.py` inventories a mounted CPA v7
`config.yaml` and its real `auth-dir`, then imports the supported accounts through
the Token Center control API. It is dry-run by default. It does not read CPA
through its management API and never mutates the source snapshot.

This importer migrates upstream account connections. CPA model aliases,
per-key prefixes, excluded-model lists and Token Center model routes remain a
separate reviewed routing migration.

## Supported source records

The parser uses PyYAML `SafeLoader`, rejects duplicate mapping keys and YAML
aliases, and accepts these CPA configuration sections:

- `openai-compatibility[].api-key-entries[]`;
- `gemini-api-key[]`, using `x-goog-api-key`;
- `codex-api-key[]`, when every entry has an explicit `base-url`;
- `claude-api-key[]`, using `x-api-key`; and
- auth JSON with the explicit normalized shape `type: api_key`, `base_url` and
  `api_key`.

Custom upstream headers, per-account proxies and Claude request cloaking stop the
entire import because the current `http-json` account cannot preserve them.
Unknown `*-api-key`/`*-compatibility` sections and unknown auth JSON types also
stop the import. No supported accounts are written before source inventory and
target metadata conflict checks finish.

Copilot and Cursor records are supported only when the CPA auth document contains
an opaque subscription handle:

```json
{
  "type": "subscription-bridge",
  "upstream": "copilot",
  "handle": "OpaqueAsciiHandle",
  "label": "Account label"
}
```

The importer reduces this document to the fields above and calls
`POST /internal/v1/imports/cpa/subscription-accounts`. It never sends unrelated
CPA auth metadata to the target. The same applies with `upstream: cursor`.

## Secret and transport boundary

Before starting, make an immutable CPA volume snapshot and stage only its
`config.yaml` and declared auth directory for the importer UID. The importer
requires:

- `config.yaml`, every auth JSON, the target service token and an optional bridge
  secret to be owner-owned, single-link, regular mode-`0600` files;
- the auth root and every nested directory to be owner-owned mode `0700`;
- no symlink or non-JSON file anywhere below the auth root; and
- source and target sizes to remain within the built-in bounded-read limits.

Do not pass a credential, service token or bridge secret in argv, an environment
variable, a ConfigMap or a shell substitution. Normal output is one count-only
JSON object. Errors do not include filenames, URLs, response bodies, hashes or
credential values. Core dumps are disabled.

Every target, provider and bridge URL must use HTTPS. The
`--allow-http-loopback` option permits only `localhost`, `127.0.0.0/8` or `::1`
and exists for black-box tests. It does not permit cluster DNS or private IP HTTP.
The HTTP client never follows redirects. Use `--ca-file` for a private control
plane CA instead of disabling TLS verification.

The apply service credential needs `providers:read`, `providers:write` and the
scope required by the CPA subscription import endpoint. Route it only to the
private control Service.

## Dry-run and apply

Run the checked-in release importer image as its non-root UID. This example shows
the command inside that image; `/source` must already meet the ownership and mode
requirements above:

```sh
/usr/local/bin/import-cpa-upstreams \
  --config /source/config.yaml \
  --auth-dir /source/auth \
  --tenant cpa-dogfood-import \
  --bridge-base-url https://cpa-subscription-bridge.internal.example
```

A successful dry-run returns counts only:

```json
{"api_account_count":6,"created_count":0,"disabled_source_count":0,"imported_subscription_count":0,"mode":"dry-run","oauth_blocked_count":0,"replayed_count":0,"subscription_account_count":2}
```

Preserve the source snapshot and count-only output for review. Do not reorder API
key entries or rename auth files between dry-run and apply: section/provider plus
list ordinal, or auth relative path, is the stable CPA source identity.

After approval, mount a least-privilege Token Center token and optional bridge
secret as separate mode-`0600` files and add:

```text
--apply
--target-api-base-url https://token-center-control.internal.example
--service-token-file /secrets/target/service-token
--bridge-secret-file /secrets/bridge/secret
```

For API accounts, the target name includes a deterministic hash of the non-secret
source identity. Apply first lists the tenant and rejects any same-name account
whose driver or configuration differs. A newly created account is then converged
through `PUT /internal/v1/upstreams/{id}/credential` with a source-derived
`Idempotency-Key`. This second write makes an ambiguous create response
recoverable: replay finds the deterministic account and obtains the exact stored
rotation result. The key contains no credential hash. Reusing the same immutable
source does not add an account or credential generation; changing credential
material under the same source identity conflicts instead of silently rotating
an audited migration source.

The subscription import endpoint derives a stable OAuth session identity from
tenant, provider and opaque handle. Replaying the identical source returns the
same account. Any returned `skipped` record is treated as failure.

Create a fresh apply Job for the replay. It must report every API account under
`replayed_count`, keep the target account count unchanged, and succeed for every
subscription account. An interrupted run is recovered by replaying the same
immutable snapshot, never by editing the snapshot or weakening a conflict gate.

## Managed OAuth import

Managed OAuth auth files are imported one at a time through
`POST /internal/v1/imports/cpa/managed-oauth`. The caller supplies only contract
version `1`, the tenant, a strict POSIX auth-file relative path, a server-advertised
`source_type`, and the opaque CPA document. It cannot supply an account name,
provider driver, provider configuration, adapter destination, or refresh URL.
Those values come from the installed server catalog and its versioned adapter.

The importer must call
`GET /internal/v1/imports/cpa/managed-oauth/capabilities` during its no-write
preflight. The response is intentionally limited to the contract version and a
sorted, unique list of source types; it contains no driver, URL, token, or
internal topology. If any inventoried managed OAuth record is unsupported, apply
must stop before its first target write.

Both operations require a global service credential with the dedicated
`imports:cpa:write` scope. A tenant-bound credential is rejected even if it has
that scope. Do not substitute `providers:write` or `oauth:write`.

The auth relative path is limited to 512 UTF-8 bytes. It must be a non-empty
POSIX relative path with no absolute prefix, empty segment, `.` or `..` segment,
backslash, NUL, or control character. The canonical JSON document is limited to
1 MiB, and the complete JSON request has a bounded 64 KiB envelope allowance.
Neither the path nor document is returned or logged.

Token Center derives the source identity and immutable payload digest with
domain-separated HMAC-SHA256 using its key pepper. Canonical JSON sorts object
keys recursively while retaining array order. The source path, document, token,
and keyed digests are never stored in response-visible account metadata. An
exact replay returns HTTP 200 with `disposition: replayed` and the same stable
account without resolving or calling an adapter. Changed `source_type` or
document material under the same source returns static HTTP 409. A new atomic
import returns HTTP 201 with `disposition: created`; a concurrent identical
winner is returned as a replay.

Before the atomic write, the normalized account must pass the active provider's
configuration and credential schemas and the complete outbound destination/SSRF
policy. An enabled, unexpired source becomes active. An enabled but expired and
refreshable source, or a disabled source, is stored disabled for managed refresh.
An expired credential without refresh state is rejected with HTTP 400 and no
write. Adapter rejection, timeout, oversized response, or invalid normalized
output returns a static HTTP 502 without exposing its response.
