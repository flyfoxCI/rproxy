use std::collections::HashSet;
use std::net::SocketAddr;
use anyhow::{Context};
use crate::errors::*;
use mobc::{Pool};
use mobc_redis::RedisConnectionManager;
use mobc_redis::{redis};
use mobc_redis::redis::aio::Connection;

pub async fn get_proxies(pool: &Pool<RedisConnectionManager>, redis_key: &String) -> Result<Vec<SocketAddr>> {
    let mut conn = pool.get().await?;
    let mut proxies = Vec::new();
    let results: HashSet<String> = redis::cmd("hgetall")
        .arg(redis_key)
        .query_async(&mut conn as &mut Connection)
        .await
        .context("fail to get key from redis")?;
    for r in results.iter() {
        println!("{}",r);
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