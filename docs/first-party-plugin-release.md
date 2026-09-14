# First-party plugin release and installation

## Availability is explicit

`mtc-model-guard` is the first-party configurable admission package in
`plugin-sources/model-guard`. Default `blocked_models: []` allows all models;
only an administrator's explicit configuration enables a denial. It has no
request rewrite, provider/OAuth, network, KV or UI-code contribution. It uses the
existing Plugins configuration editor, not a separate administration application.

Model Guard 1.0.0 is published at:

```text
ghcr.io/memeloop-online/mtc-model-guard@sha256:a8e9c03abc405130d1241c8cd57c4947184a9dee1987b296d8382a5d87662be8
```

[Release run 34900504884](https://github.com/memeloop-online/memeloop-token-center/actions/runs/34900504884)
at source `ca63f249bc075a1900a9999846352524e3947dad` passed native guest tests,
Wasm component construction, real host execution, exact-identity keyless signing
and verification, and real installer byte comparison. Its artifact
`mtc-model-guard-release-ca63f249bc075a1900a9999846352524e3947dad` contains
`plugin-release.json` with `installation_verified: true` and the published bytes.
This is an installable asset, **not production activation**. Authenticated
installation was tested; anonymous installation is not established.

`mtc-preferred-account` is the separate group-routing package in
`plugin-sources/preferred-account`. Its default `preferred_account_ids: []`
preserves the existing candidate order. Explicit configuration only reorders
already eligible candidates; `health_policy: "native"` preserves the host's
health admission and feedback behavior. It does not infer quota/reset state or
enable stickiness. It requires an installer containing the PR209 manifest schema.
Until its own successful release produces `installation_verified: true`, this
package remains **not published / unavailable for installation**. Model Guard's
evidence does not establish Preferred Account's compatibility or publication.
Never substitute an image digest, fixture Wasm, fabricated signature, branch tag
or locally compiled file.

The release coordinator must first publish a service/plugin-installer release
containing optional keyless support. Review that release's existing immutable
image/source evidence, then use a reviewed master PR to set
`.github/first-party-plugin-installer-trust.json` to `status: "ready"`, its exact
`sha256:...` digest and full `source_revision`. The repository is fixed to
`ghcr.io/memeloop-online/memeloop-token-center-plugin-installer`. This change
pins the compatible installer from successful master
[CI run 34900841632](https://github.com/memeloop-online/memeloop-token-center/actions/runs/34900841632),
source `c8b68028a21e80a610b74ee3c41b442a69b84f97` (including PR209), digest
`sha256:3762012f35acf9151b4aa49a52565be882d016da64d6b368bf901adb8985121e`.
The same run's `ghcr-release-c8b68028a21e80a610b74ee3c41b442a69b84f97`
and `image-digest-plugin-installer-c8b68028a21e80a610b74ee3c41b442a69b84f97`
artifacts bind the release manifest, image metadata, OCI index and BuildKit
SPDX/SLSA statements. Independent registry reads confirmed the immutable digest,
the statement subjects, source/revision labels and patched Cosign
`v3.1.3-mtc.3` label. This is a reviewed CI-image pin, not a claim that BuildKit
statements are independently signed or that every package is already published.
It supports Model Guard and Preferred Account's `health_policy: "native"`
manifest schema. The earlier `69b03ff0` installer used for Model Guard's first
release is not reused for Preferred Account. Future packages requiring a newer
schema must first obtain a newly reviewed compatible image pin.

Only after that review is merged, manually dispatch
`.github/workflows/publish-first-party-plugin.yml` from `master`, selecting
`model-guard` or `preferred-account`. These are the only accepted package choices;
the source helper maps them to fixed crate, output, package ID, OCI repository and
host-test names. Preferred Account additionally runs the fixed real gateway
fixture before publication. There is no installer input: dispatch cannot select
installer executable code. Before registry login,
image execution or signing, a trusted source helper rejects an unavailable,
malformed or different-repository trust record. The pulled image must also have
the reviewed source-revision label before any executable is run. Help and version
checks establish compatibility only; they do not establish executable trust.
Changing the executable digest requires another reviewed source change, never
an environment override or self-reported verification result. This is a separate,
explicitly scheduled workload; it is not triggered by PRs or master pushes and
does not compete automatically with service release jobs. Its host integration
test builds the host with two jobs; allow the coordinator to schedule capacity.

The workflow uses the existing `GITHUB_TOKEN` and GitHub OIDC (`id-token: write`),
not a signing private key or a new repository/Kubernetes Secret. It reuses the
published installer's pinned patched Cosign verifier. It fails if the image lacks
keyless CLI support, the component fails actual host execution, GHCR publishing
or OIDC issuance is denied, the signature cannot be verified, or real installation
does not reproduce the published bytes. A failed run may leave an OCI candidate
or signature in GHCR but does not produce successful release evidence.

Download the successful run's `<plugin-id>-release-<git-sha>` artifact. Review
the release JSON, full OCI manifest, plugin manifest, signature verification and
installation evidence. `reference` is the only install reference; it binds all
layers by digest. The signed identity is exactly:

```text
issuer: https://token.actions.githubusercontent.com
identity: https://github.com/memeloop-online/memeloop-token-center/.github/workflows/publish-first-party-plugin.yml@refs/heads/master
```

The identity trusts executions of that exact reviewed workflow on master, not
all GitHub Actions, all organization repositories or arbitrary branch workflows.
Protecting master and workflow review is a repository-owner responsibility;
this change does not create or assume branch-protection settings. No regexp,
insecure verification switch, offline transparency exemption or trust-on-first-use
is used in keyless mode. Sigstore trust metadata, certificate chain and Rekor/SCT
verification must be reachable according to Cosign's standard trust policy.

GHCR visibility is **not assumed public**. The CI install uses its existing
workflow token through private temporary files. It proves that authenticated
installation works; it does not prove anonymous access. For production, use
already-approved exact-source registry credentials if they exist. If the package
is private and no approved credentials exist, report **registry access unavailable**
and ask the owner for direction. Making a package public or provisioning a new
Secret is a separate externally visible action, not performed by this workflow.

## Administrator installation

Follow [Operator plugin installation](operator-plugin-installation.md) for shared
inventory storage, reviewed RWX semantics and the existing install → inspect →
approve → publish flow. A disabled inventory remains disabled; these instructions
are not an authorization to change production. Upgrade Control and the bundled
installer together before enabling the optional trust mode.

If runtime inventory and trust changes have been explicitly approved, use the
existing host-owned policy ConfigMap with this alternative to public-key mode:

```json
{
  "plugin_root": "/var/lib/memeloop-token-center/plugin-runtime/inventories",
  "allowed_sources": ["ghcr.io/memeloop-online/mtc-model-guard"],
  "cosign_keyless": {
    "issuer": "https://token.actions.githubusercontent.com",
    "identity": "https://github.com/memeloop-online/memeloop-token-center/.github/workflows/publish-first-party-plugin.yml@refs/heads/master"
  },
  "source_credentials": {}
}
```

The empty credential map is usable only when the package is anonymously readable.
Otherwise map this exact repository to **existing approved mounted files**, as in
the main installation guide. Do not paste tokens into policy JSON or requests.
For Preferred Account, use its own successful release reference and explicitly
approve `ghcr.io/memeloop-online/mtc-preferred-account` instead (or additionally,
if both packages are desired); the Model Guard source grant does not cover it.
Use that exact source in the CLI's `--allowed-source` argument as well.

Set `plugins.runtimeInventory.signaturePolicy: cosign-keyless` to omit the
public-key Secret mount. The host policy is the authority: it must contain either
nonempty `cosign_public_keys` or one valid `cosign_keyless` object, never both or
neither. The chart flag only controls mounts; it cannot grant trust. The default
remains `cosign-public-key`, including its existing mandatory Secret. The legacy
`plugins.ociInstaller` Helm mode remains public-key-only; use runtime inventory
or the CLI for keyless packages.

For an already-authorized standalone installation, use the real release reference:

```sh
install-plugin-oci "$REVIEWED_PLUGIN_REFERENCE" \
  --plugin-dir /var/lib/token-center/reviewed-candidate \
  --allowed-source ghcr.io/memeloop-online/mtc-model-guard \
  --cosign-certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --cosign-certificate-identity https://github.com/memeloop-online/memeloop-token-center/.github/workflows/publish-first-party-plugin.yml@refs/heads/master
```

The CLI's public-key and keyless arguments are mutually exclusive. No successful
signature means no package publication on disk. Installation produces a
`cosign-keyless` receipt; receipts remain provenance, not independent proof of
trust. The reviewed identity is bound into the runtime approval/checkpoint trust
digest, so changing it invalidates previous review/checkpoint reuse. Existing
public-key policy review digests remain unchanged.

In Operator, install the exact reference as part of the **complete desired plugin
set** in a fresh inventory ID. Inspect the no-capability manifest and default,
approve the exact review digest, then explicitly publish. Verify history/audit
and configuration defaults without making a paid upstream request. Changing the
blocked list is a separate explicit global/tenant configuration operation. To
roll back, publish the prior complete inventory revision; do not overwrite roots
or delete historical files. Saved configuration is retained and must be reviewed
before later reinstalling the package.

Preferred Account is configured separately on an explicitly selected existing
provider or route group, with `{"preferred_account_ids":["<account-id>"]}`.
Installing the asset does not bind any group, duplicate routes, grant account
membership or change production configuration. Follow its
[package instructions](../plugin-sources/preferred-account/README.md) and require
separate authorization for the version-fenced group change.
