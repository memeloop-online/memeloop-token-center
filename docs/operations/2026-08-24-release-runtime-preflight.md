# Service and plugin runtime preflight — 2026-08-24

This evidence explains the final-runtime change made after GitHub Actions run
`32678530047`. It contains no credentials and does not replace the final image
contracts, Trivy scans, SBOM/provenance verification or combined release
manifest.

## Observed release failure

Run `32678530047` passed every pre-publication gate for clean source `79d2b08`,
including the formal 15-minute memory acceptance. Its Alpine importer image
also passed the final-image scan and provenance checks. The plugin-installer
build stopped before publication because the TUNA Debian mirror rejected plain
HTTP with status 403. The service build was cancelled immediately because a
complete release was no longer possible.

The replacement build defaults use
`https://mirrors.tuna.tsinghua.edu.cn/debian`; the Bookworm `InRelease` endpoint
was fetched successfully over TLS before the change was committed. A packaging
contract now rejects any plain-HTTP Debian mirror default.

## Minimal final runtime

The final service and plugin-installer stages now use
`gcr.io/distroless/cc-debian13:nonroot`. The upstream Distroless project
documents this image as the minimal glibc and libgcc runtime for Rust and other
mostly statically compiled programs, includes CA certificates, and publishes a
supported `nonroot` tag without a shell or package manager:
`https://github.com/GoogleContainerTools/distroless`.

All three locally built release binaries requested only these shared objects:

```text
libgcc_s.so.1
libm.so.6
libc.so.6
/lib64/ld-linux-x86-64.so.2
```

The official Cosign 3.1.3 linux-amd64 asset was downloaded over TLS and matched
its checked-in SHA-256
`4629c757b7618056f8ddd7e2625ae9fdd94c0372a65049520bc7d9df9efc7f71`.
`ldd` reported `not a dynamic executable`, so it adds no system-library
requirement beyond the Rust installer.

Local registry access timed out while attempting an independent remote Trivy
scan of public base images. No pass is inferred from that timeout. The next
exact-SHA GitHub Actions run must build and execute both Docker image contracts,
then scan the exact three published digests with the unchanged
HIGH/CRITICAL `exit-code: 1` policy. No image is deployable until the combined
release manifest is verified.
