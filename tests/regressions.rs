use rproxy::{
    auth::Authenticator,
    config::Config,
    routing::{Capability, Protocol, Proxy, Router, Strategy},
    server::{Server, State},
    store::Store,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn fixture() -> (Config, Store) {
    let mut config = Config::default();
    config.probe.enabled = false;
    config.database = ":memory:".into();
    config.admin.enabled = false;
    config.auth.mode = rproxy::config::AuthMode::Anonymous;
    let store = Store::open(&config).await.unwrap();
    (config, store)
}
fn proxy(port: u16, ip: &str) -> Proxy {
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    Proxy {
        id: format!("http:{address}"),
        protocol: Protocol::Http,
        address,
        egress: ip.parse().unwrap(),
        tunnel: true,
        stable: true,
        verified_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    }
}

#[tokio::test]
async fn mapped_ipv6_and_ipv4_are_one_exclusive_egress() {
    let (config, store) = fixture().await;
    store.save_proxy(&proxy(12345, "1.2.3.4")).await.unwrap();
    store
        .save_proxy(&proxy(12346, "::ffff:1.2.3.4"))
        .await
        .unwrap();
    let router = Router::new(config.clone(), store);
    let mut first = Authenticator::new(&config).anonymous().unwrap();
    first.strategy = Strategy::Stick;
    first.session = "a".into();
    let _a = router.reserve(&first, Capability::Tunnel).await.unwrap();
    let mut second = first;
    second.session = "b".into();
    assert!(router.reserve(&second, Capability::Tunnel).await.is_err());
}

#[tokio::test]
async fn server_shutdown_closes_upgraded_tunnels_not_only_listener_tasks() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        rproxy::upstream::read_http_header(&mut stream)
            .await
            .unwrap();
        stream
            .write_all(b"HTTP/1.1 200 Established\r\n\r\n")
            .await
            .unwrap();
        let (mut read, mut write) = stream.split();
        let _ = tokio::io::copy(&mut read, &mut write).await;
    });
    let (mut config, store) = fixture().await;
    config.http_bind = Some("127.0.0.1:0".parse().unwrap());
    config.socks5_bind = None;
    config.auth.default_strategy = Strategy::Stick;
    store.save_proxy(&proxy(port, "1.2.3.4")).await.unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let server = Server::bind(state).await.unwrap();
    let address = server.http_address().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await
        .unwrap();
    let response = rproxy::upstream::read_http_header(&mut client)
        .await
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let mut data = [0; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut data)).await;
    assert!(read.is_ok(), "detached tunnel survived server shutdown");
    assert!(matches!(read.unwrap(), Ok(0) | Err(_)));
}

#[tokio::test]
async fn router_shutdown_ends_guarded_work_without_distributed_lease_timer() {
    let (config, store) = fixture().await;
    store.save_proxy(&proxy(12345, "1.2.3.4")).await.unwrap();
    let router = Router::new(config.clone(), store);
    let identity = Authenticator::new(&config).anonymous().unwrap();
    let reservation = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    router.shutdown();
    assert!(reservation
        .guard(std::future::pending::<()>())
        .await
        .is_err());
}
