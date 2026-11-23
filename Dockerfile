FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY xtask/Cargo.toml ./xtask/Cargo.toml
COPY xtask/src ./xtask/src
RUN cargo build --release --locked --jobs 1 -p rproxy --bins

FROM build AS fixture-build
RUN cargo build --release --locked --jobs 1 -p xtask

FROM docker.io/library/debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 rproxy \
    && useradd --uid 10001 --gid 10001 --no-create-home rproxy \
    && mkdir -p /var/lib/rproxy \
    && chown 10001:10001 /var/lib/rproxy
USER 10001:10001
WORKDIR /var/lib/rproxy

# Smoke-test only: never included in the production image or native archives.
FROM runtime AS test-fixture
COPY --from=fixture-build /build/target/release/xtask /usr/local/bin/
EXPOSE 18080 18081
ENTRYPOINT ["xtask", "fixture"]

FROM runtime AS rproxy
COPY --from=build /build/target/release/rproxy /build/target/release/rproxy-cli /usr/local/bin/
EXPOSE 9099 9089 9443
ENTRYPOINT ["rproxy"]
CMD ["--config", "/etc/rproxy/rproxy.toml", "serve"]
