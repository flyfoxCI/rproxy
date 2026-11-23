FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY xtask/Cargo.toml ./xtask/Cargo.toml
COPY xtask/src ./xtask/src
RUN cargo build --release --locked --jobs 1 -p rproxy --bins

FROM docker.io/library/debian:bookworm-slim AS rproxy
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 rproxy \
    && useradd --uid 10001 --gid 10001 --no-create-home rproxy \
    && mkdir -p /var/lib/rproxy \
    && chown 10001:10001 /var/lib/rproxy
COPY --from=build /build/target/release/rproxy /build/target/release/rproxy-cli /usr/local/bin/
USER 10001:10001
WORKDIR /var/lib/rproxy
EXPOSE 9099 9089 9443
HEALTHCHECK --interval=10s --timeout=20s --start-period=10s --retries=3 \
    CMD ["rproxy", "--config", "/etc/rproxy/rproxy.toml", "healthcheck"]
ENTRYPOINT ["rproxy"]
CMD ["--config", "/etc/rproxy/rproxy.toml", "--database", "/var/lib/rproxy/rproxy.sqlite3", "--http-bind", "0.0.0.0:9099", "--socks5-bind", "0.0.0.0:9089", "serve"]
