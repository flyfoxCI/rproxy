use rproxy::{
    auth::Authenticator,
    config::Config,
    http::serve_http,
    routing::{Capability, Protocol, Proxy, Router, Strategy},
    server::State,
    store::Store,
};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

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
async fn fixture() -> (Config, Store) {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    (config, store)
}

#[tokio::test]
async fn failed_reservation_keeps_old_stick_and_does_not_create_empty_session() {
    let (config, store) = fixture().await;
    let original = proxy(12345, "1.2.3.4");
    store.save_proxy(&original).await.unwrap();
    let router = Router::new(config.clone(), store.clone());
    let mut identity = Authenticator::new(&config).anonymous().unwrap();
    identity.strategy = Strategy::Stick;
    let reservation = router
        .reserve(&identity, Capability::Forward)
        .await
        .unwrap();
    reservation.release().await.unwrap();
    let mut stale = original.clone();
    stale.verified_at = 1;
    store.save_proxy(&stale).await.unwrap();
    assert!(router
        .reserve(&identity, Capability::Forward)
        .await
        .is_err());
    assert_eq!(store.status().await.unwrap().sessions, 1);
    identity.session = "unallocated".into();
    assert!(router
        .reserve(&identity, Capability::Forward)
        .await
        .is_err());
    assert_eq!(store.status().await.unwrap().sessions, 1);
    store.save_proxy(&original).await.unwrap();
    let next = router
        .reserve(
            &Authenticator::new(&config)
                .anonymous()
                .map(|mut id| {
                    id.strategy = Strategy::Stick;
                    id
                })
                .unwrap(),
            Capability::Forward,
        )
        .await
        .unwrap();
    assert_eq!(next.proxy.id, original.id);
    next.release().await.unwrap();
}

#[tokio::test]
async fn another_business_cooldown_cannot_silently_rebind_a_healthy_session() {
    let (config, store) = fixture().await;
    store.save_proxy(&proxy(12345, "1.2.3.4")).await.unwrap();
    let router = Router::new(config.clone(), store.clone());
    let mut first = Authenticator::new(&config).anonymous().unwrap();
    first.strategy = Strategy::Stick;
    let original = router.reserve(&first, Capability::Forward).await.unwrap();
    let mut other = first.clone();
    other.business = "other".into();
    let failure = router.reserve(&other, Capability::Forward).await.unwrap();
    router.fail(&failure, "isolated failure").await.unwrap();
    failure.release().await.unwrap();
    store.save_proxy(&proxy(12346, "2.3.4.5")).await.unwrap();
    assert!(
        router.reserve(&first, Capability::Forward).await.is_err(),
        "other business failure changed a healthy binding"
    );
    original.release().await.unwrap();
}

#[tokio::test]
async fn http_keepalive_does_not_switch_egress_after_first_request() {
    let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_port = first.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = first.accept().await.unwrap();
        drop(first);
        rproxy::upstream::read_http_header(&mut stream)
            .await
            .unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });
    let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second_port = second.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = second.accept().await.unwrap();
            tokio::spawn(async move {
                rproxy::upstream::read_http_header(&mut stream)
                    .await
                    .unwrap();
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .unwrap();
            });
        }
    });
    let (mut config, store) = fixture().await;
    config.auth.default_strategy = Strategy::Stick;
    store
        .save_proxy(&proxy(first_port, "1.2.3.4"))
        .await
        .unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store.clone()),
        config,
    });
    let ingress = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = ingress.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = ingress.accept().await.unwrap();
        let _ = serve_http(stream, state).await;
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n";
    stream.write_all(request).await.unwrap();
    let header = rproxy::upstream::read_http_header(&mut stream)
        .await
        .unwrap();
    assert!(header.starts_with(b"HTTP/1.1 200"));
    let mut body = [0; 2];
    stream.read_exact(&mut body).await.unwrap();
    store
        .save_proxy(&proxy(second_port, "2.3.4.5"))
        .await
        .unwrap();
    stream.write_all(request).await.unwrap();
    let header = rproxy::upstream::read_http_header(&mut stream)
        .await
        .unwrap();
    assert!(
        header.starts_with(b"HTTP/1.1 502"),
        "second request silently changed egress: {}",
        String::from_utf8_lossy(&header)
    );
}
