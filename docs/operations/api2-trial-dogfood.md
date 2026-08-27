# API2 trial dogfood access

This runbook covers the reversible MemeLoop Token Center API2 trial only. It
does not authorize any API3 change or replacement of the old CPA service.

## Public credential portal

Open
`https://token-center-api2-trial-portal.k3s.onetwo.website/portal` and enter a
client credential. The browser remembers the credential on that device; use the
portal's manual clear action when it should be forgotten. Client credentials
authorize the public `/v1` and `/self/v1` surfaces and cannot open the operator
control plane.

The current dogfood credential is stored only in Kubernetes Secret
`mtc-api2-trial-dogfood-client`, key `client-key`, in namespace
`memeloop-token-center-api2-trial`. An authorized operator may retrieve it
without copying it into a repository or chat log:

```sh
kubectl -n memeloop-token-center-api2-trial \
  get secret mtc-api2-trial-dogfood-client \
  -o jsonpath='{.data.client-key}' | base64 -d
```

For a CLI process, read it directly into a short-lived environment variable and
clear that variable when the process exits. Do not put it in command history,
Codex configuration files or screenshots.

## Operator control plane

Open `https://token-center-api2-trial.k3s.onetwo.website/operator`. This surface
requires a service credential with the relevant management permissions; a
client credential intentionally returns `RBAC: access denied`.

The trial bootstrap service credential is stored only in Kubernetes Secret
`memeloop-token-center-secrets`, key `service-token`, in the same namespace:

```sh
kubectl -n memeloop-token-center-api2-trial \
  get secret memeloop-token-center-secrets \
  -o jsonpath='{.data.service-token}' | base64 -d
```

Enter it once in the operator sign-in prompt. The browser remembers it until
the operator manually clears it. Treat this credential as administrative: do
not give it to ordinary portal users and do not persist it outside the cluster
Secret.

## Current trial scope

The dogfood client is bound to the reviewed API2 trial route group. Text via
Chat Completions, Responses and Codex CLI, semantic session metadata, standard
OpenAI Images generation, archived asset retrieval and per-image USD settlement
have been exercised against the trial. This does not claim that legacy CPA
client credentials or the complete legacy session archive have already been
attached; those are separate migration gates before production approval.
