use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, TcpListener, TcpStream},
    thread,
    time::Duration,
};
use xtask::fixture::{serve_http, serve_socks};
fn connection(handler: fn(TcpStream, IpAddr) -> anyhow::Result<()>) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = handler(stream, IpAddr::V4(Ipv4Addr::LOCALHOST));
    });
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
}
#[test]
fn fixture_serves_local_extraction_and_payload_without_external_requests() {
    for (path, body) in [
        ("/supplier", "127.0.0.1:18080\n"),
        ("/supplier-socks", "127.0.0.1:18081\n"),
        ("/ip", "127.0.0.1"),
        ("/payload", "mounted-compose-ok"),
    ] {
        let mut stream = connection(serve_http);
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut output = String::new();
        stream.read_to_string(&mut output).unwrap();
        assert!(output.starts_with("HTTP/1.1 200"), "{output}");
        assert!(output.ends_with(body), "{output}");
    }
}
#[test]
fn fixture_http_connect_cannot_access_arbitrary_destinations() {
    let mut stream = connection(serve_http);
    stream
        .write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .unwrap();
    let mut output = String::new();
    stream.read_to_string(&mut output).unwrap();
    assert!(output.starts_with("HTTP/1.1 403"), "{output}");
}
#[test]
fn fixture_socks_negotiates_but_refuses_external_and_wrong_port_destinations() {
    for (host, port) in [("example.com", 18080u16), ("fixture", 443)] {
        let mut stream = connection(serve_socks);
        stream.write_all(&[5, 1, 0]).unwrap();
        let mut response = [0; 2];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(response, [5, 0]);
        let mut request = vec![5, 1, 0, 3, host.len() as u8];
        request.extend(host.as_bytes());
        request.extend(port.to_be_bytes());
        stream.write_all(&request).unwrap();
        let mut response = [0; 10];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(response[1], 2);
    }
}
#[test]
fn fixture_rejects_external_absolute_urls_and_non_http_request_targets() {
    for target in [
        "http://example.com:18080/payload",
        "http://fixture:443/payload",
        "https://example.com/payload",
        "//example.com/payload",
    ] {
        let mut stream = connection(serve_http);
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut output = String::new();
        stream.read_to_string(&mut output).unwrap();
        assert!(
            output.starts_with("HTTP/1.1 403"),
            "accepted {target}: {output}"
        );
    }
}

#[test]
fn fixture_relays_http_connect_and_domain_and_numeric_socks_only_to_local_payload() {
    // The fixture's only relay destination is this loopback listener, never request-selected DNS.
    let listener = TcpListener::bind("127.0.0.1:18080").unwrap();
    let upstream = thread::spawn(move || {
        for stream in listener.incoming().take(3) {
            serve_http(stream.unwrap(), IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        }
    });
    for kind in ["connect", "domain", "numeric"] {
        let mut stream = connection(if kind == "connect" {
            serve_http
        } else {
            serve_socks
        });
        if kind == "connect" {
            stream
                .write_all(b"CONNECT fixture:18080 HTTP/1.1\r\nHost: fixture:18080\r\n\r\n")
                .unwrap();
            assert!(xtask::fixture::header(&mut stream)
                .unwrap()
                .starts_with("HTTP/1.1 200"));
        } else {
            stream.write_all(&[5, 1, 0]).unwrap();
            let mut reply = [0; 2];
            stream.read_exact(&mut reply).unwrap();
            assert_eq!(reply, [5, 0]);
            let mut request = if kind == "domain" {
                b"\x05\x01\x00\x03\x07fixture".to_vec()
            } else {
                vec![5, 1, 0, 1, 127, 0, 0, 1]
            };
            request.extend(18080u16.to_be_bytes());
            stream.write_all(&request).unwrap();
            let mut reply = [0; 10];
            stream.read_exact(&mut reply).unwrap();
            assert_eq!(&reply[..4], &[5, 0, 0, 1]);
        }
        stream
            .write_all(b"GET /payload HTTP/1.1\r\nHost: fixture:18080\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut output = String::new();
        stream.read_to_string(&mut output).unwrap();
        assert!(output.starts_with("HTTP/1.1 200"), "{output}");
        assert!(output.ends_with("mounted-compose-ok"), "{output}");
    }
    upstream.join().unwrap();
}

#[test]
fn fixture_socks_requires_supported_noauth_method() {
    let mut stream = connection(serve_socks);
    stream.write_all(&[5, 1, 2]).unwrap();
    let mut response = [0; 2];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(response, [5, 255]);
}
