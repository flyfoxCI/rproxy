# Internal collector (rproxy 0.2.4)

`rproxy` owns acquisition inside the daemon. There is no collector sidecar,
worker bearer token, external collector API, or self-HTTP polling. Source
configuration remains managed in SQLite with `rproxy-cli source` commands;
TOML is not a second source-of-truth for live sources.

The collector uses `Store::collector_plan` and `Store::collector_result`.
Provider HTTP requests run outside the SQLite state worker. Acceptance creates
candidates for normal probing; it is **not** verified admission to routing.
Existing `source run --wait` and manual proxy admission semantics are unchanged.

## Daemon configuration

```toml
[collector]
enabled = true
concurrency = 4
poll_seconds = 3
env_file = "providers.env"
```

- `concurrency`: 1–64 simultaneous source jobs, default 4.
- `poll_seconds`: 1–300 seconds between authoritative plan polls, default 3.
- `env_file`: optional dedicated provider-variable file, relative to the server
  TOML directory. Omit it for an empty provider-variable map.
- Explicit per-source CA paths are also relative to the server TOML directory.

Only enabled, nondeleted `api` sources appear in the plan. Static sources have
no acquisition job. New sources, revisions, and manual run generations become
eligible on a plan refresh without waiting for their periodic interval. Source
intervals have their own monotonic timer and do not wait for the next plan poll.
Each source's next periodic run is scheduled after the preceding run finishes.
Store change notifications can prompt additional plan refreshes.

There is at most one in-flight job per source and no unbounded queue of network
futures. Work removed or replaced in a new plan is cancelled and joined. The
plan is refreshed before filling capacity, including after another job finishes.
A late completion cannot remove a replacement job or overwrite its due time.
Every result batch retains the source ID, revision, and run generation; the
Store atomically rejects stale or paused/deleted work. Shutdown cancels and
joins the collector's owned jobs rather than waiting for network timeouts.

## Isolated provider variables

Mount a dedicated file read-only, for example at `/etc/rproxy/providers.env`.
Do not use the daemon's general process environment as the provider secret
store: that environment can contain the management root token and encryption
master key.

```dotenv
# UTF-8; blank lines and full-line comments are accepted.
PROVIDER_TOKEN='provider-only-secret'
PROVIDER_ACCOUNT="example account"
PROVIDER_URL=https://supplier.example/extract
```

Source configuration can retain `${PROVIDER_TOKEN}` or embedded references such
as `Bearer ${PROVIDER_TOKEN}`. Expansion visits string values in nested source
objects and arrays. Only the private map loaded from `env_file` is consulted;
there is **never** a fallback to OS, root-token, GitHub, or other process variables.
An absent or empty referenced value yields `env-missing` before a request is sent.

The file format is UTF-8 `KEY=VALUE`, with LF or CRLF lines. Names use ASCII
letters/digits/underscores and cannot start with a digit. Whitespace around the
assignment is ignored. Single-quoted values are literal; double-quoted values
allow escaping the next character with a backslash. Quoted values may be
followed by a comment. In unquoted values, a `#` after whitespace starts a
comment, while an embedded `#` is literal. Duplicate keys, invalid names,
unclosed quotes, invalid UTF-8, and malformed assignments are rejected.

There is no shell evaluation, command substitution, recursive expansion, or
process-environment expansion inside the file. For example, `A=${B}` stores the
literal `${B}`. Variables are loaded once per daemon start; restart after changing
the file. Limits are 1 MiB per file, 128 bytes per name, 64 KiB per value, and
4096 entries. A missing/malformed configured file fails with a value-free
diagnostic, without printing its path, contents, or OS error.

## Supported provider requests

An example managed API source configuration is:

```json
{
  "url": "${PROVIDER_URL}",
  "protocol": "http",
  "method": "GET",
  "params": {"count": 20},
  "headers": {"Authorization": "Bearer ${PROVIDER_TOKEN}"},
  "interval_seconds": 60,
  "timeout_seconds": 10,
  "attempts": 3,
  "response": {"format": "text"}
}
```

