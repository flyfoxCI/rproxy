# Rust development and release tooling

`xtask` is a `publish = false` workspace member, not a production daemon or a third shipped executable. Native packages and the production container contain only `rproxy` and `rproxy-cli`. Run tooling from the repository checkout:

```sh
cargo run --locked -p xtask -- --help
```

## Native release packages

The supported targets are exactly:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

Each target must be built and packaged on a matching native OS/architecture. Linux packages retain the musl target; this tool does not change the linker setup. Example on the corresponding native runner:

```sh
cargo run --locked -p xtask -- assert-native --target x86_64-unknown-linux-musl
cargo build --locked --release --target x86_64-unknown-linux-musl --bins -j1
cargo run --locked -p xtask -- package --target x86_64-unknown-linux-musl --tag v0.2.4 --directory dist
```

`package` accepts a strict `vMAJOR.MINOR.PATCH` tag matching the root `Cargo.toml` version. It reads the **locally built** executables from `target/TARGET/release/`; it never downloads a binary. Before creating an archive, it:

1. Checks both regular, nonempty executables and all public inputs.
2. Runs both local executables with `--version`, requiring the exact binary name and Cargo version.
3. Starts the server with a fresh temporary SQLite database, loopback ephemeral listeners, required authentication, and disabled probing/collection. Inherited `RPROXY_*` / `COMPOSE_*` overrides are removed.
4. Uses the real online CLI to create a temporary user through password stdin, then reads it back and checks that SQLite was created.
5. Reaps its owned process and packages only after the startup/management gate succeeds.

Startup failures retain the exit cause and bounded, credential-redacted server logs. Version/CLI failures and process timeouts retain safe diagnostic output. No native gate is skipped for macOS. A successful archive-only unit test is **not** native startup evidence.

Archives are named `rproxy-TAG-TARGET.zip` on Windows and `rproxy-TAG-TARGET.tar.gz` elsewhere. Each has one matching top-level directory and this fixed allowlist:

```text
rproxy[.exe]
rproxy-cli[.exe]
README.md
docs/rproxy-cli.md
docs/internal-collector.md
rproxy.example.toml
sources.example.toml
providers.env.example
```

Unix binaries have mode `0755`, public files `0644`; tar ownership/timestamps are normalized. Actual configs, `.env`, SQLite files, keys, private exports, Python files, and `xtask` are never collected by a directory glob.

After collecting all five native archives into one directory:

```sh
cargo run --locked -p xtask -- verify --tag v0.2.4 --directory dist
cargo run --locked -p xtask -- checksums --directory dist
cargo run --locked -p xtask -- verify --tag v0.2.4 --directory dist
```

`verify` requires exactly the five expected packages, optionally `SHA256SUMS`, with no old/extra assets. Every archive must have both nonempty regular binaries and all allowlisted public files, normalized modes, and no links, duplicate members, unsafe paths, or extra members. ZIP central-directory duplicates are explicitly checked because ZIP indexing can otherwise collapse duplicate names. Verification reads archive members but **never extracts or executes them**. An existing checksum manifest must exactly match the sorted SHA-256 digests. `checksums` writes the sorted `SHA256SUMS` atomically and refuses unrecognized or linked inputs.

## Managed Compose smoke

```sh
cargo run --locked -p xtask -- compose-smoke --engine docker
# Or use the installed Podman Compose provider:
cargo run --locked -p xtask -- compose-smoke --engine podman
```

The engine must provide `compose config --format json`. The smoke creates a random `rproxy-smoke-*` project, random loopback host ports and credentials, an owned temporary config directory, and project-scoped state volume/network. It uses the repository's actual `compose.yaml`, adding only a smoke fixture override. It never loads the user's `.env` or provider credentials.

The production Compose contract is one `rproxy` service with:

