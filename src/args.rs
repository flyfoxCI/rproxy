use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

use crate::config::Config;

#[derive(Parser)]
#[command(
    version,
    about = "Authenticated HTTP/HTTPS/SOCKS5 proxy with session routing"
)]
pub struct Args {
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(short = 'r', long, global = true)]
    pub redis_url: Option<String>,
    #[arg(long, global = true)]
    pub redis_key: Option<String>,
    #[arg(short = 'H', long, global = true)]
    pub http_bind: Option<SocketAddr>,
    #[arg(short = 'S', long, global = true)]
    pub socks5_bind: Option<SocketAddr>,
    #[arg(long, global = true)]
    pub https_bind: Option<SocketAddr>,
    #[arg(long, global = true)]
    pub no_http: bool,
    #[arg(long, global = true)]
    pub no_socks5: bool,
    #[arg(short, long, global = true)]
    pub quiet: bool,
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the configured proxy listeners (the default).
    Serve,
    /// Validate configuration and TLS files without connecting to Redis.
    CheckConfig,
    /// Verify actual egress and CONNECT capability for legacy pool addresses.
    VerifyPool,
    /// Release your business/session binding, retaining occupied exits until streams drain.
    ReleaseSession {
        #[arg(long)]
        username: String,
        #[arg(long, default_value = "RPROXY_PASSWORD")]
        password_env: String,
    },
}

impl Args {
    pub fn load_config(&self) -> Result<Config> {
        let mut config = self
            .config
            .as_ref()
            .map(|path| Config::load(path))
            .transpose()?
            .unwrap_or_default();
        if let Some(url) = self
            .redis_url
            .clone()
            .or_else(|| std::env::var("RPROXY_REDIS_URL").ok())
        {
            config.redis_url = url;
        }
        if let Some(key) = self
            .redis_key
            .clone()
            .or_else(|| std::env::var("RPROXY_REDIS_KEY").ok())
        {
            config.redis_key = key;
        }
        if let Some(address) = self.http_bind {
            config.http_bind = Some(address);
        }
        if let Some(address) = self.socks5_bind {
            config.socks5_bind = Some(address);
        }
        if let Some(address) = self.https_bind {
            config.https_bind = Some(address);
        }
        if self.no_http {
            config.http_bind = None;
        }
        if self.no_socks5 {
            config.socks5_bind = None;
        }
        config.validate()?;
        Ok(config)
    }
}
