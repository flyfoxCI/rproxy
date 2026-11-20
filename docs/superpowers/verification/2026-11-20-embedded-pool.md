# Embedded pool and mounted Compose verification

## Scope

Implemented on branch `v2.2`, package version `0.2.2`, based on main `0076f3e`. The requested delivery is a branch commit/push, not a main merge, release tag or published image. Existing releases are unchanged.

- Rust owns candidate admission, actual-egress exclusion, session strategies, occupancy and persistent bindings. Redis and Lua routing are removed.
- SQLite writes run on a dedicated state thread and commit before exposing changed durable state. Runtime occupancy is not persisted and is released by ownership handles; it does not expire under live sockets.
- Python 3.11+ reads strict TOML and only submits candidates. Its text/JSON extraction is supplier-configured; no real supplier was called during verification.
- Docker/Podman Compose starts Rust and the optional collector with read-only host configuration, separate supplier environment and a persistent SQLite volume. A separate user session implemented and verified deployment/README in coordination; files were integrated without branch commits by that session.

## Local verification

Completed on Windows 11 with the Windows GNU Rust toolchain and the existing MinGW tools:

- `cargo fmt --check`: pass.
- `cargo clippy --locked --all-targets -- -D warnings`: pass.
- `cargo test --locked --all-targets`: 55 passed, no Redis required.
- Collector unittest suite: 7 passed, using local HTTP supplier/management fixtures.
- Release/linker unittest suite: 8 passed, including the retained x86_64 default-linker and ARM64 musl-linker checks.
- Deployment static and real Compose-render suite: 6 passed.
- Actual native binary: authenticated candidate import, requests and curl over HTTP/HTTPS/SOCKS5, existing Chrome/Playwright over authenticated HTTP, and the live `release-session` CLI all passed.
- Final-source Podman rebuild plus `scripts/compose_smoke.py --engine podman`: exit 0. Verified two-service startup, local supplier extraction and admission, HTTP/SOCKS5 authentication and wrong-password rejection, missing-file mounts not creating directories, missing-secret rejection, SQLite sessions after restart, atomic host configuration edits with force-recreate, and idle collector. Only the smoke's own project and volumes were removed.

Linux x86_64 release-mode Rust tests ran in the cached Rust build target: **57 passed**, including the Unix database-inode ownership/deleted-sidecar regression and the non-UTF-8-secret stderr regression. The build and test command exited 0. Both platform-specific checks are therefore executed evidence, not inferred from the Windows run.

Actual Docker Engine and macOS/ARM64/Windows-MSVC release binaries were not exercised locally in this task. The existing release workflow retains all five native build/startup and exact artifact publication gates. A Docker Compose CI job was prepared locally but is not part of the pushed branch: GitHub rejected workflow changes because the available token lacks `workflow` scope, and the secure credential request timed out. Existing workflow files are therefore unchanged; the complete proposal is preserved locally in branch `backup/v2.2-ci-d6de2ce` and ignored scratch `.superpowers/v2.2-proposed-ci.yml`. All application, Compose, smoke scripts and README changes are included. The optional HTTPS Compose override was configuration-rendered; native TLS and every ingress/upstream combination are covered by Rust integration tests and the native HTTPS client smoke.

## Review and regressions

A fresh read-only reviewer examined the worktree. Confirmed findings were fixed without a second review pass:

1. **Missing supplier/admin secrets were opaque:** controlled value-free configuration diagnostics now name missing variables; enabled providers require a management token at configuration load. The regression was observed failing and then passing.
2. **Worker failure could drop ownership:** process-held lock ownership outlives the worker while connection handles exist; the worker-panic regression was observed failing and then passing.
3. **Deletable sidecar on Unix:** lock the database inode on Unix; Windows retains a sidecar with delete sharing disabled because a Windows whole-file byte lock would block SQLite itself. Local filesystems with reliable locking are required; no NFS/SMB promise.
4. **Parseable corrupt routing JSON:** validate recovered binding/history IP normalization, proxy identity, session ownership fields and unique bound exits before publishing recovered state. Mapped-IPv6 corrupt-binding acceptance was reproduced before the fix.
5. **Secret-bearing startup errors:** remove non-UTF-8 environment values from error chains and replace TOML source excerpts with a line-number-only diagnostic. An unterminated inline password was reproduced leaking before the fix.
6. **Pending rows after restart with periodic probes off:** pending candidates are resumed on startup; the stranded-candidate regression failed before the change and passed afterward.
7. **Management at proxy saturation:** use a separate bounded management connection budget; a direct local health request failed with the old shared budget and passed with the new budget.

Additional invariants exercised include failed SQLite commits not publishing reservations, cancellation during a real SQLite writer wait, generation-checked stale probe results, global capacity, old-exit occupancy while draining, normalized IPv4/IPv6 exclusion, explicit release, fixed authentication periods, HTTP keep-alive/framing, slow uploads, TLS passthrough and shutdown of upgraded tunnels.

## Decisions and remaining boundaries

- `v2.2` is a branch; the package follows the existing `0.2.x` series as `0.2.2`. A different version label can be chosen before any later release.
- `verify-pool` is asynchronous: 202 `scheduled` is acknowledgement, not completion. Callers observe status instead of holding an unbounded management request.
- SQLite is single-owner and local. No distributed backend, replica scaling or automatic Redis-session migration is included. Back up old Redis/config before switching; re-import candidates and expect that old exits may change.
- Supplier extraction is configurable, but credentialed gateways, supplier sticky-session mapping and uncontrolled rotating exits are not implemented. Rola requires an actual account-specific extraction contract; no endpoint was invented.
- A failed HTTP-over-SOCKS target connection keeps the current connection's reservation until that connection closes; its error response uses `Connection: close`. This is not an early reassignment path.
- No explicit full-database `PRAGMA integrity_check` scan or offline integrity CLI was added. SQLite operation errors and recovered domain invariants fail closed; dedicated integrity tooling remains deferred.
- SELinux relabel forwarding differs between Compose providers. README documents explicit labeling rather than promising that a `Z` field was honored.
- No Docker-only or foreign-architecture success is claimed from a Podman/native-x86_64 test. Foreign native builds remain release gates; the extra Docker CI job awaits workflow-authorized credentials, while the runnable smoke script is delivered.
- GitHub workflow permissions were respected: no workflow was changed in the delivered commit to work around the rejected push. Existing CI still starts an unused Redis service and runs Rust/collector/release tests; removing that CI service and adding automatic Compose checks remain in the separate local proposal.
