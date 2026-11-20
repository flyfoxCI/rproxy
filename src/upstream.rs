use anyhow::{bail, Context, Result};
use std::{net::IpAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{
    config::Config,
    routing::{Protocol, Proxy},
    store::Store,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}
impl Target {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        if port == 0
            || host.is_empty()
            || host
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            bail!("invalid target");
        }
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let host = if let Ok(ip) = host.parse::<IpAddr>() {
            ip.to_string()
        } else {
            url::Host::parse(host)
                .context("invalid target host")?
                .to_string()
        };
        if host.len() > 253 {
            bail!("target host too long");
        }
        Ok(Self { host, port })
    }
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("upstream connection or handshake failed: {0}")]
    Proxy(#[source] anyhow::Error),
    #[error("upstream rejected target (code {0})")]
    Target(u16),
}

pub async fn connect_tunnel(
    proxy: &Proxy,
    target: &Target,
    timeout: Duration,
) -> Result<TcpStream, ConnectError> {
    match tokio::time::timeout(timeout, connect_inner(proxy, target)).await {
        Ok(result) => result,
        Err(_) => Err(ConnectError::Proxy(anyhow::anyhow!("upstream timed out"))),
    }
}

async fn connect_inner(proxy: &Proxy, target: &Target) -> Result<TcpStream, ConnectError> {
    let mut stream = TcpStream::connect(proxy.address)
        .await
        .map_err(|error| ConnectError::Proxy(error.into()))?;
    match proxy.protocol {
        Protocol::Http => http_connect(&mut stream, target).await?,
        Protocol::Socks5 => socks_connect(&mut stream, target).await?,
    }
    Ok(stream)
}

async fn http_connect(stream: &mut TcpStream, target: &Target) -> Result<(), ConnectError> {
    stream
        .write_all(
            format!(
                "CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n",
                target.authority(),
                target.authority()
            )
            .as_bytes(),
        )
        .await
        .map_err(|e| ConnectError::Proxy(e.into()))?;
    for _ in 0..8 {
        let header = read_http_header(stream)
            .await
            .map_err(ConnectError::Proxy)?;
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut response = httparse::Response::new(&mut headers);
        response
            .parse(&header)
            .map_err(|e| ConnectError::Proxy(e.into()))?;
        let code = response
            .code
            .ok_or_else(|| ConnectError::Proxy(anyhow::anyhow!("missing upstream status")))?;
        if (100..200).contains(&code) && code != 101 {
            continue;
        }
        return match code {
            200..=299 => Ok(()),
            407 => Err(ConnectError::Proxy(anyhow::anyhow!(
                "upstream requires separate credentials"
            ))),
            _ => Err(ConnectError::Target(code)),
        };
    }
    Err(ConnectError::Proxy(anyhow::anyhow!(
        "too many interim responses"
    )))
}

pub async fn read_http_header(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= 32768 {
            bail!("upstream headers too large");
        }
        header.push(
            stream
                .read_u8()
                .await
                .context("incomplete upstream response")?,
        );
    }
    Ok(header)
}

