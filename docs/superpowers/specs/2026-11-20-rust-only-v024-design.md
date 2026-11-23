# Rust-only rproxy 0.2.4

## User direction

Continue as version **0.2.4 / v0.2.4**, remove the Python code and implement configurable collection in Rust. Preserve the existing CLI/user/source/proxy/session management, mounted configuration and five native platforms. v0.2.3 was not published because both macOS native SQLite startup checks failed; keep that tag unchanged. Fix macOS rather than bypassing the native gate.

## Architecture

One rproxy process now owns asynchronous acquisition, probing, authentication, routing and SQLite. rproxy-cli remains the separate management binary. Compose has one production service; no Python interpreter/requirements, worker container, worker token, self-HTTP polling or external collection broker is needed. Source definitions remain SQLite-authoritative; no new source-of-truth file or database reset. Existing schema2/IDs/password hashes/source encryption/bindings remain compatible.

Internal collector uses typed Store::collector_plan / Store::collector_result, never HTTP to its own management listener and never performs network I/O inside the state worker. Existing revision/run-generation checks remain the final atomic gate for every batch. Remove external /v1/collector/* routes and worker bearer access; legacy worker_token_env may be accepted as an ignored deprecated startup field for migration, but must not authenticate any request.

## Collector Interface

Create `src/collector.rs`, exported by the library:

`pub async fn run(config: CollectorConfig, store: Store, shutdown: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()>`.

CollectorConfig fields (parent defines in src/config.rs): enabled:bool=true, concurrency:usize=4 (1..64), poll_seconds:u64=3 (1..300), env_file:Option<PathBuf>=None, base_directory:PathBuf="." (serde skip). Config::load resolves env_file relative to server TOML and sets base_directory to that directory. Example configuration uses env_file="providers.env", read-only mounted at /etc/rproxy/providers.env.

Read only explicit provider variables from that file into a private map; DO NOT resolve source references from general process environment (same process now contains root token/master key). UTF-8 KEY=VALUE, blank/comment lines and optional quoted values; no shell/code/process-env expansion. File <=1 MiB, bounded keys/values. Missing/malformed configured file yields a value-free diagnostic, never contents. No configured file means an empty provider-variable map. Preserve source ${VAR} references and embedded `Bearer ${VAR}`; unknown names yield env-missing, never fall back to root/GitHub/OS secrets. Reload on daemon restart, as old worker env_file changes did.

Poll managed enabled API sources at bounded interval; new source revision or manual run_generation runs promptly regardless of periodic interval. At most concurrency network jobs, no unbounded queued futures; one in-flight job per source, cancel/drop removed or changed work where possible. Completed old work cannot overwrite a new due schedule. Source pause/remove must not launch later acquisitions from cached plans. Shutdown cancels and joins owned work. No full scoring, monitoring or distributed scheduler is added.

Generic GET/POST, params/headers/JSON request body, text/JSON mappings, optional success criterion, dotted list/item/IP/port/address paths with numeric array indexes. IPv4/IPv6 numeric endpoints only, bracketed IPv6, valid ports, deduplicate. Support <=4096 extracted addresses and <=1 MiB decompressed HTTP response; <=256 candidate batches with only final complete=true, including an empty successful completion. Redirects disabled; no ambient HTTP/SOCKS proxy environment; timeout is a total bounded request/body budget, bounded attempts<=5/backoff. Normal TLS verification, optional explicit per-source CA/insecure (CA relative to base_directory). Unknown protocol/kind/signing/gateway schemes are rejected, no eval or invented Rola endpoints.

Safe result categories remain env-missing, mapping-failed, fetch-failed. Never log source URL/query/header/body/password/master key. A bad provider must not kill unrelated sources or proxy data flow. Use monotonic scheduling. Collection acceptance is not verified admission; source run --wait semantics and proxy add --wait remain as before.

## Rust development/release tooling

Create a Cargo workspace member **xtask** (publish=false), default-members=["."], resolver2. Root Cargo remains package rproxy version0.2.4/default-run rproxy with exactly rproxy/rproxy-cli production binaries. xtask is tooling/test-only, not a third shipped daemon/CLI or archive member.

Commands via `cargo run --locked -p xtask -- ...`:
- `package --target TARGET --tag v0.2.4 [--directory dist]`: strict target/tag/Cargo-version validation, trusted local native rproxy/CLI --version and actual temporary SQLite/CLI user create/read startup, redacted logs+exit cause on failure, then archive. No downloaded executables run.
- `verify --tag TAG --directory dist`: exact five archives, both regular nonempty executables per archive (Unix executable bits), no old/extra packages, no unsafe paths/symlinks. Do not extract/run archive binaries.
- `checksums --directory dist`: SHA256SUMS for archive files.
- `assert-native --target TARGET`: check current host architecture matches the native target.
- `compose-smoke --engine docker|podman [--skip-build] [--image-prefix PREFIX]`: owned random project, existing host config file read-only binds, persistent volume, online CLI/source/manual-proxy/password/force-disconnect/restart/fail-closed/idle internal collector scenarios. Clean only its own resources.
- `fixture`: Rust local-only HTTP18080 and SOCKS518081 service restricted to test destinations; optional fixture Docker target for smoke only. No arbitrary outbound proxy.

Use std/Rust crates tar/flate2/zip/sha2/toml/serde_json/tempfile/clap; no Python or shell code evaluation. Test fake archive inputs without executing them; actual native startup only in package entrypoint. Distribute binaries, README, CLI/internal collector docs, rproxy.example.toml, sources.example.toml and providers.env.example; never actual configs/.env/data/key/private exports. Windows zip, other targets tar.gz, normalized modes. Preserve five targets Linux x86_64/ARM64 musl, macOS Intel/ARM64, Windows x86_64 MSVC and known musl linker behavior.

## Parent integration and migration

Parent owns Cargo workspace/version/dependencies/Config/server startup, admin worker removal, Compose/Docker/README/workflows, Python deletion and macOS fix. Collector and xtask work can be delegated with disjoint files; no child Git/branch changes.

Move providers.env.example to root and mount the actual providers.env read-only (not general root process env_file). Drop old scripts/proxy bootstrap/Python runtime files, requirements, .py tests/fixtures/release helpers only after equivalent Rust coverage; do not delete ignored user .venv or actual old private files. Historical documentation may describe old releases, but no tracked .py executable code or Python workflow/setup step remains in current build/runtime.

CI becomes cargo fmt/clippy/test --workspace --all-targets plus xtask/native/Compose tests as appropriate; remove unused Redis service. Release keeps native runner matrix, native startup gates and draft-then-publish exact-asset guard, replaces Python calls with xtask. Workflow editing requires a new repo+workflow credential: existing current-thread credential scopes confirmed repo only; requested secure card timed out, so local implementation may continue but do NOT silently drop required workflow edits or publish a broken pipeline. Request safely again only when still needed; never reuse unrelated thread secrets.

macOS evidence: original native checks and diagnostic 3995d72 both showed SQLite error5 database is locked. Diagnostic commit1156d9d changes only ownership lock placement: Linux database inode flock retained; macOS canonical sidecar avoids shared lock domain. Native A/B run44733290953 must be inspected before claiming fixed. Existing diagnostics never publish. If successful integrate minimal fix and platform-specific tests; document that lock files must not be removed while running. Do not move v0.2.3; publish only v0.2.4 after success.

## Verification

Port Python worker26 behaviors into Rust local fake-HTTP tests, including dynamic revisions/manual run, pause/remove/shutdown, stale batches, env isolation, GET/POST/JSON/text, TLS defaults, redirects, size/slow-drip timeout, bounded retries and source failure isolation. Retain all existing protocol/auth/management/storage CLI tests. Port archive checks and real managed Compose smoke into xtask. Run Windows and Linux workspace suites, actual single-service Podman, and native startup/storage/CLI checks on all five release runners. Capture safe failure logs; no generic opaque gate errors and no dropped gate to make a platform green.
