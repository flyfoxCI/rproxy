use rproxy::{
    collector,
    config::{CollectorConfig, Config},
    managed_types::{SourceChange, SourceInput},
    store::Store,
};
use serde_json::json;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::watch;

struct FakeHttp {
    address: String,
    body: Arc<Mutex<Vec<u8>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl FakeHttp {
    fn start(body: impl Into<Vec<u8>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let body = Arc::new(Mutex::new(body.into()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_body = body.clone();
        let thread_requests = requests.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => handle_request(stream, &thread_body, &thread_requests),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            body,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn set_body(&self, body: impl Into<Vec<u8>>) {
        *self.body.lock().unwrap() = body.into();
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address.strip_prefix("http://").unwrap());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn handle_request(
    mut stream: TcpStream,
    body: &Arc<Mutex<Vec<u8>>>,
    requests: &Arc<Mutex<Vec<Request>>>,
) {
    // Accepted sockets inherit nonblocking mode on Windows.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let count = match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => count,
            Err(_) => return,
        };
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() > 64 * 1024 {
            return;
        }
    };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let mut lines = header_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return;
    };
    let mut request_parts = request_line.split_whitespace();
    let Some(method) = request_parts.next().map(str::to_owned) else {
        return;
    };
    let Some(target) = request_parts.next().map(str::to_owned) else {
        return;
    };
    let mut headers = HashMap::new();
    let mut content_length = 0;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.to_ascii_lowercase();
        let value = value.trim().to_owned();
        if key == "content-length" {
            content_length = value.parse().unwrap_or(0);
        }
        headers.insert(key, value);
    }
    while bytes.len() - header_end < content_length {
        let Ok(count) = stream.read(&mut chunk) else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    requests.lock().unwrap().push(Request {
        method,
        target,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    });
    let body = body.lock().unwrap().clone();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&body);
}

fn collector_config(directory: &Path, env_file: Option<PathBuf>) -> CollectorConfig {
    CollectorConfig {
        enabled: true,
        concurrency: 4,
        poll_seconds: 1,
        env_file,
        base_directory: directory.to_path_buf(),
    }
}

async fn open_store(directory: &Path) -> Store {
    let config = Config {
        database: directory.join("state.sqlite3"),
        ..Config::default()
    };
    Store::open(&config).await.unwrap()
}

async fn add_source(store: &Store, name: &str, config: serde_json::Value) {
    store
        .create_source(SourceInput {
            name: name.to_owned(),
            kind: "api".into(),
            enabled: true,
            config,
        })
        .await
        .unwrap();
}

async fn wait_for<F, Future>(mut condition: F)
where
    F: FnMut() -> Future,
    Future: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition did not become true");
}

async fn run_until<F, Future>(store: Store, config: CollectorConfig, condition: F)
where
    F: FnMut() -> Future,
    Future: std::future::Future<Output = bool>,
{
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(collector::run(config, store, shutdown_rx));
    wait_for(condition).await;
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn maps_text_addresses_deduplicates_and_submits_typed_results() {
    let directory = TempDir::new().unwrap();
    let server = FakeHttp::start(b"1.2.3.4:80\n1.2.3.4:80,[::1]:1080".to_vec());
    let store = open_store(directory.path()).await;
    add_source(
        &store,
        "text",
        json!({"url": format!("{}/extract", server.address)}),
    )
    .await;

    let observed = store.clone();
    let for_wait = observed.clone();
    run_until(store, collector_config(directory.path(), None), move || {
        let observed = for_wait.clone();
        async move { observed.candidates().await.unwrap().len() == 2 }
    })
    .await;

    let candidates = observed.candidates().await.unwrap();
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.address.to_string())
            .collect::<Vec<_>>(),
        vec!["1.2.3.4:80", "[::1]:1080"]
    );
}

