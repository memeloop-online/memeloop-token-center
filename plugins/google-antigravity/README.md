# Google Antigravity provider

Install this provider contribution with the normal plugin installer. It uses the
host's standard authorization-code flow and native Google image transport; no
CPA service, imported account, compatibility bridge, or plugin network process
is required.

The contribution declares `authorization_code_pkce`. Other provider plugins can
declare the same flow with their own authorization/token/refresh endpoints.
The host owns PKCE, callback validation, encrypted durable sessions, credential
refresh, and account proxy transport. The package receives no tokens.

Start with `POST /internal/v1/oauth/authorization-code/start` using a global
operator credential. Supply `tenant_external_id`, `account_name`,
`provider_driver`, and `provider_config`. Optional `client` (`client_id`, `redirect_uri`,
`scopes`, optional `client_secret`) overrides deployment-provisioned desktop-client defaults.
Proxy URL and scope are optional paired
fields; they are encrypted with the session and resulting credential, not
copied into public account configuration. Complete with the returned
`session_token` and the full browser `callback_url` at
`POST /internal/v1/oauth/authorization-code/complete`.

Client parameters may come from an authorized desktop client configuration or
an operator-owned registration; creating a new OAuth client is not intrinsically
required. Operations supplies the desktop-client parameters through the existing
deployment Secret as `MTC_PROVIDER_OAUTH_CLIENT_DEFAULTS_JSON`. Its JSON object
maps provider IDs to `client_id`, `client_secret`, `redirect_uri`, and `scopes`.
For example, the `google-antigravity` key holds the selected authorized desktop
client configuration. Inject via a Kubernetes `secretKeyRef`, never a command-line
argument, checked-in values file, or logged environment dump. A provisioned
deployment permits login without the user entering any client fields. All
private client overrides and refresh tokens
must not be committed to this package or placed in public `provider_config`.

After completion, the existing project is discovered through `loadCodeAssist`.
This does not automatically onboard a project, accept terms, reset quota, or
switch service tiers. A project discovery error leaves issued tokens in the
encrypted ready session for retry instead of repeating code exchange.
The initial code-exchange window is ten minutes. Already-issued Ready/Consumed
results can be finalized or replayed for the following 24 hours, matching session
cleanup retention; this does not extend the authorization-code exchange window.

Use the account's live model catalog and a generation route priced per image or
job. `POST /v1/images/generations` accepts `n=1`; image output enters the same
durable submission fence, staging, settlement, and replay path as other image
providers. Native `inlineData` is streamed as OpenAI `b64_json` without decoding
and re-encoding the complete image. Supported size translations: `1024x1024`
(1:1), `1536x1024` (3:2), `1024x1536` (2:3), or `auto`.

`request_headers` supports administrator-specified extension and authentication
header overrides after default OAuth headers. Values are hidden from public
account views. Invalid header syntax and HTTP framing overrides are rejected. API URLs, proxy,
client metadata, project and model availability remain explicit configuration;
the provider does not generate authorization material or pretend that a model
label grants access.

## Protocol evidence

Implementation was checked against CPA's public upstream commit
`7fa443dc8bf8ca2f1ffd81c2472deb31b097b697`:

- [OAuth implementation](https://github.com/router-for-me/CLIProxyAPI/blob/7fa443dc8bf8ca2f1ffd81c2472deb31b097b697/internal/auth/antigravity/auth.go): Google form-encoded token exchange, existing-project discovery, separate daily/production endpoints.
- [Public desktop-client parameters](https://github.com/router-for-me/CLIProxyAPI/blob/7fa443dc8bf8ca2f1ffd81c2472deb31b097b697/internal/auth/antigravity/constants.go): deployment can provision these desktop application parameters without importing any CPA user account. Defaults may be replaced by encrypted operator client configuration.
- [Image request envelope](https://github.com/router-for-me/CLIProxyAPI/blob/7fa443dc8bf8ca2f1ffd81c2472deb31b097b697/internal/runtime/executor/antigravity_executor_request.go): `project`, `model`, `request`, `requestType=image_gen`, `requestId` and `userAgent` envelope.
- [CPA license](https://github.com/router-for-me/CLIProxyAPI/blob/7fa443dc8bf8ca2f1ffd81c2472deb31b097b697/LICENSE): MIT. Wire behavior was independently implemented; no account/token files are reused.

The CPA flow does not demonstrate S256 support for every desktop client. This
host requires S256 for the generic flow; compatibility must be verified with
the selected authorized client before production activation. Mock tests do not
claim real upstream authorization or image-generation success.
