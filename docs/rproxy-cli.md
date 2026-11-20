# rproxy-cli (v0.2.3)

`rproxy-cli` administers a running rproxy through its authenticated management API.
It does not read the server TOML, open SQLite, or require a restart. The separate
`rproxy` executable retains its existing `serve`, `healthcheck`, `verify-pool`,
`add-proxy`, and `release-session` commands and flags.

Native release archives and the Rust container image contain both executables.
`cargo build --release` builds both. `rproxy-cli --version` and `--help` work offline.

## Connection and output

```sh
# Set these through your shell or secret manager; do not put tokens in arguments.
export RPROXY_ADMIN_URL=http://127.0.0.1:9100
# RPROXY_ADMIN_TOKEN must already contain the root management bearer.
rproxy-cli status
rproxy-cli health
rproxy-cli --json user list
rproxy-cli --endpoint https://admin.example.internal --token-env MY_ROOT_TOKEN status
```

Global options can appear before or after the command:

| Option | Default | Meaning |
| --- | --- | --- |
| `--endpoint URL` | `RPROXY_ADMIN_URL`, then `http://127.0.0.1:9100` | Management base URL |
| `--token-env NAME` | `RPROXY_ADMIN_TOKEN` | Environment variable holding the root bearer, not its value |
| `--json` | off | JSON instead of human-readable tables/fields |
| `--timeout SECONDS` | 15 | Per-request timeout, 1–300 seconds |

The client first checks `/v1/info`, requires `management_version: 1`, and checks the
command group's advertised capability. It never falls back to editing a database.
Use the root token, not `RPROXY_WORKER_TOKEN`; collector access is a separate role.
A missing token, incompatible server, rejected mutation, failed admission, or wait
timeout exits nonzero. Invalid CLI arguments exit 2. Errors withhold HTTP bodies,
transport details, password values, bearer values, and import parser contents.

The client ignores HTTP/SOCKS proxy environment variables, follows no redirects,
and uses normal TLS certificate validation. Credentials, query strings, and
fragments are forbidden in endpoint URLs. Keep plain HTTP on loopback or a trusted
internal network. For remote management use trusted HTTPS or an SSH tunnel; never
publish unauthenticated/plaintext port 9100 on the Internet.

Normal JSON and tables redact password/hash/token fields and source request
secrets. Table cells escape terminal control characters. Do not treat normal
redacted source exports as a full-fidelity credential backup.

## Users

```sh
# Password comes from stdin; it is never a positional argument or --password value.
printf '%s\n' "$NEW_ENTRY_PASSWORD" |
  rproxy-cli user add alice --business shop --business news --password-stdin
rproxy-cli user list
rproxy-cli user show alice
rproxy-cli user passwd alice --password-env NEW_ENTRY_PASSWORD
rproxy-cli user grant alice --business shop,news
rproxy-cli user disable alice --dry-run
rproxy-cli user disable alice --yes
rproxy-cli user enable alice
rproxy-cli user remove alice --yes --disconnect
```

`user add` and `user passwd` accept exactly one of `--password-stdin` or
`--password-env NAME`. Without either option, a terminal gets a hidden password
prompt plus confirmation. Noninteractive use must select an explicit input
method. Stdin accepts one nonempty line (a final LF/CRLF is stripped). Passwords
are never printed. Avoid shell history or process arguments that contain literal
passwords.

`grant` **replaces** the complete business grant list; no `--business` arguments
clears it. `--businesses` is an alias, and comma-separated values are accepted.
Passwords/grants affect subsequent authentication; rotating a password alone does
not delete existing bindings. `passwd --disconnect --yes` additionally disconnects
existing streams. Disable/remove require confirmation because UserView does not
provide a complete runtime-impact count. `--dry-run` only reads the user; it does
not patch or delete it. Re-enabling does not revive revoked streams.

## Sources

```sh
rproxy-cli source import sources.toml
rproxy-cli source import - < sources.toml # Also works through docker compose exec -T.
rproxy-cli source import sources.toml --replace
rproxy-cli source list
rproxy-cli source show vendor
rproxy-cli source pause vendor
rproxy-cli source resume vendor
rproxy-cli source run vendor --wait --wait-timeout 60
rproxy-cli source remove vendor --dry-run
rproxy-cli source remove vendor --yes
rproxy-cli source export
rproxy-cli source export --output redacted-sources.json
rproxy-cli source export --include-secrets --output private-sources.json --yes
```

Imports accept the old collector `[[providers]]` format, new `[[sources]]` TOML,
or JSON `{ "schema_version": 1, "sources": [...] }`. Legacy provider fields become
`config`; name/enabled/kind are source metadata. Environment references such as
`${SUPPLIER_URL}` are preserved literally for the worker to resolve. Source files
are parsed locally, not executed. All entries are validated before the first
mutation; subsequent server failures can still leave earlier entries saved, so
inspect the list before retrying.

New TOML example:

