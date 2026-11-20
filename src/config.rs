use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::routing::Strategy;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database: PathBuf,
    pub max_proxies: usize,
    pub admin: AdminConfig,
    pub http_bind: Option<SocketAddr>,
    pub socks5_bind: Option<SocketAddr>,
    pub https_bind: Option<SocketAddr>,
    pub tls: Option<TlsConfig>,
    pub auth: AuthConfig,
    pub routing: RoutingConfig,
    pub probe: ProbeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database: "data/rproxy.sqlite3".into(),
            max_proxies: 1000,
            admin: AdminConfig::default(),
            http_bind: Some("127.0.0.1:9099".parse().unwrap()),
            socks5_bind: Some("127.0.0.1:9089".parse().unwrap()),
            https_bind: None,
            tls: None,
            auth: AuthConfig::default(),
            routing: RoutingConfig::default(),
            probe: ProbeConfig::default(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    pub enabled: bool,
    pub bind: SocketAddr,
    pub token_env: String,
}
impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: "127.0.0.1:9100".parse().unwrap(),
            token_env: "RPROXY_ADMIN_TOKEN".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub ttl_seconds: u64,
    pub default_business: String,
    pub default_session: String,
    pub default_strategy: Strategy,
    pub users: Vec<User>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 3600,
            default_business: "default".into(),
            default_session: "default".into(),
            default_strategy: Strategy::Random,
            users: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub password_env: Option<String>,
    pub businesses: Vec<String>,
    #[serde(default = "one")]
    pub version: u64,
}
fn one() -> u64 {
    1
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    pub connect_timeout_seconds: u64,
    pub handshake_timeout_seconds: u64,
    pub cooldown_seconds: u64,
    pub max_attempts: usize,
    pub max_connections: usize,
}
impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            connect_timeout_seconds: 10,
            handshake_timeout_seconds: 15,
            cooldown_seconds: 30,
            max_attempts: 3,
            max_connections: 4096,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProbeConfig {
    pub enabled: bool,
    pub url: String,
    pub tunnel_url: String,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub max_age_seconds: u64,
    pub concurrency: usize,
    pub ca_certificate: Option<PathBuf>,
    pub insecure: bool,
}
impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: "https://api.ipify.org/".into(),
            tunnel_url: "https://api.ipify.org/".into(),
            interval_seconds: 300,
            timeout_seconds: 10,
            max_age_seconds: 900,
            concurrency: 16,
            ca_certificate: None,
            insecure: false,
        }
    }
}

pub fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|error: toml::de::Error| {
            // TOML's Display includes the source line, which may contain a password.
            let offset = error.span().map_or(0, |span| span.start.min(text.len()));
            let line = text.as_bytes()[..offset].iter().filter(|byte| **byte == b'\n').count() + 1;
            anyhow::anyhow!("invalid configuration near line {line}: check TOML syntax, field names and value types")
        })?;
        config.validate()?;
        Ok(config)
    }
    pub fn load(path: &Path) -> Result<Self> {
        let mut config =
            Self::parse(&std::fs::read_to_string(path).context("cannot read configuration")?)?;
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        if config.database != Path::new(":memory:") && config.database.is_relative() {
            config.database = directory.join(&config.database);
        }
        if let Some(tls) = &mut config.tls {
            if tls.certificate.is_relative() {
                tls.certificate = directory.join(&tls.certificate);
            }
            if tls.private_key.is_relative() {
                tls.private_key = directory.join(&tls.private_key);
            }
        }
        if let Some(ca) = &mut config.probe.ca_certificate {
            if ca.is_relative() {
                *ca = directory.join(&*ca);
            }
        }
        Ok(config)
    }
    pub fn resolve_secrets(&mut self) -> Result<()> {
        for user in &mut self.auth.users {
            if let Some(name) = user.password_env.take() {
                user.password = std::env::var(&name).map_err(|_| {
                    anyhow::anyhow!(
                        "entry password environment variable {name} is missing or not valid UTF-8"
                    )
                })?;
            }
        }
        self.validate()
    }
    pub fn validate(&self) -> Result<()> {
        if self.database.as_os_str().is_empty() || self.max_proxies == 0 {
            bail!("database path and positive max_proxies are required");
        }
        if self.admin.enabled && self.admin.token_env.is_empty() {
            bail!("admin token_env is required");
        }
        if self.https_bind.is_some() && self.tls.is_none() {
            bail!("HTTPS listener requires certificate and private_key");
        }
        if self.auth.ttl_seconds == 0 || self.auth.ttl_seconds > 31_536_000 {
            bail!("authentication TTL must be 1..31536000 seconds");
        }
        for value in [&self.auth.default_business, &self.auth.default_session] {
            if !valid_identifier(value) {
                bail!("invalid default route identifier");
            }
        }
        let r = &self.routing;
        if r.connect_timeout_seconds == 0
            || r.handshake_timeout_seconds == 0
            || r.cooldown_seconds == 0
            || r.max_attempts == 0
            || r.max_connections == 0
        {
            bail!("invalid routing timeouts or limits");
        }
        if self.probe.timeout_seconds == 0
            || self.probe.concurrency == 0
            || self.probe.interval_seconds == 0
            || self.probe.max_age_seconds < self.probe.interval_seconds
        {
            bail!("invalid probe timing or concurrency");
        }
        for endpoint in [&self.probe.url, &self.probe.tunnel_url] {
            let probe = url::Url::parse(endpoint).context("invalid probe URL")?;
            if !matches!(probe.scheme(), "http" | "https")
                || probe.host_str().is_none()
                || !probe.username().is_empty()
                || probe.password().is_some()
            {
                bail!("probe must use HTTP(S) without embedded credentials");
            }
        }
        let mut names = HashSet::new();
        for user in &self.auth.users {
            if !valid_identifier(&user.name)
                || (user.password.is_empty() && user.password_env.is_none())
                || (!user.password.is_empty() && user.password_env.is_some())
                || user
                    .password_env
                    .as_ref()
                    .is_some_and(|name| name.is_empty())
                || user.password.len() > 255
                || user.businesses.is_empty()
                || !names.insert(&user.name)
                || user.businesses.iter().any(|b| !valid_identifier(b))
            {
                bail!("invalid or duplicate authentication user");
            }
        }
        Ok(())
    }
}
