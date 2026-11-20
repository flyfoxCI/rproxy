# rproxy-cli 0.2.3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the approved online administration design and publish v0.2.3 with rproxy and rproxy-cli.

**Architecture:** Rust owns all mutable management/routing state through the existing SQLite worker; a bounded management Interface serves a separate CLI and least-privileged Python collector. Startup configuration is not a second management authority.

**Tech Stack:** Rust/Tokio/Hyper/rusqlite, Argon2id, XChaCha20-Poly1305, clap/rpassword; Python requests/tomllib; existing Docker/Podman and native five-target release workflow.

**Spec:** docs/superpowers/specs/2026-11-20-rproxy-cli-design.md

## Global Constraints

- Version 0.2.3, immutable tag v0.2.3. Work on feat/rproxy-cli-v0.2.3, not main.
- Preserve protocol/strategy/occupancy invariants and existing bindings during schema migration.
- CLI/collector never open SQLite; root and worker credentials are separate; no secrets in normal output.
- TOML is bootstrap/import/export only for mutable users/sources; no implicit overwrite after restart.
- Do not modify workflow files without authorized workflow scope; package both native binaries using current commands.

## Review Focus

- Deleting the last user must never enable anonymous access; deleted/recreated names cannot inherit bindings.
- A config-file user/password must not reappear or replace CLI changes on restart.
- Late worker batches cannot restore removed/paused/edited sources or override current provenance.
- Force disconnect must close all targeted old streams even across rapid disable/re-enable events.
- Wrong/missing source key and parser/error output must not silently reset or disclose credentials.

---

### Task 1: Managed SQLite state and migration
**Files:** src/store.rs, src/store/managed.rs, src/managed_types.rs, src/secret.rs, src/config.rs, Cargo.toml, tests/management_store.rs.
**Interfaces:** typed Store user/source/proxy/session operations and safe views per spec; existing Store::open/Router API stays available. Schema v2 migrates old user owner strings in one transaction; state is published only after commit.
- [ ] Add migration/bootstrap/rotation/source encryption/membership tests; run RED.
- [ ] Implement UserRecord, SourceRecord, memberships, proxy policy/diagnostics and independent vault key; preserve exclusive file ownership.
- [ ] Implement bounded hash work and commit-before-publish management mutations.
- [ ] Run targeted tests GREEN, then existing routing/store regressions.

### Task 2: Live authentication, scoped management and connection revocation
**Files:** src/auth.rs, src/server.rs, src/http.rs, src/routing.rs, src/pool_state.rs, src/admin.rs, src/admin/management.rs, src/main.rs, tests/management_api.rs, old test fixtures.
**Interfaces:** all HTTP request/response contracts in spec, GET /v1/info version 1, worker plan/results, explicit auth mode; Runtime token revocation closes streams through existing guards.
- [ ] Add real local HTTP tests for roles/user CRUD/stale work and live credentials; run RED.
- [ ] Implement current-state authentication with versioned fixed-TTL cache and reserve-time recheck; update static compatibility parsing without weakening required mode.
- [ ] Add force-revocation watch and shutdown/drain regressions, connect management routes to typed Store.
- [ ] Run all Rust tests/clippy and preserve HTTP framing/keep-alive/TLS behavior.

### Task 3: Standalone CLI Adapter (independent file owner)
**Files:** src/bin/rproxy-cli.rs, src/control_client.rs, tests/control_cli.rs, docs/rproxy-cli.md, scripts/package_release.py, scripts/release_tests/test_package.py, Dockerfile, .dockerignore.
**Interfaces:** spec CLI groups and HTTP contract; no direct SQL or server-side management logic. Parent adds rpassword dependency and version/default-run.
- [ ] Write fake local HTTP CLI tests for auth/JSON/import/password non-echo/impact/wait; observe RED.
- [ ] Implement commands, safe human/JSON output, stdin/env secrets, import/export, compatibility checks and confirmation.
- [ ] Package/copy both binaries, preserve exact five-archive gate; test old fake binary fixture allowlist plus CLI binary.
- [ ] Report focused tests and file ownership; do not commit/push or touch parent-owned core/README/workflows.

### Task 4: Versioned collector worker (independent file owner)
**Files:** scripts/collector.py, scripts/proxy.example.toml, scripts/tests/test_collector.py, docs/collector-management.md.
**Interfaces:** GET /v1/collector/plan and POST /v1/collector/results exactly as spec. Bootstrap only connectivity/workers/poll; old provider TOML is explicit CLI import.
- [ ] Add local fake root/worker plan tests for live changes/manual runs/stale result/role/error bounds; run RED.
- [ ] Implement bounded versioned worker polling, interval scheduling, environment resolution and value-free errors.
- [ ] Retain GET/POST/text/JSON parsing tests and no arbitrary code.
- [ ] Report results; do not touch server, Compose, README, requirements, Git or workflows.

### Task 5: Integration, Compose and README
**Files:** compose.yaml, .env.example, rproxy.example.toml, scripts/compose_smoke.py, scripts/smoke.py, scripts/deployment_tests/*, README.md, docs/releases/v0.2.3.md, Cargo.lock.
- [ ] Add managed-source fixture and online user/source/proxy CLI flows to local smoke; observe failure against missing implementation.
- [ ] Update worker-only token environment, explicit auth mode and one-time bootstrap; keep read-only mounts and durable state/key backups.
- [ ] Document new commands, safe credentials, import/export, migration, CLI versioning and unchanged upstream protocol limits.
- [ ] Verify native clients and actual Podman Compose from final sources, including online changes without recreate and restart preservation.

### Task 6: Review, shipping and release
- [ ] Format, strict locked clippy, complete Rust/Python/packaging/deployment suites on Windows and Linux.
- [ ] One bounded independent review (<=12 calls) of auth, migration, stale jobs and revocation; verify/fix confirmed findings with regressions.
- [ ] Commit with attribution, push feature branch, create/link PR, wait through PR watch, merge only exact-head checks pass, then unwatch.
- [ ] Create v0.2.3 tag on merged main, await five native build/startup/publish jobs; verify both binaries per archive and SHA256SUMS using isolated download directories without executing downloaded release binaries.

## Execution result

Implementation and integration are complete: Windows Rust98, Linux release-mode Rust100, managed collector26, native archive/linker13 and deployment/render6 tests passed. Final-source Podman rebuilt and passed the complete online-management smoke after the one-shot management connection-header regression was fixed. Native requests/curl/Chrome and the real local storage/CLI packaging gate passed. Source migration/secret/cancellation/versioning regressions are included. A bounded independent review was performed with incomplete coverage disclosed in the verification record. These original checkboxes are the execution checklist; current evidence and decisions are in `docs/superpowers/verification/2026-11-20-rproxy-cli.md`. Formal delivery proceeds through exact-head PR CI and an immutable v0.2.3 tag; existing workflows/tags remain unchanged.
