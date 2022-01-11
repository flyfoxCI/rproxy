use std::collections::HashSet;
use std::error::Error;
use std::net::SocketAddr;
use anyhow::anyhow;
use log::error;
use redis::{Commands, Connection};

pub async fn get_proxies(redis_url:String,proxy_key:String) -> Result<Vec<SocketAddr>,Box<dyn Error>> {
    let mut proxies = Vec::new();
    let mut conn = conn_redis(redis_url).ok().unwrap();
    let results:HashSet<String> =conn.hgetall(proxy_key)?;
    for r in results {
        let proxy = match r.parse::<SocketAddr>(){
            Ok(proxy) => proxy,
            Err(err) => {
                error!(err);
                continue;
            },
        };
        proxies.push(proxy);
    }
    Ok(proxies)
}

fn conn_redis(&url:String) -> redis::RedisResult<Connection> {
    let client = redis::Client::open(url)?;
    let con = client.get_connection()?;
    Ok(con)
}
