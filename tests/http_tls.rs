use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use rproxy::{
    auth::Authenticator,
    config::{Config, TlsConfig},
    http::{serve_http, tls_acceptor},
    routing::{Protocol, Proxy, Router},
    server::State,
    store::Store,
};
use std::{
    convert::Infallible,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[tokio::test]
async fn streaming_upload_is_not_cut_off_by_handshake_timeout() {
    let mut state = Arc::try_unwrap(state(Protocol::Http).await).ok().unwrap();
    state.config.routing.handshake_timeout_seconds = 1;
    let address = ingress(Arc::new(state), None).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth = STANDARD.encode("v1|alice|shop|slow|stick:secret");
    stream.write_all(format!("POST http://example.com/upload HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic {auth}\r\nTransfer-Encoding: chunked\r\n\r\n").as_bytes()).await.unwrap();
    for _ in 0..7 {
        stream.write_all(b"1\r\nx\r\n").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let header = rproxy::upstream::read_http_header(&mut stream)
        .await
        .unwrap();
    assert!(
        header.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&header)
    );
}

#[tokio::test]
async fn proxy_credentials_are_not_forwarded_in_chunked_trailers() {
    let address = ingress(state(Protocol::Http).await, None).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth = STANDARD.encode("v1|alice|shop|trailers|stick:secret");
    stream.write_all(format!("POST http://example.com/trailers HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic {auth}\r\nTransfer-Encoding: chunked\r\nTrailer: Proxy-Authorization\r\nConnection: close\r\n\r\n1\r\nx\r\n0\r\nProxy-Authorization: Basic {auth}\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!response.contains(&auth), "credential leaked in trailer");
}

async fn echo_http() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(|mut req: Request<Incoming>| async move {
                    if req.method() == hyper::Method::CONNECT {
                        let target = req.uri().to_string();
                        let mut target = TcpStream::connect(&target).await.unwrap();
                        let upgraded = hyper::upgrade::on(&mut req);
                        tokio::spawn(async move {
                            if let Ok(stream) = upgraded.await {
                                let _ = tokio::io::copy_bidirectional(
                                    &mut TokioIo::new(stream),
                                    &mut target,
                                )
                                .await;
                            }
                        });
                        return Ok::<_, Infallible>(Response::new(Full::new(Bytes::new())));
                    }
                    let uri = req.uri().to_string();
                    let auth = req
                        .headers()
                        .get("proxy-authorization")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .unwrap_or_default();
                    let nominated = req.headers().contains_key("x-remove");
                    let collected = req.into_body().collect().await.unwrap();
                    let auth = collected
                        .trailers()
                        .and_then(|headers| headers.get("proxy-authorization"))
                        .map(|value| value.to_str().unwrap().to_owned())
                        .unwrap_or(auth);
                    let body = collected.to_bytes();
                    Ok::<_, Infallible>(
                        Response::builder()
                            .header("connection", "x-response-remove")
                            .header("x-response-remove", "secret")
                            .body(Full::new(Bytes::from(format!(
                                "{uri}|{auth}|{nominated}|{}",
                                String::from_utf8_lossy(&body)
                            ))))
                            .unwrap(),
                    )
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    address
}

async fn upstream(protocol: Protocol) -> std::net::SocketAddr {
    if protocol == Protocol::Http {
        return echo_http().await;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).await.unwrap();
                stream.write_all(&[5, 0]).await.unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).await.unwrap();
                let host = match request[3] {
                    1 => {
                        let mut b = [0; 4];
                        stream.read_exact(&mut b).await.unwrap();
                        std::net::Ipv4Addr::from(b).to_string()
                    }
                    3 => {
                        let n = stream.read_u8().await.unwrap();
                        let mut b = vec![0; n as usize];
                        stream.read_exact(&mut b).await.unwrap();
                        String::from_utf8(b).unwrap()
                    }
                    _ => panic!("unexpected address type"),
                };
                let port = stream.read_u16().await.unwrap();
                let mut target = TcpStream::connect((host.as_str(), port)).await.unwrap();
                stream
                    .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
            });
        }
    });
    address
}

async fn state(protocol: Protocol) -> Arc<State> {
    let address = upstream(protocol).await;
    let mut config =
        Config::parse("[[auth.users]]\nname='alice'\npassword='secret'\nbusinesses=['shop']")
            .unwrap();
    config.redis_url =
        std::env::var("RPROXY_TEST_REDIS").unwrap_or("redis://127.0.0.1:16379/".into());
    config.redis_key = format!("http-test-{}", rand::random::<u64>());
    let store = Store::connect(&config).await.unwrap();
    store
        .save_proxy(&Proxy {
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
        })
        .await
        .unwrap();
    Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    })
}

async fn ingress(state: Arc<State>, tls: Option<TlsConfig>) -> std::net::SocketAddr {
    let acceptor = tls.map(|tls| tls_acceptor(&tls).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = state.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Some(acceptor) = acceptor {
                    let tls = acceptor.accept(stream).await.unwrap();
                    let _ = serve_http(tls, state).await;
                } else {
                    let _ = serve_http(stream, state).await;
                }
            });
        }
    });
    address
}

#[tokio::test]
async fn http_requires_authentication_and_strips_credentials_and_hop_headers() {
    let address = ingress(state(Protocol::Http).await, None).await;
    let proxy = format!("http://{address}");
    let denied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(&proxy).unwrap())
        .build()
        .unwrap()
        .get("http://example.com/path")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 407);
    assert!(denied.headers().contains_key("proxy-authenticate"));
    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::all(proxy)
                .unwrap()
                .basic_auth("v1|alice|shop|a|stick", "secret"),
        )
        .build()
        .unwrap();
    let response = client
        .post("http://example.com/path?q=1")
        .header("connection", "x-remove")
        .header("x-remove", "secret")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("x-response-remove"));
    assert_eq!(
        response.text().await.unwrap(),
        "http://example.com/path?q=1||false|payload"
    );
}

