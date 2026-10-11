# Declarative authorization-code transport

Signed provider contributions can select the host-owned `authorization_code_pkce`
flow without executing a provider component. These settings share the existing
install/signature review, pinned application revision, URL/proxy policy, encrypted
session and credential state, PKCE/state checks and idempotent completion logic.
They do not grant plugins access to credentials or arbitrary outbound headers.

Optional example fields inside a provider contribution:

```json
{
  "oauth_adapter": {
    "api_version": "oauth-adapter-v1",
    "flow_kind": "authorization_code_pkce",
    "login_url": "https://provider.example/authorize",
    "poll_url": "https://provider.example/token",
    "refresh_url": "https://provider.example/token",
    "response_mode": "fragment_manual",
    "authorization_code": {
      "token_encoding": "json",
      "include_state": true,
      "client_default": {
        "client_id": "reviewed-public-client",
        "redirect_uri": "https://provider.example/callback",
        "scopes": ["profile", "inference"]
      }
    },
    "identity": {
      "url": "https://provider.example/profile",
      "id_pointer": "/account/id",
      "headers": {"anthropic_beta": ["subscription-v1"]}
    }
  },
  "request_compatibility": {
    "third_party": true,
    "responses_via_anthropic_messages_v1": true,
    "anthropic_messages_headers": {
      "anthropic_version": "2023-06-01",
      "anthropic_beta": ["subscription-v1"]
    }
  }
}
```

This illustrates optional contracts, not a complete manifest or a working supplier.
The `authorization_code` object is valid only for the PKCE flow. Omission retains
form-encoded token and refresh requests; `json` selects a JSON object containing
the same host-owned grant/client/code/verifier fields. `include_state` adds only
the verified session state to authorization-code exchange, never refresh.
It cannot supply arbitrary grant fields, a client secret or a credential template.

An explicit client supplied by an authorized global operator retains its existing
behavior. Otherwise the reviewed `client_default` is used; absent that declaration,
the existing deployment Secret client-default mechanism remains the fallback.
Public defaults have no `client_secret` field. A confidential client's secret
continues to belong only in the existing encrypted operator/session path.
The global-operator requirement on the generic start API is unchanged.

Identity and Messages headers use a closed, typed protocol contract: only
`anthropic_version` and a bounded array of `anthropic_beta` feature tokens are
accepted. Tokens are 1–128 ASCII letters/digits/dot/underscore/hyphen; at most 16
features are allowed. Authorization, API keys, cookies, Host, redirects, arbitrary
header names, interpolation and CR/LF values are rejected. Identity headers are
applied alongside the host credential to the declared HTTPS identity endpoint.
Messages declarations require an Anthropic protocol or the Messages bridge;
trusted version overrides the inbound version, and beta features merge without
duplicates. Both native Messages and translated Responses use the same path.

New credentials snapshot the complete OAuth adapter in encrypted state. Refresh
and reauthorization reject a changed adapter before sending tokens. Reauthorization
reuses the installed client/scopes, account/name/config/proxy and proven identity;
different identities are rejected by the existing transaction before account
replacement. Legacy form credentials remain supported when no new transport or
identity-header contract is introduced; upgrading such contracts requires reconnect.
No built-in provider is removed or rewritten by this extension.

GitHub Actions runs HTTP mocks for JSON exchange/refresh, wrong-state no-dispatch,
duplicate completion, declared identity headers and contract/identity drift, plus
gateway Responses header integration and existing form regressions. No local
tests/builds/installations, real OAuth or paid inference are used. This backend
contract does not establish supplier entitlement or UI activation readiness.
The supplier receives inference content; request-field sanitization does not
promise zero leakage. Plugin release pins, signing and activation remain separate
reviewed work; this change does not publish packages or alter PR5.