pub async fn probe_one(
    config: &Config,
    protocol: Protocol,
    address: std::net::SocketAddr,
) -> Result<Proxy> {
    let scheme = match protocol {
        Protocol::Http => "http",
        Protocol::Socks5 => "socks5h",
    };
    let mut builder = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("{scheme}://{address}"))?)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(config.probe.timeout_seconds))
        .pool_max_idle_per_host(0)
        .danger_accept_invalid_certs(config.probe.insecure);
    if let Some(path) = &config.probe.ca_certificate {
        builder =
            builder.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(path)?)?);
    }
    let client = builder.build()?;
    async fn exit(client: &reqwest::Client, url: &str) -> Result<IpAddr> {
        let mut response = client.get(url).send().await?.error_for_status()?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if body.len() + chunk.len() > 4096 {
                bail!("exit response exceeds 4096 bytes");
            }
            body.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&body)?.trim();
        let ip = if let Ok(ip) = text.parse::<IpAddr>() {
            ip
        } else {
            let object: serde_json::Value = serde_json::from_str(text)?;
            object
                .get("ip")
                .or_else(|| object.get("origin"))
                .and_then(|value| value.as_str())
                .context("exit endpoint must return an IP or JSON ip/origin")?
                .parse::<IpAddr>()?
        };
        Ok(match ip {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
            ip => ip,
        })
    }
    let egress = exit(&client, &config.probe.url).await?;
    let second = exit(&client, &config.probe.url).await?;
    let mut proxy = Proxy {
        id: format!("{protocol}:{address}"),
        protocol,
        address,
        egress,
        tunnel: protocol == Protocol::Socks5,
        stable: egress == second,
        verified_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    };
    if protocol == Protocol::Http {
        let endpoint = url::Url::parse(&config.probe.tunnel_url)?;
        let target = Target::new(
            endpoint.host_str().context("missing tunnel probe host")?,
            endpoint
                .port_or_known_default()
                .context("missing tunnel probe port")?,
        )?;
        proxy.tunnel = connect_tunnel(
            &proxy,
            &target,
            Duration::from_secs(config.probe.timeout_seconds),
        )
        .await
        .is_ok();
    }
    proxy.validate()?;
    Ok(proxy)
}

pub async fn verify_pool(config: &Config, store: &Store) -> Result<usize> {
    let candidates = store.candidates().await?;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.probe.concurrency));
    let mut jobs = tokio::task::JoinSet::new();
    for candidate in candidates {
        if candidate
            .verified
            .as_ref()
            .is_some_and(|proxy| !proxy.stable)
        {
            continue;
        }
        let permit = semaphore.clone().acquire_owned().await?;
        let config = config.clone();
        let store = store.clone();
        jobs.spawn(async move {
            let _permit = permit;
            match probe_one(&config, candidate.protocol, candidate.address).await {
                Ok(proxy) => {
                    let updated = store.update_probe(&proxy, candidate.generation).await?;
                    Ok::<_, anyhow::Error>(usize::from(updated && proxy.stable))
                }
                Err(_) => {
                    log::warn!(
                        "exit verification failed upstream={}:{}",
                        candidate.protocol,
                        candidate.address
                    );
                    let id = format!("{}:{}", candidate.protocol, candidate.address);
                    store.probe_failed(&id, candidate.generation).await?;
                    Ok(0)
                }
            }
        });
    }
    let mut verified = 0;
    while let Some(result) = jobs.join_next().await {
        verified += result??;
    }
    Ok(verified)
}

async fn socks_connect(stream: &mut TcpStream, target: &Target) -> Result<(), ConnectError> {
    async fn negotiate(stream: &mut TcpStream, target: &Target) -> Result<u8> {
        stream.write_all(&[5, 1, 0]).await?;
        let mut reply = [0; 2];
        stream.read_exact(&mut reply).await?;
        if reply != [5, 0] {
            bail!("upstream refused anonymous SOCKS5 authentication");
        }
        let mut request = vec![5, 1, 0];
        match target.host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => {
                request.push(1);
                request.extend(ip.octets());
            }
            Ok(IpAddr::V6(ip)) => {
                request.push(4);
                request.extend(ip.octets());
            }
            Err(_) => {
                request.extend([3, target.host.len() as u8]);
                request.extend(target.host.as_bytes());
            }
        }
        request.extend(target.port.to_be_bytes());
        stream.write_all(&request).await?;
        let mut response = [0; 4];
        stream.read_exact(&mut response).await?;
        if response[0] != 5 || response[2] != 0 {
            bail!("malformed upstream SOCKS5 response");
        }
        if response[1] != 0 {
            return Ok(response[1]);
        }
        let length = match response[3] {
            1 => 4,
            4 => 16,
            3 => stream.read_u8().await? as usize,
            _ => bail!("malformed upstream SOCKS5 address"),
        };
        let mut address = vec![0; length + 2];
        stream.read_exact(&mut address).await?;
        Ok(0)
    }
    match negotiate(stream, target)
        .await
        .map_err(ConnectError::Proxy)?
    {
        0 => Ok(()),
        code => Err(ConnectError::Target(code as u16)),
    }
}
