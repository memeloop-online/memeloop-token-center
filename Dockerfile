# syntax=docker/dockerfile:1.7
ARG NODE_IMAGE=node:24.18.0-bookworm-slim
ARG RUST_IMAGE=rust:1.95.0-bookworm
ARG GO_IMAGE=golang:1.26.7-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/base-nossl-debian13:nonroot

FROM ${GO_IMAGE} AS cosign-builder
ADD --checksum=sha256:3a718446bac51466efff6853639e1ca108b456ecbf07cd92938f548715d22d6b \
    https://codeload.github.com/sigstore/cosign/tar.gz/11926fa5bbbbde47e88fc006b625a17769b743b2 \
    /tmp/cosign.tar.gz
RUN mkdir -p /build/cosign \
    && tar -xzf /tmp/cosign.tar.gz -C /build/cosign --strip-components=1
WORKDIR /build/cosign
COPY packaging/cosign/v3.1.3-security.patch /tmp/v3.1.3-security.patch
RUN git apply --unidiff-zero --check /tmp/v3.1.3-security.patch \
    && git apply --unidiff-zero /tmp/v3.1.3-security.patch \
    && GOTOOLCHAIN=local go mod verify \
    && CGO_ENABLED=0 GOTOOLCHAIN=local go build -trimpath \
      -ldflags '-buildid= -X sigs.k8s.io/release-utils/version.gitVersion=v3.1.3-mtc.3 -X sigs.k8s.io/release-utils/version.gitCommit=11926fa5bbbbde47e88fc006b625a17769b743b2 -X sigs.k8s.io/release-utils/version.gitTreeState=clean -X sigs.k8s.io/release-utils/version.buildDate=2026-08-06T00:10:15Z' \
      -o /tmp/cosign ./cmd/cosign \
    && /tmp/cosign version --json \
      | grep -Eq '"gitVersion"[[:space:]]*:[[:space:]]*"v3\.1\.3-mtc\.3"'

FROM ${NODE_IMAGE} AS web-builder
ARG NPM_REGISTRY=https://registry.npmjs.org
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm config set registry "${NPM_REGISTRY}" && npm ci
COPY web ./
RUN npm run build

FROM ${RUST_IMAGE} AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# Keep dependency-cache cleanup deterministic even when a trusted mirror base
# image defines its own global Cargo target directory.
ENV CARGO_TARGET_DIR=/build/target
COPY .cargo/config.toml /build/.cargo/config.toml
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir -p src/bin tests \
    && printf 'pub fn dependency_cache_marker() {}\n' > src/lib.rs \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf 'fn main() {}\n' > src/bin/install-plugin-oci.rs \
    && printf 'fn main() {}\n' > tests/cucumber.rs \
    && printf 'fn main() {}\n' > tests/postgres.rs \
    && cargo build --locked --release --features experimental-plugin-revisions,plugin-distribution --bin memeloop-token-center --bin install-plugin-oci \
    && cargo clean --release --package memeloop-token-center \
    && rm -rf src
COPY build.rs ./build.rs
COPY src ./src
COPY migrations ./migrations
COPY schemas ./schemas
COPY wit ./wit
ARG MTC_BUILD_GIT_SHA_INPUT=unknown
ARG MTC_BUILD_TIMESTAMP_INPUT=unknown
ARG MTC_BUILD_TARGET_INPUT=unknown
FROM builder AS release-input
RUN MTC_BUILD_GIT_SHA="${MTC_BUILD_GIT_SHA_INPUT}" \
    MTC_BUILD_TIMESTAMP="${MTC_BUILD_TIMESTAMP_INPUT}" \
    MTC_BUILD_TARGET="${MTC_BUILD_TARGET_INPUT}" \
    cargo build --locked --release --features experimental-plugin-revisions,plugin-distribution --bin memeloop-token-center --bin install-plugin-oci \
    && install -D -m 0555 target/release/memeloop-token-center /release-input/memeloop-token-center \
    && install -D -m 0555 target/release/install-plugin-oci /release-input/install-plugin-oci \
    && install -D -m 0644 "$(gcc -print-file-name=libgcc_s.so.1)" /release-input/libgcc_s.so.1 \
    && install -D -m 0644 "$(g++ -print-file-name=libstdc++.so.6)" /release-input/libstdc++.so.6 \
    && rm -rf target /usr/local/cargo/registry /usr/local/cargo/git

