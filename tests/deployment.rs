use serde_json::Value;
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn yaml(name: &str) -> Value {
    serde_yaml::from_str(&fs::read_to_string(root().join(name)).unwrap()).unwrap()
}
fn mount<'a>(service: &'a Value, target: &str) -> &'a Value {
    service["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|volume| volume["target"] == target)
        .unwrap_or_else(|| panic!("missing mount {target}"))
}

#[test]
fn production_compose_is_one_hardened_service_with_private_management() {
    let compose = yaml("compose.yaml");
    let services = compose["services"].as_object().unwrap();
    assert_eq!(services.keys().collect::<Vec<_>>(), vec!["rproxy"]);
    let service = &services["rproxy"];
    assert_eq!(service["build"]["target"], "rproxy");
    assert_eq!(service["user"], "10001:10001");
    assert_eq!(service["restart"], "unless-stopped");
    assert_eq!(service["read_only"], true);
    assert_eq!(service["cap_drop"], serde_json::json!(["ALL"]));
    assert!(
        service["env_file"].is_null(),
        "provider secrets must not be process env"
    );
    let environment = service["environment"].as_object().unwrap();
    assert!(!environment.contains_key("RPROXY_WORKER_TOKEN"));
    assert!(environment.contains_key("RPROXY_ADMIN_TOKEN"));
    for target in ["/etc/rproxy/rproxy.toml", "/etc/rproxy/providers.env"] {
        let volume = mount(service, target);
        assert_eq!(volume["type"], "bind");
        assert_eq!(volume["read_only"], true);
        assert_eq!(volume["bind"]["create_host_path"], false);
    }
    assert_eq!(mount(service, "/var/lib/rproxy")["type"], "volume");
    let ports = service["ports"].as_array().unwrap();
    assert_eq!(ports.len(), 2);
    assert!(ports.iter().all(|port| {
        let port = port.as_str().unwrap();
        port.contains("127.0.0.1") && !port.contains("9100")
    }));
    let command = service["command"].as_array().unwrap();
    for value in [
        "/var/lib/rproxy/rproxy.sqlite3",
        "0.0.0.0:9099",
        "0.0.0.0:9089",
        "0.0.0.0:9100",
    ] {
        assert!(command.iter().any(|arg| arg == value));
    }
    assert!(service["healthcheck"]["test"]
        .as_array()
        .unwrap()
        .iter()
        .any(|arg| arg == "healthcheck"));
}

#[test]
fn https_remains_opt_in_and_build_context_excludes_private_files() {
    let https = yaml("compose.https.yaml");
    let service = &https["services"]["rproxy"];
    let certs = mount(service, "/etc/rproxy/certs");
    assert_eq!(certs["read_only"], true);
    assert_eq!(certs["bind"]["create_host_path"], false);
    assert!(service["ports"][0].as_str().unwrap().ends_with(":9443"));
    let ignore = fs::read_to_string(root().join(".dockerignore")).unwrap();
    assert_eq!(ignore.lines().next(), Some("**"));
    for forbidden in [
        "!.env",
        "!rproxy.toml",
        "!providers.env",
        "!data/**",
        "!certs/**",
    ] {
        assert!(!ignore.lines().any(|line| line == forbidden));
    }
    assert!(ignore.lines().any(|line| line == "!xtask/src/**"));
    let dockerfile = fs::read_to_string(root().join("Dockerfile")).unwrap();
    assert!(!dockerfile.contains("COPY . "));
    assert!(!dockerfile.to_lowercase().contains("python"));
    assert!(dockerfile.contains("AS test-fixture"));
    assert!(dockerfile.contains("AS rproxy"));
}

#[test]
fn workflows_use_workspace_tests_and_rust_native_release_gates() {
    let ci = fs::read_to_string(root().join(".github/workflows/ci.yml")).unwrap();
    let release = fs::read_to_string(root().join(".github/workflows/release.yml")).unwrap();
    for workflow in [&ci, &release] {
        assert!(!workflow.to_lowercase().contains("python"));
        let _: Value = serde_yaml::from_str(workflow).unwrap();
    }
    assert!(!ci.contains("redis:"));
    assert!(ci.contains("cargo test --locked --workspace --all-targets"));
    assert!(ci.contains("cargo clippy --locked --workspace --all-targets"));
    assert!(ci.contains("cargo fmt --all --check"));
    for command in ["assert-native", "package", "verify", "checksums"] {
        assert!(release.contains(&format!("-p xtask -- {command}")));
    }
    assert!(release.contains("--draft"));
    assert!(release.contains("Refusing to overwrite"));
    assert!(release.contains("aarch64-unknown-linux-musl\" ]]; then"));
    let release_model = yaml(".github/workflows/release.yml");
    let matrix = release_model["jobs"]["build"]["strategy"]["matrix"]["include"]
        .as_array()
        .unwrap();
    let targets = matrix
        .iter()
        .map(|row| row["target"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        targets,
        std::collections::BTreeSet::from([
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc"
        ])
    );
    let steps = release_model["jobs"]["build"]["steps"].as_array().unwrap();
    assert!(
        steps.iter().any(|step| {
            step["if"] == "runner.os == 'macOS'"
                && step["run"].as_str().is_some_and(|command| {
                    command
                        .contains("cargo test --release --locked -p rproxy --test embedded_store")
                })
        }),
        "macOS ownership regression must execute before packaging"
    );
}
