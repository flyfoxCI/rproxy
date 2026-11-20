# Collector worker (v0.2.3)

The Python collector is an acquisition worker. Rust stores and manages sources, validates results, and owns proxy admission, verification and routing. Updating a source does not require restarting the collector. The worker never opens SQLite and never calls root user, source, proxy or session management routes.

## Bootstrap and credentials

Requires Python 3.11+ and the existing `requests[socks]` and `certifi` dependencies. Copy `scripts/proxy.example.toml` to your bootstrap location:

```toml
workers = 4
poll_seconds = 3

[rproxy]
url = "http://127.0.0.1:9100"
token_env = "RPROXY_WORKER_TOKEN"
timeout_seconds = 15
# ca_certificate = "certs/rproxy-ca.pem"
```

Supply the **worker** bearer through `RPROXY_WORKER_TOKEN` (or the environment variable named by `token_env`). Root management uses a separate `RPROXY_ADMIN_TOKEN`; do not give it to the worker. Never put either token in command arguments or source files. Set the independent worker token on the Rust server as well. Worker access is disabled when the server has no worker token configured.

Use an internal trusted network, verified HTTPS, or an SSH tunnel. Do not expose an unauthenticated or plaintext management listener publicly. Management HTTPS always validates certificates; there is no management `insecure` option. Client sessions ignore environment proxy/netrc settings, never follow redirects, and do not forward the worker bearer to suppliers.

```sh
python scripts/collector.py --config scripts/proxy.toml --check-config
python scripts/collector.py --config scripts/proxy.toml
python scripts/collector.py --config scripts/proxy.toml --once
```

- `--check-config` validates only the bootstrap, without a token or HTTP request. It does not validate live source definitions or supplier credentials.
- Normal execution and `--once` require the worker token even when there are no sources. An empty plan leaves normal execution alive and polling.
- `--once` fetches one plan, runs every returned source once with bounded concurrency, and exits. Success (including zero sources/addresses) is exit code 0; plan, source, submission or authorization failures give a nonzero exit. Successful sources still submit if another source fails. Counts mean accepted candidates, **not verified proxies**.
- Invalid bootstrap fields, malformed TOML and missing bootstrap environment references fail startup. A temporarily unavailable server is retried on the poll interval in normal execution, not by a tight retry loop. HTTP 401/403 requires an operator fix and terminates the worker. One-shot mode does not retry a failed management request.

`workers` is an integer from 1 to 32; `poll_seconds` from 1 to 300 (default 3); management timeout from 1 to 300 seconds (default 15). Relative CA paths, including those in received sources, resolve against the **bootstrap file's directory**, not the current working directory. Bootstrap changes themselves require restarting the process.

## Explicit source import

A nonempty `[[providers]]` array in bootstrap TOML now fails with an explicit `rproxy-cli source import` instruction, even if every provider is disabled. An empty legacy `providers = []` is harmless. This prevents two runtime authorities.

1. Keep the old provider file as an **import manifest**, separate from the new bootstrap. Protect it if it contains literal credentials.
2. Review `rproxy-cli source import --help`, then explicitly import that manifest using the root CLI. Source import accepts legacy `[[providers]]` files and `[[sources]]` manifests. Import is never performed automatically by the collector.
3. Give the worker the environment variables referenced by the imported definitions. Restart the worker only if its process environment needs to change.
4. Use `rproxy-cli source list/show/pause/resume/run/remove` to manage the server's source state online. Pause/removal stop future acquisition; they do not implicitly withdraw existing pool members.

### Generic provider mapping examples

These belong in a separate legacy import manifest, **not** in the collector bootstrap. Use your supplier's documented URL/schema; no supplier endpoint is assumed. Environment references are preserved by server storage and resolved only on the worker for each run.

```toml
[[providers]]
name = "text-provider"
url = "${SUPPLIER_API_URL}"
protocol = "http"
method = "GET"
interval_seconds = 60
timeout_seconds = 10
attempts = 3
[providers.params]
count = 10
[providers.headers]
Authorization = "Bearer ${SUPPLIER_API_TOKEN}"
[providers.response]
format = "text"

[[providers]]
name = "json-provider"
url = "${JSON_PROVIDER_API_URL}"
protocol = "socks5"
method = "POST"
[providers.json]
count = 1
[providers.response]
format = "json"
items_path = "data.proxies"
address_path = "endpoint"
```

