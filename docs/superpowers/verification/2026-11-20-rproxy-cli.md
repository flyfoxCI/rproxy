# rproxy-cli 0.2.3 verification

## Scope

User-approved online administration: SQLite-authoritative entry users/passwords/business grants, versioned API/static sources, manual protocol/IP:port entries, sessions, standalone CLI and limited Python worker. Version/tag is **0.2.3 / v0.2.3**, with both executables in each of five native archives. Work began on `feat/rproxy-cli-v0.2.3` from v0.2.2 main `60a25e7`.

## Implemented invariants

- One Rust worker owns database writes, candidate admission and routing. Mutable TOML data imports once, not on every restart. Required/anonymous policy is explicit and unrelated to user count.
- Immutable user IDs, salted Argon2id hashes, bounded blocking verification and a keyed fixed-TTL successful-password cache; current status/grants/revision are checked on new authentication and managed reserve.
- Password rotation leaves bindings intact. Soft deletion/name reuse cannot take over old sessions. Forced disconnect revokes actual runtime tokens; rapid re-enable cannot revive them.
- Source config is encrypted with XChaCha20-Poly1305 and an independent key outside SQLite. Missing/wrong key fails closed. Literal string/numeric credentials are redacted normally; explicit secret export is acknowledged, file-only and refuses overwrite.
- Every worker batch checks enabled/deleted/revision/run generation before mutation. Source pause/delete keeps existing pool members; membership is many-to-many.
- Disabled sources are created paused in one transaction. Source exports retain enabled state. CLI import from stdin avoids modifying/recopying read-only container mounts.
- Schema migration preserves legacy owners/stick, including a legitimate user named `anonymous`, atomically with source/membership migration. Unknown old owners are disabled historical principals, not newly authorized users.

## Executed local checks

- Windows GNU Rust: final complete stack **98 tests passed**, including the named-anonymous migration and explicit management connection-close regressions.
- Final strict locked clippy and rustfmt passed. Linux x86_64 release-mode full suite: **100 tests passed** (including Unix ownership-lock and non-UTF-8 credential regressions), using the cached final-source builder and read-only mounted tests.
- Actual native binary: requests/curl over HTTP/HTTPS/SOCKS5, existing Chrome/Playwright, candidate admission and live legacy release-session CLI passed.
- Python managed worker: 26 local fake-HTTP tests passed independently after handoff, including dynamic plans, stale batches, concurrency, finite slow-drip deadlines, role failures, idle and one-shot modes.
- Native archive/linker suite: 13 tests pass, with both binaries required in all five archives, native CLI version check and management startup gate, source template allowlist and private input exclusion. Archive-builder fake inputs are never executed.
- Deployment static/effective Compose render suite: 6 tests pass with separate root/worker tokens and no root token in collector.
- New native packaging gate was exercised against real locally built Windows binaries: temporary loopback-only server, real SQLite initialization, CLI user creation and read passed. Five release runners must execute this gate as well; no downloaded release binary is executed by the verifier.

## Actual container integration

Final-source Podman Compose smoke is required, not inferred from unit tests. It uses random projects/ports, temporary config/env and only its own volume, plus local restricted HTTP/SOCKS5 fixtures.

First run built both images and passed managed acquisition, online password change, sticky egress/restart, mounted startup-policy changes and ignored stale bootstrap users. It then failed while waiting for a manual acquisition with a management transport error; this run is **not** counted as passed.

Investigation found the management server intentionally closed each connection but emitted no `Connection: close`. A direct real native response confirmed the missing header; a regression test failed (`None` versus `close`), and the response builder was corrected. Focused management tests now pass. Final-source Podman rebuild and complete retry (`brp1gmjtm`) exited **0**. All scenarios passed: managed acquisition, immediate acquisition/source edits, password rotation preserving egress/restart, mounted startup policy without bootstrap overwrite, online add/disable/enable, manual HTTP/SOCKS5 upstreams with real forwarding, non-destructive source removal, real CONNECT force-close, last-user fail-closed, administrator recovery and a running idle worker denied root access. The earlier failure is therefore resolved in the real integration run, not merely assumed from the header unit test.

## Independent review coverage and rulings

One fresh read-only reviewer was given a bounded call budget. They reported no verified actionable defect in the excerpts read, but explicitly did **not** issue a complete pass: password-cache, server batch ingest, revocation, vault and CLI bodies were not fully covered. This is disclosed rather than described as independent approval of every path. Parent verification supplements that limited review with real HTTP, storage, client and container tests; no costly repeated review is requested.

The unverified residual concerned reconstructing memberships on reopen. Current network/manual and worker imports always persist memberships in the same transaction; source deletion keeps memberships. The missing-membership path accommodates v0.2.2 records and the trusted direct verified-proxy library helper used by fixtures. No supported management operation saves an empty membership set for an active candidate, so no reachable managed-state regression was established. Direct external database editing is unsupported.

The local default parallel Rust build exhausted Windows memory (allocation failures at 1–2 MiB, 85% physical memory pressure); changing only local build concurrency to `-j 1` compiled successfully. Docker builds use one job for the existing 2 GiB Podman VM. No user services, global compiler settings or data were reset to hide failures.

## Remaining release boundaries

No credentialed supplier gateway/rotation mapping, HTTPS upstream, UDP/BIND, Web UI, full administrator RBAC or cross-machine allocator is added. Source environment references require collector environment recreation when the variable values themselves change. Startup listener/TLS/auth-policy changes still require restart; ordinary user/source/proxy management does not.

Do not publish until final exact-source checks and five native startup/storage/CLI gates pass, both binaries exist in all five archives, and SHA256SUMS plus GitHub asset digests are verified in isolated download directories. Existing releases/tags must remain unchanged. Workflow files remain unchanged because workflow-edit scope is unavailable; the current workflow invokes the strengthened native packaging script.