#[tokio::test]
async fn http_over_socks_supports_keepalive_and_different_target_authorities() {
    let first = echo_http().await;
    let second = echo_http().await;
    let address = ingress(state(Protocol::Socks5).await, None).await;
    let client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::all(format!("http://{address}"))
                .unwrap()
                .basic_auth("v1|alice|shop|a|stick", "secret"),
        )
        .build()
        .unwrap();
    for target in [first, second, first] {
        let response = client
            .post(format!("http://{target}/different"))
            .body("body")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "/different||false|body");
    }
}

#[tokio::test]
async fn raw_chunked_http_is_framed_and_connection_identity_cannot_change() {
    let address = ingress(state(Protocol::Http).await, None).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth = STANDARD.encode("v1|alice|shop|a|stick:secret");
    stream.write_all(format!("POST http://example.com/chunked HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic {auth}\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").as_bytes()).await.unwrap();
    let mut buffer = vec![0; 4096];
    let n = stream.read(&mut buffer).await.unwrap();
    let mut response = String::from_utf8_lossy(&buffer[..n]).into_owned();
    while !response.contains("abcde") {
        let n = stream.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        response.push_str(&String::from_utf8_lossy(&buffer[..n]));
    }
    assert!(response.starts_with("HTTP/1.1 200"));
    let changed = STANDARD.encode("v1|alice|shop|b|stick:secret");
    stream.write_all(format!("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic {changed}\r\n\r\n").as_bytes()).await.unwrap();
    let n = stream.read(&mut buffer).await.unwrap();
    assert!(String::from_utf8_lossy(&buffer[..n]).starts_with("HTTP/1.1 400"));
}

#[tokio::test]
async fn connect_relays_arbitrary_bytes_over_socks_upstream() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut s, _) = echo.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        tokio::io::copy(&mut r, &mut w).await.unwrap();
    });
    let address = ingress(state(Protocol::Socks5).await, None).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let auth = STANDARD.encode("v1|alice|shop|a|stick:secret");
    stream.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: Basic {auth}\r\n\r\n").as_bytes()).await.unwrap();
    let header = rproxy::upstream::read_http_header(&mut stream)
        .await
        .unwrap();
    assert!(header.starts_with(b"HTTP/1.1 200"));
    stream.write_all(b"TLS bytes unchanged").await.unwrap();
    let mut data = [0; 19];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"TLS bytes unchanged");
}

#[tokio::test]
async fn https_target_tls_passes_through_every_ingress_and_upstream_without_mitm() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let certificate = directory.path().join("cert.pem");
    let private_key = directory.path().join("key.pem");
    std::fs::write(&certificate, cert.cert.pem()).unwrap();
    std::fs::write(&private_key, cert.key_pair.serialize_pem()).unwrap();
    let tls_config = TlsConfig {
        certificate,
        private_key,
    };
    let target_acceptor = tls_acceptor(&tls_config).unwrap();
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = target.accept().await.unwrap();
            let acceptor = target_acceptor.clone();
            tokio::spawn(async move {
                let stream = acceptor.accept(stream).await.unwrap();
                let service = service_fn(|_: Request<Incoming>| async {
                    Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                        b"end-to-end TLS",
                    ))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    for protocol in [Protocol::Http, Protocol::Socks5] {
        for ingress_protocol in ["http", "https", "socks5h"] {
            let state = state(protocol).await;
            let address = if ingress_protocol == "socks5h" {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    loop {
                        let (stream, _) = listener.accept().await.unwrap();
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _ = rproxy::socks5::serve_socks(stream, state).await;
                        });
                    }
                });
                address
            } else {
                ingress(
                    state,
                    if ingress_protocol == "https" {
                        Some(tls_config.clone())
                    } else {
                        None
                    },
                )
                .await
            };
            let client = reqwest::Client::builder()
                .add_root_certificate(
                    reqwest::Certificate::from_pem(cert.cert.pem().as_bytes()).unwrap(),
                )
                .proxy(
                    reqwest::Proxy::all(format!(
                        "{ingress_protocol}://{}:{}",
                        if ingress_protocol == "socks5h" {
                            "127.0.0.1"
                        } else {
                            "localhost"
                        },
                        address.port()
                    ))
                    .unwrap()
                    .basic_auth("v1|alice|shop|a|stick", "secret"),
                )
                .build()
                .unwrap();
            let response = client
                .get(format!("https://localhost:{target_port}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{ingress_protocol} via {protocol}");
            assert_eq!(response.text().await.unwrap(), "end-to-end TLS");
        }
    }
}

#[tokio::test]
async fn https_proxy_ingress_works_with_explicit_certificate_trust() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let certificate = directory.path().join("cert.pem");
    let private_key = directory.path().join("key.pem");
    std::fs::write(&certificate, cert.cert.pem()).unwrap();
    std::fs::write(&private_key, cert.key_pair.serialize_pem()).unwrap();
    let address = ingress(
        state(Protocol::Http).await,
        Some(TlsConfig {
            certificate,
            private_key,
        }),
    )
    .await;
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert.cert.pem().as_bytes()).unwrap())
        .proxy(
            reqwest::Proxy::all(format!("https://localhost:{}", address.port()))
                .unwrap()
                .basic_auth("v1|alice|shop|a|stick", "secret"),
        )
        .build()
        .unwrap();
    let response = client.get("http://example.com/tls").send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        "http://example.com/tls||false|"
    );
}
