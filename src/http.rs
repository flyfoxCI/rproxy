use tokio::net::{TcpStream};
use std::error::Error;
use std::net::SocketAddr;
use anyhow::Context;
use futures::future::try_join;
use mobc::Pool;
use mobc_redis::RedisConnectionManager;

use rand::prelude::SliceRandom;
use tokio::io;
use crate::redis_util::{get_proxies};

pub async fn serve(socket: TcpStream, pool: &Pool<RedisConnectionManager>,redis_key:&String) -> Result<(), Box<dyn Error>> {
    let proxies = get_proxies(pool,redis_key, "http").await?;
    let proxy = proxies
        .choose(&mut rand::thread_rng())
        .context("No proxies configured")?;
    transfer(socket, proxy).await?;
    Ok(())
}

async fn transfer(mut inbound: TcpStream, proxy_addr: &SocketAddr) -> Result<(), Box<dyn Error>> {
    println!("{}", proxy_addr.to_string());
    let mut outbound = TcpStream::connect(proxy_addr).await?;

    // tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
    //     .await
    //     .context("Failed to relay data")?;
    let (mut ri, mut wi) = inbound.split();
    let (mut ro, mut wo) = outbound.split();

    let client_to_server = io::copy(&mut ri, &mut wo);
    let server_to_client = io::copy(&mut ro, &mut wi);

    try_join(client_to_server, server_to_client).await?;

    Ok(())
}