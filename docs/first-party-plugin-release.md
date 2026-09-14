# First-party Model Guard release and installation

## Availability is explicit

`mtc-model-guard` is the first-party configurable admission package in
`plugin-sources/model-guard`. Default `blocked_models: []` allows all models;
only an administrator's explicit configuration enables a denial. It has no
request rewrite, provider/OAuth, network, KV or UI-code contribution. It uses the
existing Plugins configuration editor, not a separate administration application.

This change adds the release workflow and compatible installer, **not a claimed
published digest**. Until a successful `publish-first-party-plugin` run produces
`plugin-release.json` with `installation_verified: true`, release state is
**not published / unavailable for installation**. Never substitute an image
digest, fixture Wasm, fabricated signature, branch tag or locally compiled file.

The release coordinator must first publish a service/plugin-installer release
containing optional keyless support. Review that release's existing immutable
image/source evidence, then use a reviewed master PR to set
`.github/first-party-plugin-installer-trust.json` to `status: "ready"`, its exact
`sha256:...` digest and full `source_revision`. The repository is fixed to
`ghcr.io/memeloop-online/memeloop-token-center-plugin-installer`. This change
pins the compatible installer from successful master
[CI run 34891993755](https://github.com/memeloop-online/memeloop-token-center/actions/runs/34891993755),
source `69b03ff0f37f76458bd6b49a05faa9aa43383493`, digest
`sha256:5d01c0a753358c241fa01ae3c94afe1dd71ec4c1708711dd64f774f49488a980`.
The same run's `ghcr-release-69b03ff0f37f76458bd6b49a05faa9aa43383493`
and `image-digest-plugin-installer-69b03ff0f37f76458bd6b49a05faa9aa43383493`
artifacts bind the release manifest, image metadata, OCI index and BuildKit
SPDX/SLSA statements. Independent registry reads confirmed the immutable digest,
the statement subjects, source/revision labels and patched Cosign
`v3.1.3-mtc.3` label. This is a reviewed CI-image pin, not a claim that BuildKit
statements are independently signed or that the plugin is already published.
It supports Model Guard; future packages requiring a newer installer schema must
first obtain a newly reviewed compatible image pin.

Only after that review is merged, manually dispatch
`.github/workflows/publish-first-party-plugin.yml` from `master`. There is no
installer input: dispatch cannot select executable code. Before registry login,
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

Download the successful run's `mtc-model-guard-release-<git-sha>` artifact. Review
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
