use anyhow::{bail, Context, Result};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{
    auth::Identity,
    server::{relay, State},
    upstream::{ConnectError, Target},
};

pub async fn serve_socks(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let timeout = Duration::from_secs(state.config.routing.handshake_timeout_seconds);
    let handshake = tokio::time::timeout(timeout, handshake(&mut stream, &state))
        .await
        .context("client SOCKS5 handshake timed out")??;
    let (identity, target) = handshake;
    match state.tunnel(&identity, &target).await {
        Ok((upstream, reservation)) => {
            stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            relay(stream, upstream, reservation).await
        }
        Err(error) => {
            let code = match error.downcast_ref::<ConnectError>() {
                Some(ConnectError::Target(code @ 1..=8)) => *code as u8,
                Some(ConnectError::Target(_)) => 2,
                _ => 1,
            };
            reply_error(&mut stream, code).await?;
            Err(error)
        }
    }
}

async fn handshake(stream: &mut TcpStream, state: &State) -> Result<(Identity, Target)> {
    let version = stream.read_u8().await?;
    let count = stream.read_u8().await? as usize;
    if version != 5 || count == 0 {
        bail!("invalid SOCKS5 greeting");
    }
    let mut methods = vec![0; count];
    stream.read_exact(&mut methods).await?;
    let method = if state.auth.required() {
        if methods.contains(&2) {
            2
        } else {
            255
        }
    } else if methods.contains(&0) {
        0
    } else {
        255
    };
    stream.write_all(&[5, method]).await?;
    if method == 255 {
        bail!("no acceptable SOCKS5 authentication method");
    }
    let identity = if method == 2 {
        let version = stream.read_u8().await?;
        let username = read_string(stream).await?;
        let password = read_string(stream).await?;
        let verified = if version == 1 && !username.is_empty() && !password.is_empty() {
            state.authenticate(Some((&username, &password))).await
        } else {
            Err(anyhow::anyhow!("invalid SOCKS5 credentials"))
        };
        stream
            .write_all(&[1, if verified.is_ok() { 0 } else { 1 }])
            .await?;
        verified?
    } else {
        state.authenticate(None).await?
    };
    let mut command = [0; 4];
    stream.read_exact(&mut command).await?;
    if command[0] != 5 || command[2] != 0 {
        reply_error(stream, 1).await?;
        bail!("invalid SOCKS5 request");
    }
    let host = match command[3] {
        1 => {
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await?;
            Ipv4Addr::from(bytes).to_string()
        }
        4 => {
            let mut bytes = [0; 16];
            stream.read_exact(&mut bytes).await?;
            Ipv6Addr::from(bytes).to_string()
        }
        3 => read_string(stream).await?,
        _ => {
            reply_error(stream, 8).await?;
            bail!("unsupported SOCKS5 address type");
        }
    };
    let port = stream.read_u16().await?;
    if command[1] != 1 {
        reply_error(stream, 7).await?;
        bail!("only SOCKS5 TCP CONNECT is supported");
    }
    let target = match Target::new(&host, port) {
        Ok(target) => target,
        Err(error) => {
            reply_error(stream, 8).await?;
            return Err(error);
        }
    };
    Ok((identity, target))
}

async fn read_string(stream: &mut TcpStream) -> Result<String> {
    let length = stream.read_u8().await? as usize;
    let mut data = vec![0; length];
    stream.read_exact(&mut data).await?;
    String::from_utf8(data).context("SOCKS5 string must be UTF-8")
}

async fn reply_error(stream: &mut TcpStream, code: u8) -> Result<()> {
    stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    Ok(())
}
