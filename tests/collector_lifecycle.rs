use rproxy::{
    collector,
    config::{CollectorConfig, Config},
    managed_types::{SourceChange, SourceInput},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::{watch, Semaphore},
    task::{JoinHandle, JoinSet},
};

#[derive(Clone, Debug)]
struct Request {
    target: String,
    headers: HashMap<String, String>,
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    drip: Option<Duration>,
    gate: Option<Arc<Semaphore>>,
}

impl Reply {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            drip: None,
            gate: None,
        }
    }
}

struct Http {
    url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Http {
    async fn start(reply: Reply) -> Self {
        Self::start_transport(reply, None).await
    }

    async fn start_transport(reply: Reply, tls: Option<tokio_rustls::TlsAcceptor>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let scheme = if tls.is_some() { "https" } else { "http" };
        let url = format!(
            "{scheme}://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let reply = reply.clone();
                        let recorded = recorded.clone();
                        let tls = tls.clone();
                        connections.spawn(async move {
                            if let Some(tls) = tls {
                                if let Ok(stream) = tls.accept(stream).await {
                                    respond(stream, reply, recorded).await;
                                }
                            } else {
                                respond(stream, reply, recorded).await;
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    reply: Reply,
    requests: Arc<Mutex<Vec<Request>>>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let end = loop {
        let Ok(count) = stream.read(&mut chunk).await else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if bytes.len() > 64 * 1024 {
            return;
        }
    };
    let text = String::from_utf8_lossy(&bytes[..end]);
    let mut lines = text.split("\r\n");
    let target = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    requests.lock().unwrap().push(Request { target, headers });
    if let Some(gate) = reply.gate {
        let Ok(permit) = gate.acquire().await else {
            return;
        };
        permit.forget();
    }
    let mut head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (key, value) in reply.headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if let Some(delay) = reply.drip {
        for byte in reply.body {
            tokio::time::sleep(delay).await;
            if stream.write_all(&[byte]).await.is_err() {
                return;
            }
        }
    } else {
        let _ = stream.write_all(&reply.body).await;
    }
}

async fn store(directory: &Path) -> Store {
    let config = Config {
        database: directory.join("state.sqlite3"),
        ..Config::default()
    };
    Store::open(&config).await.unwrap()
}

async fn source(store: &Store, name: &str, mut fields: Value) {
    fields
        .as_object_mut()
        .unwrap()
        .entry("attempts")
        .or_insert(json!(1));
    store
        .create_source(SourceInput {
            name: name.into(),
            kind: "api".into(),
            enabled: true,
            config: fields,
        })
        .await
        .unwrap();
}

struct Running {
    stop: watch::Sender<bool>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn start(store: Store, directory: &Path, concurrency: usize, poll_seconds: u64) -> Self {
        let (stop, shutdown) = watch::channel(false);
        let config = CollectorConfig {
            enabled: true,
            concurrency,
            poll_seconds,
            env_file: None,
            base_directory: directory.to_path_buf(),
        };
        let task = tokio::spawn(collector::run(config, store, shutdown));
        Self { stop, task }
    }

    async fn stop(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_millis(500), self.task)
            .await
            .expect("collector did not join its owned work")
            .unwrap()
            .unwrap();
    }
}

async fn until<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if condition().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("collector condition timed out");
}

async fn finished(store: &Store, name: &str) -> bool {
    let source = store.source(name.into()).await.unwrap().unwrap();
    source.last_finished_generation == source.run_generation
}

#[tokio::test]
async fn source_interval_does_not_wait_for_the_plan_poll_interval() {
    let directory = TempDir::new().unwrap();
    let http = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let store = store(directory.path()).await;
    source(
        &store,
        "periodic",
        json!({"url":http.url, "interval_seconds":1}),
    )
    .await;
    let running = Running::start(store.clone(), directory.path(), 1, 300);
    until(|| async { finished(&store, "periodic").await }).await;
    let result = tokio::time::timeout(Duration::from_millis(1800), async {
        until(|| async { http.requests().len() >= 2 }).await;
    })
    .await;
    running.stop().await;
    assert!(
        result.is_ok(),
        "one-second acquisition waited for a 300-second plan poll"
    );
}

#[tokio::test]
async fn cached_queued_sources_are_not_fetched_after_pause_or_removal() {
    let directory = TempDir::new().unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let mut reply = Reply::ok(b"1.2.3.4:80".to_vec());
    reply.gate = Some(gate.clone());
    let http = Http::start(reply).await;
    let store = store(directory.path()).await;
    for name in ["first", "second", "third"] {
        source(&store, name, json!({"url":format!("{}/{name}", http.url)})).await;
    }
    let running = Running::start(store.clone(), directory.path(), 1, 300);
    until(|| async { http.requests().len() == 1 }).await;
    let current = http.requests()[0].target.trim_start_matches('/').to_owned();
    let queued: Vec<_> = ["first", "second", "third"]
        .into_iter()
        .filter(|s| *s != current)
        .collect();
    store
        .update_source(
            queued[0].into(),
            SourceChange {
                enabled: Some(false),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    store.remove_source(queued[1].into()).await.unwrap();
    gate.add_permits(3);
    until(|| async { finished(&store, &current).await }).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    running.stop().await;
    assert_eq!(
        http.requests().len(),
        1,
        "cached paused/deleted work reached the network"
    );
}

#[tokio::test]
async fn idle_collector_observes_add_edit_manual_run_pause_resume_and_removal() {
    let directory = TempDir::new().unwrap();
    let http = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let store = store(directory.path()).await;
    let running = Running::start(store.clone(), directory.path(), 2, 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(http.requests().is_empty());
    source(
        &store,
        "dynamic",
        json!({"url":format!("{}/first",http.url), "interval_seconds":60}),
    )
    .await;
    until(|| async { finished(&store, "dynamic").await }).await;
    store
        .update_source(
            "dynamic".into(),
            SourceChange {
                config: Some(json!({"url":format!("{}/edited",http.url), "attempts":1})),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    until(|| async { finished(&store, "dynamic").await }).await;
    assert_eq!(http.requests().last().unwrap().target, "/edited");
    store.run_source("dynamic".into()).await.unwrap();
    until(|| async { finished(&store, "dynamic").await }).await;
    assert_eq!(http.requests().len(), 3);
    store
        .update_source(
            "dynamic".into(),
            SourceChange {
                enabled: Some(false),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(http.requests().len(), 3);
    store
        .update_source(
            "dynamic".into(),
            SourceChange {
                enabled: Some(true),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    until(|| async { finished(&store, "dynamic").await }).await;
    assert_eq!(http.requests().len(), 4);
    store.remove_source("dynamic".into()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    running.stop().await;
    assert_eq!(http.requests().len(), 4);
}

#[tokio::test]
async fn changed_inflight_work_is_cancelled_and_cannot_complete_the_new_generation() {
    let directory = TempDir::new().unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let mut reply = Reply::ok(b"1.2.3.4:80".to_vec());
    reply.gate = Some(gate.clone());
    let old = Http::start(reply).await;
    let new = Http::start(Reply::ok(b"2.3.4.5:81".to_vec())).await;
    let store = store(directory.path()).await;
    source(&store, "change", json!({"url":old.url})).await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { old.requests().len() == 1 }).await;
    store
        .update_source(
            "change".into(),
            SourceChange {
                config: Some(json!({"url":new.url,"attempts":1})),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    until(|| async { finished(&store, "change").await }).await;
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    running.stop().await;
    let candidates = store.candidates().await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].address.to_string(), "2.3.4.5:81");
    assert_eq!(new.requests().len(), 1);
}

#[tokio::test]
async fn bounds_network_jobs_and_cancels_them_on_shutdown() {
    let directory = TempDir::new().unwrap();
    let mut reply = Reply::ok(b"1.2.3.4:80".to_vec());
    reply.gate = Some(Arc::new(Semaphore::new(0)));
    let http = Http::start(reply).await;
    let store = store(directory.path()).await;
    for index in 0..12 {
        source(
            &store,
            &format!("source-{index}"),
            json!({"url":http.url,"timeout_seconds":300}),
        )
        .await;
    }
    let running = Running::start(store.clone(), directory.path(), 2, 1);
    until(|| async { http.requests().len() == 2 }).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(http.requests().len(), 2);
    running.stop().await;
    assert!(store.candidates().await.unwrap().is_empty());
    assert!(store
        .sources()
        .await
        .unwrap()
        .iter()
        .all(|s| s.last_finished_generation == 0));
}

#[tokio::test]
async fn paused_inflight_source_does_not_import_or_reschedule() {
    let directory = TempDir::new().unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let mut reply = Reply::ok(b"1.2.3.4:80".to_vec());
    reply.gate = Some(gate.clone());
    let http = Http::start(reply).await;
    let store = store(directory.path()).await;
    source(
        &store,
        "paused",
        json!({"url":http.url,"interval_seconds":1}),
    )
    .await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { !http.requests().is_empty() }).await;
    store
        .update_source(
            "paused".into(),
            SourceChange {
                enabled: Some(false),
                ..SourceChange::default()
            },
        )
        .await
        .unwrap();
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    running.stop().await;
    assert_eq!(http.requests().len(), 1);
    assert!(store.candidates().await.unwrap().is_empty());
    assert_eq!(
        store
            .source("paused".into())
            .await
            .unwrap()
            .unwrap()
            .last_finished_generation,
        0
    );
}

#[tokio::test]
async fn retries_are_bounded_and_bad_sources_do_not_block_good_sources() {
    let directory = TempDir::new().unwrap();
    let mut reply = Reply::ok(b"private-provider-body".to_vec());
    reply.status = 503;
    let failed = Http::start(reply).await;
    let bad_mapping = Http::start(Reply::ok(b"private-provider-body".to_vec())).await;
    let good = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let store = store(directory.path()).await;
    source(
        &store,
        "fetch",
        json!({"url":format!("{}/secret?token=private",failed.url),"attempts":3}),
    )
    .await;
    source(
        &store,
        "mapping",
        json!({"url":bad_mapping.url,"attempts":5}),
    )
    .await;
    source(
        &store,
        "env",
        json!({"url":good.url,"headers":{"X-Secret":"${NO_PROVIDER_VARIABLE}"}}),
    )
    .await;
    source(&store, "good", json!({"url":good.url})).await;
    let started = Instant::now();
    let running = Running::start(store.clone(), directory.path(), 4, 1);
    until(|| async {
        store
            .sources()
            .await
            .unwrap()
            .iter()
            .all(|s| s.last_finished_generation > 0)
    })
    .await;
    running.stop().await;
    assert_eq!(failed.requests().len(), 3);
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "fetch retries skipped their backoff"
    );
    assert_eq!(
        bad_mapping.requests().len(),
        1,
        "mapping failures must not retry"
    );
    assert_eq!(
        good.requests().len(),
        1,
        "missing env must not send a request"
    );
    let errors: HashMap<_, _> = store
        .sources()
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.name, s.last_error))
        .collect();
    assert_eq!(errors["fetch"].as_deref(), Some("fetch-failed"));
    assert_eq!(errors["mapping"].as_deref(), Some("mapping-failed"));
    assert_eq!(errors["env"].as_deref(), Some("env-missing"));
    assert_eq!(errors["good"], None);
    assert_eq!(store.candidates().await.unwrap().len(), 1);
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let directory = TempDir::new().unwrap();
    let target = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let mut reply = Reply::ok(Vec::new());
    reply.status = 302;
    reply.headers.push(("Location".into(), target.url.clone()));
    let redirect = Http::start(reply).await;
    let store = store(directory.path()).await;
    source(&store, "redirect", json!({"url":redirect.url})).await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { finished(&store, "redirect").await }).await;
    running.stop().await;
    assert_eq!(
        store
            .source("redirect".into())
            .await
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("fetch-failed")
    );
    assert!(target.requests().is_empty());
}

#[tokio::test]
async fn slow_drip_body_obeys_total_request_timeout() {
    let directory = TempDir::new().unwrap();
    let mut reply = Reply::ok(b"1.2.3.4:80".to_vec());
    reply.drip = Some(Duration::from_millis(300));
    let http = Http::start(reply).await;
    let store = store(directory.path()).await;
    source(&store, "slow", json!({"url":http.url,"timeout_seconds":1})).await;
    let started = Instant::now();
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { finished(&store, "slow").await }).await;
    running.stop().await;
    assert!(started.elapsed() < Duration::from_millis(1800));
    assert_eq!(
        store
            .source("slow".into())
            .await
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("fetch-failed")
    );
}

#[tokio::test]
async fn response_size_and_address_limits_fail_without_partial_admission() {
    let directory = TempDir::new().unwrap();
    let oversized = Http::start(Reply::ok(vec![b'x'; 1024 * 1024 + 1])).await;
    let many = Http::start(Reply::ok("1.2.3.4:80\n".repeat(4097))).await;
    let bad_utf8 = Http::start(Reply::ok(vec![0xff])).await;
    let store = store(directory.path()).await;
    for (name, url) in [
        ("size", &oversized.url),
        ("many", &many.url),
        ("utf8", &bad_utf8.url),
    ] {
        source(&store, name, json!({"url":url})).await;
    }
    let running = Running::start(store.clone(), directory.path(), 3, 1);
    until(|| async {
        store
            .sources()
            .await
            .unwrap()
            .iter()
            .all(|s| s.last_finished_generation > 0)
    })
    .await;
    running.stop().await;
    assert!(store.candidates().await.unwrap().is_empty());
    for (name, expected) in [
        ("size", "fetch-failed"),
        ("many", "mapping-failed"),
        ("utf8", "mapping-failed"),
    ] {
        assert_eq!(
            store
                .source(name.into())
                .await
                .unwrap()
                .unwrap()
                .last_error
                .as_deref(),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn gzip_is_decoded_before_mapping_and_the_decompressed_body_limit() {
    let directory = TempDir::new().unwrap();
    let mut small = Reply::ok(vec![
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 51, 212, 51, 210, 51, 214, 51, 177, 178, 48, 0, 0, 1,
        169, 14, 237, 10, 0, 0, 0,
    ]);
    small
        .headers
        .push(("Content-Encoding".into(), "gzip".into()));
    let good = Http::start(small).await;
    // A fixed gzip stream of 1,048,577 'x' bytes, only 1,051 bytes on the wire.
    let mut compressed = vec![
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 237, 193, 49, 1, 0, 0, 0, 194, 160, 218, 139, 111, 8,
        95, 160, 0,
    ];
    compressed.resize(1041, 0);
    compressed.extend_from_slice(&[78, 3, 159, 124, 30, 68, 1, 0, 16, 0]);
    let mut large = Reply::ok(compressed);
    large
        .headers
        .push(("Content-Encoding".into(), "gzip".into()));
    let oversized = Http::start(large).await;
    let store = store(directory.path()).await;
    source(&store, "gzip", json!({"url":good.url})).await;
    source(&store, "gzip-large", json!({"url":oversized.url})).await;
    let running = Running::start(store.clone(), directory.path(), 2, 1);
    until(|| async {
        store
            .sources()
            .await
            .unwrap()
            .iter()
            .all(|s| s.last_finished_generation > 0)
    })
    .await;
    running.stop().await;
    assert_eq!(
        store
            .source("gzip".into())
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None
    );
    assert_eq!(
        store.candidates().await.unwrap()[0].address.to_string(),
        "1.2.3.4:80"
    );
    assert_eq!(
        store
            .source("gzip-large".into())
            .await
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("fetch-failed")
    );
}

#[tokio::test]
async fn empty_success_finishes_generation_without_admitting_candidates() {
    let directory = TempDir::new().unwrap();
    let http = Http::start(Reply::ok(Vec::new())).await;
    let store = store(directory.path()).await;
    source(&store, "empty", json!({"url":http.url})).await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { finished(&store, "empty").await }).await;
    running.stop().await;
    assert!(store.candidates().await.unwrap().is_empty());
    assert_eq!(
        store
            .source("empty".into())
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None
    );
}

#[tokio::test]
async fn tls_verifies_by_default_and_supports_relative_ca_or_explicit_insecure() {
    let directory = TempDir::new().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    std::fs::write(directory.path().join("provider-ca.pem"), cert.cert.pem()).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .unwrap();
    let http = Http::start_transport(
        Reply::ok(b"1.2.3.4:80".to_vec()),
        Some(tokio_rustls::TlsAcceptor::from(Arc::new(tls))),
    )
    .await;
    let store = store(directory.path()).await;
    source(&store, "untrusted", json!({"url":http.url})).await;
    source(
        &store,
        "explicit-ca",
        json!({"url":http.url,"ca_certificate":"provider-ca.pem"}),
    )
    .await;
    source(&store, "insecure", json!({"url":http.url,"insecure":true})).await;
    let running = Running::start(store.clone(), directory.path(), 3, 1);
    until(|| async {
        store
            .sources()
            .await
            .unwrap()
            .iter()
            .all(|s| s.last_finished_generation > 0)
    })
    .await;
    running.stop().await;
    assert_eq!(
        store
            .source("untrusted".into())
            .await
            .unwrap()
            .unwrap()
            .last_error
            .as_deref(),
        Some("fetch-failed")
    );
    assert_eq!(
        store
            .source("explicit-ca".into())
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None
    );
    assert_eq!(
        store
            .source("insecure".into())
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None
    );
    assert_eq!(http.requests().len(), 2);
}

#[tokio::test]
async fn ambient_proxy_environment_is_ignored() {
    const CHILD: &str = "RPROXY_COLLECTOR_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = tokio::task::spawn_blocking(|| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ambient_proxy_environment_is_ignored",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("http_proxy", "http://127.0.0.1:1")
                .env("HTTPS_PROXY", "http://127.0.0.1:1")
                .env("https_proxy", "http://127.0.0.1:1")
                .env("ALL_PROXY", "socks5://127.0.0.1:1")
                .env("all_proxy", "socks5://127.0.0.1:1")
                .env("NO_PROXY", "")
                .env("no_proxy", "")
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            result.status.success(),
            "isolated proxy test failed: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        return;
    }
    let directory = TempDir::new().unwrap();
    let http = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let store = store(directory.path()).await;
    source(&store, "direct", json!({"url":http.url})).await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { finished(&store, "direct").await }).await;
    running.stop().await;
    assert_eq!(
        store
            .source("direct".into())
            .await
            .unwrap()
            .unwrap()
            .last_error,
        None
    );
    assert_eq!(http.requests().len(), 1);
}

#[tokio::test]
async fn get_parameters_and_provider_headers_are_sent_without_root_authorization() {
    let directory = TempDir::new().unwrap();
    let http = Http::start(Reply::ok(b"1.2.3.4:80".to_vec())).await;
    let store = store(directory.path()).await;
    source(&store,"get",json!({"url":format!("{}/extract",http.url),"params":{"count":2},"headers":{"X-Provider":"local"}})).await;
    let running = Running::start(store.clone(), directory.path(), 1, 1);
    until(|| async { finished(&store, "get").await }).await;
    running.stop().await;
    let request = http.requests().remove(0);
    assert_eq!(request.target, "/extract?count=2");
    assert_eq!(
        request.headers.get("x-provider").map(String::as_str),
        Some("local")
    );
    assert!(!request.headers.contains_key("authorization"));
}