#[tokio::test]
async fn reads_only_explicit_provider_env_file_and_never_process_environment() {
    let directory = TempDir::new().unwrap();
    let server = FakeHttp::start(b"1.2.3.4:80".to_vec());
    let env_path = directory.path().join("providers.env");
    std::fs::write(&env_path, "# provider only\nPROVIDER_HEADER=from-file\n").unwrap();
    std::env::set_var("COLLECTOR_ROOT_SECRET", "must-not-be-used");
    let store = open_store(directory.path()).await;
    add_source(
        &store,
        "file-env",
        json!({
            "url": format!("{}/extract", server.address),
            "headers": {"X-Provider": "${PROVIDER_HEADER}"}
        }),
    )
    .await;
    add_source(
        &store,
        "root-env-isolation",
        json!({
            "url": format!("{}/extract", server.address),
            "headers": {"X-Root": "${COLLECTOR_ROOT_SECRET}"}
        }),
    )
    .await;

    let observed = store.clone();
    let for_wait = observed.clone();
    run_until(
        store,
        collector_config(directory.path(), Some(env_path)),
        move || {
            let observed = for_wait.clone();
            async move {
                observed
                    .sources()
                    .await
                    .unwrap()
                    .iter()
                    .all(|source| source.last_finished_generation > 0)
            }
        },
    )
    .await;
    std::env::remove_var("COLLECTOR_ROOT_SECRET");

    let requests = server.requests();
    let file_request = requests
        .iter()
        .find(|request| request.headers.contains_key("x-provider"))
        .unwrap();
    assert_eq!(
        file_request.headers.get("x-provider"),
        Some(&"from-file".into())
    );
    let sources = observed.sources().await.unwrap();
    let root = sources
        .iter()
        .find(|source| source.name == "root-env-isolation")
        .unwrap();
    assert_eq!(root.last_error.as_deref(), Some("env-missing"));
}

#[tokio::test]
async fn maps_json_items_with_numeric_paths_and_posts_json_body() {
    let directory = TempDir::new().unwrap();
    let server = FakeHttp::start(
        br#"{"meta":[{"ok":true}],"data":[{"items":[{"endpoint":["1.2.3.4",80]},{"endpoint":["::1","1080"]}]}]}"#.to_vec(),
    );
    let store = open_store(directory.path()).await;
    add_source(
        &store,
        "json",
        json!({
            "url": format!("{}/list", server.address),
            "method": "POST",
            "json": {"kind": "local"},
            "response": {
                "format": "json",
                "items_path": "data.0.items",
                "ip_path": "endpoint.0",
                "port_path": "endpoint.1",
                "success_path": "meta.0.ok",
                "success_value": true
            }
        }),
    )
    .await;

    let observed = store.clone();
    let for_wait = observed.clone();
    run_until(store, collector_config(directory.path(), None), move || {
        let observed = for_wait.clone();
        async move { observed.candidates().await.unwrap().len() == 2 }
    })
    .await;

    let requests = server.requests();
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/list");
    assert_eq!(
        requests[0].headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap(),
        json!({"kind":"local"})
    );
}

#[tokio::test]
async fn batches_at_256_and_marks_only_the_last_result_complete() {
    let directory = TempDir::new().unwrap();
    let body = (1..=513)
        .map(|port| format!("1.2.3.4:{port}"))
        .collect::<Vec<_>>()
        .join("\n");
    let server = FakeHttp::start(body);
    let store = open_store(directory.path()).await;
    add_source(
        &store,
        "many",
        json!({"url": format!("{}/many", server.address)}),
    )
    .await;
    let observed = store.clone();
    let for_wait = observed.clone();
    run_until(store, collector_config(directory.path(), None), move || {
        let observed = for_wait.clone();
        async move { observed.candidates().await.unwrap().len() == 513 }
    })
    .await;
    assert_eq!(observed.candidates().await.unwrap().len(), 513);
    let source = observed.source("many".into()).await.unwrap().unwrap();
    assert_eq!(source.last_finished_generation, source.run_generation);
    assert_eq!(source.last_error, None);
}

#[tokio::test]
async fn revision_change_prompts_new_work_and_shutdown_joins_it() {
    let directory = TempDir::new().unwrap();
    let server = FakeHttp::start(b"1.2.3.4:80".to_vec());
    let store = open_store(directory.path()).await;
    add_source(
        &store,
        "dynamic",
        json!({"url": format!("{}/first", server.address)}),
    )
    .await;
    let source = store.source("dynamic".into()).await.unwrap().unwrap();
    let observed = store.clone();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(collector::run(
        collector_config(directory.path(), None),
        store,
        shutdown_rx,
    ));
    wait_for(|| {
        let observed = observed.clone();
        async move { observed.sources().await.unwrap()[0].last_finished_generation > 0 }
    })
    .await;
    server.set_body(b"2.3.4.5:81".to_vec());
    observed
        .update_source(
            "dynamic".into(),
            SourceChange {
                config: Some(json!({"url": format!("{}/second", server.address)})),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    wait_for(|| {
        let observed = observed.clone();
        async move {
            observed
                .source("dynamic".into())
                .await
                .unwrap()
                .unwrap()
                .last_finished_generation
                > source.last_finished_generation.max(source.run_generation)
        }
    })
    .await;
    assert!(server
        .requests()
        .iter()
        .any(|request| request.target == "/second"));
    assert!(observed
        .candidates()
        .await
        .unwrap()
        .iter()
        .any(|candidate| candidate.address.to_string() == "2.3.4.5:81"));
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}
