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
use env_logger::Env;
use structopt::StructOpt;
use tokio::net::TcpListener;
// use tokio::signal::unix::{signal, SignalKind};
use anyhow::Result;
use mobc::Pool;
use mobc_redis::{redis, RedisConnectionManager};

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
    // let mut sighup = signal(SignalKind::hangup())?;
    info!("Binding socks5 listener to {}", args.socks5_bind);
    let socks5_listener = TcpListener::bind(args.socks5_bind).await?;
    info!("Binding http listener to {}", args.http_bind);
    let http_listener = TcpListener::bind(args.http_bind).await?;


    let client = redis::Client::open(args.redis_url).unwrap();
    let manager = RedisConnectionManager::new(client);
    let pool = Pool::builder().max_open(10).build(manager);
    let redis_key = args.redis_key;

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
                let pool = pool.clone();
                let key = redis_key.clone();
                tokio::spawn(async move {
                    if let Err(err) = socks5::serve(socket, &pool,&key).await {
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
                 let pool = pool.clone();
                 let key = redis_key.clone();
                tokio::spawn(async move {
                    if let Err(err) = http::serve(socket,&pool,&key).await{
                        warn!("Error serving client: {:#}", err);
                    }
                });
            }
            // _ = sighup.recv() => {
            //     debug!("Got signal HUP");
            //     match redis_util::get_random_proxy(&pool,redis_key).await {
            //         Ok(x) => {
            //
            //         }
            //         Err(err) => {
            //             error!("Failed to reload proxy list: {:#}", err);
            //         }
            //     }
            // }
        }
    }
}
