use crate::{auth::Identity, config::Config, store::Store};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    Stick,
    Random,
    RoundRobin,
    Shuffle,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stick => "stick",
            Self::Random => "random",
            Self::RoundRobin => "round-robin",
            Self::Shuffle => "shuffle",
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Http,
    Socks5,
}
impl FromStr for Protocol {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "http" => Ok(Self::Http),
            "socks5" => Ok(Self::Socks5),
            _ => bail!("unsupported upstream protocol"),
        }
    }
}
impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proxy {
    pub id: String,
    pub protocol: Protocol,
    pub address: SocketAddr,
    pub egress: IpAddr,
    pub tunnel: bool,
    pub stable: bool,
    pub verified_at: u64,
}
impl Proxy {
    pub fn normalized(&self) -> Self {
        let mut proxy = self.clone();
        if let IpAddr::V6(ip) = proxy.egress {
            if let Some(ip) = ip.to_ipv4_mapped() {
                proxy.egress = IpAddr::V4(ip);
            }
        }
        proxy
    }
    pub fn validate(&self) -> Result<()> {
        if self.id != format!("{}:{}", self.protocol, self.address)
            || self.egress.is_unspecified()
            || self.egress != self.normalized().egress
        {
            bail!("invalid proxy metadata");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Capability {
    Forward,
    Tunnel,
}

#[derive(Clone)]
pub struct Router {
    pub config: Config,
    pub store: Store,
    shutdown: Arc<tokio::sync::watch::Sender<bool>>,
}
impl Router {
    pub fn new(config: Config, store: Store) -> Self {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self {
            config,
            store,
            shutdown: Arc::new(shutdown),
        }
    }
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    pub async fn reserve(
        &self,
        identity: &Identity,
        capability: Capability,
    ) -> Result<Reservation> {
        if *self.shutdown.borrow() {
            bail!("proxy server stopping");
        }
        // Create cleanup ownership before awaiting: cancellation must enqueue release after reserve.
        let lease = Arc::new(Lease {
            router: self.clone(),
            identity: identity.clone(),
            token: format!("{:032x}", rand::random::<u128>()),
            released: AtomicBool::new(false),
        });
        let (proxy, changed) = self
            .store
            .reserve(identity, lease.token.clone(), capability)
            .await?;
        if changed {
            log::warn!(
                "stick rebound business={} session={} new_proxy={} new_egress={}",
                identity.business,
                identity.session,
                proxy.id,
                proxy.egress
            );
        }
        Ok(Reservation { proxy, lease })
    }

    pub async fn fail(&self, reservation: &Reservation, reason: &str) -> Result<()> {
        self.store
            .fail(
                &reservation.lease.identity,
                &reservation.proxy.id,
                Duration::from_secs(self.config.routing.cooldown_seconds),
            )
            .await?;
        log::warn!(
            "upstream failed business={} session={} old_proxy={} old_egress={} reason={}",
            reservation.lease.identity.business,
            reservation.lease.identity.session,
            reservation.proxy.id,
            reservation.proxy.egress,
            reason
        );
        Ok(())
    }
    pub async fn release_session(&self, identity: &Identity) -> Result<()> {
        self.store.release_session(identity).await?;
        Ok(())
    }
}

struct Lease {
    router: Router,
    identity: Identity,
    token: String,
    released: AtomicBool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.router
                .store
                .release_now(self.identity.clone(), self.token.clone());
        }
    }
}

#[derive(Clone)]
pub struct Reservation {
    pub proxy: Proxy,
    lease: Arc<Lease>,
}
impl Reservation {
    pub async fn renew(&self) -> Result<()> {
        if self.lease.released.load(Ordering::Acquire) || *self.lease.router.shutdown.borrow() {
            bail!("lease released or server stopping");
        }
        self.lease
            .router
            .store
            .active(&self.lease.identity, &self.lease.token)
            .await?;
        Ok(())
    }
    pub async fn release(&self) -> Result<()> {
        self.lease
            .router
            .store
            .release(&self.lease.identity, &self.lease.token)
            .await?;
        self.lease.released.store(true, Ordering::Release);
        Ok(())
    }
    pub async fn guard<F: std::future::Future>(&self, future: F) -> Result<F::Output> {
        tokio::pin!(future);
        let mut shutdown = self.lease.router.shutdown.subscribe();
        if *shutdown.borrow() {
            bail!("proxy server stopping");
        }
        let mut revocations = self.lease.router.store.subscribe_revocations();
        self.renew().await?;
        loop {
            tokio::select! {
                result = &mut future => return Ok(result),
                _ = shutdown.changed() => bail!("proxy server stopping"),
                _ = revocations.changed() => self.renew().await?,
            }
        }
    }
}

impl FromStr for Strategy {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "stick" => Ok(Self::Stick),
            "random" => Ok(Self::Random),
            "round-robin" => Ok(Self::RoundRobin),
            "shuffle" => Ok(Self::Shuffle),
            _ => bail!("unknown route strategy"),
        }
    }
}
