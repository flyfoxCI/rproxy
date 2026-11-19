use anyhow::{bail, Context, Result};
use redis::{aio::ConnectionManager, AsyncCommands};
use serde_json::{json, Value};
use std::time::Duration;

use crate::{
    auth::Identity,
    config::Config,
    routing::{Protocol, Proxy},
};

#[derive(Clone)]
pub struct Store {
    connection: ConnectionManager,
    prefix: String,
    timeout: Duration,
}

impl Store {
    pub async fn connect(config: &Config) -> Result<Self> {
        let client = redis::Client::open(config.redis_url.as_str()).context("invalid Redis URL")?;
        let timeout = Duration::from_secs(config.routing.renew_seconds);
        let connection = tokio::time::timeout(timeout, client.get_connection_manager())
            .await
            .context("Redis connection timed out")?
            .context("cannot connect to Redis")?;
        Ok(Self {
            connection,
            prefix: config.redis_key.clone(),
            timeout,
        })
    }

    pub async fn save_proxy(&self, proxy: &Proxy) -> Result<()> {
        let proxy = proxy.normalized();
        proxy.validate()?;
        let mut connection = self.connection.clone();
        let _: () = tokio::time::timeout(
            self.timeout,
            redis::pipe()
                .atomic()
                .hset_nx(
                    format!("{}:{}", self.prefix, proxy.protocol),
                    proxy.address.to_string(),
                    "{}",
                )
                .hset(
                    format!("{}:verified", self.prefix),
                    &proxy.id,
                    serde_json::to_string(&proxy)?,
                )
                .query_async(&mut connection),
        )
        .await
        .context("Redis save timed out")??;
        Ok(())
    }

    pub async fn pool_addresses(&self) -> Result<Vec<(Protocol, std::net::SocketAddr)>> {
        let mut connection = self.connection.clone();
        let mut result = Vec::new();
        for protocol in [Protocol::Http, Protocol::Socks5] {
            let addresses: Vec<String> = tokio::time::timeout(
                self.timeout,
                connection.hkeys(format!("{}:{}", self.prefix, protocol)),
            )
            .await
            .context("Redis pool read timed out")??;
            for address in addresses {
                if let Ok(address) = address.parse() {
                    result.push((protocol, address));
                }
            }
        }
        Ok(result)
    }

    pub async fn update_verified(&self, proxy: &Proxy) -> Result<()> {
        let proxy = proxy.normalized();
        proxy.validate()?;
        let mut connection = self.connection.clone();
        let _: i32 = tokio::time::timeout(self.timeout, redis::Script::new(
            "if redis.call('HEXISTS',KEYS[1],ARGV[1])==0 then return 0 end; redis.call('HSET',KEYS[2],ARGV[2],ARGV[3]); return 1")
            .key(format!("{}:{}", self.prefix, proxy.protocol))
            .key(format!("{}:verified", self.prefix))
            .arg(proxy.address.to_string()).arg(&proxy.id).arg(serde_json::to_string(&proxy)?)
            .invoke_async(&mut connection)).await.context("Redis metadata update timed out")??;
        Ok(())
    }

    pub async fn metadata(&self) -> Result<Vec<Proxy>> {
        let mut connection = self.connection.clone();
        let values: Vec<String> = tokio::time::timeout(
            self.timeout,
            connection.hvals(format!("{}:verified", self.prefix)),
        )
        .await
        .context("Redis metadata read timed out")??;
        let mut proxies: Vec<_> = values
            .into_iter()
            .filter_map(|value| {
                let proxy: Proxy = serde_json::from_str(&value).ok()?;
                proxy.validate().is_ok().then_some(proxy)
            })
            .collect();
        proxies.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(proxies)
    }

    pub async fn proxies(&self) -> Result<Vec<Proxy>> {
        Ok(self
            .metadata()
            .await?
            .into_iter()
            .filter(|proxy| proxy.stable)
            .collect())
    }

    pub async fn operation(&self, identity: &Identity, mut request: Value) -> Result<Value> {
        request["identity"] = json!({
            "owner": identity.owner, "session": identity.session, "strategy": identity.strategy,
        });
        let mut connection = self.connection.clone();
        let script = redis::Script::new(include_str!("lua/routing.lua"));
        let output: String = tokio::time::timeout(
            self.timeout,
            script
                .key(format!("{}:route:{}", self.prefix, identity.business))
                .key(format!("{}:cooldown", self.prefix))
                .key(format!("{}:http", self.prefix))
                .key(format!("{}:socks5", self.prefix))
                .key(format!("{}:verified", self.prefix))
                .arg(serde_json::to_string(&request)?)
                .invoke_async(&mut connection),
        )
        .await
        .context("Redis routing timed out")??;
        let output: Value = serde_json::from_str(&output)?;
        if let Some(error) = output.get("error").and_then(Value::as_str) {
            bail!("{error}");
        }
        Ok(output)
    }

    pub async fn authenticate(&self, identity: &Identity, ttl: u64) -> Result<bool> {
        let mut connection = self.connection.clone();
        let renewed: i32 = tokio::time::timeout(self.timeout, redis::Script::new(
            "local v=redis.call('GET',KEYS[1]); if v~=ARGV[1] then redis.call('SET',KEYS[1],ARGV[1],'EX',ARGV[2]); return 1 end; return 0")
            .key(format!("{}:auth:{}", self.prefix, identity.owner))
            .arg(identity.credential_version).arg(ttl)
            .invoke_async(&mut connection)).await.context("Redis authentication timed out")??;
        Ok(renewed == 1)
    }
}
