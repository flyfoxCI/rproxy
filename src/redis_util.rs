use std::collections::HashSet;
use std::net::SocketAddr;
use anyhow::{Context};
use crate::errors::*;
use mobc::{Pool};
use mobc_redis::RedisConnectionManager;
use mobc_redis::{redis};
use mobc_redis::redis::aio::Connection;

pub async fn get_proxies(pool: &Pool<RedisConnectionManager>, redis_key: &String,prefix:&str) -> Result<Vec<SocketAddr>> {
    let mut conn = pool.get().await?;
    let mut proxies = Vec::new();
    let redis_key_str = format!("{}:{}", redis_key, prefix);
    let results: HashSet<String> = redis::cmd("hkeys")
        .arg(redis_key_str)
        .query_async(&mut conn as &mut Connection)
        .await
        .context("fail to get key from redis")?;
    for r in results.iter() {
        let proxy = match r.parse::<SocketAddr>() {
            Ok(proxy) => proxy,
            Err(err) => {
                error!("parse addr error: {}",err);
                continue;
            }
        };
        proxies.push(proxy);
    }
    Ok(proxies)
}