The management layer supplies defaults. The collector accepts HTTP(S) URLs
without embedded credentials/fragments, GET or POST, query `params`, string
`headers`, and an object-valued `json` body for POST. JSON bodies default to
`Content-Type: application/json` unless explicitly overridden. `protocol` is
`http` or `socks5` and describes the extracted proxies, not a special supplier
transport. Unsupported signing, gateway, executable, or vendor-specific fields
are rejected rather than interpreted.

Redirects are disabled. Ambient HTTP/HTTPS/SOCKS proxy settings are ignored.
TLS verifies certificates and hostnames normally. `ca_certificate` can provide
an explicit PEM trust certificate; `insecure: true` deliberately disables
certificate verification for that source only. Do not use it for production
providers when a CA can be supplied.

Each attempt has a total request-and-body timeout of 1–300 seconds, default 10;
a slow-dripping response cannot continually reset the deadline. There are 1–5
attempts, default 3, with bounded exponential backoff (250 ms, 500 ms, 1 s,
then at most 2 s). Only fetch failures retry; invalid configuration or response
mapping fails immediately. Non-200 responses are fetch failures. A bad source
does not stop other sources or proxy data flow.

## Response mappings and bounds

Text responses split on whitespace, commas, and semicolons. UTF-8 with an
optional BOM is supported. Only numeric IP endpoints with ports 1–65535 are
accepted: `1.2.3.4:80` or `[2001:db8::1]:1080`. Hostnames, schemes, credentials,
unspecified addresses, scoped IPv6, unbracketed IPv6, and malformed ports are
rejected. Addresses are normalized and deduplicated in first-seen order.

JSON can select an array of address strings:

```json
{"format":"json", "items_path":"data.items", "address_path":"endpoint"}
```

Or combine IP and port fields:

```json
{
  "format":"json",
  "items_path":"data.0.items",
  "ip_path":"endpoint.0",
  "port_path":"endpoint.1",
  "success_path":"meta.0.ok",
  "success_value":true
}
```

Paths are dotted object keys with numeric array indexes. An empty path selects
the current value; a root array of strings needs only `{"format":"json"}`.
`success_path` and `success_value` must occur together and must match the
response. Use either `address_path` or both `ip_path` and `port_path`, not both
forms. Invalid paths or any invalid extracted address reject the whole response
before submitting candidates.

The decompressed response is limited to 1 MiB. Gzip, deflate, and Brotli decoding
is provided by the HTTP client. At most 4096 addresses may be extracted, counted
before deduplication. Results are submitted in batches of at most 256 addresses;
only the final batch sets `complete=true`. An empty successful response still
submits a complete result. The first Store submission error stops later batches.

## Diagnostics

Source results use only these acquisition failure categories:

| Category | Meaning |
| --- | --- |
| `env-missing` | A referenced provider variable is absent or empty. |
| `mapping-failed` | Invalid source configuration, CA material, UTF-8, JSON/mapping, or endpoint. |
| `fetch-failed` | Transport/TLS/status error, total timeout, or response-size overflow. |

The collector never prints provider URLs, query strings, headers, response
bodies, passwords, root tokens, or encryption keys. Check the source's run
status with the management CLI; do not enable verbose transport logging on
production secrets just to diagnose a supplier error.

## Focused development checks

On Windows Git Bash, include the installed MinGW tools and limit Cargo jobs:

```bash
PATH=/c/tools/mingw64/bin:$PATH cargo test -j1 --lib collector::tests
PATH=/c/tools/mingw64/bin:$PATH cargo test -j1 --test collector --test collector_lifecycle
rustfmt --edition 2021 --check src/collector.rs tests/collector.rs tests/collector_lifecycle.rs
PATH=/c/tools/mingw64/bin:$PATH cargo clippy -j1 --lib --test collector --test collector_lifecycle -- -D warnings
```

Tests use local HTTP/TLS fixtures and a temporary real Store, not suppliers or
an external management listener. The ambient-proxy test runs in an isolated
child test process so its environment cannot affect parallel tests. These
focused checks do not replace the parent's full workspace/release verification.
