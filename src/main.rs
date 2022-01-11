// https://github.com/rust-lang/rust-clippy/issues/7271
#![allow(clippy::needless_lifetimes)]

extern crate anyhow;

pub mod args;
pub mod errors;
pub mod list;
pub mod socks5;
pub mod http;
mod redis_util;

use crate::args::Args;
use crate::errors::*;
use arc_swap::ArcSwap;
use env_logger::Env;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::net::TcpListener;
use tokio::signal::unix::{signal, SignalKind};
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::from_args();

    let logging = match (args.quiet, args.verbose) {
        (true, _) => "warn",
        (false, 0) => "info",
        (false, 1) => "info,laundry5=debug",
        (false, 2) => "debug",
        (false, _) => "debug,laundry5=trace",
    };
    env_logger::init_from_env(Env::default().default_filter_or(logging));

    // a stream of sighup signals
    let mut sighup = signal(SignalKind::hangup())?;

    let proxies = list::load_from_path(&args.proxy_list)
        .await
        .context("Failed to load proxy list")?;
    let proxies = ArcSwap::from(Arc::new(proxies));

    info!("Binding socks5 listener to {}", args.socks5_bind);
    let socks5_listener = TcpListener::bind(args.socks5_bind).await?;
    info!("Binding http listener to {}", args.http_bind);
    let http_listener = TcpListener::bind(args.http_bind).await?;


    loop {
        tokio::select! {
            s5_res = socks5_listener.accept() => {
                let (socket, src) = match s5_res {
                    Ok(x) => x,
                    Err(err) => {
                        error!("Failed to accept connection: {:#}", err);
                        continue;
                    },
                };
                debug!("Got new client connection from {}", src);
                let proxies = proxies.load();
                tokio::spawn(async move {
                    if let Err(err) = socks5::serve(socket, proxies.clone()).await {
                        warn!("Error serving client: {:#}", err);
                    }
                });
            }
            http_res = http_listener.accept()=>{
                let(socket,src) = match http_res{
                    Ok(x)=>x,
                    Err(err)=>{
                        error!("failed to accept http connection: {:#}",err);
                        continue;
                    },
                };
                debug!("Got connection from {}",src);
                let proxies = proxies.load();
                tokio::spawn(async move{
                    if let Err(err) = http::serve(socket,proxies.clone()).await{
                        warn!("Error serving client: {:#}", err);
                    }
                });
            }
            _ = sighup.recv() => {
                debug!("Got signal HUP");
                match list::load_from_path(&args.proxy_list).await {
                    Ok(list) => {
                        let list = Arc::new(list);
                        proxies.store(list);
                    }
                    Err(err) => {
                        error!("Failed to reload proxy list: {:#}", err);
                    }
                }
            }
        }
    }
}
