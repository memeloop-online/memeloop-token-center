# syntax=docker/dockerfile:1.7
ARG NODE_IMAGE=node:24.18.0-bookworm-slim
ARG RUST_IMAGE=rust:1.95.0-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${NODE_IMAGE} AS web-builder
ARG NPM_REGISTRY=https://registry.npmmirror.com
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm config set registry "${NPM_REGISTRY}" && npm ci
COPY web ./
RUN npm run build

FROM ${RUST_IMAGE} AS builder
ARG DEBIAN_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian
ARG CARGO_REGISTRY=sparse+https://rsproxy.cn/index/
ARG MTC_BUILD_GIT_SHA=unknown
ARG MTC_BUILD_TIMESTAMP=unknown
ARG MTC_BUILD_TARGET=unknown
RUN sed -i "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /usr/local/cargo \
    && printf '[source.crates-io]\nreplace-with = "build-mirror"\n[source.build-mirror]\nregistry = "%s"\n' "${CARGO_REGISTRY}" \
      > /usr/local/cargo/config.toml
WORKDIR /build
COPY .cargo/config.toml /build/.cargo/config.toml
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir -p src tests \
    && printf 'pub fn dependency_cache_marker() {}\n' > src/lib.rs \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf 'fn main() {}\n' > tests/cucumber.rs \
    && printf 'fn main() {}\n' > tests/postgres.rs \
    && cargo build --locked --release --bin memeloop-token-center \
    && rm -rf target/release/.fingerprint/memeloop-token-center-* \
      target/release/deps/libmemeloop_token_center* \
      target/release/deps/memeloop_token_center-* \
      target/release/memeloop-token-center \
    && rm -rf src
COPY src ./src
COPY migrations ./migrations
COPY schemas ./schemas
COPY wit ./wit
RUN MTC_BUILD_GIT_SHA="${MTC_BUILD_GIT_SHA}" \
    MTC_BUILD_TIMESTAMP="${MTC_BUILD_TIMESTAMP}" \
    MTC_BUILD_TARGET="${MTC_BUILD_TARGET}" \
    cargo build --locked --release --bin memeloop-token-center --bin import-cpa-session-archive \
    && cp target/release/memeloop-token-center /tmp/memeloop-token-center \
    && cp target/release/import-cpa-session-archive /tmp/import-cpa-session-archive \
    && rm -rf target /usr/local/cargo/registry /usr/local/cargo/git

FROM ${RUNTIME_IMAGE}
ARG DEBIAN_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian
RUN sed -i "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home token-center
COPY --from=builder /tmp/memeloop-token-center /usr/local/bin/memeloop-token-center
COPY --from=builder /tmp/import-cpa-session-archive /usr/local/bin/import-cpa-session-archive
COPY --from=web-builder /build/web/dist /usr/share/memeloop-token-center/web
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/memeloop-token-center"]
CMD ["serve"]
