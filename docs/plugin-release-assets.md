# Browser installation release asset

A public GitHub release can expose a small JSON download asset (for example
`https://github.com/owner/plugin/releases/download/v1.0.0/release.json`):

```json
{
  "format_version": 1,
  "reference": "ghcr.io/owner/plugin@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "signer": {
    "issuer": "https://token.actions.githubusercontent.com",
    "identity": "https://github.com/owner/plugin/.github/workflows/publish.yml@refs/heads/master"
  }
}
```

The digest above is illustrative. Publishers must use the actual signed OCI
artifact digest. The signing workflow must belong to the release repository.
The downloader accepts explicit `.json` release assets and GitHub's
`/releases/latest/download/release.json` form.

This JSON discovers the package and proposes a signer identity. It grants no
installation or execution authority. The browser must show the exact source,
digest and signer for operator confirmation. Installation must subsequently use
the existing OCI verifier with that exact identity and immutable digest, followed
by the existing plugin capability review. A descriptor's SHA-256 is supplied so
the confirmation can bind the fetched bytes even if a release asset changes.

The release download follows at most four redirects, restricted to GitHub and
its release asset hosts, rejects private DNS answers, and caps the complete
operation at 60 seconds and 64 KiB. Direct connections pin validated DNS answers.
An explicitly selected proxy receives only this bounded GitHub request; its
credentials are passed in memory and are absent from the descriptor receipt.
Archives, executables and repository HTML pages are not descriptor assets.
