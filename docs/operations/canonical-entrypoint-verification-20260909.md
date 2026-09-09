# Canonical entrypoint verification — 2026-09-09

Scope: read-only checks of the deployed Ingress rules, the control Deployment's
Secret reference (metadata only), and unauthenticated HTTP GETs. No Kubernetes
Secret values were read and neither documented credential command was executed.
No credentials, routing, allowlists or other runtime configuration were changed.

| Entry | HTTP result |
| --- | --- |
| `https://token.k3s.onetwo.website/portal` | 200, HTML |
| `https://token.api.onetwo.website/portal` | 200, HTML |
| `https://token-operator.k3s.onetwo.website/operator` | 200, HTML |
| `https://token.k3s.onetwo.website/v1/models` | 401, JSON |
| `https://token.api.onetwo.website/v1/models` | 401, JSON |
| `https://token-operator.k3s.onetwo.website/internal/v1/keys` | 403, plain text |

Ingress rules confirm `/internal/v1` and `/operator` on the private Operator
host, and `/v1`, `/self` and `/portal` on the gateway hosts. The management API's
403 does **not** prove authenticated application access; it remains a separate
acceptance check. A 200 HTML shell does not prove every UI interaction works.

The running `memeloop-token-center-api2-trial-control` Deployment in namespace
`memeloop-token-center-api2-trial` references
`memeloop-token-center-secrets` / `service-token` for `MTC_SERVICE_TOKEN`.
README commands match that reference and only perform a Kubernetes Secret GET.
They are operator-run instructions, not CI commands.

`tests/ops/readme-canonical-contract.test.ts` locks the exact six canonical URLs,
credential-boundary statement, two single-line command blocks, read-only target,
failure guards and secret-output warning. It is included in the existing GitHub
Actions release-source contract runner. These new tests were not run locally;
their result must be taken from the subsequent CI run. They validate documentation
structure, not Kubernetes permissions or Bash/PowerShell runtime behavior.
