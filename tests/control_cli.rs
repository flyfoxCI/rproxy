use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Step {
    method: &'static str,
    path: &'static str,
    body: Option<Value>,
    status: u16,
    reply: Value,
    stall: bool,
}

fn step(method: &'static str, path: &'static str, body: Option<Value>, reply: Value) -> Step {
    Step {
        method,
        path,
        body,
        status: 200,
        reply,
        stall: false,
    }
}

struct FakeRoot {
    endpoint: String,
    task: thread::JoinHandle<()>,
}

impl FakeRoot {
    fn new(mut steps: Vec<Step>) -> Self {
        steps.insert(
            0,
            step(
                "GET",
                "/v1/info",
                None,
                json!({
                    "version":"0.2.3", "management_version":1,
                    "capabilities":["users", "sources", "proxies", "sessions"]
                }),
            ),
        );
        Self::raw(steps)
    }

    fn raw(steps: Vec<Step>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            for expected in steps {
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "missing request {} {}",
                                expected.method,
                                expected.path
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept: {error}"),
                    }
                };
                let (line, headers, body) = request(&mut stream);
                assert_eq!(
                    line,
                    format!("{} {} HTTP/1.1", expected.method, expected.path)
                );
                assert!(headers
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-root-token\r\n"));
                if let Some(body_expected) = expected.body {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&body).unwrap(),
                        body_expected
                    );
                }
                if expected.stall {
                    thread::sleep(Duration::from_secs(4));
                    continue;
                }
                let reply = expected.reply.to_string();
                write!(stream, "HTTP/1.1 {} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", expected.status, reply.len(), reply).unwrap();
            }
        });
        Self { endpoint, task }
    }

    fn finish(self) {
        self.task.join().unwrap();
    }
}

fn request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
    // Winsock inherits the listener's nonblocking mode on accepted sockets.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        headers.lines().next().unwrap().to_owned(),
        headers,
        bytes[end..end + length].to_vec(),
    )
}

fn cli() -> Command {
    let binary = option_env!("CARGO_BIN_EXE_rproxy-cli").unwrap_or("target/debug/rproxy-cli");
    let mut command = Command::new(binary);
    command
        .env("CLI_TEST_ROOT", "test-root-token")
        .env_remove("RPROXY_ADMIN_TOKEN")
        .env_remove("RPROXY_ADMIN_URL")
        .stdin(Stdio::null());
    command
}

fn run(root: &FakeRoot, args: &[&str]) -> Output {
    cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "--json",
        ])
        .args(args)
        .output()
        .expect("standalone rproxy-cli binary must be runnable")
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_private(output: &Output, secret: &str) {
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
}

#[test]
fn version_is_offline_and_legacy_server_flags_are_not_accepted() {
    let output = cli()
        .arg("--version")
        .output()
        .expect("standalone CLI is missing");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("rproxy-cli "));
    assert!(!cli().arg("serve").output().unwrap().status.success());
}

#[test]
fn list_uses_bearer_info_check_and_ignores_proxy_environment() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/users",
        None,
        json!({"items":[],"total":0}),
    )]);
    let output = cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "--json",
            "user",
            "list",
        ])
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .output()
        .unwrap();
    assert_eq!(success(&output), json!({"items":[],"total":0}));
    root.finish();
}

#[test]
fn incompatible_management_version_is_rejected_before_mutation() {
    let root = FakeRoot::raw(vec![step(
        "GET",
        "/v1/info",
        None,
        json!({"management_version":2,"capabilities":["users"]}),
    )]);
    let output = run(&root, &["user", "enable", "alice"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("management version"));
    root.finish();
}

#[test]
fn missing_management_capability_is_explicit() {
    let root = FakeRoot::raw(vec![step(
        "GET",
        "/v1/info",
        None,
        json!({"management_version":1,"capabilities":[]}),
    )]);
    let output = run(&root, &["source", "list"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("capability"));
    root.finish();
}

#[test]
fn password_stdin_is_sent_but_never_echoed() {
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/users",
        Some(json!({"name":"alice","password":"private-password","businesses":["shop"]})),
        json!({"id":"u_1","name":"alice","password":"private-password","hash":"private-hash"}),
    )]);
    let mut child = cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "--json",
            "user",
            "add",
            "alice",
            "--business",
            "shop",
            "--password-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"private-password\r\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    assert_private(&output, "private-password");
    assert_private(&output, "private-hash");
    root.finish();
}

