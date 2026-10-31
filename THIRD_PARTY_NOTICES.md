# Third-party notices

MemeLoop Token Center depends on third-party open-source software. Published
container images include image-level SPDX SBOM and SLSA provenance evidence.

The distributed release artifact also contains these separately identifiable
components:

- Sigstore Cosign, official `v3.1.4` Linux amd64 release binary, pinned to
  SHA-256 `424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa`.
  Its release SBOM records Go `1.27.3` and `github.com/klauspost/compress v1.20.1`.
  Its upstream Apache-2.0 license from commit
  `0e48502cf5ef34e2e60bbd3617aa19945712f705` is included
  as `third-party-licenses/cosign-LICENSE`.
- The source-vendored `rust_decimal` crate. Its MIT license is included as
  `third-party-licenses/rust_decimal-LICENSE`.

- The source-vendored `wreq-util` 3.0.0-rc.14 crate, with only its zstd
  dependency constraint updated. Its Apache-2.0 license is included as
  `third-party-licenses/wreq-util-LICENSE`. The authenticated upstream archive
  and manifest patch are documented in `vendor/wreq-util/MEMELOOP-FORK.md`.

These notices do not replace or modify the licenses of any dependency.
