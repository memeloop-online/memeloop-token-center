# Memeloop Token Center

## Canonical addresses

| Purpose | Address | Credential |
| --- | --- | --- |
| Internal model API | `https://token.k3s.onetwo.website/v1` | Client credential; private network access |
| Public model API | `https://token.api.onetwo.website/v1` | Client credential |
| Internal Portal | [Portal](https://token.k3s.onetwo.website/portal) | Client credential; private network access |
| Public Portal | [Portal](https://token.api.onetwo.website/portal) | Client credential |
| Private Operator | [Operator](https://token-operator.k3s.onetwo.website/operator) | Management service credential; private network access |
| Private management API | `https://token-operator.k3s.onetwo.website/internal/v1` | Management service credential; private network access |

Client credentials cannot access the management API. The public host does not
expose Operator or management APIs. Product and engineering documentation is in
the [project overview](docs/project-overview.md).

## Read the existing management credential

Use an authorized Kubernetes context. These commands only read the existing
Secret; they do not create or rotate credentials. They print a secret for your
own use: do not run them in CI, recorded terminals or shared logs, or paste their
output into source control, tickets or conversations. The namespace below is
the current runtime identifier, not a user-facing website.

Bash (one line):

```bash
(set -euo pipefail; mtc_service_token_b64="$(kubectl -n memeloop-token-center-api2-trial get secret memeloop-token-center-secrets -o 'jsonpath={.data.service-token}')"; test -n "$mtc_service_token_b64"; printf '%s' "$mtc_service_token_b64" | base64 --decode; printf '\n')
```

PowerShell (one line):

```powershell
$mtcServiceTokenBase64=& kubectl -n memeloop-token-center-api2-trial get secret memeloop-token-center-secrets -o 'jsonpath={.data.service-token}'; if($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($mtcServiceTokenBase64)){throw 'Cannot read the existing management credential from the current Kubernetes context'}; [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($mtcServiceTokenBase64.Trim()))
```
