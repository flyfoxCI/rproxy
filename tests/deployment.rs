use serde_json::Value;
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}
fn yaml(name: &str) -> Value {
    serde_yaml::from_str(&fs::read_to_string(root().join(name)).unwrap()).unwrap()
}
#[test]
fn single_docker_image_requires_no_compose_or_fixture() {
    for obsolete in ["compose.yaml", "compose.https.yaml", ".env.example"] {
        assert!(
            !root().join(obsolete).exists(),
            "obsolete deployment file {obsolete}"
        );
    }
    let dockerfile = fs::read_to_string(root().join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("AS build"));
    assert!(dockerfile.contains("AS rproxy"));
    assert!(!dockerfile.contains("fixture"));
    assert!(!dockerfile.contains("-p xtask"));
    assert!(dockerfile.contains("USER 10001:10001"));
    assert!(dockerfile.contains("rproxy-cli /usr/local/bin/"));
    assert!(dockerfile.contains("HEALTHCHECK"));
    assert!(!dockerfile.contains("EXPOSE 9100"));
    for value in [
        "/var/lib/rproxy/rproxy.sqlite3",
        "0.0.0.0:9099",
        "0.0.0.0:9089",
    ] {
        assert!(
            dockerfile.contains(value),
            "missing container default {value}"
        );
    }
}

#[test]
fn docker_build_context_excludes_private_files_and_ci_uses_plain_docker() {
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
    let ci = fs::read_to_string(root().join(".github/workflows/ci.yml")).unwrap();
    assert!(ci.contains("docker build"));
    assert!(ci.contains("docker run"));
    assert!(!ci.to_lowercase().contains("compose"));
    assert!(!ci.to_lowercase().contains("podman"));
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