#[test]
fn password_env_rotation_and_grant_use_patch() {
    let root = FakeRoot::new(vec![step(
        "PATCH",
        "/v1/users/alice",
        Some(json!({"password":"private-env-password","disconnect":false})),
        json!({"name":"alice","version":2}),
    )]);
    let output = cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "--json",
            "user",
            "passwd",
            "alice",
            "--password-env",
            "CLI_TEST_PASSWORD",
        ])
        .env("CLI_TEST_PASSWORD", "private-env-password")
        .output()
        .unwrap();
    success(&output);
    assert_private(&output, "private-env-password");
    root.finish();
    let root = FakeRoot::new(vec![step(
        "PATCH",
        "/v1/users/alice",
        Some(json!({"businesses":["shop","news"]})),
        json!({"name":"alice"}),
    )]);
    success(&run(
        &root,
        &["user", "grant", "alice", "--business", "shop,news"],
    ));
    root.finish();
}

#[test]
fn error_bodies_and_bearer_are_never_displayed() {
    let mut failure = step(
        "GET",
        "/v1/users",
        None,
        json!({"error":"supplier-private-url-password","code":"also-private"}),
    );
    failure.status = 500;
    let root = FakeRoot::new(vec![failure]);
    let output = run(&root, &["user", "list"]);
    assert!(!output.status.success());
    assert_private(&output, "supplier-private");
    assert_private(&output, "also-private");
    assert_private(&output, "test-root-token");
    assert!(String::from_utf8_lossy(&output.stderr).contains("500"));
    root.finish();
}

#[test]
fn source_import_legacy_provider_preserves_environment_references() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("providers.toml");
    std::fs::write(&path, "[[providers]]\nname='vendor'\nurl='${SUPPLIER_URL}'\nprotocol='http'\n[providers.headers]\nAuthorization='Bearer ${SUPPLIER_TOKEN}'\n").unwrap();
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/sources",
        Some(
            json!({"name":"vendor","kind":"api","config":{"url":"${SUPPLIER_URL}","protocol":"http","headers":{"Authorization":"Bearer ${SUPPLIER_TOKEN}"}}}),
        ),
        json!({"name":"vendor","config":{"url":"private-supplier-url","headers":{"Authorization":"private-supplier-token"}}}),
    )]);
    let output = run(&root, &["source", "import", path.to_str().unwrap()]);
    success(&output);
    assert_private(&output, "private-supplier");
    root.finish();
}

#[test]
fn source_replace_checks_version_and_uses_config_envelope() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sources.json");
    std::fs::write(&path, r#"{"schema_version":1,"sources":[{"name":"vendor","kind":"api","config":{"url":"${URL}","protocol":"socks5"}}]}"#).unwrap();
    let root = FakeRoot::new(vec![
        step(
            "GET",
            "/v1/sources/vendor",
            None,
            json!({"name":"vendor","kind":"api","version":7}),
        ),
        step(
            "PATCH",
            "/v1/sources/vendor",
            Some(json!({"config":{"url":"${URL}","protocol":"socks5"},"expected_version":7})),
            json!({"name":"vendor","version":8}),
        ),
    ]);
    success(&run(
        &root,
        &["source", "import", path.to_str().unwrap(), "--replace"],
    ));
    root.finish();
}