Text responses are separated by whitespace, commas or semicolons. JSON mapping supports dot paths and numeric array indices. Instead of `address_path`, set both `ip_path` and `port_path` to read separate fields. `success_path` and `success_value` can be configured together to require an exact JSON value before extracting items. Empty paths address the current object. Results must contain numeric IP:port addresses (IPv6 is bracketed); hostnames, embedded credentials, invalid ports, unspecified and scoped addresses are rejected. Duplicates are normalized and removed.

Source fields retain the existing generic GET/POST behavior: `url`, `protocol`, `method`, `params`, `headers`, `json`, `response`, `interval_seconds`, `timeout_seconds`, `attempts`, optional `ca_certificate` and explicit supplier-only `insecure`. GET cannot have a JSON body. Defaults are HTTP, GET, text response, a 60-second interval, 10-second timeout and 3 attempts. Limits are 86400 seconds per interval, 300 seconds per timeout and 5 attempts. Mapping failures are not retried; transport/status/size failures use the bounded attempts with short backoff. Missing `${ENV}` values produce `env-missing` without revealing the variable name or value.

There is no executable mapping, invented Rola API URL, credentialed rotating gateway, or supplier sticky-session parameter support. Entry credentials are not upstream credentials.

## Versioned HTTP contract and scheduling

Every management call uses `Authorization: Bearer <worker token>`.

```text
GET /v1/collector/plan
{
  "version": "0.2.3",
  "sources": [{
    "id": "s_...", "name": "source-name", "version": 1, "run_generation": 0,
    "config": {"name": "source-name", "protocol": "http", "url": "..."}
  }]
}
```

The plan contains **enabled API sources only**. The collector polls independently of source intervals, applies additions/edits/removals without restarting, and schedules new revisions or manual `run_generation` changes immediately when capacity is available. There is at most one in-flight fetch/submission per immutable source ID; an updated run waits for any old run to finish. Normal periodic runs wait `interval_seconds` after completion. The executor does not accumulate an obsolete job backlog.

```text
POST /v1/collector/results
{
  "source_id": "s_...", "version": 1, "run_generation": 0,
  "addresses": ["1.2.3.4:80"], "complete": true, "error": null
}
-> 202 {"accepted": 1}
```

Results contain **no protocol field**; the server uses the stored source protocol. Each batch contains at most 256 addresses. Only the final batch has `complete: true`; empty successful runs still submit one complete result. Failures submit an empty complete result with exactly one of `fetch-failed`, `mapping-failed`, or `env-missing`. An HTTP 409 stops remaining batches and suppresses that stale revision/generation until a new one appears; it is never retried. Other submission failures wait until the normal interval, rather than replaying a possibly partially accepted job immediately.

The server must atomically check source ID, enabled/deleted state, revision and generation before accepting **every** batch or status update. Worker access to root routes must return 403; this is enforced by the server, not by Python. Old in-flight work can finish after pause/edit/remove and is expected to receive 409. No request queue or lease platform is needed; deploy one collector per source plan to avoid duplicate acquisitions by independent collector processes.

Provider and plan bodies are capped at 1 MiB; plans contain at most 1000 sources. Provider extraction is capped at 4096 addresses per run. Result acknowledgements are capped at 64 KiB. Requests have finite timeouts and streamed reads check size/deadline limits. A failed or invalid plan leaves the last valid schedule in place; the server's stale-result checks remain authoritative during outages. Logs contain validated immutable IDs, counts and safe categories, never source names, URLs, query strings, headers, bodies or tokens.

## Local tests

```sh
python -m unittest discover -s scripts/tests -v
```

Tests use only the standard library and local fake HTTP servers. They cover online changes/manual generations, one-in-flight scheduling, empty/batched results, stale suppression, role rejection, idle/one-shot operation, environment resolution, mapping, address validation and bounded safe errors. They do not call real suppliers or substitute for Rust authorization and atomic stale-result integration tests.
