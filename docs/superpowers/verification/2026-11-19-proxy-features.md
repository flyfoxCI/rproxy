# Proxy feature verification — 2026-11-19

Branch: `feat/protocols-session-routing`, based on `724ad3b`. Changes are uncommitted; no push or PR was requested.

## Delivered

- HTTP forwarding, CONNECT, TLS HTTPS ingress and SOCKS5 TCP CONNECT; IPv4/domain/IPv6 targets.
- Standard HTTP Basic / SOCKS5 username-password authentication, anonymous mode, configured business allowlists and fixed authentication periods without dropping stick bindings.
- `v1|user|business|session|strategy` usernames, with four connection-level strategies: stick/random/round-robin/shuffle.
- Redis atomic allocation, normalized actual-egress exclusivity within a business, sharing across businesses, persistent stick bindings, drain-aware explicit release and lease cleanup.
- Bounded pre-application connection failover, failure/rebinding logs, old-stream retention and Redis-loss shutdown of guarded streams.
- Actual-egress verification, HTTP upstream CONNECT capability probes, certificate configuration, CLI and repaired Python maintenance.

Scope exclusions remain UDP/BIND, HTTPS upstream, supplier-specific sticky adapters, uncontrolled rotating exits and full P3 pool governance.

## Executed checks

| Command / check | Result |
|---|---|
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo test --all-targets` | 40 tests passed; 0 failures, 0 ignored |
| Python maintenance unittest discovery | 3 tests passed |
| `cargo build --release --locked` | Passed, optimized Windows GNU executable |
| requests HTTP / HTTPS / SOCKS5 ingress | Passed against actual executable |
| curl HTTP / HTTPS / SOCKS5 ingress | Passed against actual executable |
| Chromium via Playwright, authenticated HTTP ingress | Passed against actual executable |
| End-to-end HTTPS target TLS through HTTP / HTTPS / SOCKS5 ingress × HTTP / SOCKS5 upstream | All six combinations passed in integration tests |

Tests used an isolated real Redis 5.0.7 at port 16379 and local fake upstreams/targets. No paid supplier, real login account or public exit endpoint was used. External clients were exercised with temporary test credentials/certificates and distinct businesses sharing the same fake exit. The smoke program cleans up its processes and Redis prefix.

## Whole-branch review and regression fixes

One independent read-only review examined the complete dirty branch, including untracked files. No second review was dispatched. Confirmed findings were fixed by the implementer with observed failing tests followed by passing tests:

1. Allocation failure could save a cleared stick binding. Failure now commits no partial routing state. Known bindings awaiting verification/cooldown are preserved rather than silently changed. The same rollback also prevents failed allocations from accumulating empty sessions.
2. A reused HTTP connection could switch egress on a later request. Once application forwarding begins, upstream failure closes/fails that connection instead of silently reselecting; subsequent new connections may use coordinated session failover.
3. The handshake deadline incorrectly bounded whole streaming uploads. Removed this whole-request deadline; guarded renewal still covers streaming. Transport initialization/definite HTTP transport failures can mark an upstream failed without replaying transmitted requests.
4. Additional regressions cover mapped-IPv6/IPv4 exit aliases, shutdown of upgraded tunnels, and Redis disconnection through a cut real TCP gateway.

A suggested proxy-credential trailer leak was not reproduced: a real chunked-trailer regression passed before any implementation change. That test remains to protect the behavior.

## Rulings made

- Use the requested new branch in the current checkout rather than create an extra worktree; cost: no additional filesystem isolation. Preserve existing untracked data.
- Execute inline, with a single independent whole-branch review; cost: no fresh reviewer at every task. No commits/pushes were authorized.
- Verify supplied passwords on every request, even inside the configured authentication period; cost: extra hashing/backend work, avoids treating the period as permission to ignore stale credentials.
- Raise Tokio's minimum to 1.44 for stable task tracking; cost: a newer dependency baseline (the lockfile selects 1.54.1).
- Replace obsolete Python pool/SOCKS libraries with bounded asyncio and aiohttp-socks; cost: Python 3.10+ and consistently seconds-based `-e` scheduling, documented as a compatibility change.
- Keep upstream cooldown keyed by proxy identity, not only by exit IP: an inaccessible address/port does not prove every other proxy sharing that exit is broken. Healthy bindings in another business are not silently cleared by that cooldown; cost: temporary new-connection failure until cooldown/probe recovery rather than an unexpected IP change.
- HTTP client library initialization is not a network protocol handshake. After sending an HTTP request, response silence cannot reliably distinguish a dead proxy from a slow target; do not replay or silently rebind an in-flight request. Cost: no whole-request deadline for streaming uploads; callers retain their application deadlines, richer idle policies remain P3.
- Retain current guards for atomic aliases, Redis-loss closure, identity/version checks, relay release ordering and the ten-byte SOCKS success reply: no confirmed surviving-lease or double-allocation failure was identified. Cost: untested environments/client implementations are not guaranteed by these local checks.
- Keep current handling of an HTTP upstream 407 as unsupported upstream authentication; an origin erroneously returning 407 is indistinguishable here. Cost: such a nonstandard origin response could be classified as an upstream-auth failure.
- Keep UDP/BIND/HTTPS-upstream/governance exclusions; cost: those use cases require a future implementation, not a silent fallback.

## Deferred minor

The automatic rebind event logs the new upstream/exit; the corresponding failure event logs the old upstream/exit and cause. These can be correlated by business/session, but are not yet combined into one old/new/reason event (notably for externally removed/changed metadata). This does not affect allocation or credential safety.

## Client/environment observations

- Windows Schannel curl did not accept the generated test certificate through an IP-name proxy URL; using its matching `localhost` name passed without disabling verification.
- For plain HTTP targets through an HTTPS proxy, this requests/urllib3 version defaults to unverified proxy TLS even when a CA file is passed through the usual target `verify` option. The smoke driver uses an explicit verification Adapter; README warns about this instead of hiding the warning.
- Existing local MinGW tools were added only to command-local PATH. Redis packages were extracted into ignored directories without installing system services.
- Service restart persistence relies on Redis retaining its state; Redis restart durability depends on its AOF/RDB configuration.
- Stable-exit guarantees depend on supplier behavior. Two matching samples detect some rotation, not all possible future exit changes.
