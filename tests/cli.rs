use std::process::Command;

#[test]
fn check_config_validates_without_connecting_to_redis() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(
        &path,
        "redis_url='redis://127.0.0.1:1/'\n[probe]\nenabled=false",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rproxy"))
        .args(["--config", path.to_str().unwrap(), "check-config"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn invalid_tls_configuration_fails_without_plaintext_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(&path, "https_bind='127.0.0.1:9443'").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rproxy"))
        .args(["--config", path.to_str().unwrap(), "check-config"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("HTTPS"));
}

#[test]
fn release_command_requires_authentication_before_connecting_backend() {
    let result = Command::new(env!("CARGO_BIN_EXE_rproxy"))
        .args(["release-session", "--username", "v1|alice|shop|a|stick"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("authentication"));
}
