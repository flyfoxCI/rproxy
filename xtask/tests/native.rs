use anyhow::Result;
use std::{collections::VecDeque, path::Path, time::Duration};
use xtask::{
    native::{admin_port, assert_native, check_versions, safe_log},
    process::{Output, Request, Runner},
};

struct Versions {
    answers: VecDeque<&'static str>,
    requests: Vec<Request>,
}
impl Runner for Versions {
    fn run(&mut self, request: &Request) -> Result<Output> {
        self.requests.push(request.clone());
        Ok(Output {
            success: true,
            status: "exit code: 0".into(),
            stdout: self.answers.pop_front().unwrap().into(),
            stderr: String::new(),
        })
    }
}
#[test]
fn both_local_executables_must_report_matching_versions_with_deadlines() {
    let mut runner = Versions {
        answers: VecDeque::from(["rproxy 0.2.4\n", "rproxy-cli 0.2.4\n"]),
        requests: vec![],
    };
    check_versions(
        &mut runner,
        Path::new("rproxy"),
        Path::new("rproxy-cli"),
        "0.2.4",
    )
    .unwrap();
    assert_eq!(runner.requests.len(), 2);
    assert_eq!(runner.requests[0].program, Path::new("rproxy"));
    assert_eq!(runner.requests[1].program, Path::new("rproxy-cli"));
    for request in runner.requests {
        assert_eq!(request.args, ["--version"]);
        assert_eq!(request.timeout, Duration::from_secs(10));
        assert!(request.stdin.is_none());
    }
    for outputs in [
        ["rproxy 0.2.3\n", "rproxy-cli 0.2.4\n"],
        ["rproxy 0.2.4\n", "rproxy 0.2.4\n"],
    ] {
        let mut runner = Versions {
            answers: outputs.into(),
            requests: vec![],
        };
        assert!(check_versions(
            &mut runner,
            Path::new("rproxy"),
            Path::new("rproxy-cli"),
            "0.2.4"
        )
        .is_err());
    }
}
#[test]
fn failed_version_gate_reports_safe_output_and_exit_cause() {
    struct Failed;
    impl Runner for Failed {
        fn run(&mut self, _: &Request) -> Result<Output> {
            Ok(Output {
                success: false,
                status: "signal: SIGABRT".into(),
                stdout: "native loader output".into(),
                stderr: "\u{1b}[31mdyld: missing dependency\n".into(),
            })
        }
    }
    let error = check_versions(
        &mut Failed,
        Path::new("rproxy"),
        Path::new("rproxy-cli"),
        "0.2.4",
    )
    .unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("signal: SIGABRT"));
    assert!(
        diagnostic.contains("dyld: missing dependency"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("native loader output"));
    assert!(!diagnostic.contains('\u{1b}'));
}

#[test]
fn startup_diagnostics_retain_sqlite_cause_but_redact_credentials_and_terminal_controls() {
    let log = "Error: SQLite database is locked (5)\nroot=TOKEN password=PASSWORD\n\u{1b}[31mexit code: 1\n";
    let safe = safe_log(log, &["TOKEN", "PASSWORD"]);
    assert!(safe.contains("SQLite database is locked (5)"));
    assert!(safe.contains("exit code: 1"));
    assert!(!safe.contains("TOKEN"));
    assert!(!safe.contains("PASSWORD"));
    assert!(!safe.contains('\u{1b}'));
}
#[test]
fn listener_detection_requires_the_loopback_admin_listener_and_nonzero_port() {
    assert_eq!(
        admin_port("INFO listening protocol=admin address=127.0.0.1:41234\n"),
        Some(41234)
    );
    for log in [
        "listening protocol=http address=127.0.0.1:41234",
        "listening protocol=admin address=0.0.0.0:9100",
        "listening protocol=admin address=127.0.0.1:0",
        "listening protocol=admin address=127.0.0.1:99999",
    ] {
        assert_eq!(admin_port(log), None);
    }
}
#[test]
fn native_target_guard_rejects_foreign_architecture_and_unknown_targets() {
    assert!(assert_native("unknown").is_err());
    let native = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => return,
    };
    assert_native(native).unwrap();
    let foreign = if native.starts_with("aarch64") {
        "x86_64-apple-darwin"
    } else {
        "aarch64-apple-darwin"
    };
    assert!(assert_native(foreign).is_err());
}
