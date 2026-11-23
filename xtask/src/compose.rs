use crate::{
    fixture::header,
    native::{random_secret, safe_log},
    process::{environment, Output, Request, Runner, SystemRunner},
};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn normalized(path: &str) -> String {
    path.replace('\\', "/")
}

pub fn validate_rendered(config: &Value, server_config: &str, providers: &str) -> Result<()> {
    let services = config["services"]
        .as_object()
        .context("Compose services missing")?;
    ensure!(
        services.len() == 2 && services.contains_key("rproxy") && services.contains_key("fixture"),
        "smoke must contain exactly rproxy and the smoke-only fixture, no worker"
    );
    let rproxy = &services["rproxy"];
    ensure!(
        rproxy["environment"].get("RPROXY_WORKER_TOKEN").is_none(),
        "worker credentials must not exist"
    );
    for name in ["RPROXY_ADMIN_TOKEN", "RPROXY_PASSWORD"] {
        ensure!(
            rproxy["environment"][name]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "required smoke bootstrap credential missing"
        );
    }
    let project = config["name"].as_str().context("Compose project missing")?;
    ensure!(
        project.starts_with("rproxy-smoke-"),
        "smoke requires an owned project name"
    );
    for service in services.values() {
        for key in [
            "container_name",
            "network_mode",
            "env_file",
            "devices",
            "secrets",
            "configs",
            "volumes_from",
        ] {
            ensure!(empty(&service[key]), "unsafe smoke service setting: {key}");
        }
        ensure!(
            service["privileged"] != true,
            "privileged smoke services are forbidden"
        );
    }
    let ports = rproxy["ports"]
        .as_array()
        .context("smoke proxy ports missing")?;
    ensure!(
        ports.len() == 2
            && [9089, 9099].iter().all(|target| ports
                .iter()
                .any(|port| port["target"] == *target && port["host_ip"] == "127.0.0.1")),
        "only loopback HTTP/SOCKS ports may be published; management must remain private"
    );
    let fixture = &services["fixture"];
    ensure!(
        empty(&fixture["ports"]) && empty(&fixture["volumes"]) && empty(&fixture["environment"]),
        "fixture must be internal-only, without host mounts or credentials"
    );
    ensure!(
        fixture["build"]["target"] == "test-fixture",
        "fixture requires the smoke-only test-fixture build target"
    );
    let volumes = rproxy["volumes"]
        .as_array()
        .context("Compose binds missing")?;
    ensure!(
        volumes.len() == 3,
        "smoke must mount only its two config files and owned state volume"
    );
    for (target, source) in [
        ("/etc/rproxy/rproxy.toml", server_config),
        ("/etc/rproxy/providers.env", providers),
    ] {
        let volume = volumes
            .iter()
            .find(|v| v["target"] == target)
            .context("required mounted config missing")?;
        ensure!(volume["type"] == "bind" && volume["read_only"] == true && does_not_create_host_path(volume) && volume["source"].as_str().is_some_and(|p| normalized(p) == normalized(source)), "configuration must bind the existing owned file read-only, without creating missing host paths");
    }
    ensure!(
        volumes
            .iter()
            .any(|v| v["type"] == "volume" && v["target"] == "/var/lib/rproxy"),
        "persistent state volume missing"
    );
    ensure!(
        services["fixture"]["entrypoint"] == json!(["xtask", "fixture"]),
        "fixture must use the native Rust xtask"
    );
    let definitions = config["volumes"]
        .as_object()
        .context("owned volume definition missing")?;
    ensure!(definitions.len() == 1, "unexpected smoke volumes");
    let volume = volumes
        .iter()
        .find(|v| v["type"] == "volume")
        .context("state volume missing")?;
    let source = volume["source"]
        .as_str()
        .context("state volume source missing")?;
    let definition = definitions
        .get(source)
        .context("state volume definition missing")?;
    ensure!(
        definition["name"] == format!("{project}_{source}")
            && definition["external"] != true
            && empty(&definition["driver_opts"])
            && (empty(&definition["driver"]) || definition["driver"] == "local"),
        "state volume must be local and owned by the random smoke project"
    );
    let networks = config["networks"]
        .as_object()
        .context("owned network definition missing")?;
    ensure!(networks.len() == 1, "unexpected smoke networks");
    let network = &networks["default"];
    ensure!(
        network["name"] == format!("{project}_default")
            && network["external"] != true
            && empty(&network["driver_opts"])
            && (empty(&network["driver"]) || network["driver"] == "bridge"),
        "network must be owned by the random smoke project"
    );
    Ok(())
}

