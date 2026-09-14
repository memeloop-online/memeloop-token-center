# Model Guard

First-party, opt-in model admission for Token Center's existing WIT 0.2
`traffic-policy.post-auth` hook. This is a product package, not the request-rewrite
or OAuth example. It rejects only exact, case-sensitive model IDs explicitly
listed in `blocked_models`. It never rewrites a model, body or account choice.
It contributes no provider, OAuth adapter, browser code, network or KV capability.

The default is `{"blocked_models":[]}`: all normal admission decisions remain
with the host. Installing, approving and publishing the default package does not
deny any model. The existing Plugins configuration editor supports global
configuration and tenant overrides; the existing host authorization controls who
may write each scope. Configure `{"blocked_models":["retired-model"]}` only
after reviewing its intended scope. Clear the list or clear the override to
restore inherited/default behavior. Removing the package does not delete saved
configuration: review it before reinstallation. The plugin does not replace
credential/model authorization or upstream health controls.

The hook sees the model at its position in the host's manifest-ID-sorted policy
chain. A separately approved later rewriting plugin can change the model after
this hook; Model Guard is not a final-dispatch authorization boundary. The denial
reason is fixed text and never includes request content or configuration.

Build inputs are the adjacent locked Rust crate, `plugin.json`, and the existing
root `wit/token-center.wit`. Use Rust 1.95.0, `wasm32-unknown-unknown`, and
checksum-pinned wasm-tools 1.252.0. Do not use `wasm32-wasip2`: the host does not
grant WASI imports. The crate's required provider-world exports explicitly return
errors but cannot be reached through the provider catalog because it declares no
provider contribution.

Source lives outside the installed `plugins/` root deliberately: it is not a
loadable package until the release pipeline has produced `plugin.wasm`. Do not
mount this source directory as a runtime inventory or loosen package validation
to make an unbuilt source tree appear installed.

The manual [publish workflow](../../.github/workflows/publish-first-party-plugin.yml)
tests default and configured behavior, builds the component, runs it in the actual
host, publishes OCI layers with the installer's media types, signs using GitHub
OIDC, verifies the exact certificate identity and installs the signed digest with
the actual installer. Its `plugin-release.json` exists only after all these gates
succeed. Source being merged does **not** mean a signed release exists.

See [first-party release and installation](../../docs/first-party-plugin-release.md)
for release status, trust policy and the administrator workflow. This repository
does not enable runtime inventory, modify production trust, publish an inventory,
or change any tenant configuration as part of adding this asset.
