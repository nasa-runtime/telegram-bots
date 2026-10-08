FROM docker.io/library/rust:1.94-alpine@sha256:77237dd363a0b127bb5ef532c2d64c0deb380b738e43a9c4bdac73398d6d0a08 AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin telegram-bots -j 2 \
    --config 'profile.release.opt-level="s"' \
    --config 'profile.release.lto="thin"' \
    --config 'profile.release.codegen-units=1' \
    --config 'profile.release.strip="symbols"'

FROM docker.io/library/alpine:3.23@sha256:85fe1e81d6758c208f3e1eed4338a1997e19d4be002d4dd32d3100c9a8c010a0 AS runtime
ARG APP_VERSION=1.0.0
ARG SOURCE_REVISION=unknown
LABEL org.opencontainers.image.title="telegram-bots" \
    org.opencontainers.image.description="Telegram notification gateway with atomic catalog reloads, bounded queues and per-bot ordering" \
    org.opencontainers.image.source="https://github.com/nasa-runtime/telegram-bots" \
    org.opencontainers.image.url="https://hub.docker.com/r/nasaruntime/telegram-bots" \
    org.opencontainers.image.licenses="MIT OR Apache-2.0" \
    org.opencontainers.image.version="${APP_VERSION}" \
    org.opencontainers.image.revision="${SOURCE_REVISION}"
RUN apk add --no-cache ca-certificates \
    && addgroup -S -g 10001 telegram \
    && adduser -S -D -H -u 10001 -G telegram -s /sbin/nologin telegram \
    && mkdir -p /usr/local/logs/telegram-bots \
    && chown 10001:10001 /usr/local/logs/telegram-bots
WORKDIR /app
COPY --from=builder /build/target/release/telegram-bots /app/telegram-bots
COPY zcf/application.yml zcf/application-nacos.yml ./zcf/
COPY LICENSE-MIT LICENSE-APACHE /usr/share/licenses/telegram-bots/
USER 10001:10001
EXPOSE 2060
STOPSIGNAL SIGTERM
ENTRYPOINT ["/app/telegram-bots"]
