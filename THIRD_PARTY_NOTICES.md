# Third-party notices

MemeLoop Token Center depends on third-party open-source software. Published
container images include image-level SPDX SBOM and SLSA provenance evidence.

The distributed release artifact also contains these separately identifiable
components:

- Sigstore Cosign, built from upstream commit
  `11926fa5bbbbde47e88fc006b625a17769b743b2` with the checked-in security
  patch under `packaging/cosign/`. Its upstream Apache-2.0 license is included
  as `third-party-licenses/cosign-LICENSE`.
- The source-vendored `rust_decimal` crate. Its MIT license is included as
  `third-party-licenses/rust_decimal-LICENSE`.

These notices do not replace or modify the licenses of any dependency.