#[test]
fn malformed_import_never_echoes_source_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.toml");
    std::fs::write(
        &path,
        "[[sources]]\nname='oops'\nurl='private-import-secret\n",
    )
    .unwrap();
    let root = FakeRoot::new(vec![]);
    let output = run(&root, &["source", "import", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert_private(&output, "private-import-secret");
    root.finish();
}

#[test]
fn secret_export_requires_file_and_acknowledgement() {
    let output = cli()
        .args(["source", "export", "--include-secrets"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private.json");
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/sources/export?include_secrets=true",
        None,
        json!({"schema_version":1,"sources":[{"name":"vendor","kind":"api","config":{"url":"private-export-secret"}}]}),
    )]);
    let output = run(
        &root,
        &[
            "source",
            "export",
            "--include-secrets",
            "--output",
            path.to_str().unwrap(),
            "--yes",
        ],
    );
    success(&output);
    assert_private(&output, "private-export-secret");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("private-export-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    root.finish();
}

#[test]
fn proxy_action_keeps_colon_id_in_body_and_previews_before_confirmation() {
    let preview = json!({"applied":false,"affected_sessions":2,"active_connections":3});
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/proxies/action",
        Some(
            json!({"id":"http:127.0.0.1:8000","enabled":false,"remove":false,"disconnect":false,"dry_run":true,"confirm":false}),
        ),
        preview,
    )]);
    let output = run(&root, &["proxy", "disable", "http:127.0.0.1:8000"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    root.finish();
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/proxies/action",
            Some(
                json!({"id":"http:127.0.0.1:8000","enabled":false,"remove":false,"disconnect":true,"dry_run":true,"confirm":false}),
            ),
            json!({"applied":false,"affected_sessions":2,"active_connections":3}),
        ),
        step(
            "POST",
            "/v1/proxies/action",
            Some(
                json!({"id":"http:127.0.0.1:8000","enabled":false,"remove":false,"disconnect":true,"dry_run":false,"confirm":true}),
            ),
            json!({"applied":true,"affected_sessions":2,"active_connections":3}),
        ),
    ]);
    assert_eq!(
        success(&run(
            &root,
            &[
                "proxy",
                "disable",
                "http:127.0.0.1:8000",
                "--yes",
                "--disconnect"
            ]
        ))["applied"],
        true
    );
    root.finish();
}

#[test]
fn dry_run_does_not_mutate_even_with_yes() {
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/proxies/action",
        Some(
            json!({"id":"socks5:[::1]:9000","remove":true,"disconnect":false,"dry_run":true,"confirm":false}),
        ),
        json!({"applied":false,"affected_sessions":1,"active_connections":0}),
    )]);
    success(&run(
        &root,
        &["proxy", "remove", "socks5:[::1]:9000", "--dry-run", "--yes"],
    ));
    root.finish();
}

#[test]
fn proxy_show_uses_query_and_add_wait_reports_ready() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/proxies/show?id=http%3A127.0.0.1%3A8000",
        None,
        json!({"id":"http:127.0.0.1:8000","state":"ready"}),
    )]);
    success(&run(&root, &["proxy", "show", "http:127.0.0.1:8000"]));
    root.finish();
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/proxies",
            Some(json!({"protocol":"http","addresses":["127.0.0.1:8000"],"source":"manual"})),
            json!({"accepted":1,"ids":["http:127.0.0.1:8000"],"verification":"pending"}),
        ),
        step(
            "GET",
            "/v1/proxies/show?id=http%3A127.0.0.1%3A8000",
            None,
            json!({"state":"ready"}),
        ),
    ]);
    assert_eq!(
        success(&run(
            &root,
            &["proxy", "add", "http", "127.0.0.1:8000", "--wait"]
        ))["verification"],
        "ready"
    );
    root.finish();
}

#[test]
fn rejected_proxy_wait_is_nonzero_and_deadline_is_bounded() {
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/proxies",
            None,
            json!({"accepted":1,"ids":["id"],"verification":"pending"}),
        ),
        step(
            "GET",
            "/v1/proxies/show?id=id",
            None,
            json!({"state":"rejected","reason":"probe-failed"}),
        ),
    ]);
    let output = run(&root, &["proxy", "add", "http", "127.0.0.1:8000", "--wait"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("failed"));
    root.finish();
    let output = cli()
        .args([
            "proxy",
            "add",
            "http",
            "127.0.0.1:8000",
            "--wait",
            "--wait-timeout",
            "999999",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn source_run_wait_uses_finished_generation() {
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/sources/vendor/run",
            Some(json!({})),
            json!({"source_id":"s_1","version":3,"run_generation":4,"job_id":"j_4"}),
        ),
        step(
            "GET",
            "/v1/sources/vendor",
            None,
            json!({"enabled":true,"last_finished_generation":4,"last_error":null}),
        ),
    ]);
    assert_eq!(
        success(&run(&root, &["source", "run", "vendor", "--wait"]))["verification"],
        "ready"
    );
    root.finish();
}

