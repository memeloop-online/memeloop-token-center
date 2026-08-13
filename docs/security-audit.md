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
| Authentication | Control and gateway protected routes authenticate before body extraction. Service scopes and tenant scope are checked separately; a private upstream destination additionally requires a global service credential. |
| Credential storage | Downstream and service credentials are stored as keyed hashes. Upstream OAuth/API credentials and replayable rotation responses are encrypted. Rotation advances a generation under a stable key/account identity so history does not move. Secret values are not written to normal tracing fields. |
| SQL injection | Runtime values are bound parameters. The migration-time dynamic column identifier is selected only from a fixed in-code two-item allowlist. |
| Command injection | Production code does not invoke an operating-system shell or child process. The `Command` references in `main.rs` are CLI enum variants, not process execution. |
| Path traversal | Archive object locations use a restricted internal alphabet and object-store parsing. Plugin component paths reject absolute/parent components, are canonicalized, and must remain below the canonical package root; package symlinks are rejected. |
| Browser boundary | Responses set a restrictive CSP, deny framing and object embedding, and constrain scripts/connects to the same origin. Bearer credentials are sent only in authorization headers. |
| Resource bounds | Request bodies, upstream bodies, archive reads, image concurrency, gateway concurrency, plugin memory/table/instance counts, plugin HTTP bodies, and plugin execution time are bounded. Redirect following is disabled. |
| Plugin isolation | Wasmtime components receive only declared host capabilities. HTTP allowlists are exact HTTPS origins, and plugin HTTP remains public-only until private-destination approval metadata exists. |
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

## Open findings and deployment requirements

### P2: readiness probes can be reached through a catch-all ingress

`/readyz` already has application-level amplification bounds: database and
archive checks run in parallel with an independent two-second timeout, their
result is cached for five seconds, and a non-blocking singleflight lock prevents
concurrent probes from building a waiter queue or starting duplicate checks.
The response exposes only coarse ready/failed state.

The default Ingress nevertheless forwards the path anonymously. An Internet
caller can still trigger at most one database query and one S3 check per process
per five-second cache window, compete with the real load-balancer probe, and
observe dependency availability/timing. This is bounded operational traffic and
coarse information disclosure, not a direct privilege-boundary bypass, so it is
P2 rather than P1. Production ingress or edge policy should expose `/readyz`
only to the cluster/load-balancer health checker. The same restriction should
cover `/livez` and the deprecated `/healthz` where practical. `/metrics`
requires a `metrics:read` service credential and is not served by the
gateway-only role.

### P2: operator bearer token is browser-readable

The operator UI keeps its service credential in `sessionStorage`. The CSP
reduces the XSS attack surface, but any same-origin script compromise can read a
long-lived token. Moving the token to React/process memory is the recommended
near-term P2 fix: reload or browser restart requires another login/paste, and a
token is no longer recoverable from browser storage after the page is gone.
Memory-only storage does not stop active same-origin XSS, which can inspect
runtime state or intercept authenticated requests.

An `HttpOnly; Secure; SameSite=Strict` cookie would provide the stronger browser
boundary, but the service currently has no server-side operator session. That
design requires short session expiry, rotation/revocation, origin and CSRF
controls, and must not translate the current long-lived bootstrap bearer
directly into a persistent cookie.

### P1 deployment condition: an unrestricted public control plane

Regardless of token storage, exposing the control hostname to arbitrary
Internet clients while operators use a long-lived global service credential is
a P1 deployment blocker. Keep it behind SSO, VPN or a strict source allowlist;
the tenant gateway can remain the public product endpoint. With that perimeter
in place, the `sessionStorage` and readiness findings above remain P2 hardening
items rather than release blockers.

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
