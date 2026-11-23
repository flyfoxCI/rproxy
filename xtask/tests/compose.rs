use serde_json::{json, Value};
use xtask::compose::validate_rendered;
fn rendered() -> Value {
    json!({"name":"rproxy-smoke-owned", "services": {
        "rproxy":{"environment":{"RPROXY_ADMIN_TOKEN":"test-only", "RPROXY_PASSWORD":"test-only"}, "volumes":[
            {"type":"bind","source":"/temporary/rproxy.toml","target":"/etc/rproxy/rproxy.toml","read_only":true,"bind":{"create_host_path":false}},
            {"type":"bind","source":"/temporary/providers.env","target":"/etc/rproxy/providers.env","read_only":true,"bind":{"create_host_path":false}},
            {"type":"volume","target":"/var/lib/rproxy","source":"rproxy-data"}],
            "ports":[{"host_ip":"127.0.0.1","target":9099,"published":"19099"},{"host_ip":"127.0.0.1","target":9089,"published":"19089"}]},
        "fixture":{"entrypoint":["xtask","fixture"], "build":{"target":"test-fixture"}, "ports":[]}
    }, "volumes":{"rproxy-data":{"name":"rproxy-smoke-owned_rproxy-data"}},
       "networks":{"default":{"name":"rproxy-smoke-owned_default"}}})
}
#[test]
fn preflight_failure_preserves_diagnostics_without_deleting_unvalidated_resources() {
    use xtask::{
        compose::{run_with, Options},
        process::{Output, Request, Runner},
    };
    struct Engine {
        requests: Vec<Request>,
        cleanup_fails: bool,
    }
    impl Runner for Engine {
        fn run(&mut self, request: &Request) -> anyhow::Result<Output> {
            self.requests.push(request.clone());
            let cleanup = request.args.iter().any(|a| a == "down");
            let logs = request.args.iter().any(|a| a == "logs");
            Ok(Output {
                success: logs || (cleanup && !self.cleanup_fails),
                status: "exit code: 19".into(),
                stdout: if logs {
                    "fixture startup diagnostic".into()
                } else {
                    String::new()
                },
                stderr: "engine unavailable".into(),
            })
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for cleanup_fails in [false, true] {
        let mut runner = Engine {
            requests: vec![],
            cleanup_fails,
        };
        let result = run_with(
            root,
            Options {
                engine: "podman".into(),
                skip_build: true,
                image_prefix: Some("prebuilt".into()),
            },
            &mut runner,
        )
        .unwrap_err();
        let error = format!("{result:#}");
        assert!(error.contains("exit code: 19"));
        assert!(error.contains("fixture startup diagnostic"));
        assert!(!error.contains("cleanup failed"));
        assert_eq!(runner.requests.len(), 2);
        let project = &runner.requests[0].args[2];
        assert!(project.starts_with("rproxy-smoke-"));
        for request in &runner.requests {
            assert_eq!(request.program, std::path::Path::new("podman"));
            assert_eq!(&request.args[..3], ["compose", "-p", project]);
        }
        assert!(!runner.requests.iter().any(|r| r
            .args
            .iter()
            .any(|a| a == "prune" || a == "build" || a == "down")));
    }
}

#[test]
fn smoke_checks_real_rendered_host_ports_https_binds_and_missing_root_token_before_starting() {
    use xtask::{
        compose::{run_with, Options},
        process::{Output, Request, Runner},
    };
    struct RenderEngine {
        requests: Vec<Request>,
        corrupt: &'static str,
    }
    impl Runner for RenderEngine {
        fn run(&mut self, request: &Request) -> anyhow::Result<Output> {
            self.requests.push(request.clone());
            let render = request.args.iter().any(|a| a == "--format");
            let https = request
                .args
                .iter()
                .any(|a| a.ends_with("compose.https.yaml"));
            let quiet = request.args.iter().any(|a| a == "--quiet");
            let cleanup = request.args.iter().any(|a| a == "down");
            let logs = request.args.iter().any(|a| a == "logs");
            let stdout = if render {
                let mut model = rendered();
                let project = &request.args[2];
                model["name"] = json!(project);
                model["volumes"]["rproxy-data"]["name"] = json!(format!("{project}_rproxy-data"));
                model["networks"]["default"]["name"] = json!(format!("{project}_default"));
                if self.corrupt == "named-volume" {
                    model["volumes"]["rproxy-data"]["name"] = json!("real-user-data");
                }
                let volumes = &mut model["services"]["rproxy"]["volumes"];
                volumes[0]["source"] = json!(request.env["RPROXY_CONFIG"]);
                volumes[1]["source"] = json!(request.env["RPROXY_PROVIDERS_ENV"]);
                model["services"]["rproxy"]["ports"][0]["published"] =
                    json!(request.env["RPROXY_HTTP_PORT"]);
                model["services"]["rproxy"]["ports"][1]["published"] =
                    json!(request.env["RPROXY_SOCKS5_PORT"]);
                if self.corrupt == "host-port" {
                    model["services"]["rproxy"]["ports"][0]["published"] = json!("1");
                }
                if https {
                    assert!(std::path::Path::new(&request.env["RPROXY_CERTS_DIR"]).is_dir());
                    model["services"]["rproxy"]["ports"].as_array_mut().unwrap().push(json!({"host_ip":"127.0.0.1","target":9443,"published":request.env["RPROXY_HTTPS_PORT"]}));
                    model["services"]["rproxy"]["volumes"].as_array_mut().unwrap().push(json!({"type":"bind","target":"/etc/rproxy/certs","source":request.env["RPROXY_CERTS_DIR"],"read_only": self.corrupt != "https-bind", "bind":{"create_host_path":false}}));
                }
                model.to_string()
            } else {
                String::new()
            };
            if quiet {
                assert_eq!(request.env["RPROXY_ADMIN_TOKEN"], "");
            }
            Ok(Output {
                success: render || (cleanup && self.corrupt != "cleanup") || logs,
                status: "exit code: 19".into(),
                stdout,
                stderr: "intentional stop before startup".into(),
            })
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    for corrupt in ["", "host-port", "https-bind", "named-volume", "cleanup"] {
        let mut runner = RenderEngine {
            requests: vec![],
            corrupt,
        };
        let error = run_with(
            root,
            Options {
                engine: "podman".into(),
                skip_build: true,
                image_prefix: Some("prebuilt".into()),
            },
            &mut runner,
        )
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        if corrupt.is_empty() || corrupt == "cleanup" {
            assert!(diagnostic.contains("exit code: 19"));
            assert_eq!(diagnostic.contains("cleanup failed"), corrupt == "cleanup");
            let cleanup = runner.requests.last().unwrap();
            assert_eq!(
                &cleanup.args[cleanup.args.len() - 3..],
                ["down", "--volumes", "--remove-orphans"]
            );
            assert!(
                runner
                    .requests
                    .iter()
                    .any(|r| r.args.iter().any(|arg| arg.ends_with("compose.https.yaml"))),
                "HTTPS override was not rendered"
            );
            assert!(
                runner
                    .requests
                    .iter()
                    .any(|r| r.args.iter().any(|arg| arg == "--quiet")),
                "missing root token was not rejected"
            );
        } else {
            assert!(
                diagnostic.contains(if corrupt == "host-port" {
                    "published"
                } else if corrupt == "https-bind" {
                    "certificate"
                } else {
                    "owned"
                }),
                "{diagnostic}"
            );
            assert!(!runner.requests.iter().any(|r| r
                .args
                .iter()
                .any(|arg| arg == "run" || arg == "up" || arg == "down")));
        }
    }
}

#[test]
fn rendered_smoke_requires_only_one_production_service_and_readonly_existing_config_binds() {
    validate_rendered(
        &rendered(),
        "/temporary/rproxy.toml",
        "/temporary/providers.env",
    )
    .unwrap();
    for change in [
        "worker",
        "secret",
        "writable",
        "create",
        "missing",
        "path",
        "fixture",
        "fixture-port",
        "admin-port",
        "public-port",
        "external-volume",
        "named-volume",
        "external-network",
        "named-network",
        "host-network",
        "container-name",
        "extra-bind",
        "env-file",
        "fixture-env",
        "fixture-target",
        "volume-driver",
        "network-driver",
        "fixture-volume",
        "privileged",
    ] {
        let mut config = rendered();
        match change {
            "worker" => {
                config["services"]["collector"] = json!({});
            }
            "secret" => {
                config["services"]["rproxy"]["environment"]["RPROXY_WORKER_TOKEN"] =
                    json!("forbidden");
            }
            "writable" => {
                config["services"]["rproxy"]["volumes"][0]["read_only"] = json!(false);
            }
            "create" => {
                config["services"]["rproxy"]["volumes"][0]["bind"]["create_host_path"] =
                    json!(true);
            }
            "missing" => {
                config["services"]["rproxy"]["volumes"]
                    .as_array_mut()
                    .unwrap()
                    .remove(1);
            }
            "path" => {
                config["services"]["rproxy"]["volumes"][0]["source"] = json!("/other/config");
            }
            "fixture" => {
                config["services"]["fixture"]["entrypoint"] = json!(["python", "fixture.py"]);
            }
            "fixture-port" => {
                config["services"]["fixture"]["ports"] =
                    json!([{"target":18080,"published":"18080"}])
            }
            "admin-port" => config["services"]["rproxy"]["ports"][0]["target"] = json!(9100),
            "public-port" => config["services"]["rproxy"]["ports"][0]["host_ip"] = json!("0.0.0.0"),
            "external-volume" => config["volumes"]["rproxy-data"]["external"] = json!(true),
            "named-volume" => config["volumes"]["rproxy-data"]["name"] = json!("real-user-data"),
            "external-network" => config["networks"]["default"]["external"] = json!(true),
            "named-network" => config["networks"]["default"]["name"] = json!("real-user-network"),
            "host-network" => config["services"]["rproxy"]["network_mode"] = json!("host"),
            "container-name" => {
                config["services"]["fixture"]["container_name"] = json!("existing-service")
            }
            "extra-bind" => config["services"]["rproxy"]["volumes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"bind","source":"/real-data","target":"/other"})),
            "env-file" => config["services"]["rproxy"]["env_file"] = json!(["providers.env"]),
            "fixture-env" => {
                config["services"]["fixture"]["environment"] =
                    json!({"RPROXY_ADMIN_TOKEN":"secret"})
            }
            "fixture-target" => config["services"]["fixture"]["build"]["target"] = json!("rproxy"),
            "volume-driver" => {
                config["volumes"]["rproxy-data"]["driver_opts"] =
                    json!({"device":"/real-data","o":"bind","type":"none"})
            }
            "network-driver" => config["networks"]["default"]["driver"] = json!("host"),
            "fixture-volume" => {
                config["services"]["fixture"]["volumes"] =
                    json!([{"source":"real-data","target":"/other"}])
            }
            "privileged" => config["services"]["rproxy"]["privileged"] = json!(true),
            _ => unreachable!(),
        }
        assert!(
            validate_rendered(
                &config,
                "/temporary/rproxy.toml",
                "/temporary/providers.env"
            )
            .is_err(),
            "accepted {change}"
        );
    }
}
