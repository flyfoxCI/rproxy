use std::process::Command;

fn xtask() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
}

#[test]
fn command_help_exposes_exact_tooling_contract() {
    let output = xtask().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for command in [
        "package",
        "verify",
        "checksums",
        "assert-native",
        "compose-smoke",
        "fixture",
    ] {
        assert!(help.contains(command), "missing {command}: {help}");
    }
    let output = xtask().args(["package", "--help"]).output().unwrap();
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("--directory"));
}

#[test]
fn compose_cli_rejects_invalid_engines_and_image_reuse_without_skip_build() {
    for args in [
        vec!["compose-smoke", "--engine", "sh"],
        vec!["compose-smoke", "--image-prefix", "prebuilt"],
    ] {
        let output = xtask().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn invalid_package_tag_and_target_do_not_create_output_or_execute_release_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let output_path = directory.path().join("never-created");
    let native = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return,
    };
    for target in [native, "invalid-target"] {
        let output = xtask()
            .args([
                "package",
                "--target",
                target,
                "--tag",
                "../invalid",
                "--directory",
            ])
            .arg(&output_path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!output_path.exists());
    }
}
