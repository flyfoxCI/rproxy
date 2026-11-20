# Embedded Pool Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the approved Redis-free proxy pool and mounted-config Compose deployment to branch v2.2 with an accurate README.

**Architecture:** One Rust state worker owns routing and SQLite; connection occupancy is process-local. Python only fetches candidates via a bearer-authenticated management Interface. Two Compose services share a private network, not a database.

**Tech Stack:** Rust/Tokio/Hyper/rusqlite bundled/fs2; Python 3.11+ tomllib/requests; Docker/Podman Compose.

**Spec:** docs/superpowers/specs/2026-11-20-embedded-pool-design.md

## Global Constraints

- Keep rproxy.example.toml; add scripts/proxy.example.toml; no secrets committed.
- Retain HTTP/HTTPS ingress, HTTP CONNECT, SOCKS5 and four strategies.
- Preserve normalized actual-egress exclusion, stick restart persistence and active connection drain.
- Single owner per SQLite file; no implicit memory fallback or multi-instance promises.
- Branch v2.2, package 0.2.2; do not merge main or publish a release.

## Review Focus

- SQLite write failure must not publish a binding or return a usable reservation.
- Cancellation between reserve and socket use must not leave an unowned connection token indefinitely.
- Old probe results must not resurrect a removed/replaced candidate.
- Missing config/environment secrets must fail clearly without exposing values or creating directories at file mount paths.
- A queued/healthy collection response must not imply verified or stable egress.

---

### Task 1: Typed state worker and routing

**Files:** src/store.rs, src/pool_state.rs, src/routing.rs, src/config.rs, Cargo.toml, tests/routing.rs, tests/embedded_store.rs, tests/review_regressions.rs.

**Interfaces:** Store::open(&Config) -> Result<Store>; async save_proxy/update_verified/proxies/pool_addresses; reserve(&Identity, Capability) -> Allocation; release/fail/release_session typed methods. Preserve Router/Reservation caller-facing shape.

- [ ] Write embedded-store tests for durable stick reopen, exclusive file locking, stale metadata updates and atomic failed allocation.
- [ ] Run `cargo test --test embedded_store` and record initial failure.
- [ ] Implement worker with bundled SQLite, schema version validation, persist-before-publish mutations, runtime-only active tokens and RAII release.
- [ ] Port routing regressions from Redis fixtures; retain semantic assertions, replace distributed lease expiry with lifetime occupancy assertions.
- [ ] Run routing/store/review regression tests and strict clippy.

### Task 2: Management, CLI and process lifecycle

**Files:** src/admin.rs, src/main.rs, src/server.rs, src/args.rs, src/auth.rs, src/config.rs, src/upstream.rs, tests/admin.rs, tests/cli.rs, tests/config_auth.rs, tests/regressions.rs.

**Interfaces:** endpoints and body schema exactly as spec; candidate import triggers Store pool-change watch, server verifier coalesces notifications; CLI contacts running process.

- [ ] Add auth/body/capacity tests and live release CLI assertions; prove RED.
- [ ] Implement bounded management listener with constant-time bearer comparison, aggregate status and live commands; resolve password_env at startup.
- [ ] Keep shutdown/HTTP framing/TLS regressions; remove Redis failure fixture, replace with SQLite worker failure/ownership scenarios.
- [ ] Run `cargo test --all-targets` without Redis and `cargo clippy --all-targets -- -D warnings`.

### Task 3: Configurable Python collector

**Files:** scripts/collector.py, scripts/proxy.example.toml, scripts/requirements.txt, scripts/tests/test_collector.py; old crawl/check/conf/maintenance code retired or explicitly replaced.

**Interfaces:** POST /v1/proxies exact spec schema; env-based bearer token. No direct SQLite, egress assertion or gateway-credential forwarding.

- [ ] Add local fake HTTP tests for text/JSON fields, GET/POST, strict TOML, environment secrets, retries and idle providers.
- [ ] Run unittest to prove RED, implement and rerun GREEN.
- [ ] Provide collector --config, --once, --check-config and safe non-secret error reporting.

### Task 4: Compose and packaging

**Files:** Dockerfile, compose.yaml, .dockerignore, .env.example, scripts/deployment_tests/*, scripts/package_release.py, .github/workflows/ci.yml.

**Interfaces:** rproxy and collector image targets; config mounts, ports, UID and env names exactly as spec. Health command `rproxy --config /etc/rproxy/rproxy.toml healthcheck`.

- [ ] Add static/render tests for read-only mounts, no published management port/Redis, no secrets in build context and both targets.
- [ ] Implement deployment files and run config validation for available Docker/Podman provider.
- [ ] Build and run actual containers if local runtime is usable; otherwise add CI smoke validation and report local limitation.
- [ ] Update release packaging allowlist to include Python TOML template; retain exact five target verification.

### Task 5: Integration, README and commit

**Files:** README.md, rproxy.example.toml, scripts/smoke.py, docs/releases/v0.2.2.md, .gitignore, Cargo.lock.

- [ ] Document prerequisites, one-command startup, all mounted paths, secrets, static import, supplier mappings, gateway limitations, single-instance restriction, backup/upgrade and Redis migration break.
- [ ] Run format, strict clippy, all Rust/Python/release/deployment tests and local fake-upstream smoke; report skipped container checks if any.
- [ ] Run focused independent review of state consistency and deployment security; fix confirmed defects.
- [ ] Commit with required attribution and push branch v2.2 only; provide commit and verification summary.

## Execution result

Implementation and review fixes are complete. Windows Rust: 55 tests; Linux x86_64 release-mode Rust: 57 tests; collector: 7 tests; release/linker packaging: 8 tests; deployment static/render: 6 tests. Final-source Podman Compose rebuild and isolated end-to-end startup/config-mount/persistence smoke passed. Native requests/curl/Chrome and live release-session smoke passed. Full evidence, decisions and platform limitations are in `docs/superpowers/verification/2026-11-20-embedded-pool.md`; checkboxes above retain the original execution checklist, not a claim of unfinished implementation. Delivery is branch-only, without a main merge or release. The extra CI workflow edit is deferred because the available GitHub token lacks workflow scope and the secure credential request timed out; existing workflows remain unchanged. The complete workflow proposal is retained in local branch `backup/v2.2-ci-d6de2ce`, not lost or silently published.
