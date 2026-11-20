use crate::{config::Config, routing::Protocol};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Authenticated HTTP/HTTPS/SOCKS5 proxy with embedded session routing"
)]
pub struct Args {
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub database: Option<PathBuf>,
    #[arg(long, global = true)]
    pub admin_bind: Option<SocketAddr>,
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
    /// Validate configuration and TLS files without opening the state database.
    CheckConfig,
    /// Check the running process through its authenticated management listener.
    Healthcheck,
    /// Verify actual egress and CONNECT capability in the running process.
    VerifyPool,
    /// Submit one upstream candidate to the running process.
    AddProxy {
        #[arg(long)]
        protocol: Protocol,
        #[arg(long)]
        address: SocketAddr,
    },
    /// Release a session in the running process, retaining occupancy until streams drain.
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
            .map(|p| Config::load(p))
            .transpose()?
            .unwrap_or_default();
        if let Some(path) = self
            .database
            .clone()
            .or_else(|| std::env::var_os("RPROXY_DATABASE").map(PathBuf::from))
        {
            config.database = path;
        }
        if let Some(address) = self.admin_bind {
            config.admin.bind = address;
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
