use rproxy::{config::Config, managed_types::SourceInput, store::Store};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;

#[test]
fn internal_collection_has_bounded_explicit_configuration() {
    let config = Config::parse("[collector]\nenabled=true\nconcurrency=2\npoll_seconds=1").unwrap();
    assert!(config.collector.enabled);
    assert_eq!(config.collector.concurrency, 2);
    assert!(config.collector.env_file.is_none());
    for text in [
        "[collector]\nconcurrency=0",
        "[collector]\nconcurrency=65",
        "[collector]\npoll_seconds=0",
        "[collector]\npoll_seconds=301",
    ] {
        assert!(
            Config::parse(text).is_err(),
            "invalid collector bounds accepted"
        );
    }
}

#[test]
fn provider_file_is_relative_to_server_config_not_process_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rproxy.toml");
    std::fs::write(&path, "[collector]\nenv_file='providers.env'").unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(
        config.collector.env_file.as_deref(),
        Some(directory.path().join("providers.env").as_path())
    );
    assert_eq!(config.collector.base_directory, directory.path());
}

#[tokio::test]
async fn relative_config_path_loads_only_its_own_provider_file() {
    let cwd = std::env::current_dir().unwrap();
    let directory = tempfile::tempdir_in(&cwd).unwrap();
    let config_directory = directory.path().join("config");
    std::fs::create_dir(&config_directory).unwrap();
    let path = config_directory.join("rproxy.toml");
    let relative = path.strip_prefix(&cwd).unwrap();
    std::fs::write(
        &path,
        "database=':memory:'\n[collector]\nenv_file='providers.env'\npoll_seconds=1\n",
    )
    .unwrap();
    std::fs::write(
        config_directory.join("providers.env"),
        "SUPPLIER_HEADER=intended-file\n",
    )
    .unwrap();
    // Make the former double-prefix location readable too: this must not silently
    // select a different set of supplier variables instead of the intended file.
    let wrong = config_directory.join(relative.parent().unwrap());
    std::fs::create_dir_all(&wrong).unwrap();
    std::fs::write(wrong.join("providers.env"), "SUPPLIER_HEADER=wrong-file\n").unwrap();
    let config = Config::load(relative).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let store = Store::open(&config).await.unwrap();
    store
        .create_source(SourceInput {
            name: "relative-path".into(),
            kind: "api".into(),
            enabled: true,
            config: json!({
                "url": format!("http://{}/supplier", listener.local_addr().unwrap()),
                "headers": {"X-Supplier": "${SUPPLIER_HEADER}"},
                "attempts": 1
            }),
        })
        .await
        .unwrap();
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let running = tokio::spawn(rproxy::collector::run(config.collector, store, shutdown));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        let (mut stream, _) = listener.accept().await.unwrap();
        rproxy::upstream::read_http_header(&mut stream)
            .await
            .unwrap()
    })
    .await;
    stop.send_replace(true);
    running.await.unwrap().unwrap();
    let header = String::from_utf8(request.expect("collector did not reach supplier")).unwrap();
    assert!(
        header
            .to_ascii_lowercase()
            .contains("x-supplier: intended-file\r\n"),
        "relative config selected an unintended provider-variable file"
    );
    assert!(!header.contains("wrong-file"));
}
