use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use xtask::process::{environment, Request, Runner, SystemRunner};

// Re-execute this test executable, never a fake archive input or shell script.
#[test]
fn process_child() {
    match std::env::var("XTASK_PROCESS_TEST").as_deref() {
        Ok("timeout") => {
            println!("startup before timeout");
            eprintln!("SQLite diagnostic before timeout");
            std::thread::sleep(Duration::from_secs(10));
        }
        Ok("environment") => {
            assert!(environment().keys().all(|name| {
                let name = name.to_ascii_uppercase();
                !name.starts_with("RPROXY_") && !name.starts_with("COMPOSE_")
            }));
        }
        Ok("input") => {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).unwrap();
            assert_eq!(input, "test-only stdin\n");
            println!("stdin received");
        }
        _ => {}
    }
}

fn request(mode: &str) -> Request {
    Request {
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "process_child".into(),
            "--nocapture".into(),
        ],
        env: BTreeMap::from([("XTASK_PROCESS_TEST".into(), mode.into())]),
        cwd: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        stdin: None,
        timeout: Duration::from_secs(5),
    }
}

#[test]
fn timeout_retains_exit_cause_and_bounded_output_after_reaping_owned_child() {
    let mut request = request("timeout");
    request.timeout = Duration::from_millis(500);
    let started = std::time::Instant::now();
    let output = SystemRunner
        .run(&request)
        .expect("timeouts retain diagnostics as failed output");
    assert!(!output.success);
    assert!(output.status.contains("timed out"));
    assert!(output.status.contains("exit"), "{}", output.status);
    assert!(output.stdout.contains("startup before timeout"));
    assert!(output.stderr.contains("SQLite diagnostic before timeout"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn native_and_compose_environments_drop_case_insensitive_parent_overrides() {
    let mut request = request("environment");
    request.env.extend([
        ("rproxy_DATABASE".into(), "never-inherit.sqlite3".into()),
        ("compose_PROJECT_NAME".into(), "never-inherit".into()),
    ]);
    let output = SystemRunner.run(&request).unwrap();
    assert!(output.success, "{}\n{}", output.stdout, output.stderr);
}

#[test]
fn runner_delivers_stdin_without_exposing_it_in_arguments() {
    let mut request = request("input");
    request.stdin = Some("test-only stdin\n".into());
    let output = SystemRunner.run(&request).unwrap();
    assert!(output.success, "{}", output.stderr);
    assert!(output.stdout.contains("stdin received"));
    assert!(!request.args.iter().any(|arg| arg.contains("test-only")));
}
