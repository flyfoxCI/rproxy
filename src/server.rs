use anyhow::{bail, Result};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};

use crate::{
    auth::{Authenticator, Identity},
    config::Config,
    routing::{Capability, Reservation, Router},
    upstream::{connect_tunnel, ConnectError, Target},
};

pub struct State {
    pub config: Config,
    pub auth: Authenticator,
    pub router: Router,
}

impl State {
    pub async fn authenticate(&self, credentials: Option<(&str, &str)>) -> Result<Identity> {
        let identity = match credentials {
            Some((username, password)) => self.auth.verify(username, password)?,
            None => self.auth.anonymous()?,
        };
        if self.auth.required() {
            let renewed = self
                .router
                .store
                .authenticate(&identity, self.config.auth.ttl_seconds)
                .await?;
            if renewed {
                log::info!("proxy authentication validated owner={}", identity.owner);
            }
        }
        Ok(identity)
    }

    pub async fn tunnel(
        &self,
        identity: &Identity,
        target: &Target,
    ) -> Result<(TcpStream, Reservation)> {
        let mut last = None;
        for _ in 0..self.config.routing.max_attempts {
            let reservation = self.router.reserve(identity, Capability::Tunnel).await?;
            let result = reservation
                .guard(connect_tunnel(
                    &reservation.proxy,
                    target,
                    Duration::from_secs(self.config.routing.connect_timeout_seconds),
                ))
                .await?;
            match result {
                Ok(stream) => return Ok((stream, reservation)),
                Err(ConnectError::Target(code)) => {
                    reservation.release().await?;
                    return Err(ConnectError::Target(code).into());
                }
                Err(error @ ConnectError::Proxy(_)) => {
                    self.router
                        .fail(&reservation, "upstream connection/handshake failure")
                        .await?;
                    reservation.release().await?;
                    last = Some(error);
                }
            }
        }
        bail!(
            "{}",
            last.map_or("no upstream available".into(), |e| e.to_string())
        )
    }
}

pub struct Server {
    state: Arc<State>,
    http: Option<TcpListener>,
    https: Option<TcpListener>,
    socks: Option<TcpListener>,
    tls: Option<tokio_rustls::TlsAcceptor>,
    admin: Option<TcpListener>,
    admin_token: Option<Arc<String>>,
}

impl Server {
    pub async fn bind(state: Arc<State>) -> Result<Self> {
        state.config.validate()?;
        let tls = state
            .config
            .tls
            .as_ref()
            .map(crate::http::tls_acceptor)
            .transpose()?;
        async fn bind(address: Option<std::net::SocketAddr>) -> Result<Option<TcpListener>> {
            match address {
                Some(address) => Ok(Some(TcpListener::bind(address).await?)),
                None => Ok(None),
            }
        }
        let http = bind(state.config.http_bind).await?;
        let https = bind(state.config.https_bind).await?;
        let socks = bind(state.config.socks5_bind).await?;
        if http.is_none() && https.is_none() && socks.is_none() {
            bail!("no listeners enabled");
        }
        let (admin, admin_token) = if state.config.admin.enabled {
            let token = crate::admin::token(&state.config)?;
            (
                Some(TcpListener::bind(state.config.admin.bind).await?),
                Some(Arc::new(token)),
            )
        } else {
            (None, None)
        };
        Ok(Self {
            state,
            http,
            https,
            socks,
            tls,
            admin,
            admin_token,
        })
    }
    pub fn admin_address(&self) -> Option<std::net::SocketAddr> {
        self.admin.as_ref().and_then(|l| l.local_addr().ok())
    }
    pub fn http_address(&self) -> Option<std::net::SocketAddr> {
        self.http.as_ref().and_then(|l| l.local_addr().ok())
    }
    pub fn https_address(&self) -> Option<std::net::SocketAddr> {
        self.https.as_ref().and_then(|l| l.local_addr().ok())
    }
    pub fn socks_address(&self) -> Option<std::net::SocketAddr> {
        self.socks.as_ref().and_then(|l| l.local_addr().ok())
    }

