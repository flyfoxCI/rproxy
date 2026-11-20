use rproxy::{
    admin, auth::Authenticator, config::Config, routing::Router, server::State, store::Store,
};
use std::sync::Arc;
use tokio::net::TcpListener;

async fn fixture() -> (String, Arc<State>) {
    let config = Config {
        database: ":memory:".into(),
        max_proxies: 1,
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let running = state.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = running.clone();
            tokio::spawn(async move {
                let _ = admin::serve(stream, state, Arc::new("test-admin-token".into())).await;
            });
        }
    });
    (format!("http://{address}"), state)
}

#[tokio::test]
async fn management_requires_bearer_and_candidate_is_not_trusted_as_verified() {
    let (url, state) = fixture().await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{url}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{url}/health"))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{url}/health"))
            .bearer_auth("test-admin-token")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = client
        .post(format!("{url}/v1/proxies"))
        .bearer_auth("test-admin-token")
        .body(r#"{"protocol":"http","addresses":["127.0.0.1:8080"],"source":"test"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert_eq!(state.router.store.status().await.unwrap().candidates, 1);
    assert!(state.router.store.proxies().await.unwrap().is_empty());
    let response = client
        .post(format!("{url}/v1/proxies"))
        .bearer_auth("test-admin-token")
        .body(r#"{"protocol":"http","addresses":["127.0.0.1:8081"],"source":"test"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
}

#[tokio::test]
async fn management_remains_available_when_proxy_connection_limit_is_full() {
    use rproxy::{config::AdminConfig, server::Server};
    use tokio::{io::AsyncWriteExt, net::TcpStream};
    let token_env = format!("RPROXY_TEST_ADMIN_{}", rand::random::<u64>());
    std::env::set_var(&token_env, "test-admin-token");
    let mut config = Config {
        database: ":memory:".into(),
        http_bind: Some("127.0.0.1:0".parse().unwrap()),
        socks5_bind: None,
        admin: AdminConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            token_env: token_env.clone(),
            ..Default::default()
        },
        ..Config::default()
    };
    config.routing.max_connections = 1;
    config.probe.enabled = false;
    let store = Store::open(&config).await.unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let server = Server::bind(state).await.unwrap();
    std::env::remove_var(token_env);
    let proxy_address = server.http_address().unwrap();
    let admin_address = server.admin_address().unwrap();
    let (stop, wait) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = wait.await;
    }));
    let mut held = TcpStream::connect(proxy_address).await.unwrap();
    held.write_all(b"GET ").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{admin_address}/health"))
        .bearer_auth("test-admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn pending_candidates_are_verified_after_restart_with_periodic_probes_disabled() {
    use rproxy::{config::AdminConfig, routing::Protocol, server::Server};
    use tokio::io::AsyncWriteExt;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    let fake = tokio::spawn(async move {
        loop {
            let (mut stream, _) = upstream.accept().await.unwrap();
            tokio::spawn(async move {
                let header = rproxy::upstream::read_http_header(&mut stream)
                    .await
                    .unwrap();
                let response: &[u8] = if header.starts_with(b"CONNECT ") {
                    b"HTTP/1.1 200 Established\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\n192.0.2.1"
                };
                let _ = stream.write_all(response).await;
            });
        }
    });
    let mut config = Config {
        database: ":memory:".into(),
        http_bind: Some("127.0.0.1:0".parse().unwrap()),
        socks5_bind: None,
        admin: AdminConfig {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    config.probe.enabled = false;
    config.probe.url = "http://exit.test/ip".into();
    config.probe.tunnel_url = "http://exit.test/".into();
    let store = Store::open(&config).await.unwrap();
    store
        .add_candidates(Protocol::Http, vec![address], "persisted".into())
        .await
        .unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store.clone()),
        config,
    });
    let server = Server::bind(state).await.unwrap();
    let (stop, wait) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = wait.await;
    }));
    for _ in 0..50 {
        if store.status().await.unwrap().verified == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let verified = store.status().await.unwrap().verified;
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    fake.abort();
    assert_eq!(
        verified, 1,
        "persisted candidate was stranded until manual verify-pool"
    );
}

#[tokio::test]
async fn management_bounds_body_and_rejects_injected_metadata() {
    let (url, state) = fixture().await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/v1/proxies"))
        .bearer_auth("test-admin-token")
        .body("x".repeat(65537))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    let response = client.post(format!("{url}/v1/proxies")).bearer_auth("test-admin-token")
        .body(r#"{"protocol":"http","addresses":["127.0.0.1:8080"],"source":"test","egress":"1.2.3.4"}"#).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(state.router.store.status().await.unwrap().candidates, 0);
}
