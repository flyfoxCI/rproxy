use anyhow::{bail, ensure, Context, Result};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream, UdpSocket},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

pub fn header(stream: &mut impl Read) -> Result<String> {
    let mut data = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        data.push(byte[0]);
        if data.ends_with(b"\r\n\r\n") {
            break;
        }
        ensure!(data.len() < 32768, "HTTP header too large");
    }
    Ok(String::from_utf8(data)?)
}

fn timeouts(stream: &TcpStream) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(())
}
fn response(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}
fn local_host(host: &str, ip: IpAddr) -> bool {
    host == "fixture" || host == "127.0.0.1" || host == ip.to_string()
}

fn relay(mut client: TcpStream) -> Result<()> {
    let mut upstream =
        TcpStream::connect_timeout(&"127.0.0.1:18080".parse()?, Duration::from_secs(5))?;
    timeouts(&upstream)?;
    let mut client_copy = client.try_clone()?;
    let mut upstream_copy = upstream.try_clone()?;
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut client_copy, &mut upstream_copy);
        let _ = upstream_copy.shutdown(Shutdown::Both);
        let _ = client_copy.shutdown(Shutdown::Both);
    });
    let _ = io::copy(&mut upstream, &mut client);
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = forward.join();
    Ok(())
}

pub fn serve_http(mut stream: TcpStream, own_ip: IpAddr) -> Result<()> {
    timeouts(&stream)?;
    let request = header(&mut stream)?;
    let mut parts = request
        .lines()
        .next()
        .context("missing HTTP request")?
        .split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method == "CONNECT" {
        if target != "fixture:18080" {
            return response(&mut stream, "403 Forbidden", "restricted fixture");
        }
        stream.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        return relay(stream);
    }
    if method != "GET" {
        return response(&mut stream, "405 Method Not Allowed", "GET only");
    }
    let path = if let Some(url) = target.strip_prefix("http://") {
        let (authority, path) = url.split_once('/').unwrap_or((url, ""));
        let allowed = authority
            .strip_suffix(":18080")
            .is_some_and(|host| local_host(host, own_ip));
        if !allowed {
            return response(&mut stream, "403 Forbidden", "restricted fixture");
        }
        format!("/{path}")
    } else if target.starts_with('/') && !target.starts_with("//") {
        target.to_owned()
    } else {
        return response(&mut stream, "403 Forbidden", "restricted fixture");
    };
    let path = path.split('?').next().unwrap_or("");
    let body = match path {
        "/supplier" => format!("{own_ip}:18080\n"),
        "/supplier-socks" => format!("{own_ip}:18081\n"),
        "/ip" => own_ip.to_string(),
        _ => "mounted-compose-ok".into(),
    };
    response(&mut stream, "200 OK", &body)
}

fn bytes<const N: usize>(stream: &mut TcpStream) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn serve_socks(mut stream: TcpStream, own_ip: IpAddr) -> Result<()> {
    timeouts(&stream)?;
    let [version, count] = bytes(&mut stream)?;
    let mut methods = vec![0; count as usize];
    stream.read_exact(&mut methods)?;
    if version != 5 || !methods.contains(&0) {
        stream.write_all(&[5, 255])?;
        return Ok(());
    }
    stream.write_all(&[5, 0])?;
    let [version, command, reserved, kind] = bytes(&mut stream)?;
    let host = match kind {
        1 => Ipv4Addr::from(bytes::<4>(&mut stream)?).to_string(),
        3 => {
            let [count] = bytes(&mut stream)?;
            let mut host = vec![0; count as usize];
            stream.read_exact(&mut host)?;
            String::from_utf8(host)?
        }
        _ => {
            stream.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0])?;
            return Ok(());
        }
    };
    let port = u16::from_be_bytes(bytes(&mut stream)?);
    if version != 5 || command != 1 || reserved != 0 || port != 18080 || !local_host(&host, own_ip)
    {
        stream.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])?;
        return Ok(());
    }
    stream.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])?;
    relay(stream)
}

fn accept(
    listener: TcpListener,
    ip: IpAddr,
    handler: fn(TcpStream, IpAddr) -> Result<()>,
) -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream?;
        let active = active.clone();
        if active.fetch_add(1, Ordering::Relaxed) >= 128 {
            active.fetch_sub(1, Ordering::Relaxed);
            continue;
        }
        thread::spawn(move || {
            let _ = handler(stream, ip);
            active.fetch_sub(1, Ordering::Relaxed);
        });
    }
    bail!("fixture listener closed")
}

/// Smoke-only service. No request-selected remote connection is ever opened.
pub fn run() -> Result<()> {
    // UDP connect selects the container's local interface; it sends no datagram.
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("192.0.2.1:9")?;
    let ip = socket.local_addr()?.ip();
    let http = TcpListener::bind("0.0.0.0:18080")?;
    let socks = TcpListener::bind("0.0.0.0:18081")?;
    thread::spawn(move || {
        let _ = accept(socks, ip, serve_socks);
    });
    accept(http, ip, serve_http)
}