fn does_not_create_host_path(volume: &Value) -> bool {
    // Compose v2 omits false bind options in normalized JSON. The zero value
    // still uses the mount API without creating a missing host path.
    let Some(bind) = volume.get("bind") else {
        return true;
    };
    let Some(bind) = bind.as_object() else {
        return false;
    };
    matches!(
        bind.get("create_host_path"),
        None | Some(Value::Bool(false))
    )
}

fn empty(value: &Value) -> bool {
    value.is_null()
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
        || value.as_str() == Some("")
}

fn validate_published_ports(config: &Value, http: u16, socks: u16) -> Result<()> {
    let ports = config["services"]["rproxy"]["ports"]
        .as_array()
        .context("published proxy ports missing")?;
    for (target, published) in [(9099, http), (9089, socks)] {
        ensure!(
            ports.iter().any(|port| port["target"] == target
                && port["published"].as_str() == Some(&published.to_string())),
            "published host port override was ignored"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Options {
    pub engine: String,
    pub skip_build: bool,
    pub image_prefix: Option<String>,
}

struct Smoke<'a, R> {
    runner: &'a mut R,
    root: PathBuf,
    directory: tempfile::TempDir,
    command: Vec<String>,
    environment: BTreeMap<String, String>,
    engine: String,
    project: String,
    config: PathBuf,
    providers: PathBuf,
    http: u16,
    socks: u16,
    secrets: Vec<String>,
    cleanup_allowed: bool,
}

fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}
fn path_string(path: &Path) -> String {
    normalized(&path.to_string_lossy())
}

impl<'a, R: Runner> Smoke<'a, R> {
    fn new(root: &Path, options: &Options, runner: &'a mut R) -> Result<Self> {
        ensure!(
            ["docker", "podman"].contains(&options.engine.as_str()),
            "unsupported Compose engine"
        );
        ensure!(
            options.skip_build || options.image_prefix.is_none(),
            "--image-prefix requires --skip-build"
        );
        if let Some(prefix) = &options.image_prefix {
            ensure!(
                !prefix.is_empty()
                    && prefix
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.:/".contains(&c)),
                "invalid image prefix"
            );
        }
        let project = format!(
            "rproxy-smoke-{}",
            &random_secret().to_ascii_lowercase()[..12]
        );
        let prefix = options.image_prefix.as_deref().unwrap_or(&project);
        let directory = tempfile::Builder::new()
            .prefix("rproxy-compose-")
            .tempdir()?;
        let config = directory.path().join("rproxy.toml");
        let providers = directory.path().join("providers.env");
        let mut contents: toml::Value =
            toml::from_str(&fs::read_to_string(root.join("rproxy.example.toml"))?)?;
        contents["probe"]["url"] = "http://fixture:18080/ip".into();
        contents["probe"]["tunnel_url"] = "http://fixture:18080/ip".into();
        contents
            .as_table_mut()
            .context("example configuration must be a table")?
            .insert(
                "collector".into(),
                toml::toml! {enabled=true concurrency=2 poll_seconds=1 env_file="providers.env"}
                    .into(),
            );
        fs::write(&config, toml::to_string_pretty(&contents)?)?;
        fs::write(
            &providers,
            "# No real provider credentials.\nSMOKE_FIXTURE_URL=http://fixture:18080/supplier\n",
        )?;
        let secrets: Vec<_> = (0..4).map(|_| random_secret()).collect();
        let env_file = directory.path().join(".env");
        fs::write(
            &env_file,
            format!(
                "RPROXY_PASSWORD={}\nRPROXY_ADMIN_TOKEN={}\n",
                secrets[0], secrets[3]
            ),
        )?;
        let http = free_port()?;
        let mut socks = free_port()?;
        while socks == http {
            socks = free_port()?;
        }
        let mut environment = environment();
        environment.extend([
            ("RPROXY_CONFIG".into(), path_string(&config)),
            ("RPROXY_PROVIDERS_ENV".into(), path_string(&providers)),
            ("RPROXY_BIND_ADDRESS".into(), "127.0.0.1".into()),
            ("RPROXY_HTTP_PORT".into(), http.to_string()),
            ("RPROXY_SOCKS5_PORT".into(), socks.to_string()),
        ]);
        let override_file = directory.path().join("smoke.json");
        fs::write(
            &override_file,
            serde_json::to_vec_pretty(&json!({"services": {
                "rproxy":{"image":format!("{prefix}-rproxy")},
                "fixture":{"image":format!("{prefix}-fixture"),"build":{"context":path_string(root),"target":"test-fixture"},
                    "entrypoint":["xtask","fixture"],"command":[],"read_only":true,"user":"10001:10001","cap_drop":["ALL"],"security_opt":["no-new-privileges:true"]}
            }}))?,
        )?;
        let command = vec![
            "compose".into(),
            "-p".into(),
            project.clone(),
            "--env-file".into(),
            path_string(&env_file),
            "-f".into(),
            path_string(&root.join("compose.yaml")),
            "-f".into(),
            path_string(&override_file),
        ];
        Ok(Self {
            runner,
            root: root.to_owned(),
            directory,
            command,
            environment,
            engine: options.engine.clone(),
            project,
            config,
            providers,
            http,
            socks,
            secrets,
            cleanup_allowed: false,
        })
    }

    fn invoke(
        &mut self,
        args: &[&str],
        input: Option<&str>,
        environment: Option<BTreeMap<String, String>>,
    ) -> Result<Output> {
        let mut command = self.command.clone();
        command.extend(args.iter().map(|s| (*s).to_owned()));
        self.runner.run(&Request {
            program: self.engine.clone().into(),
            args: command,
            env: environment.unwrap_or_else(|| self.environment.clone()),
            cwd: self.root.clone(),
            stdin: input.map(str::to_owned),
            timeout: if args.first() == Some(&"build") {
                Duration::from_secs(1800)
            } else {
                Duration::from_secs(180)
            },
        })
    }
    fn compose(&mut self, args: &[&str], input: Option<&str>) -> Result<Output> {
        let output = self.invoke(args, input, None)?;
        ensure!(
            output.success,
            "Compose {} failed: {}; {}",
            args.first().unwrap_or(&"command"),
            output.status,
            self.redact(&format!("{}\n{}", output.stdout, output.stderr))
        );
        Ok(output)
    }
    fn redact(&self, text: &str) -> String {
        safe_log(
            text,
            &self.secrets.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }
    fn cli_output(&mut self, args: &[&str], input: Option<&str>) -> Result<Output> {
        let mut command = vec!["exec", "-T", "rproxy", "rproxy-cli", "--json"];
        command.extend_from_slice(args);
        self.invoke(&command, input, None)
    }
    fn cli(&mut self, args: &[&str], input: Option<&str>) -> Result<Value> {
        let output = self.cli_output(args, input)?;
        ensure!(
            output.success,
            "container CLI {} failed: {}; {}",
            args.first().unwrap_or(&"command"),
            output.status,
            self.redact(&output.stderr)
        );
        serde_json::from_str(&output.stdout).context("container CLI returned invalid JSON")
    }
    fn wait(&mut self, label: &str, predicate: impl Fn(&Value) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if self
                .cli(&["health"], None)
                .is_ok_and(|status| predicate(&status))
            {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "timed out waiting for {label}");
            thread::sleep(Duration::from_secs(1));
        }
    }
    fn request(&self, username: Option<&str>, password: &str) -> Result<(u16, String)> {
        proxy_request(self.http, username, password)
    }
    fn expect_request(&self, username: Option<&str>, password: &str, status: u16) -> Result<()> {
        let (actual, body) = self.request(username, password)?;
        ensure!(
            actual == status,
            "proxy HTTP status mismatch: expected {status}, got {actual}"
        );
        if status == 200 {
            ensure!(body == "mounted-compose-ok", "proxy payload mismatch");
        }
        Ok(())
    }

    fn scenarios(&mut self, skip_build: bool) -> Result<()> {
        let rendered: Value =
            serde_json::from_str(&self.compose(&["config", "--format", "json"], None)?.stdout)?;
        ensure!(
            rendered["name"] == self.project,
            "rendered Compose project escaped its owned namespace"
        );
        validate_rendered(
            &rendered,
            &path_string(&self.config),
            &path_string(&self.providers),
        )?;
        validate_published_ports(&rendered, self.http, self.socks)?;
        // Render (never start) the optional HTTPS override with an existing owned
        // certificate directory. This checks host overrides without a real cert/key.
        let certs = self.directory.path().join("certs");
        fs::create_dir(&certs)?;
        let mut https_port = free_port()?;
        while https_port == self.http || https_port == self.socks {
            https_port = free_port()?;
        }
        let mut https_env = self.environment.clone();
        https_env.insert("RPROXY_CERTS_DIR".into(), path_string(&certs));
        https_env.insert("RPROXY_HTTPS_PORT".into(), https_port.to_string());
        let https_file = path_string(&self.root.join("compose.https.yaml"));
        let https_output = self.invoke(
            &["-f", &https_file, "config", "--format", "json"],
            None,
            Some(https_env),
        )?;
        ensure!(
            https_output.success,
            "Compose HTTPS rendering failed: {}; {}",
            https_output.status,
            self.redact(&https_output.stderr)
        );
        let mut https: Value = serde_json::from_str(&https_output.stdout)?;
        let ports = https["services"]["rproxy"]["ports"]
            .as_array_mut()
            .context("HTTPS published ports missing")?;
        ensure!(
            ports.len() == 3,
            "HTTPS override must preserve both proxy ports and add only 9443"
        );
        let index = ports
            .iter()
            .position(|port| port["target"] == 9443)
            .context("HTTPS published port missing")?;
        let port = ports.remove(index);
        ensure!(
            port["host_ip"] == "127.0.0.1"
                && port["published"].as_str() == Some(&https_port.to_string()),
            "HTTPS published host port override was ignored"
        );
        let volumes = https["services"]["rproxy"]["volumes"]
            .as_array_mut()
            .context("HTTPS certificate bind missing")?;
        let index = volumes
            .iter()
            .position(|v| v["target"] == "/etc/rproxy/certs")
            .context("HTTPS certificate bind missing")?;
        let bind = volumes.remove(index);
        ensure!(
            bind["type"] == "bind"
                && bind["read_only"] == true
                && does_not_create_host_path(&bind)
                && bind["source"]
                    .as_str()
                    .is_some_and(|p| normalized(p) == path_string(&certs)),
            "HTTPS certificate directory must bind the existing owned path read-only"
        );
        validate_rendered(
            &https,
            &path_string(&self.config),
            &path_string(&self.providers),
        )?;
        validate_published_ports(&https, self.http, self.socks)?;
        let mut empty_token = self.environment.clone();
        empty_token.insert("RPROXY_ADMIN_TOKEN".into(), String::new());
        ensure!(
            !self
                .invoke(&["config", "--quiet"], None, Some(empty_token))?
                .success,
            "Compose accepted empty root token"
        );
        let missing = self.directory.path().join("missing.toml");
        let mut missing_env = self.environment.clone();
        missing_env.insert("RPROXY_CONFIG".into(), path_string(&missing));
        // Never feed unvalidated volume/network definitions to `down --volumes`.
        // From this point all mutating commands use the validated owned model.
        self.cleanup_allowed = true;
        // Validate the production bind contract; --no-deps cannot create any foreign service.
        if !skip_build {
            println!("Building owned rproxy and test-fixture images");
            self.compose(&["build"], None)?;
        }
        ensure!(
            !self
                .invoke(
                    &["run", "--rm", "--no-deps", "rproxy", "check-config"],
                    None,
                    Some(missing_env)
                )?
                .success
                && !missing.exists(),
            "missing config bind silently created a host path"
        );
        self.compose(&["up", "-d", "--no-build"], None)?;
        self.wait("empty managed pool", |s| s["candidates"] == 0)?;
        self.cli(
            &["source", "import", "-", "--wait", "--wait-timeout", "60"],
            Some(&source_manifest(1)),
        )?;
        self.wait("internal collector acquisition and admission", |s| {
            s["verified"] == 1
        })?;
        let user = "v1|alice|shop|compose-smoke|stick";
        let password = self.secrets[0].clone();
        let new_password = self.secrets[1].clone();
        let bob_password = self.secrets[2].clone();
        self.expect_request(Some(user), &password, 200)?;
        self.expect_request(Some(user), "incorrect-password", 407)?;
        socks_request(self.socks, user, &password)?;
        let original =
            self.cli(&["session", "list", "--business", "shop"], None)?["items"][0].clone();
        self.cli(
            &["user", "passwd", "alice", "--password-stdin"],
            Some(&format!("{new_password}\n")),
        )?;
        self.expect_request(Some(user), &password, 407)?;
        self.expect_request(Some(user), &new_password, 200)?;
        let rotated =
            self.cli(&["session", "list", "--business", "shop"], None)?["items"][0].clone();
        ensure!(
            original["owner"] == rotated["owner"] && original["egress"] == rotated["egress"],
            "password rotation changed sticky binding"
        );
        self.compose(&["stop", "rproxy"], None)?;
        self.compose(&["start", "rproxy"], None)?;
        self.wait("durable managed restart", |s| s["sessions"] == 1)?;
        self.expect_request(Some(user), &new_password, 200)?;
        self.expect_request(Some(user), &password, 407)?;
        let restarted =
            self.cli(&["session", "list", "--business", "shop"], None)?["items"][0].clone();
        ensure!(
            original["owner"] == restarted["owner"] && original["egress"] == restarted["egress"],
            "restart changed sticky egress"
        );
        println!("PASS internal collection, password rotation, sticky egress and restart");

        let mut config: toml::Value = toml::from_str(&fs::read_to_string(&self.config)?)?;
        config["auth"]["users"][0]["name"] = "file-only-user".into();
        config["auth"]["mode"] = "anonymous".into();
        config["auth"]["default_strategy"] = "round-robin".into();
        self.replace_config(&config)?;
        self.compose(
            &["up", "-d", "--no-build", "--force-recreate", "rproxy"],
            None,
        )?;
        self.wait("mounted startup policy edit", |s| s["verified"] == 1)?;
        self.expect_request(None, "", 200)?;
        let names = self.cli(&["user", "list"], None)?;
        let names = names["items"].as_array().context("user list missing")?;
        ensure!(
            names.iter().any(|u| u["name"] == "alice")
                && !names.iter().any(|u| u["name"] == "file-only-user"),
            "bootstrap TOML overwrote managed users"
        );
        self.expect_request(Some(user), &new_password, 200)?;
        config["auth"]["mode"] = "required".into();
        self.replace_config(&config)?;
        self.compose(
            &["up", "-d", "--no-build", "--force-recreate", "rproxy"],
            None,
        )?;
        self.wait("required authentication restored", |s| s["verified"] == 1)?;
        self.expect_request(None, "", 407)?;
        println!("PASS read-only host config policy and bootstrap-once users");

        self.cli(
            &[
                "user",
                "add",
                "bob",
                "--business",
                "billing",
                "--password-stdin",
            ],
            Some(&format!("{bob_password}\n")),
        )?;
        let bob = "v1|bob|billing|created-online|stick";
        self.expect_request(Some(bob), &bob_password, 200)?;
        let grant = "v1|bob|reports|grant-check|stick";
        self.expect_request(Some(grant), &bob_password, 407)?;
        self.cli(
            &["user", "grant", "bob", "--business", "billing,reports"],
            None,
        )?;
        self.expect_request(Some(grant), &bob_password, 200)?;
        self.cli(&["user", "grant", "bob"], None)?;
        self.expect_request(Some(bob), &bob_password, 407)?;
        self.cli(&["user", "grant", "bob", "--business", "billing"], None)?;
        self.cli(&["user", "disable", "bob", "--yes"], None)?;
        self.expect_request(Some(bob), &bob_password, 407)?;
        self.cli(&["user", "enable", "bob"], None)?;
        self.expect_request(Some(bob), &bob_password, 200)?;
        let address = self.cli(&["proxy", "list", "--protocol", "http"], None)?["items"][0]
            ["address"]
            .as_str()
            .context("source proxy address missing")?
            .to_owned();
        let id = format!("http:{address}");
        self.cli(&["source", "pause", "local-fixture"], None)?;
        ensure!(
            !self
                .cli_output(&["source", "run", "local-fixture"], None)?
                .success,
            "paused source ran"
        );
        ensure!(
            self.cli(&["proxy", "show", &id], None)?["state"] == "ready",
            "pause destroyed admitted proxy"
        );
        self.cli(&["source", "resume", "local-fixture"], None)?;
        self.cli(
            &[
                "source",
                "run",
                "local-fixture",
                "--wait",
                "--wait-timeout",
                "60",
            ],
            None,
        )?;
        self.cli(
            &[
                "source",
                "import",
                "-",
                "--replace",
                "--wait",
                "--wait-timeout",
                "60",
            ],
            Some(&source_manifest(2)),
        )?;
        self.cli(
            &[
                "proxy",
                "add",
                "http",
                &address,
                "--source",
                "manual-http",
                "--wait",
            ],
            None,
        )?;
        let host = address
            .rsplit_once(':')
            .context("numeric fixture address missing port")?
            .0;
        self.cli(
            &[
                "proxy",
                "add",
                "socks5",
                &format!("{host}:18081"),
                "--source",
                "manual-socks",
                "--wait",
                "--wait-timeout",
                "60",
            ],
            None,
        )?;
        self.cli(&["source", "remove", "local-fixture", "--yes"], None)?;
        ensure!(
            self.cli(&["proxy", "show", &id], None)?["state"] == "ready",
            "source removal destroyed proxy"
        );
        ensure!(
            self.cli(&["proxy", "disable", &id, "--dry-run"], None)?["applied"] == false,
            "dry-run applied mutation"
        );
        self.cli(&["proxy", "disable", &id, "--yes"], None)?;
        self.expect_request(Some("v1|alice|news|manual-socks|stick"), &new_password, 200)?;
        socks_request(self.socks, user, &new_password)?;
        println!("PASS online users/grants/sources, manual HTTP/SOCKS5 and source removal");

        let mut tunnel = open_tunnel(self.http, user, &new_password)?;
        self.cli(&["user", "disable", "alice", "--disconnect", "--yes"], None)?;
        tunnel.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut byte = [0];
        match tunnel.read(&mut byte) {
            Ok(0) => {}
            Err(e)
                if [
                    std::io::ErrorKind::ConnectionReset,
                    std::io::ErrorKind::ConnectionAborted,
                ]
                .contains(&e.kind()) => {}
            _ => bail!("force-disconnected tunnel remained alive"),
        }
        self.cli(&["user", "enable", "alice"], None)?;
        self.expect_request(Some(user), &new_password, 200)?;
        for name in ["alice", "bob"] {
            self.cli(&["user", "remove", name, "--disconnect", "--yes"], None)?;
        }
        ensure!(
            self.cli(&["user", "list"], None)?["items"] == json!([]),
            "last user removal failed"
        );
        self.expect_request(None, "", 407)?;
        self.compose(&["stop", "rproxy"], None)?;
        self.compose(&["start", "rproxy"], None)?;
        self.wait("last-user fail-closed restart", |s| s.is_object())?;
        ensure!(
            self.cli(&["user", "list"], None)?["items"] == json!([]),
            "restart revived bootstrap user"
        );
        self.expect_request(None, "", 407)?;
        self.cli(
            &["session", "release", "shop", "compose-smoke", "--yes"],
            None,
        )?;
        thread::sleep(Duration::from_secs(2));
        self.cli(&["health"], None)?;
        let id = self
            .compose(&["ps", "-q", "rproxy"], None)?
            .stdout
            .trim()
            .to_owned();
        ensure!(!id.is_empty(), "owned rproxy container missing");
        let running = self.runner.run(&Request {
            program: self.engine.clone().into(),
            args: vec![
                "inspect".into(),
                "--format".into(),
                "{{.State.Running}}".into(),
                id,
            ],
            env: self.environment.clone(),
            cwd: self.root.clone(),
            stdin: None,
            timeout: Duration::from_secs(15),
        })?;
        ensure!(
            running.success && running.stdout.trim() == "true",
            "idle internal collector killed rproxy"
        );
        println!(
            "PASS explicit force disconnect, last-user fail-closed and idle internal collector"
        );
        Ok(())
    }

    fn replace_config(&self, config: &toml::Value) -> Result<()> {
        let replacement = self.directory.path().join("edited.toml");
        fs::write(&replacement, toml::to_string_pretty(config)?)?;
        fs::rename(&replacement, &self.config).context("replace owned startup configuration")
    }
}

fn source_manifest(revision: u8) -> String {
    format!("schema_version=1\n[[sources]]\nname=\"local-fixture\"\nkind=\"api\"\nenabled=true\n[sources.config]\nurl=\"${{SMOKE_FIXTURE_URL}}?revision={revision}\"\nprotocol=\"http\"\ninterval_seconds=3600\ntimeout_seconds=2\nattempts=1\n")
}

pub fn run(root: &Path, options: Options) -> Result<()> {
    run_with(root, options, &mut SystemRunner)
}

pub fn run_with(root: &Path, options: Options, runner: &mut impl Runner) -> Result<()> {
    let mut smoke = Smoke::new(root, &options, runner)?;
    println!("Owned Compose project: {}", smoke.project);
    let result = smoke.scenarios(options.skip_build);
    let diagnostic = if result.is_err() {
        smoke
            .invoke(&["logs", "--no-color", "--tail", "100"], None, None)
            .ok()
            .map(|out| smoke.redact(&format!("{}\n{}", out.stdout, out.stderr)))
            .unwrap_or_else(|| "owned Compose logs unavailable".into())
    } else {
        String::new()
    };
    let cleanup = smoke
        .cleanup_allowed
        .then(|| smoke.invoke(&["down", "--volumes", "--remove-orphans"], None, None));
    let cleanup_error = match cleanup {
        None => None,
        Some(Ok(output)) if output.success => None,
        Some(Ok(output)) => Some(format!(
            "{}: {}",
            output.status,
            smoke.redact(&output.stderr)
        )),
        Some(Err(e)) => Some(format!("{e:#}")),
    };
    if let Some(error) = cleanup_error {
        bail!("owned project {} cleanup failed: {error}; scenario result: {}; redacted logs:\n{diagnostic}", smoke.project, result.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_else(|| "passed".into()));
    }
    if let Err(error) = result {
        bail!("{error:#}\nredacted owned-project logs:\n{diagnostic}");
    }
    Ok(())
}

fn connect(port: u16) -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse()?,
        Duration::from_secs(10),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(stream)
}
fn response(stream: &mut TcpStream) -> Result<(u16, String)> {
    let headers = header(stream)?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .context("missing HTTP status")?
        .parse()?;
    let mut body = String::new();
    stream.take(1024 * 1024).read_to_string(&mut body)?;
    Ok((status, body))
}
fn proxy_request(port: u16, username: Option<&str>, password: &str) -> Result<(u16, String)> {
    let mut stream = connect(port)?;
    let auth = username
        .map(|user| {
            format!(
                "Proxy-Authorization: Basic {}\r\n",
                STANDARD.encode(format!("{user}:{password}"))
            )
        })
        .unwrap_or_default();
    write!(stream, "GET http://fixture:18080/payload HTTP/1.1\r\nHost: fixture:18080\r\n{auth}Connection: close\r\n\r\n")?;
    response(&mut stream)
}
fn open_tunnel(port: u16, username: &str, password: &str) -> Result<TcpStream> {
    let mut stream = connect(port)?;
    let auth = STANDARD.encode(format!("{username}:{password}"));
    write!(stream, "CONNECT fixture:18080 HTTP/1.1\r\nHost: fixture:18080\r\nProxy-Authorization: Basic {auth}\r\n\r\n")?;
    ensure!(
        header(&mut stream)?.starts_with("HTTP/1.1 200"),
        "tunnel could not be established"
    );
    Ok(stream)
}
fn socks_request(port: u16, username: &str, password: &str) -> Result<()> {
    let mut stream = connect(port)?;
    stream.write_all(&[5, 1, 2])?;
    let mut auth = [0; 2];
    stream.read_exact(&mut auth)?;
    ensure!(auth == [5, 2], "SOCKS authentication negotiation failed");
    ensure!(
        username.len() <= 255 && password.len() <= 255,
        "SOCKS fixture credential too long"
    );
    let mut credentials = vec![1, username.len() as u8];
    credentials.extend(username.as_bytes());
    credentials.push(password.len() as u8);
    credentials.extend(password.as_bytes());
    stream.write_all(&credentials)?;
    stream.read_exact(&mut auth)?;
    ensure!(auth == [1, 0], "SOCKS authentication failed");
    let mut request = b"\x05\x01\x00\x03\x07fixture".to_vec();
    request.extend(18080u16.to_be_bytes());
    stream.write_all(&request)?;
    let mut reply = [0; 4];
    stream.read_exact(&mut reply)?;
    ensure!(reply[..3] == [5, 0, 0], "SOCKS connect failed");
    let len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0];
            stream.read_exact(&mut len)?;
            len[0] as usize
        }
        _ => bail!("invalid SOCKS reply address"),
    };
    let mut bound = vec![0; len + 2];
    stream.read_exact(&mut bound)?;
    stream
        .write_all(b"GET /payload HTTP/1.1\r\nHost: fixture:18080\r\nConnection: close\r\n\r\n")?;
    let (status, body) = response(&mut stream)?;
    ensure!(
        status == 200 && body == "mounted-compose-ok",
        "SOCKS payload mismatch"
    );
    Ok(())
}
