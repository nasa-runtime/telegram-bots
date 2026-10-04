FROM rust:1.94-bookworm AS builder
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin telegram-bots

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 telegram \
    && useradd --system --uid 10001 --gid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin telegram
WORKDIR /app
COPY --from=builder /workspace/target/release/telegram-bots /usr/local/bin/telegram-bots
COPY zcf/application.yml zcf/application-nacos.yml ./zcf/
COPY LICENSE-MIT LICENSE-APACHE /usr/share/licenses/telegram-bots/
USER 10001:10001
EXPOSE 2060
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/telegram-bots"]
