# Security audit

Last reviewed: 2026-08-14

This document records the security boundary reviewed before exposing the control
and gateway services to dogfood traffic. It is not a substitute for an external
penetration test or continuous dependency scanning.

## Threat model

The gateway accepts untrusted request bodies, model names and bearer
credentials. Tenant service credentials are less trusted than global operator
credentials. Upstream responses, OAuth responses, generation result URLs,
plugin packages and plugin HTTP input are also treated as untrusted. Kubernetes
Secrets, the key pepper, upstream credentials and archived bodies are sensitive.

The principal risks reviewed were tenant-boundary bypass, credential disclosure,
SQL/command/path injection, SSRF into cluster or cloud metadata services,
DNS-rebinding after validation, unbounded request/response memory, and plugin
escape from its declared capabilities.

## Controls verified

| Area | Current control |
| --- | --- |
| Authentication | Control and gateway protected routes authenticate before body extraction. Service scopes and tenant scope are checked separately; a private upstream destination additionally requires a global service credential. The global bootstrap token must be generated from at least 32 random bytes; startup rejects encoded values shorter than 32 bytes or containing Unicode whitespace/`Cc` control characters anywhere. This prevents invisible-character ambiguity and validates a minimum format, not entropy. |
| Credential storage | Downstream and service credentials are stored as keyed hashes. Upstream OAuth/API credentials and replayable rotation responses are encrypted. Rotation advances a generation under a stable key/account identity so history does not move. Secret values are not written to normal tracing fields. |
| SQL injection | Runtime values are bound parameters. The migration-time dynamic column identifier is selected only from a fixed in-code two-item allowlist. |
| Command injection | Production code does not invoke an operating-system shell or child process. The `Command` references in `main.rs` are CLI enum variants, not process execution. |
| Path traversal | Archive object locations use a restricted internal alphabet and object-store parsing. Plugin component paths reject absolute/parent components, are canonicalized, and must remain below the canonical package root; package symlinks are rejected. |
| Browser boundary | Responses set a restrictive CSP, deny framing and object embedding, and constrain scripts/connects to the same origin. Bearer credentials are sent only in authorization headers. |
| Resource bounds | Request bodies, upstream bodies, archive reads, image concurrency, gateway concurrency, plugin memory/table/instance counts, plugin HTTP bodies, and plugin execution time are bounded. Redirect following is disabled. |
| Plugin isolation | Wasmtime components receive only declared host capabilities. Plugin/provider IDs are restricted to 1–64 lowercase ASCII letters, digits or hyphens. Traffic-policy reasons and `host.log` guest messages are never retained, reflected to clients or recorded verbatim; plugin logs contain only the validated plugin ID and host-owned fixed decision/event codes. HTTP allowlists are exact HTTPS origins, and plugin HTTP remains public-only until private-destination approval metadata exists. |
| Dependency surface | SQLx and `rust_decimal` default features are disabled to avoid unused MySQL/RSA and `rkyv` dependency paths. A freshly fetched 1,216-entry RustSec database reported zero known vulnerabilities on the review date. Registry yanked-status queries returned HTTP 403 in this environment, so that separate signal was not verified. |

## Outbound network boundary

Public outbound operations now use a single-resolution client for each
operation:

1. Parse an HTTP(S) URL and reject embedded credentials or fragments.
2. Require HTTPS, except for the explicit loopback-only test switch.
3. Resolve DNS once with a three-second deadline.
4. Reject the complete answer set if any address is loopback, private,
   link-local, documentation, carrier-grade NAT, benchmarking, multicast or
   otherwise reserved. This fail-closed handling covers mixed public/private DNS
   answers.
5. Pin the accepted socket addresses into reqwest while retaining the original
   URL, so the original hostname remains the HTTP Host and TLS SNI/certificate
   name.
6. Disable inherited environment proxies and redirects so another resolver
   cannot bypass the pin.

