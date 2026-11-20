use std::process::Command;

#[test]
fn check_config_validates_without_opening_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("proxy.toml");
    std::fs::write(
        &path,
        "database='must-not-be-created/state.sqlite3'\n[probe]\nenabled=false",
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

#[cfg(unix)]
#[test]
fn non_unicode_secret_is_not_printed_in_startup_errors() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("proxy.toml");
    std::fs::write(&config, "[[auth.users]]\nname='alice'\npassword_env='RPROXY_TEST_BAD_PASSWORD'\nbusinesses=['shop']").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rproxy"))
        .args(["--config", config.to_str().unwrap()])
        .env(
            "RPROXY_TEST_BAD_PASSWORD",
            OsString::from_vec([b"private-secret-".as_slice(), &[0xff]].concat()),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-secret"));
}

#[test]
fn invalid_toml_does_not_echo_inline_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("proxy.toml");
    std::fs::write(
        &config,
        "[[auth.users]]\nname='alice'\npassword='sensitive-config-value\nbusinesses=['shop']",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rproxy"))
        .args(["--config", config.to_str().unwrap(), "check-config"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-config-value"));
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
