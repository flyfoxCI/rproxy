# rproxy-cli and online administration (0.2.3)

## Approved intent

The user approved an online management client for entry users/passwords, proxy sources, manual protocol/IP:port candidates and sessions, with SQLite as the authority for mutable management data. Deliver version 0.2.3, merge through a checked PR, publish immutable v0.2.3 with both binaries for the existing five native targets. Do not repeat the v2.2-branch/version ambiguity. Existing tags remain unchanged.

## Ownership and configuration

One Rust state worker remains the only SQLite writer and authority for admission, routing, durable bindings and occupancy. CLI is a Rust client, not a database editor or a TOML-restart wrapper. Python becomes a versioned collector worker; it never writes SQLite or controls users. Two Compose services remain; ship rproxy-cli inside the Rust image and native archives.

Startup TOML retains listeners/TLS/database/auth policy/limits/credential references. Mutable users, grants, sources and manual entries are stored in SQLite. Existing TOML users bootstrap once on new DB/upgrade, never overwrite CLI changes on restart. Explicit import/export is provided. Anonymous mode is explicit `[auth] mode="anonymous"`; default is `required`, independent of user count. No users means deny entry, not fail-open.

## Users and authorization

Persist immutable user IDs (prefix u_, random 128 bits), unique active names, Argon2id PHC hashes, businesses, enabled/deleted and credential revision. User create/change password is hashed outside the state worker with bounded blocking concurrency. Authentication checks current user enabled/grants/revision; bounded short-lived successful-password cache uses a process-keyed digest and fixed TTL, never a username-only cache. Password changes invalidate cached credentials immediately, not bindings. Existing tunnels are retained unless the operator requests disconnect. Disable/delete denies new auth; optional disconnect revokes the affected runtime tokens and closes guarded tunnels. Re-enabling cannot revive revoked old connections. Deleted/recreated names do not inherit another principal's bindings.

Migrate v0.2.2 owner names to immutable IDs and preserve every business/session binding in the same migration transaction. Keep explicit anonymous owners separate. Root administrators may inspect/release sessions without knowing entry passwords. Existing self-release/legacy rproxy commands remain compatible where feasible.

Passwords are prompted without echo or passed with --password-stdin / --password-env. Never expose hashes or passwords in list/show/JSON/errors/logs. Management/root bearer and worker bearer are independent (worker token env RPROXY_WORKER_TOKEN); workers can only read collector plans and submit results. Missing worker token disables worker access, not root management. Equal configured root/worker tokens fail startup. Root stays loopback/internal by default; remote use requires SSH or trusted HTTPS transport, no plaintext public 9100.

## Sources and secrets

Sources have immutable IDs s_, active unique names, kind api/static, enabled/deleted, revision, run_generation, encrypted config, last-run/error/finished-generation. Config follows current Python provider fields: url, protocol=http|socks5, method GET|POST, params/headers/json, response mapping, interval_seconds/timeout_seconds/attempts, optional CA/insecure. No arbitrary code, invented Rola URLs, credentialed gateways or supplier sticky guarantee.

Encrypt complete source config with XChaCha20-Poly1305 (random nonce); user password hashes are not encryption keys. Key is independently supplied as base64 32 bytes via RPROXY_SOURCE_KEY or generated once in a private database-adjacent .key file (outside SQLite, mode 0600 on Unix). Existing encrypted data with missing/wrong key fails, never silently resets. Back up state and key securely; key file may be replaced with an external environment secret. Preserve environment references for worker resolution. Normal source views/exports redact literal credentials, URL contents with secrets and request strings; explicit --include-secrets export requires an output file and acknowledgement.

Pause/resume controls acquisition only and increments source revision. Removing a source keeps existing pool members by default, marks the source deleted and rejects late worker results. Multiple sources may refer to one deduplicated protocol/address: maintain memberships, never drop B's membership when A is removed. Destructive withdrawal is an explicit proxy operation, not an implicit source delete. Manual import uses a named static source (default manual).

Worker polls plan every 3 seconds, applies new source revisions without process restart, schedules periodic fetches and runs immediately when run_generation changes. Submissions carry source_id, revision and run_generation; core atomically rejects stale/paused/deleted work before changing candidates or source status. Results are batches <=256; empty successful results are allowed; complete=true marks the last batch. Safe error category only, never provider body/URL/credentials. Collector bootstrap TOML only has rproxy connectivity, workers and poll timing. Existing provider TOML is accepted by explicit CLI import, not a second runtime authority.

## Proxy/session lifecycle

Root can list/filter/show/import, verify, disable/enable/remove candidates and inspect occupancy. Adding is idempotent and returns IDs plus pending state, not verified success. --wait polls admission and reports rejection reason; retain bounded recent diagnostic records when failed candidates are retired so failures do not vanish silently or consume live capacity. Public rows include protocol/address, sources, state, verified egress/capability/time, active connection/binding counts, not secrets.

Disable/remove previews impacted bindings/connections. --dry-run performs no mutation; nonzero impact requires explicit confirmation. Existing connections retain exit occupancy until drained unless disconnect is requested. Never directly reassign an old live exit. Admin session release preserves draining and supports explicit disconnect.

