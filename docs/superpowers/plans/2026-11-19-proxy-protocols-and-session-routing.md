# Proxy Protocols and Session Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement all first-release features approved in the proxy protocol/session routing design on `feat/protocols-session-routing`.

**Architecture:** Keep a Rust/Tokio executable with Redis-backed atomic routing and Python pool maintenance. Separate configuration/identity, routing state, upstream connections, and HTTP/SOCKS protocol handling; use Hyper for HTTP framing and rustls for TLS. Shared Redis scripts enforce egress ownership across instances; connection leases protect old connections during rebinding and crash recovery.

**Tech Stack:** Rust 2021, Tokio, Hyper 1, rustls, Redis async client/Lua, serde/TOML, reqwest for controlled exit probes; Python standard unittest for script regressions.

**Spec:** `docs/superpowers/specs/2026-11-19-proxy-protocols-and-session-routing-design.md`

## Global Constraints

- Implement HTTP forward proxy, CONNECT, HTTPS ingress, SOCKS5 CONNECT; UDP/BIND and HTTPS upstream are P3.
- Four strategies: stick, random, round-robin, shuffle; real egress exclusive within a business, shareable across businesses.
- Authentication expiry, client disconnect and application restart must not clear stick bindings.
- No unauthenticated fallback, direct-target fallback, target MITM, or replay of sent application requests.
- Preserve legacy `proxy:pool:http` and `proxy:pool:socks5` hashes and existing untracked `data/`.
- No Git commit, push or PR without a subsequent explicit user request.

## Review Focus

- Two instances simultaneously reserve aliases of the same egress: only one business/session wins.
- Old active streams overlap stick rebinding or release: old egress remains unavailable to another session until drained.
- HTTP connection reuse, chunked bodies and proxy credentials: framed streaming works and credentials never leak upstream.
- Redis failure while leases are active: close affected streams before lease expiry rather than allowing duplicate ownership.
- Client-supplied route identities, expiry and password changes: reject ambiguity and unauthorized ownership without changing a good stick binding.

---

## File Structure and Interfaces

- `src/config.rs`: serializable `Config`, `User`, `Listener/Tls` settings and CLI override validation.
- `src/auth.rs`: `Identity { owner, business, session, strategy }`, versioned username parsing, credential checks and expiry records.
- `src/routing.rs`: `Proxy`, `Strategy`, `Router`, `Reservation`; only allocation interface exposed to protocols.
- `src/store.rs` and `src/lua/*.lua`: async Redis operations, atomic allocation/renew/release/failure/session-release scripts.
- `src/upstream.rs`: `Target`, HTTP CONNECT/SOCKS5 wire handshakes, forward-proxy transport, egress probing.
- `src/http.rs`: Hyper request framing, HTTP auth, forwarding, upgrades, HTTPS stream support.
- `src/socks5.rs`: strict methods/auth/CONNECT parsing and protocol error mapping.
- `src/server.rs`, `src/lib.rs`, `src/main.rs`, `src/args.rs`: shared state, listeners, lifecycle and management subcommands.
- `tests/`: local fake targets/upstreams, temporary Redis integration, TLS fixtures.
- `scripts/`: repair existing ingestion/checking and add regressions without building a governance system.
- `rproxy.example.toml`, `README.md`: runnable configuration, client examples, guarantees and limitations.

### Task 1: Configuration, identity and baseline

**Interfaces:** `Config::load(path) -> Result<Config>`; `Identity::parse(username, defaults) -> Result<Identity>`; `Authenticator::verify(username,password) -> Result<Identity>`; `Strategy: FromStr`.

- [ ] Add configuration/identity tests: all four strategy names, route encoding, 255-byte SOCKS limit, unknown strategy, half-configured TLS/auth and unauthorized business rejection.
- [ ] Run `cargo test --lib` and observe missing-module/API failures before implementation (baseline may first require dependency resolution).
- [ ] Replace obsolete dependencies and missing `list` declaration; implement validated TOML configuration and CLI, `v1|user|business|session|strategy` route username with restricted identifiers and ordinary-username defaults; compare credential digests in constant time and use configurable fixed authentication TTL (default 3600s).
- [ ] Run `cargo test --lib`; verify tests pass. No commit.

### Task 2: Atomic routing and persistence

**Interfaces:** `Router::reserve(identity, capability) -> Result<Reservation>`; `Reservation::renew/release`; `Router::fail(reservation, reason)`; `Router::release_session(identity)`; `Proxy { id, protocol, address, egress, capabilities }`.

