use crate::{
    process::{environment, read_tail, OwnedChild, Request, Runner, SystemRunner},
    release::{NativeGate, TARGETS},
};
use anyhow::{bail, ensure, Context, Result};
use rand::{distributions::Alphanumeric, Rng};
use serde_json::Value;
use std::{
    fs::{self, File},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn random_secret() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(48)
        .map(char::from)
        .collect()
}

pub fn assert_native(target: &str) -> Result<()> {
    ensure!(
        TARGETS.contains(&target),
        "unsupported native release target"
    );
    let arch = if target.starts_with("aarch64-") {
        "aarch64"
    } else {
        "x86_64"
    };
    let os = if target.contains("windows") {
        "windows"
    } else if target.contains("apple") {
        "macos"
    } else {
        "linux"
    };
    ensure!(
        std::env::consts::OS == os && std::env::consts::ARCH == arch,
        "release target must match native host architecture and OS"
    );
    Ok(())
}

pub fn check_versions(
    runner: &mut impl Runner,
    server: &Path,
    cli: &Path,
    version: &str,
) -> Result<()> {
    for (program, name) in [(server, "rproxy"), (cli, "rproxy-cli")] {
        let output = runner.run(&Request {
            program: program.to_owned(),
            args: vec!["--version".into()],
            env: environment(),
            cwd: std::env::current_dir()?,
            stdin: None,
            timeout: Duration::from_secs(10),
        })?;
        ensure!(
            output.success && output.stdout.trim() == format!("{name} {version}"),
            "native {name} --version must match Cargo.toml; {}\nredacted stdout:\n{}\nredacted stderr:\n{}",
            output.status,
            safe_log(&output.stdout, &[]),
            safe_log(&output.stderr, &[])
        );
    }
    Ok(())
}

pub fn safe_log(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "<redacted>");
        }
    }
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

pub fn admin_port(log: &str) -> Option<u16> {
    log.lines().find_map(|line| {
        let rest = line
            .split_once("listening protocol=admin address=127.0.0.1:")?
            .1;
        let port = rest.split_whitespace().next()?.parse::<u16>().ok()?;
        (port != 0).then_some(port)
    })
}

pub struct SystemNativeGate;
impl NativeGate for SystemNativeGate {
    fn check(&mut self, server: &Path, cli: &Path, version: &str) -> Result<()> {
        let server = server.canonicalize()?;
        let cli = cli.canonicalize()?;
        check_versions(&mut SystemRunner, &server, &cli, version)?;
        management_smoke(&server, &cli)
    }
}

/// Real temporary SQLite/server and authenticated online CLI gate, only for local builds.
fn management_smoke(server: &Path, cli: &Path) -> Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("rproxy-native-gate-")
        .tempdir()?;
    let config = directory.path().join("rproxy.toml");
    fs::write(&config, "database=\"state.sqlite3\"\nhttp_bind=\"127.0.0.1:0\"\n[admin]\nbind=\"127.0.0.1:0\"\n[auth]\nmode=\"required\"\n[probe]\nenabled=false\n[collector]\nenabled=false\n")?;
    let token = random_secret();
    let password = random_secret();
    let mut env = environment();
    env.insert("RPROXY_ADMIN_TOKEN".into(), token.clone());
    env.insert("RUST_LOG".into(), "info".into());
    let path = directory.path().join("server.log");
    let log = File::create(&path)?;
    let mut child = OwnedChild(
        Command::new(server)
            .args(["--config"])
            .arg(&config)
            .arg("--no-socks5")
            .current_dir(directory.path())
            .env_clear()
            .envs(&env)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("native server spawn failed")?,
    );
    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let port = loop {
            if let Some(status) = child.0.try_wait()? {
                bail!("native SQLite/server startup failed: {status}");
            }
            if let Some(port) = admin_port(&read_tail(&path, 64 * 1024)?) {
                break port;
            }
            ensure!(
                Instant::now() < deadline,
                "native management listener startup timed out"
            );
            thread::sleep(Duration::from_millis(50));
        };
        let base = vec![
            "--endpoint".to_owned(),
            format!("http://127.0.0.1:{port}"),
            "--json".into(),
        ];
        let run = |arguments: &[&str], input: Option<String>| -> Result<Value> {
            let mut args = base.clone();
            args.extend(arguments.iter().map(|s| (*s).to_owned()));
            let output = SystemRunner.run(&Request {
                program: cli.to_owned(),
                args,
                env: env.clone(),
                cwd: directory.path().to_owned(),
                stdin: input,
                timeout: Duration::from_secs(20),
            })?;
            ensure!(
                output.success,
                "native CLI management failed: {}; {}",
                output.status,
                safe_log(&output.stderr, &[&token, &password])
            );
            serde_json::from_str(&output.stdout)
                .context("native CLI returned invalid management JSON")
        };
        let created = run(
            &[
                "user",
                "add",
                "packaging-check",
                "--business",
                "packaging",
                "--password-stdin",
            ],
            Some(format!("{password}\n")),
        )?;
        ensure!(
            created["name"] == "packaging-check",
            "native online user creation failed"
        );
        let listed = run(&["user", "list"], None)?;
        ensure!(
            listed["items"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "packaging-check")),
            "native durable user read failed"
        );
        ensure!(
            directory.path().join("state.sqlite3").is_file(),
            "native SQLite database missing"
        );
        Ok(())
    })();
    if let Err(error) = result {
        let status = child.0.try_wait()?.map_or_else(
            || "still running at gate failure".to_owned(),
            |status| status.to_string(),
        );
        let log = read_tail(&path, 64 * 1024).unwrap_or_else(|_| "startup log unavailable".into());
        bail!(
            "{error:#}\nserver exit cause: {status}\nredacted startup log:\n{}",
            safe_log(&log, &[&token, &password])
        );
    }
    Ok(())
}
