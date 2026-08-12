# syntax=docker/dockerfile:1.7
FROM node:24-bookworm-slim AS web-builder
WORKDIR /build/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web ./
RUN npm run build

FROM rust:1.95-bookworm AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --locked --release --bin memeloop-token-center

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home token-center
COPY --from=builder /build/target/release/memeloop-token-center /usr/local/bin/memeloop-token-center
COPY --from=web-builder /build/web/dist /usr/share/memeloop-token-center/web
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/memeloop-token-center"]
CMD ["serve"]