COPY --from=cosign-builder /tmp/cosign /release-input/cosign
COPY --from=cosign-builder /build/cosign/LICENSE /release-input/third-party-licenses/cosign-LICENSE
COPY --from=web-builder /build/web/dist /release-input/web
COPY LICENSE THIRD_PARTY_NOTICES.md /release-input/
COPY vendor/rust_decimal/LICENSE /release-input/third-party-licenses/rust_decimal-LICENSE

FROM ${RUNTIME_IMAGE} AS release-input-smoke
# SQLite copies every bound archive ciphertext through libc. Keep those
# transient ~114 KiB allocations out of glibc arenas so they are unmapped when
# the statement clears its bindings instead of fragmenting long-lived heaps.
# Deployments can override this standard glibc tunable through the container
# environment when profiling a different libc workload.
ENV LD_LIBRARY_PATH=/usr/local/lib \
    GLIBC_TUNABLES=glibc.malloc.mmap_threshold=65536
COPY --from=release-input /release-input /release-input
COPY --from=release-input /release-input/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1
COPY --from=release-input /release-input/libstdc++.so.6 /usr/local/lib/libstdc++.so.6
COPY --from=release-input /release-input/memeloop-token-center /usr/local/bin/memeloop-token-center
COPY --from=release-input /release-input/install-plugin-oci /usr/local/bin/install-plugin-oci
COPY --from=release-input /release-input/cosign /usr/local/bin/cosign
COPY --from=release-input /release-input/LICENSE /usr/share/licenses/memeloop-token-center/LICENSE
COPY --from=release-input /release-input/THIRD_PARTY_NOTICES.md /usr/share/licenses/memeloop-token-center/THIRD_PARTY_NOTICES.md
COPY --from=release-input /release-input/third-party-licenses /usr/share/licenses/memeloop-token-center/third-party
# A release-input artifact is only valid when the Docker-native binary starts
# against the exact distroless runtime that will publish it.
RUN ["/usr/local/bin/memeloop-token-center", "--help"]
RUN ["/usr/local/bin/install-plugin-oci", "--help"]
RUN ["/usr/local/bin/cosign", "version", "--json"]

FROM scratch AS release-input-export
COPY --from=release-input-smoke /release-input /

FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.licenses="Apache-2.0"
ENV LD_LIBRARY_PATH=/usr/local/lib \
    GLIBC_TUNABLES=glibc.malloc.mmap_threshold=65536
COPY --from=release-input /release-input/libgcc_s.so.1 /usr/local/lib/libgcc_s.so.1
COPY --from=release-input /release-input/libstdc++.so.6 /usr/local/lib/libstdc++.so.6
COPY --from=release-input /release-input/memeloop-token-center /usr/local/bin/memeloop-token-center
COPY --from=release-input /release-input/install-plugin-oci /usr/local/bin/install-plugin-oci
COPY --from=release-input /release-input/cosign /usr/local/bin/cosign
COPY --from=release-input /release-input/web /usr/share/memeloop-token-center/web
COPY --from=release-input /release-input/LICENSE /usr/share/licenses/memeloop-token-center/LICENSE
COPY --from=release-input /release-input/THIRD_PARTY_NOTICES.md /usr/share/licenses/memeloop-token-center/THIRD_PARTY_NOTICES.md
COPY --from=release-input /release-input/third-party-licenses /usr/share/licenses/memeloop-token-center/third-party
RUN ["/usr/local/bin/memeloop-token-center", "--help"]
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/memeloop-token-center"]
CMD ["serve"]