#[test]
fn session_release_previews_and_keeps_disconnect_explicit() {
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/sessions/release",
            Some(json!({"business":"shop","session":"cart","disconnect":false,"dry_run":true})),
            json!({"active_connections":2,"affected_sessions":1}),
        ),
        step(
            "POST",
            "/v1/sessions/release",
            Some(json!({"business":"shop","session":"cart","disconnect":false,"dry_run":false})),
            json!({"released_or_draining":1}),
        ),
    ]);
    success(&run(
        &root,
        &["session", "release", "shop", "cart", "--yes"],
    ));
    root.finish();
}

#[test]
fn noninteractive_password_requires_an_explicit_safe_input_method() {
    let root = FakeRoot::new(vec![]);
    let output = run(&root, &["user", "add", "alice"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("password-stdin"));
    root.finish();
}

#[test]
fn table_output_escapes_terminal_control_characters() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/users",
        None,
        json!({"items":[{"id":"u_1","name":"alice\u{001b}[31m\nforged","enabled":true}],"total":1}),
    )]);
    let output = cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "user",
            "list",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stdout).contains("NAME"));
    root.finish();
}

#[test]
fn zero_impact_preview_does_not_authorize_a_later_impact_race() {
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/proxies/action",
            Some(
                json!({"id":"id","remove":true,"disconnect":false,"dry_run":true,"confirm":false}),
            ),
            json!({"applied":false,"affected_sessions":0,"active_connections":0}),
        ),
        step(
            "POST",
            "/v1/proxies/action",
            Some(
                json!({"id":"id","remove":true,"disconnect":false,"dry_run":false,"confirm":false}),
            ),
            json!({"applied":true,"affected_sessions":0,"active_connections":0}),
        ),
    ]);
    success(&run(&root, &["proxy", "remove", "id"]));
    root.finish();
}

#[test]
fn normal_source_output_redacts_alternate_credential_fields() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/sources/vendor",
        None,
        json!({"name":"vendor","config":{"api_key":"private-api-key","credentials":"private-credentials","cookie":"private-cookie"}}),
    )]);
    let output = run(&root, &["source", "show", "vendor"]);
    success(&output);
    assert_private(&output, "private-");
    root.finish();
}

#[test]
fn user_disable_dry_run_never_sends_patch_or_disconnect() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/users/alice",
        None,
        json!({"id":"u_1","name":"alice","enabled":true}),
    )]);
    assert_eq!(
        success(&run(
            &root,
            &[
                "user",
                "disable",
                "alice",
                "--dry-run",
                "--yes",
                "--disconnect"
            ]
        ))["applied"],
        false
    );
    root.finish();
}

#[test]
fn source_remove_dry_run_keeps_members_and_does_not_delete() {
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/sources/vendor",
        None,
        json!({"id":"s_1","name":"vendor"}),
    )]);
    assert_eq!(
        success(&run(
            &root,
            &["source", "remove", "vendor", "--dry-run", "--yes"]
        ))["keeps_proxies"],
        true
    );
    root.finish();
}

#[test]
fn disabled_source_is_created_atomically_without_an_enabled_window() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("providers.toml");
    std::fs::write(
        &path,
        "[[providers]]\nname='vendor'\nenabled=false\nurl='${URL}'\nprotocol='http'\n",
    )
    .unwrap();
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/sources",
        Some(
            json!({"name":"vendor","kind":"api","enabled":false,"config":{"url":"${URL}","protocol":"http"}}),
        ),
        json!({"name":"vendor","enabled":false,"version":1}),
    )]);
    assert_eq!(
        success(&run(&root, &["source", "import", path.to_str().unwrap()]))["items"][0]["enabled"],
        false
    );
    root.finish();
}

#[test]
fn source_import_accepts_stdin_for_readonly_container_deployments() {
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/sources",
        Some(json!({"name":"manual","kind":"static","config":{}})),
        json!({"name":"manual","kind":"static"}),
    )]);
    let mut child = cli()
        .args([
            "--endpoint",
            &root.endpoint,
            "--token-env",
            "CLI_TEST_ROOT",
            "--json",
            "source",
            "import",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"schema_version=1\n[[sources]]\nname='manual'\nkind='static'\n[sources.config]\n",
        )
        .unwrap();
    success(&child.wait_with_output().unwrap());
    root.finish();
}

