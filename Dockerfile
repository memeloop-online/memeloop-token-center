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
RUN sed -i "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /usr/local/cargo \
    && printf '[source.crates-io]\nreplace-with = "build-mirror"\n[source.build-mirror]\nregistry = "%s"\n' "${CARGO_REGISTRY}" \
      > /usr/local/cargo/config.toml
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'pub fn dependency_cache_marker() {}\n' > src/lib.rs \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --locked --release --bin memeloop-token-center \
    && rm -rf src
COPY src ./src
COPY migrations ./migrations
COPY schemas ./schemas
COPY wit ./wit
RUN cargo build --locked --release --bin memeloop-token-center \
    && cp target/release/memeloop-token-center /tmp/memeloop-token-center \
    && rm -rf target /usr/local/cargo/registry /usr/local/cargo/git

FROM ${RUNTIME_IMAGE}
ARG DEBIAN_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/debian
RUN sed -i "s|http://deb.debian.org/debian|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home token-center
COPY --from=builder /tmp/memeloop-token-center /usr/local/bin/memeloop-token-center
COPY --from=web-builder /build/web/dist /usr/share/memeloop-token-center/web
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/memeloop-token-center"]
CMD ["serve"]
