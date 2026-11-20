use rproxy::{
    auth::Authenticator,
    config::Config,
    routing::{Protocol, Proxy, Router},
    server::{Server, State},
    store::Store,
    upstream::probe_one,
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn setup() -> (Config, Store) {
    let config = Config {
        database: ":memory:".into(),
        admin: rproxy::config::AdminConfig {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    (config, store)
}

#[tokio::test]
async fn authentication_ttl_is_fixed_and_credential_version_revalidates() {
    let (mut config, store) = setup().await;
    config.auth =
        Config::parse("[[auth.users]]\nname='alice'\npassword='secret'\nbusinesses=['shop']")
            .unwrap()
            .auth;
    let identity = Authenticator::new(&config)
        .verify("v1|alice|shop|a|stick", "secret")
        .unwrap();
    assert!(store.authenticate(&identity, 1).await.unwrap());
    assert!(!store.authenticate(&identity, 1).await.unwrap());
    tokio::time::sleep(Duration::from_millis(1150)).await;
    assert!(store.authenticate(&identity, 1).await.unwrap());
    let mut updated = identity;
    updated.credential_version = 2;
    assert!(store.authenticate(&updated, 1).await.unwrap());
    assert!(!store.authenticate(&updated, 1).await.unwrap());
}

#[tokio::test]
async fn probe_identifies_actual_exit_and_rejects_invalid_or_failed_responses() {
    for (status, body, works) in [
        (200, "{\"ip\":\"2.3.4.5\"}", true),
        (200, "not an ip", false),
        (503, "2.3.4.5", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let header = rproxy::upstream::read_http_header(&mut stream)
                        .await
                        .unwrap();
                    if header.starts_with(b"CONNECT ") {
                        stream
                            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                            .await
                            .unwrap();
                        return;
                    }
                    stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                });
            }
        });
        let mut config = Config::default();
        config.probe.url = "http://exit.test/ip".into();
        let result = probe_one(&config, Protocol::Http, address).await;
        assert_eq!(result.is_ok(), works);
        if let Ok(proxy) = result {
            assert_eq!(proxy.egress.to_string(), "2.3.4.5");
            assert!(!proxy.tunnel);
        }
    }
}

#[tokio::test]
async fn server_binds_real_listener_and_shuts_down_without_erasing_stick() {
    let (mut config, store) = setup().await;
    config.http_bind = Some("127.0.0.1:0".parse().unwrap());
    config.socks5_bind = None;
    config.probe.enabled = false;
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let server = Server::bind(state).await.unwrap();
    let address = server.http_address().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();
    let mut response = [0; 1024];
    let size = stream.read(&mut response).await.unwrap();
    assert!(String::from_utf8_lossy(&response[..size]).starts_with("HTTP/1.1 502"));
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn invalid_or_unstable_metadata_cannot_enter_selection() {
    let (config, store) = setup().await;
    store
        .save_proxy(&Proxy {
            id: "http:127.0.0.1:12345".into(),
            protocol: Protocol::Http,
            address: "127.0.0.1:12345".parse().unwrap(),
            egress: "2.3.4.5".parse().unwrap(),
            tunnel: true,
            stable: false,
            verified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        })
        .await
        .unwrap();
    let router = Router::new(config.clone(), store);
    let id = Authenticator::new(&config).anonymous().unwrap();
    assert!(router
        .reserve(&id, rproxy::routing::Capability::Tunnel)
        .await
        .is_err());
}
