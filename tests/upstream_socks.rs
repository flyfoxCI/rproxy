use rproxy::{
    auth::Authenticator,
    config::Config,
    routing::{Protocol, Proxy, Router},
    server::State,
    socks5::serve_socks,
    store::Store,
    upstream::{connect_tunnel, Target},
};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn echo_proxy(protocol: Protocol) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                if protocol == Protocol::Http {
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                    }
                    assert!(header.starts_with(b"CONNECT "));
                    stream
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await
                        .unwrap();
                } else {
                    let mut greeting = [0; 3];
                    stream.read_exact(&mut greeting).await.unwrap();
                    assert_eq!(greeting, [5, 1, 0]);
                    stream.write_all(&[5, 0]).await.unwrap();
                    let mut request = [0; 4];
                    stream.read_exact(&mut request).await.unwrap();
                    let n = match request[3] {
                        1 => 4,
                        4 => 16,
                        3 => stream.read_u8().await.unwrap() as usize,
                        _ => panic!(),
                    };
                    let mut address = vec![0; n + 2];
                    stream.read_exact(&mut address).await.unwrap();
                    stream
                        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                }
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    address
}

fn proxy(protocol: Protocol, address: std::net::SocketAddr) -> Proxy {
    Proxy {
        id: format!("{protocol}:{address}"),
        protocol,
        address,
        egress: "1.1.1.1".parse().unwrap(),
        tunnel: true,
        stable: true,
        verified_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    }
}

#[tokio::test]
async fn upstream_connect_supports_http_and_socks_with_all_address_types() {
    for protocol in [Protocol::Http, Protocol::Socks5] {
        let address = echo_proxy(protocol).await;
        for host in ["example.com", "127.0.0.1", "::1"] {
            let target = Target::new(host, 443).unwrap();
            let mut stream =
                connect_tunnel(&proxy(protocol, address), &target, Duration::from_secs(2))
                    .await
                    .unwrap();
            stream.write_all(b"hello").await.unwrap();
            let mut data = [0; 5];
            stream.read_exact(&mut data).await.unwrap();
            assert_eq!(&data, b"hello");
            stream.shutdown().await.unwrap();
            assert_eq!(stream.read(&mut data).await.unwrap(), 0);
        }
    }
}

async fn ingress(authenticated: bool, protocol: Protocol) -> std::net::SocketAddr {
    let address = echo_proxy(protocol).await;
    let mut config = if authenticated {
        Config::parse("[[auth.users]]\nname='alice'\npassword='secret'\nbusinesses=['shop']")
            .unwrap()
    } else {
        Config::default()
    };
    config.database = ":memory:".into();
    let store = Store::open(&config).await.unwrap();
    store.save_proxy(&proxy(protocol, address)).await.unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = state.clone();
            tokio::spawn(async move {
                let _ = serve_socks(stream, state).await;
            });
        }
    });
    address
}

#[tokio::test]
async fn socks_ingress_requires_credentials_and_relays_after_success() {
    let address = ingress(true, Protocol::Http).await;
    let mut denied = TcpStream::connect(address).await.unwrap();
    denied.write_all(&[5, 1, 0]).await.unwrap();
    let mut reply = [0; 2];
    denied.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 255]);
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&[5, 2, 0, 2]).await.unwrap();
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 2]);
    let username = b"v1|alice|shop|account|stick";
    let mut credentials = vec![1, username.len() as u8];
    credentials.extend(username);
    credentials.push(6);
    credentials.extend(b"secret");
    stream.write_all(&credentials).await.unwrap();
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [1, 0]);
    stream
        .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, 1, 187])
        .await
        .unwrap();
    let mut result = [0; 10];
    stream.read_exact(&mut result).await.unwrap();
    assert_eq!(result[1], 0);
    stream.write_all(b"hello").await.unwrap();
    let mut data = [0; 5];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"hello");
}

#[tokio::test]
async fn socks_anonymous_and_unsupported_commands_are_explicit() {
    let address = ingress(false, Protocol::Socks5).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 0]);
    stream
        .write_all(&[5, 3, 0, 1, 127, 0, 0, 1, 1, 187])
        .await
        .unwrap();
    let mut result = [0; 10];
    stream.read_exact(&mut result).await.unwrap();
    assert_eq!(result[1], 7);
}

#[test]
fn target_rejects_authority_injection_and_zero_port() {
    assert!(Target::new("example.com\r\nX-Injected: yes", 443).is_err());
    assert!(Target::new("example.com", 0).is_err());
    assert_eq!(Target::new("::1", 443).unwrap().authority(), "[::1]:443");
}
