# Security audit

This document records product security controls and release evidence expectations.

| Boundary | Control |
| --- | --- |
| Authentication | Client and service credentials are verified before protected request bodies are buffered. Rotation revokes the prior generation. |
| Authorization | Service scopes and tenant boundaries are checked on every administrative object lookup and write. Client credentials remain self-service only. |
| Routing | Model grants, provider-account state, egress controls and health gates are enforced before upstream execution. |
| Secrets | Plaintext credentials use authenticated encryption at rest and never appear in API responses, logs, fixtures, workflow output or UI state after issuance. |
| Input | JSON schemas, bounded buffers, request limits, cursor validation and safe archive range parsing reject malformed or overlarge input. |
| Network | Ingress separates gateway from control. Egress is restricted by network policy, configured proxy policy and SSRF validation. |
| Archives | Asset and request content requires credential or tenant authorization. Missing archive content is a sanitized storage failure. |
| Plugins | Components have declared capabilities, bounded memory/time, approved origins and no direct credential access. |
| Observability | Metrics labels are bounded. Profiling is restricted to authenticated internal control traffic and requires an explicit switch. |

## Required verification

- Verify cross-tenant denial for every new resource and list filter.
- Exercise every service scope against both its allowed and forbidden operation.
- Inject provider, database, archive and worker failures and assert that traces and
  client errors do not contain secrets or private network values.
- Confirm unauthorized requests fail before expensive body parsing or outbound work.
- Validate browser CSP, HSTS, frame, MIME and permissions headers at the final ingress.
- Confirm diagnostic endpoints are absent from public gateway ingress.
- Review provider proxy and destination configuration against the deployment's
  network policy before each release.

## Operational response

Treat credential disclosure, cross-tenant data exposure, unauthorized outbound
access, accounting bypass and unbounded-memory request handling as release-blocking.
Rotate affected credentials through the control plane, preserve audit evidence,
and use the disaster-recovery procedure for data restoration. Do not copy secrets
into incident notes or a source-control change.