## HTTP contract (management version 1)

All root/worker requests require their bearer token; existing 64 KiB body and finite timeouts remain. JSON errors are `{error:"safe message", code:"optional-safe-code"}`; no secret-bearing lower-level errors.

Root routes:
- GET /v1/info -> {version:"0.2.3", management_version:1, capabilities:["users","sources","proxies","sessions"]}; GET /health and /v1/status retained.
- GET /v1/users -> {items:[UserView],total:N}; POST /v1/users {name,password,businesses} -> UserView. UserView: id,name,businesses,enabled,deleted,version (NO hash).
- GET /v1/users/NAME -> UserView. PATCH /v1/users/NAME {password?,businesses?,enabled?,disconnect?:false,expected_version?} -> UserView. DELETE /v1/users/NAME?disconnect=true|false -> UserView (soft delete).
- GET /v1/sources -> {items:[SourceView],total:N}; POST /v1/sources {name,kind:"api"|"static",enabled?:true,config:{...}} -> SourceView. SourceView: id,name,kind,enabled,deleted,version,run_generation,last_finished_generation,last_run_at,last_error,config (redacted).
- GET /v1/sources/NAME -> SourceView; PATCH /v1/sources/NAME {config?,enabled?,expected_version?} -> SourceView; DELETE /v1/sources/NAME -> SourceView (keep existing proxies).
- POST /v1/sources/NAME/run {} -> {source_id,version,run_generation,job_id}; GET /v1/sources/export?include_secrets=true|false -> {schema_version:1,sources:[{name,kind,config}]}.
- GET /v1/proxies with optional protocol/source/state/limit/offset -> {items:[ProxyView],total:N}. GET /v1/proxies/show?id=ID -> ProxyView. POST /v1/proxies {protocol,addresses:[...],source:"manual"} -> 202 {accepted:N,ids:[...],verification:"pending"}; backwards-compatible accepted field remains.
- POST /v1/proxies/action {id,enabled?,remove?:false,disconnect?:false,dry_run?:false,confirm?:false} -> {applied,affected_sessions,active_connections}.
- POST /v1/verify {} -> 202 scheduled; existing compatibility commands keep using this.
- GET /v1/sessions with optional business/owner -> {items:[{business,session,owner,owner_name,strategy,proxy_id,egress,connections,releasing}],total:N}.
- POST /v1/sessions/release accepts either existing {username,password} or root {business,session,disconnect?:false,dry_run?:false}; response includes released_or_draining/impact.

Worker-only routes (root may inspect explicitly for operational tests):
- GET /v1/collector/plan -> {version:"0.2.3",sources:[{id,name,version,run_generation,config:{...full provider config, including protocol/name}}]} (enabled api sources only).
- POST /v1/collector/results {source_id,version,run_generation,addresses:[...],complete:true,error:null|"fetch-failed"|"mapping-failed"|"env-missing"} -> 202 {accepted:N}. Authoritative source protocol comes from stored config, not worker input. Stale work -> 409. Worker bearer attempting other routes -> 403.

Lists are bounded (default 100, max 1000); filters and IDs validated. Compatibility endpoints return safe errors. CLI detects unsupported management version and never falls back to direct database writes.

## CLI contract

`rproxy-cli [--endpoint URL] [--token-env NAME] [--json] [--timeout SECONDS] ...`, defaults RPROXY_ADMIN_URL or http://127.0.0.1:9100, token env RPROXY_ADMIN_TOKEN. Client does not inherit proxy environment or follow cross-host redirects. Endpoint HTTPS validates CA normally; no automatic insecure fallback. --version works offline.

Groups: status/health; user add/list/show/passwd/grant/disable/enable/remove; source import/list/show/pause/resume/run/remove/export; proxy add/import/list/show/verify/disable/enable/remove; session list/release. Clear human tables or --json, useful nonzero errors. Password create/passwd options --password-stdin or --password-env; interactive masked prompt only on a TTY. Bulk proxy input is protocol + lines of IP:port, no embedded credentials. Source import accepts current [[providers]] files and new [[sources]] manifests, supports --replace with version check. Export defaults redacted; --include-secrets requires --output and --yes. Impacting actions require --yes or interactive confirmation after dry-run. Source/proxy --wait has bounded timeout and distinguishes saved/pending/failed/ready.

## Deployment, verification, release

Update Compose worker credential, bootstrap examples, CLI docs and native usage; do not rewrite mounted files. Docker image contains both binaries. Preserve five native target definitions; package both binaries and check CLI --version during native packaging without changing workflow files (current GitHub credential lacks workflow scope). Test migration/restart, password rotation preserving bindings, delete/recreate isolation, last-user no-anonymous, worker role/stale results, secret redaction/encryption, source memberships, cancellation/drain, CLI real local HTTP, and actual Podman Compose with online changes. Run bounded focused independent review (at most 12 tool calls, concrete findings) once after integration; do not repeat the prior expensive unconstrained review. Publish only after exact-source CI, native jobs, startup checks and all ten binaries/five archives/checksums pass.