    pub async fn run<F: std::future::Future<Output = ()>>(self, shutdown: F) -> Result<()> {
        for (protocol, address) in [
            ("http", self.http_address()),
            ("https", self.https_address()),
            ("socks5", self.socks_address()),
            ("admin", self.admin_address()),
        ] {
            if let Some(address) = address {
                log::info!("listening protocol={protocol} address={address}");
            }
        }
        let mut tasks = tokio::task::JoinSet::new();
        let state = self.state.clone();
        let mut changes = state.router.store.subscribe();
        tasks.spawn(async move {
            let pending = match state.router.store.candidates().await {
                Ok(candidates) => candidates.iter().any(|candidate| candidate.verified.is_none()),
                Err(_) => { log::error!("cannot restore pending verification state"); return; },
            };
            let mut verify = state.config.probe.enabled || pending;
            loop {
                if verify {
                    changes.borrow_and_update();
                    if crate::upstream::verify_pool(&state.config, &state.router.store).await.is_err() {
                        log::warn!("pool verification could not complete");
                    }
                }
                tokio::select! {
                    _ = changes.changed() => {},
                    _ = async {
                        if state.config.probe.enabled {
                            tokio::time::sleep(Duration::from_secs(state.config.probe.interval_seconds)).await;
                        } else { std::future::pending::<()>().await; }
                    } => {},
                }
                verify = true;
            }
        });
        async fn accept(
            listener: &Option<TcpListener>,
        ) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
            match listener {
                Some(listener) => listener.accept().await,
                None => std::future::pending().await,
            }
        }
        let permits = Arc::new(tokio::sync::Semaphore::new(
            self.state.config.routing.max_connections,
        ));
        let admin_permits = Arc::new(tokio::sync::Semaphore::new(32));
        tokio::pin!(shutdown);
        loop {
            let (accepted, protocol) = tokio::select! {
                _ = &mut shutdown => break,
                result = accept(&self.http) => (result, "http"),
                result = accept(&self.https) => (result, "https"),
                result = accept(&self.socks) => (result, "socks5"),
                result = accept(&self.admin) => (result, "admin"),
                _ = tasks.join_next(), if !tasks.is_empty() => continue,
            };
            let (stream, peer) = match accepted {
                Ok(result) => result,
                Err(error) => {
                    log::warn!("accept failed: {error}");
                    continue;
                }
            };
            let budget = if protocol == "admin" {
                &admin_permits
            } else {
                &permits
            };
            let permit = match budget.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    log::warn!("connection limit reached");
                    drop(stream);
                    continue;
                }
            };
            let state = self.state.clone();
            let tls = self.tls.clone();
            let admin_token = self.admin_token.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let result = match protocol {
                    "admin" => match tokio::time::timeout(
                        Duration::from_secs(20),
                        crate::admin::serve(stream, state, admin_token.unwrap()),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!("management connection timed out")),
                    },
                    "socks5" => crate::socks5::serve_socks(stream, state).await,
                    "https" => {
                        let timeout =
                            Duration::from_secs(state.config.routing.handshake_timeout_seconds);
                        match tokio::time::timeout(timeout, tls.unwrap().accept(stream)).await {
                            Ok(Ok(stream)) => crate::http::serve_http(stream, state).await,
                            _ => Err(anyhow::anyhow!("TLS ingress handshake failed")),
                        }
                    }
                    _ => crate::http::serve_http(stream, state).await,
                };
                if result.is_err() {
                    log::debug!(
                        "client connection ended with error protocol={protocol} peer={peer}"
                    );
                }
            });
        }
        self.state.router.shutdown();
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

pub async fn relay<S>(
    mut client: S,
    mut upstream: TcpStream,
    reservation: Reservation,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result = reservation
        .guard(async move { tokio::io::copy_bidirectional(&mut client, &mut upstream).await })
        .await;
    let release = reservation.release().await;
    result??;
    release?;
    Ok(())
}