#[test]
fn new_toml_sources_import_uses_nested_config() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sources.toml");
    std::fs::write(&path, "schema_version=1\n[[sources]]\nname='manual'\nkind='static'\n[sources.config]\nprotocol='http'\n").unwrap();
    let root = FakeRoot::new(vec![step(
        "POST",
        "/v1/sources",
        Some(json!({"name":"manual","kind":"static","config":{"protocol":"http"}})),
        json!({"name":"manual","kind":"static"}),
    )]);
    assert_eq!(
        success(&run(&root, &["source", "import", path.to_str().unwrap()]))["items"][0]
            ["verification"],
        "saved"
    );
    root.finish();
}

#[test]
fn proxy_import_validates_all_lines_before_sending() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("proxies.txt");
    std::fs::write(
        &path,
        "127.0.0.1:8000\nhttp://private-input-password@example.org:8000\n",
    )
    .unwrap();
    let root = FakeRoot::new(vec![]);
    let output = run(&root, &["proxy", "import", "http", path.to_str().unwrap()]);
    assert!(!output.status.success());
    assert_private(&output, "private-input-password");
    root.finish();
}

#[test]
fn export_never_overwrites_an_existing_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.json");
    std::fs::write(&path, "existing-backup").unwrap();
    let root = FakeRoot::new(vec![step(
        "GET",
        "/v1/sources/export?include_secrets=true",
        None,
        json!({"sources":[{"config":{"url":"private-secret-export"}}]}),
    )]);
    let output = run(
        &root,
        &[
            "source",
            "export",
            "--include-secrets",
            "--output",
            path.to_str().unwrap(),
            "--yes",
        ],
    );
    assert!(!output.status.success());
    assert_private(&output, "private-secret-export");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "existing-backup");
    root.finish();
}

#[test]
fn status_health_and_source_pause_routes_are_unchanged() {
    for (args, route, method, body) in [
        (vec!["status"], "/v1/status", "GET", None),
        (vec!["health"], "/health", "GET", None),
        (
            vec!["source", "pause", "vendor"],
            "/v1/sources/vendor",
            "PATCH",
            Some(json!({"enabled":false})),
        ),
        (
            vec!["source", "resume", "vendor"],
            "/v1/sources/vendor",
            "PATCH",
            Some(json!({"enabled":true})),
        ),
    ] {
        let root = FakeRoot::new(vec![step(method, route, body, json!({"ok":true}))]);
        assert_eq!(success(&run(&root, &args))["ok"], true);
        root.finish();
    }
}

#[test]
fn source_wait_failure_withholds_provider_error_contents() {
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/sources/vendor/run",
            Some(json!({})),
            json!({"run_generation":4}),
        ),
        step(
            "GET",
            "/v1/sources/vendor",
            None,
            json!({"enabled":true,"last_finished_generation":4,"last_error":"private-provider-secret-error"}),
        ),
    ]);
    let output = run(&root, &["source", "run", "vendor", "--wait"]);
    assert!(!output.status.success());
    assert_private(&output, "private-provider-secret-error");
    root.finish();
}

#[test]
fn wait_deadline_bounds_a_stalled_poll_independently_of_request_timeout() {
    let mut stalled = step(
        "GET",
        "/v1/proxies/show?id=id",
        None,
        json!({"state":"pending"}),
    );
    stalled.stall = true;
    let root = FakeRoot::new(vec![
        step(
            "POST",
            "/v1/proxies",
            None,
            json!({"accepted":1,"ids":["id"],"verification":"pending"}),
        ),
        stalled,
    ]);
    let started = Instant::now();
    let output = run(
        &root,
        &[
            "--timeout",
            "15",
            "proxy",
            "add",
            "http",
            "127.0.0.1:8000",
            "--wait",
            "--wait-timeout",
            "1",
        ],
    );
    assert!(!output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "poll exceeded its bounded wait deadline"
    );
    root.finish();
}
