# Embedded proxy pool and Compose design

## Intent and approval

The user approved implementing the discussed Redis-free design, Docker/Podman Compose startup, mounted configuration, configurable Python collection and README, and asked to commit to `v2.2`. Work is on branch `v2.2`; package version follows the existing series as `0.2.2`. No main merge or formal release is implied.

## Architecture

A single Rust process owns proxy admission, routing, actual-egress exclusion, connection occupancy and durable session bindings. SQLite is embedded (bundled library); a dedicated worker owns the connection and serializes typed state transitions. Blocking database work never runs on a Tokio executor thread. Durable mutations commit before success and before publishing changed memory state. Socket I/O and network probes occur outside the worker.

Persist proxy records and business session state (owner, strategy, binding, last/seen/cursor). Keep connection occupancy, authentication periods and cooldown deadlines in memory. Process restart clears dead connections, not stick bindings. Process-lifetime file locking rejects a second owner of a file-backed database: lock its inode on Unix, or a non-deletable canonical sidecar on Windows (whole-file Windows byte locks would also block SQLite). Require a local filesystem with reliable locking. Retain ownership if the state worker fails while connection handles still exist. No multi-machine global exclusion is promised. State corruption/unsupported schema or write errors fail closed, never reset to an empty pool silently.

Protocol adapters consume the same Router and Reservation interface. A Reservation owns an occupancy token; release/Drop removes it, while logical stick bindings survive. No distributed lease-expiration timer may free an exit while its socket is alive. Server shutdown still closes upgraded tunnels.

Python is an optional collector, not a database writer or a second health authority. It uses Python 3.11+ TOML, configurable GET/POST requests, params/headers, environment references, text/JSON field extraction and bounded batches. It submits numeric IP:port candidates to Rust. No arbitrary eval, supplier-specific gateway session guarantee, or fabricated Rola API URL. Credentialed rotating gateways remain unsupported and must be documented explicitly.

## Interfaces and configuration

Rust top-level `database` defaults to `data/rproxy.sqlite3`; `max_proxies` defaults to 1000. Remove Redis config, flags and dependency. Relative database/certificate paths resolve relative to TOML. Auth users can use either `password` or `password_env`; startup resolves required credentials, check-config is structural. Default `admin.bind` is loopback `127.0.0.1:9100`, `admin.token_env` is `RPROXY_ADMIN_TOKEN`. Missing token with management enabled fails startup. Explicit `bind = false` is not TOML socket syntax: disable using `enabled = false`.

Management Interface, bearer token required for every endpoint, body at most 64 KiB, batches at most 256:
- `GET /health`: 200 JSON when the state worker responds, otherwise 503.
- `POST /v1/proxies`: `{ "protocol": "http"|"socks5", "addresses": ["127.0.0.1:8080"], "source": "supplier-name" }`; returns 202 `{ "accepted": N }`. Validate whole batch first, enforce capacity atomically, never trust submitted egress metadata. New candidates trigger coalesced background probing; collection acknowledgement is not admission. Pending candidates also resume verification at startup when periodic refresh is disabled. Failed unverified candidates are removed; recent verified bindings are retained on an isolated probe error, but expired records failing recheck are retired.
- `POST /v1/verify`: coalesce a full-pool verification request; return 202 `{ "scheduled": true }` immediately. Use status to observe verified counts, not an unbounded synchronous HTTP job.
- `POST /v1/sessions/release`: `{ "username": "...", "password": "..." }`; verify entry identity and release through live Router, preserving old active occupancy until drain.
- `GET /v1/status`: aggregate counts only, no credentials.

CLI `healthcheck`, `verify-pool`, `add-proxy --protocol ... --address ...`, `release-session` contact the running management Interface. `check-config` does not open SQLite or require secret values. The management listener is not published by Compose; bearer tokens do not encrypt HTTP, so it is for trusted local/container networks only.

## Deployment

One multi-stage Dockerfile has `rproxy` and `collector` targets. One `compose.yaml` works with Docker Compose and Podman Compose provider, two services, no Redis. Use named volume at `/var/lib/rproxy`, config binds read-only at `/etc/rproxy/rproxy.toml` and `/etc/rproxy/proxy.toml`, certificates read-only at `/etc/rproxy/certs`. Runtime UID/GID 10001, init volume ownership at image build, Linux amd64/arm64-compatible source build. Proxy host ports bind loopback by default; management is internal only. Collector retries readiness independently of Compose depends_on. No secrets/config/data in build context. Do not mount an absent certificate directory by default; HTTPS is opt-in override/config.

Copy `rproxy.example.toml` and `scripts/proxy.example.toml` to actual ignored configs, copy `.env.example` to `.env` and replace secrets, then `docker compose up -d --build` or `podman compose up -d --build`. Real configs reference secret environment names. Mount existing files without silently creating directories. Restart applies config; down preserves state, down -v deletes it. Collector with no enabled providers stays healthy/idle, does not crash-loop.

## Non-goals and migration

No Redis compatibility backend, distributed locks, hot reload, web UI or automatic importing of Redis sessions. Old Redis deployments require an explicit maintenance window and candidate re-import; old bindings are not silently claimed migrated. Existing protocol/framing/TLS and strategy regression tests remain.

## Verification

Routing tests use isolated in-memory/file SQLite, including cross-process lock, durable reopen, failed commit, normalized egress exclusivity, failed reservation rollback, stale probe generation and old connection drain. Management tests use real local HTTP, bounded auth/body parsing and live CLI operations. Python uses local fake supplier and management endpoints, no real credentials. Container config rendering/build/startup is tested where a runtime is available; any unavailable runtime check is reported honestly. Five-platform binary packaging remains intact and includes the Python TOML template.
