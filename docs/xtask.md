# Rust development and release tooling

`xtask` is a `publish = false` workspace member, not a production daemon or third shipped executable. Native packages and the optional single Docker image contain only `rproxy` and `rproxy-cli`.

```sh
cargo run --locked -p xtask -- --help
```

The supported commands are `assert-native`, `package`, `verify`, and `checksums`. Container orchestration and fixture commands have been removed; Docker deployment uses the repository Dockerfile directly. Rust integration tests use their own local fixtures and do not require a container engine.

## Native release packages

The supported targets are exactly:

- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

Build and package each target on the matching native OS/architecture. Linux packages retain musl and the existing linker selection:

```sh
cargo run --locked -p xtask -- assert-native --target x86_64-unknown-linux-musl
cargo build --locked --release -p rproxy --target x86_64-unknown-linux-musl --bins -j1
cargo run --locked -p xtask -- package --target x86_64-unknown-linux-musl --tag v0.2.4 --directory dist
```

`package` requires a strict `vMAJOR.MINOR.PATCH` tag matching the root Cargo version. It reads locally built programs from `target/TARGET/release/`, never downloads one, and before archiving:

1. Checks both regular, nonempty programs and all public inputs.
2. Runs both programs with `--version`, requiring the exact name and Cargo version.
3. Starts the server with fresh temporary SQLite, ephemeral loopback listeners, required auth and disabled probing/collection. Inherited `RPROXY_*` overrides are removed.
4. Uses the real online CLI to create a temporary user through password stdin, then reads it back and checks SQLite creation.
5. Reaps its own process before returning. Startup failures retain bounded, credential-redacted logs and exit/timeout causes.

No native startup gate is skipped on macOS. An archive-only unit test is not native startup evidence.

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

Unix programs have mode0755 and public files0644; tar ownership/timestamps are normalized. Actual configs, `.env`, SQLite, keys, private exports, Python, and xtask are never packaging inputs. The single-TOML design has not changed this implemented archive contract.

## Complete-set and checksum verification

After collecting all five native archives into one directory:

```sh
cargo run --locked -p xtask -- verify --tag v0.2.4 --directory dist
cargo run --locked -p xtask -- checksums --directory dist
cargo run --locked -p xtask -- verify --tag v0.2.4 --directory dist
```

`verify` requires exactly five expected packages, optionally SHA256SUMS, without old/extra assets. Each archive needs both nonempty regular programs, all public inputs, normalized modes and safe paths. Links, duplicate members and extra files are rejected; ZIP central-directory duplicates are checked explicitly. Verification never extracts or executes archive inputs.

An existing checksum manifest must exactly match sorted SHA-256 digests. `checksums` writes the sorted manifest atomically and rejects unknown or linked inputs. Release publishing remains draft-first and refuses to overwrite a public release.

## Optional Docker image

```sh
docker build --tag rproxy:0.2.4 .
docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges rproxy:0.2.4 --version
docker run --rm --network none --read-only --cap-drop ALL --security-opt no-new-privileges --entrypoint rproxy-cli rproxy:0.2.4 --version
```

The Dockerfile has one builder and one production runtime, UID/GID10001. It copies only the two programs into the runtime, installs CA certificates, and provides a management-based healthcheck. Default HTTP/SOCKS binds listen inside the container; administration is not published. README documents direct docker run, read-only config/provider binds and the persistent state volume. No fixture target or second service remains.

## Local verification

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -j1 -- -D warnings
cargo test --locked --workspace --all-targets -j1
```

Windows GNU builds may need command-local `PATH=/c/tools/mingw64/bin:$PATH`. Tests use disposable archive bytes without executing them; process tests re-execute their trusted test binary. Protocol/TLS, collector lifecycle, real server shutdown, users/sources/SQLite and CLI tests are independent of Docker. CI separately builds the optional production Docker image, checks both program versions without network access, then starts one offline, read-only container with its own anonymous data volume and exercises health/user management. Cleanup removes only the returned container ID and its anonymous volume.