- [ ] Add tests for stick reuse/restart, same-business conflicts, cross-business reuse, alias egress conflicts, random no-repeat/exhaustion, round-robin and shuffle cycles, session ownership and strategy consistency, release/drain, atomic failover and lease expiry.
- [ ] Run tests and observe missing behavior.
- [ ] Implement Redis hash compatibility, verified exit metadata, scripts for ownership and leases, bounded failed-upstream cooldown, session/strategy state and bounded retry budget; reject unstable/unknown exits.
- [ ] Run unit and Redis integration tests, including concurrent independent router instances. No commit.

### Task 3: Upstream channels and SOCKS5

**Interfaces:** `connect_tunnel(proxy,target,timeout) -> Result<TcpStream, ConnectError>`; `serve_socks(stream,state)`; `relay(stream,upstream,reservation)` renews leases and terminates on lost ownership.

- [ ] Add local fake-upstream tests for all SOCKS address types, no-auth/password negotiation, wrong/missing credentials, unsupported commands, HTTP CONNECT upstream failure and half-close; no target connection before authentication.
- [ ] Run selected tests red.
- [ ] Implement HTTP CONNECT and SOCKS5 upstream handshakes, precise failure classification (target refusal is not proxy failure), SOCKS5 ingress, bounded handshakes and lease-guarded relay; initial connect failures may replace a failed proxy before application bytes.
- [ ] Run tests green. No commit.

### Task 4: HTTP/CONNECT/TLS ingress

**Interfaces:** `serve_http(io,state)` handles TCP or TLS; shared routing identity stays fixed per client connection, while HTTP request authentication is checked per request.

- [ ] Add tests for HTTP forwarding over both upstream types, CONNECT over both types, 407, credential stripping, chunked POST/response, keep-alive to different targets, rejected route mutation, successful/failed CONNECT, TLS ingress and self-signed trust behavior.
- [ ] Run selected tests red.
- [ ] Implement Hyper streaming/framing and upgrades, sanitize hop-by-hop headers (including Connection-nominated fields), prevent proxy credential leakage, preserve required WebSocket upgrade behavior where supported by the HTTP path, validate authorities/targets, load rustls certificate chain/key and never downgrade on TLS failure.
- [ ] Run tests green. No commit.

### Task 5: Lifecycle, exit verification, script repairs and clients

**Interfaces:** `run(Config)` binds listeners, initializes Redis and egress verification; CLI `release-session` requires configured administrative credential checks and safe draining; `verify-pool` probes configured exit endpoints through upstreams and saves verified egress metadata.

- [ ] Add tests for authentication TTL/version changes, anonymous defaults, Redis failure cleanup, app restart reuse, explicit release, fixed/unstable metadata and configuration errors; Python tests for SOCKS crawler arguments, protocol-specific check, capacity enforcement and interval units.
- [ ] Run tests red.
- [ ] Wire lifecycle, TLS/HTTP/SOCKS listeners, managed renewal and shutdown, exit probe via configurable HTTP(S) endpoint/CA with explicit insecure flag, local release command, configuration overrides and script repairs (async retry iteration, status checks, finite timeouts, seconds-based scheduler documented consistently).
- [ ] Run tests green. No commit.

### Task 6: Full verification, documentation and review

- [ ] Document configuration, route username format/escaping, requests/curl/browser automation examples, TLS trust caveats, real-exit limits and connection-level selection; update README and example config.
- [ ] Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets` and Python unittest suite.
- [ ] Run controlled local end-to-end tests against real temporary Redis and fake HTTP/SOCKS upstreams; verify CLI/config and HTTPS ingress using generated test certificates. Record any environment/client coverage gaps honestly.
- [ ] Have a fresh reviewer inspect the entire branch against the spec, fix verified defects and rerun affected/full checks.
- [ ] Report branch, implemented behavior, exact checks and any remaining limitations. Do not commit or push.

## Execution result

Tasks 1–6 executed inline on `feat/protocols-session-routing`, without commits or pushes. Final checks: 40 Rust tests, 3 Python tests, strict Clippy, release build and seven real-client smoke paths passed. Confirmed review findings were fixed with RED→GREEN regressions; full details and remaining limits are in `docs/superpowers/verification/2026-11-19-proxy-features.md`. Checkboxes above retain the original task template; this result and the verification report are the completion record.

## Self-review

The six tasks cover all first-release design sections. The review-focus cases belong to Tasks 2/4/5 and must have executable regressions. Configuration defaults and internal library choices are implementation decisions, not extra product scope. The user requested implementation, so execute inline on the requested branch without another approval round; reserve questions for actual blockers.