```toml
schema_version = 1

[[sources]]
name = "vendor"
kind = "api"
# Optional enabled=false pauses acquisition after creation.
[sources.config]
url = "${SUPPLIER_URL}"
protocol = "http"
method = "GET"
interval_seconds = 60
[sources.config.headers]
Authorization = "Bearer ${SUPPLIER_TOKEN}"
[sources.config.response]
format = "text"
```

A new source is posted as `{name,kind,config,enabled?}`. An explicitly paused source is created paused in one transaction, never briefly made eligible for acquisition. `--replace` fetches its current
version and uses `PATCH /v1/sources/NAME` with `{config,expected_version}` (and
`enabled` only when explicitly imported). A 409 conflict is not automatically
retried or overwritten. Replacement cannot change the kind. Names absent during
`--replace` are created. A legacy `enabled=false` is respected through a separate
atomic creation flag (with a version-checked compatibility update only if the response omits the requested state); it is not part of the provider config.

`source import --wait` saves API sources and requests an immediate acquisition;
a paused source must be resumed first. Without `--wait`, import reports `saved`
and run reports `pending`. Waiting polls the finished generation and reports
`ready` only after completion, or exits nonzero on a safe failure/timeout. A saved
static source has no collector job. A source job being ready means acquisition
completed, **not** that every returned proxy passed verification; inspect proxies.

Pause/resume only controls acquisition. Removal is soft deletion and **keeps
existing pool members**. Withdrawing candidates is a separate proxy action.

Exports are JSON, redacted by default. Full-secret export requires both `--output`
and `--yes` and never writes secrets to stdout. Export files must be new: existing
files/symlinks are never overwritten. Files are created mode 0600 on Unix. On
Windows, use an ACL-restricted directory. Back up the server's separate source
encryption key as well as state; CLI exports are not substitutes for that key.

## Proxies

```sh
rproxy-cli proxy add http 192.0.2.10:8080 --wait
rproxy-cli proxy add socks5 '[2001:db8::10]:1080' --source manual
rproxy-cli proxy import http addresses.txt --source manual --wait --wait-timeout 60
printf '%s\n' '192.0.2.10:8080' | rproxy-cli proxy import http -
rproxy-cli proxy list --protocol http --source manual --state ready --limit 100 --offset 0
rproxy-cli proxy show 'http:192.0.2.10:8080'
rproxy-cli proxy verify
rproxy-cli proxy disable 'http:192.0.2.10:8080' --dry-run
rproxy-cli proxy disable 'http:192.0.2.10:8080' --yes
rproxy-cli proxy enable 'http:192.0.2.10:8080'
rproxy-cli proxy remove 'http:192.0.2.10:8080' --yes --disconnect
```

Addresses must be numeric IPv4:port or bracketed IPv6:port; URLs, DNS names,
embedded credentials and zero ports are rejected without echoing input. Import
uses one address per line, ignoring blank lines and whole-line `#` comments.
Inputs are validated before sending; duplicates are collapsed. Requests are
batched at 256 entries, each within the API's 64 KiB body limit. The named static
source defaults to `manual`.

Add/import are idempotent admissions, **not** verified success. Without `--wait`
they report `pending`. `--wait` polls the returned IDs until ready, a terminal
failure, or the deadline. Recent diagnostic failures are surfaced as nonzero
results, with safe reason categories only. `proxy verify --wait` schedules a
verification and waits for the current bounded snapshot (at most 1000 proxies);
it does not provide a new verification-generation guarantee.

IDs can contain colons and IPv6 addresses. Show sends an encoded `id` query value;
actions send an `id` JSON field, never an ID route segment.

Disable/remove always request an impact preview first. `--dry-run` performs no
mutation, even together with `--yes`. Nonzero or unknown impact requires `--yes`
or interactive confirmation. The preview prints only session/connection counts.
Existing streams retain exit occupancy while draining unless `--disconnect` was
explicitly selected; enabling does not reassign an occupied live exit.

## Sessions

```sh
rproxy-cli session list --business shop --owner u_123
rproxy-cli session release shop checkout --dry-run
rproxy-cli session release shop checkout --yes
rproxy-cli session release shop checkout --yes --disconnect
```

Root release does not need an entry password. It previews before applying, uses
the same confirmation rules as proxy actions, and preserves draining unless
`--disconnect` is explicit. Lists use the management API's bounded results; proxy
lists additionally expose limit/offset (limit 1–1000).

## Waiting and packaging

`--wait-timeout SECONDS` defaults to 30 and accepts 1–300. Polling and individual
poll requests share a deadline; a slow response cannot extend a wait indefinitely.
For multi-source imports this bound applies to each source job. A timeout does
not cancel the already-saved operation; inspect its status before retrying.

The release package command executes only the trusted native just-built
`rproxy-cli --version`, checks it against Cargo's version, then archives both
binaries. The lower-level archive builder never executes its inputs (including
unit-test fixtures). Publication still requires exactly the existing five target
archives and SHA256SUMS. Real configs, `.env`, SQLite state, and encryption keys
are not packaging inputs or Docker build context inputs.
