use tokio::net::{TcpStream};
use std::error::Error;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use std::net::SocketAddr;
use anyhow::Context;

use rand::prelude::SliceRandom;

pub async fn serve(mut socket: TcpStream, proxies: Arc<Vec<SocketAddr>>)  -> Result<(),Box<dyn Error>> {
    let proxy = proxies
        .choose(&mut rand::thread_rng())
        .context("No proxies configured")?;
    transfer(socket, proxy).await?;
    Ok(())
}

async fn transfer(mut inbound: TcpStream, proxy_addr: &SocketAddr) -> Result<(), Box<dyn Error>> {
    let mut outbound = TcpStream::connect(proxy_addr).await?;

    tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
        .await
        .context("Failed to relay data")?;
    // let (mut ri, mut wi) = inbound.split();
    // let (mut ro, mut wo) = outbound.split();
    //
    // let client_to_server = io::copy(&mut ri, &mut wo);
    // let server_to_client = io::copy(&mut ro, &mut wi);
    //
    // try_join(client_to_server, server_to_client).await?;

    Ok(())
}


async fn get_proxies() -> Vec<String> {
    let proxies_path = std::env::var("PROXY_PATH")
        .expect("Please specify absolute path in PROXY_PATH environment variable");

    let mut proxies_file_handle = File::open(proxies_path)
        .await
        .expect("Unable to open proxy file");

    let mut proxies_file_content = vec![];

    proxies_file_handle
        .read_to_end(&mut proxies_file_content)
        .await
        .expect("Unable to read proxy file");

    let proxies_ =
        String::from_utf8(proxies_file_content).expect("Cant convert prixes content to string");

    let mut proxies = vec![];

    proxies_
        .split("\n")
        .for_each(|proxy| proxies.push(String::from(proxy)));

    proxies
}