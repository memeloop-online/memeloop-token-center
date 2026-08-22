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
- `claude-api-key[]`, using `x-api-key`;
- auth JSON with the explicit normalized shape `type: api_key`, `base_url` and
  `api_key`; and
- Codex and legacy Gemini OAuth documents, imported into the matching managed
  OAuth provider supplied by Token Center.

Custom upstream headers, per-account proxies and Claude request cloaking stop the
entire import because the current `http-json` account cannot preserve them.
Unknown `*-api-key`/`*-compatibility` sections and unknown auth JSON types also
stop the import. No supported accounts are written before source inventory and
target metadata conflict checks finish.

Opaque Copilot and Cursor handle records cannot be used as native credentials.
The importer never sends their handle, login, label or source document to Token
Center and never creates an upstream for them. Dry-run and apply instead include
each record in `native_reauthorization_required` with only its provider,
disabled state and keyed `source_stable_id`. Connect those accounts again
through their native or plugin-provided authorization flow.

## Secret and transport boundary

Before starting, make an immutable CPA volume snapshot and stage only its
`config.yaml` and declared auth directory for the importer UID. The importer
requires:

- `config.yaml`, every auth JSON, the target service token and, when opaque
  records exist, the source identity key to be owner-owned, single-link,
  regular mode-`0600` files;
- the auth root and every nested directory to be owner-owned mode `0700`;
- no symlink or non-JSON file anywhere below the auth root; and
- source and target sizes to remain within the built-in bounded-read limits.

Do not pass a credential, service token or source identity key in argv, an
environment variable, a ConfigMap or a shell substitution. Normal output is one
JSON inventory object. Errors do not include filenames, URLs, response bodies,
credential values or secret-derived hashes. Core dumps are disabled.

Every target and provider URL must use HTTPS. The
`--allow-http-loopback` option permits only `localhost`, `127.0.0.0/8` or `::1`
and exists for black-box tests. It does not permit cluster DNS or private IP HTTP.
The HTTP client never follows redirects. Use `--ca-file` for a private control
plane CA instead of disabling TLS verification.

The apply service credential needs `providers:read`, `providers:write` and, for
managed OAuth documents, `imports:cpa:write`. Route it only to the private
control Service.

## Dry-run and apply

Run the checked-in release importer image as its non-root UID. This example shows
the command inside that image; `/source` must already meet the ownership and mode
requirements above:

```sh
/usr/local/bin/import-cpa-upstreams \
  --config /source/config.yaml \
  --auth-dir /source/auth \
  --source-identity-key-file /secrets/migration/source-identity.key \
  --tenant cpa-dogfood-import
```

`--source-identity-key-file` is required only when the snapshot contains opaque
Copilot/Cursor records. It must be an absolute path to a mode-`0600` regular,
non-symlink, single-link file owned by the importer UID. Generate it only with
the release image's `/usr/local/bin/generate-source-identity-key` command. The
command takes one new absolute target path, requires its parent directory to be
owned by the current UID and not writable by group or other users, rejects every
symlink path component and refuses to overwrite an existing target. It writes
all bytes, fsyncs the file and parent, and produces no key material on stdout.

The file format is a fixed magic and version followed by exactly 32 bytes from
Python's operating-system-backed `secrets.token_bytes`. The importer accepts
only that exact binary format; passwords, raw keys, hex/base64 text, a trailing
LF, wrong versions and wrong lengths fail before any target request. Format
validation is not presented as proof that arbitrary caller-written bytes are
random. Use the reviewed generator, store the resulting file in the migration
secret manager, and mount it in Kubernetes as binary Secret file data with mode
`0600`; never use `stringData` or an environment variable.

A successful dry-run returns counts and the non-secret native-authorization
worklist:

```json
{"api_account_count":6,"created_count":0,"created_managed_oauth_count":0,"disabled_source_count":0,"managed_oauth_account_count":0,"managed_oauth_source_type_counts":{},"mode":"dry-run","native_reauthorization_required":[{"provider":"copilot","source_disabled":false,"source_stable_id":"3e37cd527b6365313440b4be4df9184b4dbe06c2aeb4c80628134bd38cb0ea38"},{"provider":"cursor","source_disabled":false,"source_stable_id":"2a8d8d1ee60dad9b93c5f8a479fb24fd38f48095da0c1e9cde5a00fc7ad650b3"}],"native_reauthorization_required_count":2,"replayed_count":0,"replayed_managed_oauth_count":0}
```

Preserve the source snapshot and inventory output for review. Do not reorder API
key entries or rename auth files between dry-run and apply: section/provider plus
list ordinal, or auth relative path, is the stable CPA source identity.

Preserve `native_reauthorization_required` alongside the migration ledger. Its
`source_stable_id` is derived from the non-secret source identity, not the opaque
handle, using domain-separated HMAC-SHA256 and the source identity key. This
prevents common auth filenames from being recovered with an offline dictionary.
The ID is the durable join key for historical account/request mapping and the
later native reauthorization record. Replaying the same immutable snapshot with
the same key produces the same worklist; another key intentionally produces
different IDs. Preserve the key in the migration secret store and use the same
key for every dry-run, apply and recovery. Renaming an auth file changes its
source identity, so never rename or reorganize the reviewed snapshot between
runs.

After approval, mount a least-privilege Token Center token as a mode-`0600` file
and add:

```text
--apply
--target-api-base-url https://token-center-control.internal.example
--service-token-file /secrets/target/service-token
```

If the snapshot contains only Copilot/Cursor opaque handles, `--apply` remains a
report-only operation: it still requires the source identity key, but needs no
target URL or service token and performs no HTTP request. This is intentional
because those records require a fresh native authorization.

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

Create a fresh apply Job for the replay. It must report every API account under
`replayed_count` and keep the target account count unchanged. An interrupted run
is recovered by replaying the same
immutable snapshot, never by editing the snapshot or weakening a conflict gate.
The source-derived credential idempotency key also makes a committed write with
an ambiguous network response safe to replay without adding a credential
generation.

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