Plugin `host.http-request` additionally validates method and the complete header
map before DNS access. It accepts only `GET`, `HEAD`, `POST`, `PUT`, `PATCH` and
`DELETE`; rejects authority/content-length, RFC hop-by-hop, proxy/forwarding and
method-override headers; and bounds the JSON encoding, header count, decoded
total and individual name/value lengths. Normal `Authorization` and vendor API
key headers remain available for the exact allowlisted origin. Because a plugin
cannot supply `Host`, the original URL hostname used by the pinned client is
also the HTTP Host and TLS SNI/certificate identity.

The policy is applied at the actual request, not only when configuration is
saved. It covers normal model forwarding, OAuth polling and refresh URLs,
generation submission/polling, approved generation result origins and asset
downloads, image forwarding, and plugin HTTP calls.

`network_scope: "private"` is deliberately a different authority: it uses the
shared cluster-aware client and can be persisted only by a global operator.
Installed provider-adapter plugins are also administrator-owned and may use
their declared internal OAuth endpoints. Plugin HTTP contributions themselves
currently have no equivalent per-capability approval record and therefore fail
closed for private or plain-HTTP origins.

Implicit `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` behavior is disabled for
application clients. If an enterprise proxy is needed, add an explicit trusted
proxy configuration with its own destination policy and tests; do not restore
ambient proxy inheritance.

## Browser and deployment requirements

The operator and self-service UIs keep bearer credentials only in React/process
memory. Reloading or closing the page requires another paste, and a token is not
recoverable from browser storage after the page is gone. The CSP reduces the
XSS attack surface, but memory-only storage does not stop an active same-origin
script compromise from inspecting runtime state or intercepting authenticated
requests.

An `HttpOnly; Secure; SameSite=Strict` cookie would provide the stronger browser
boundary, but the service currently has no server-side operator session. That
design requires short session expiry, rotation/revocation, origin and CSRF
controls, and must not translate the current long-lived bootstrap bearer
directly into a persistent cookie.

### Control-plane ingress boundary

Regardless of token storage, exposing the control hostname to arbitrary
Internet clients while operators use a long-lived global service credential is
a P1 deployment blocker. The Helm chart therefore keeps the control ingress off
by default and, when enabled, requires TLS, a Higress/ingress-nginx-compatible
class and at least one explicit source CIDR; every `/0` form is rejected.
It renders the controller-supported source-range and forced HTTPS annotations
itself, overriding conflicting user annotations. Custom GitOps routes outside
the chart must provide an equivalent SSO, VPN or source-allowlist boundary; the
tenant gateway can remain the public product endpoint.

### Operational requirements

- Use separate gateway and control hostnames. Expose the control hostname only
  to administrators, SSO, VPN or an equivalent allowlist.
- Keep Kubernetes NetworkPolicies enabled and allow only PostgreSQL, approved S3
  endpoints and required upstream egress. A NetworkPolicy is defense in depth;
  the application destination policy remains required.
- MinIO root credentials belong only to the MinIO server. Give each application
  environment an independent identity restricted to its own bucket and use that
  environment's dedicated Kubernetes Secret.
- Use managed/SOPS-backed Secrets, no plaintext Git values and no
  `last-applied-configuration` copy of secret material. Follow
  `docs/operations/secret-management.md` for rotation.
- Terminate TLS at the approved ingress and enforce HTTPS/HSTS there. Keep
  private upstream creation and plugin installation restricted to global
  operators.
- Refresh the RustSec database in CI. A stale or unavailable advisory database,
  or an incomplete yanked-status check, is a failed security signal rather than
  a clean supply-chain scan.

## Reproduction and review commands

These commands do not print credentials:

```bash
cargo fmt --all --check
cargo check --all-targets
cargo test network::tests --lib
cargo test pinned_client_keeps_the_original_http_host --lib
cargo test plugin::tests --lib
cargo audit
helm lint charts/memeloop-token-center
```

The unit coverage includes private/reserved and embedded IP representations,
mixed public/private DNS answers, explicit loopback-test gating, retained Host
semantics on a pinned connection, redirect refusal, plugin path escape/symlink
rejection, private plugin HTTP capability rejection, and signed generation URL
redaction from retry errors. End-to-end security features should continue to run
against mocked upstreams in CI.
