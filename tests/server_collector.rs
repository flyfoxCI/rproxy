use rproxy::{
    auth::Authenticator,
    config::Config,
    managed_types::SourceInput,
    routing::Router,
    server::{Server, State},
    store::Store,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, net::TcpListener};

#[tokio::test]
async fn server_owns_collection_without_management_listener_and_cancels_it_on_shutdown() {
    let supplier = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = supplier.local_addr().unwrap();
    let mut config = Config {
        database: ":memory:".into(),
        http_bind: Some("127.0.0.1:0".parse().unwrap()),
        socks5_bind: None,
        ..Config::default()
    };
    config.admin.enabled = false;
    config.probe.enabled = false;
    config.collector.poll_seconds = 1;
    let store = Store::open(&config).await.unwrap();
    store
        .create_source(SourceInput {
            name: "internal-supplier".into(),
            kind: "api".into(),
            enabled: true,
            config: json!({
                "url": format!("http://{address}/proxies"),
                "protocol": "http",
                "timeout_seconds": 30,
                "attempts": 1,
                "response": {"format": "text"}
            }),
        })
        .await
        .unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let server = Server::bind(state).await.unwrap();
    assert!(server.admin_address().is_none());
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let running = tokio::spawn(server.run(async {
        let _ = shutdown.await;
    }));
    let accepted = tokio::time::timeout(Duration::from_secs(5), supplier.accept()).await;
    // Signal even on failure, so a failed assertion leaves no daemon task behind.
    if accepted.is_err() {
        let _ = stop.send(());
        let _ = running.await;
        panic!("server did not start its internal collector");
    }
    let (mut request, _) = accepted.unwrap().unwrap();
    let header = rproxy::upstream::read_http_header(&mut request)
        .await
        .unwrap();
    assert!(header.starts_with(b"GET /proxies HTTP/1.1\r\n"));
    // Keep the response pending: shutdown must cancel the owned network request,
    // not merely stop polling while a detached collector continues fetching.
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut remaining = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), request.read_to_end(&mut remaining))
        .await
        .expect("supplier request survived server shutdown");
    assert!(closed.is_ok() || closed.unwrap_err().kind() == std::io::ErrorKind::ConnectionReset);
}
