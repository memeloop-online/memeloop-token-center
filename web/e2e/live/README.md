# Live read-only browser acceptance

This Cucumber.js profile targets an already deployed Token Center. It never starts the local SQLite/mock runtime and has no fixture or seeding steps. Every browser request is intercepted; only `GET`, `HEAD`, and `OPTIONS` are allowed.

Set these variables before running `npm run test:e2e:live:readonly`:

- `MTC_LIVE_CONTROL_URL`: control-plane origin used for `/operator`.
- `MTC_LIVE_GATEWAY_URL`: public gateway origin used for self-service and isolation checks.
- `MTC_LIVE_SERVICE_CREDENTIAL_FILE`: operator service credential file.
- `MTC_LIVE_CLIENT_CREDENTIAL_FILE`: legacy client credential file.
- `MTC_LIVE_EXPECTED_KEY_ID`: expected stable key ID. This is optional only when the client file contains `key_id` or `expected_key_id`.

Credential file requirements are deliberately strict: a current-user-owned regular file, exact mode `0600`, no symbolic links, and at most 64 KiB. A file may contain the raw credential or a JSON object with `key`, `token`, or `credential`. Client JSON may additionally contain the non-secret stable identity as `key_id` or `expected_key_id`.

The profile uses Cucumber's progress formatter only. It does not capture screenshots, traces, videos, response bodies, or credential values.
