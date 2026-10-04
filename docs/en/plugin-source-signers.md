# Per-source installation signers

The host installation policy supports `source_keyless`, a map from exact OCI
repository names (without tags or digests) to exact `{ "issuer", "identity" }`
objects. The map is limited to sixteen entries and the policy file remains
limited to 64 KiB. Each issuer must be `https://token.actions.githubusercontent.com`;
identities use the existing exact GitHub Actions workflow identity validation.
No regular-expression identity flags are passed to the installer.

Choose one trust mode: existing `cosign_public_keys`, existing `cosign_keyless`,
or nonempty `source_keyless`. Map mode must cover exactly `allowed_sources`.
Missing, unknown, duplicate, excessive, or invalid entries and mixed trust modes
are rejected. An omitted or empty map preserves legacy policy behavior and
trust fingerprints. The entire nonempty map is included in the trust fingerprint,
so changing any source or signer invalidates existing review/checkpoint trust.
This is host configuration, not an HTTP request field; no OpenAPI change is needed.

To prepare a policy for health-intelligence and Claude, copy every existing
allowed source into the map with its currently trusted exact issuer and identity.
Then add `ghcr.io/memeloop-online/claude-code-wire` to both `allowed_sources` and
the map with:

```json
{
  "issuer": "https://token.actions.githubusercontent.com",
  "identity": "https://github.com/memeloop-online/claude-code-wire/.github/workflows/publish-oci.yml@refs/heads/master"
}
```

Remove the global `cosign_keyless` field only after its existing identity has
been preserved for every existing source in the map. Do not replace health's
identity with Claude's identity or broaden either identity. Review the complete
policy before applying it.

The supplied official release evidence records Claude
`ghcr.io/memeloop-online/claude-code-wire@sha256:3e4160580005b15c9db1e3bd419df79ee65de1d86a8fe1d4ec5aac4c88fe5906`
and the identity above. It records successful installation and rejection of a
wrong identity; anonymous registry access was not established. Source credentials
remain separately scoped through `source_credentials`. This support does not
activate a plugin, create credentials, or change deployed GitOps policy.
