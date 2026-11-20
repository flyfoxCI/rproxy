FROM docker.io/library/rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --jobs 1

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
ENTRYPOINT ["rproxy"]
CMD ["--config", "/etc/rproxy/rproxy.toml", "serve"]

FROM docker.io/library/python:3.13-slim-bookworm AS collector
RUN groupadd --gid 10001 collector \
    && useradd --uid 10001 --gid 10001 --no-create-home collector
WORKDIR /app
COPY scripts/requirements.txt ./requirements.txt
RUN pip install --no-cache-dir -r requirements.txt
COPY scripts/collector.py ./collector.py
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1
USER 10001:10001
ENTRYPOINT ["python", "-I", "/app/collector.py"]
CMD ["--config", "/etc/rproxy/proxy.toml"]