- HTTP/SOCKS listener ports published only to loopback; no published management port.
- Existing `RPROXY_CONFIG` and `RPROXY_PROVIDERS_ENV` files bound read-only to `/etc/rproxy/rproxy.toml` and `/etc/rproxy/providers.env`, both with `create_host_path: false`.
- `providers.env` read by the internal collector through `[collector].env_file`, **not** injected using Compose `env_file` or general process environment.
- A persistent project-owned volume at `/var/lib/rproxy`, with no external or custom-named resources.

Normalized Compose v2 JSON may omit false-valued `bind.create_host_path` options; the validator accepts that default-false representation as well as explicit `false`, but rejects `true` and malformed values. The runtime missing-config check still requires that no host path is created.

Before starting containers, the tool validates effective configuration, host-port overrides, and empty-root-token rejection. It also **renders only** `compose.https.yaml` using an existing owned temporary certificate directory, checking preservation of HTTP/SOCKS ports, the HTTPS port override and read-only certificate bind. No TLS container is started and no private certificate is required for this render check.

Runtime smoke exercises:

- Missing-config mount rejection without silently creating a host directory.
- Empty managed pool, provider-file expansion, real source import and internal acquisition/admission.
- Authenticated HTTP/SOCKS payloads and wrong-password rejection.
- Online password changes, sticky owner/egress preservation and durable restart.
- Mounted startup policy changes without re-importing bootstrap users.
- Online user add/grant/clear/disable/enable operations.
- Source pause/resume/manual run/revision replacement; manual HTTP/SOCKS proxy admission; non-destructive source removal and dry-run mutation.
- Forced closure of a live CONNECT tunnel, last-user removal, fail-closed restart, session release, management recovery and idle internal collector liveness.

On failure, bounded owned-project logs are redacted. After ownership preflight succeeds, cleanup uses only that random project's `down --volumes --remove-orphans`; unvalidated resource definitions are never passed to volume cleanup. There is no global prune. Cleanup errors are reported alongside the scenario failure. Built image tags may remain for explicit reuse; prebuilt images are not deleted.

To reuse already built images:

```sh
cargo run --locked -p xtask -- compose-smoke --engine podman --skip-build --image-prefix existing-prefix
```

This requires `existing-prefix-rproxy` and `existing-prefix-fixture`. `--image-prefix` is rejected without `--skip-build`; project names, config files, ports, network and state volume remain fresh even when images are reused. Without `--image-prefix`, the generated project name is also the image prefix.

### Smoke-only Docker target and fixture

The Dockerfile's `test-fixture` target must provide a Linux `xtask` on `PATH`, runnable as UID/GID `10001:10001`. The generated override specifies:

```text
build.target: test-fixture
entrypoint: [xtask, fixture]
command: []
```

The fixture listens on HTTP `18080` and SOCKS5 `18081` **inside the test network only**; neither is host-published. The production target must remain `rproxy` and must not contain `xtask` or another service.

The Rust fixture returns local `/supplier`, `/supplier-socks`, `/ip`, and `/payload` responses. CONNECT is restricted to `fixture:18080`; SOCKS accepts only the fixture/loopback/own-IP HTTP destination on port `18080` and always relays to its own loopback listener. It is not an arbitrary outbound proxy. HTTP absolute targets outside the fixture and unsupported request-target forms are refused. Do not publish fixture ports or deploy it as a production service.

## Focused verification

```sh
cargo test --locked -p xtask --all-targets -j1
cargo fmt -p xtask -- --check
cargo clippy --locked -p xtask --all-targets -j1 -- -D warnings
```

On the Windows Git Bash build environment, prepend `PATH=/c/tools/mingw64/bin:$PATH` to Cargo commands when the native toolchain requires it. Tests use disposable fake archive bytes without executing them; process tests re-execute their own trusted test binary. Fixture tests use loopback, including one short-lived listener on `127.0.0.1:18080` to exercise the fixture's fixed relay contract. No focused test builds images or runs Compose smoke. The real Compose lifecycle and all five native packaging gates must be run separately on the appropriate engines/runners before release.
