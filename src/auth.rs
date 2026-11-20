use crate::{
    config::{valid_identifier, AuthMode, Config},
    routing::Strategy,
    store::Store,
};
use anyhow::{bail, Context, Result};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub owner: String,
    pub business: String,
    pub session: String,
    pub strategy: Strategy,
    pub credential_version: u64,
}
impl Identity {
    pub fn same_route(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.business == other.business
            && self.session == other.session
            && self.strategy == other.strategy
    }
}
#[derive(Clone)]
struct Cached {
    version: u64,
    digest: [u8; 32],
    until: Instant,
}
#[derive(Clone)]
pub struct Authenticator {
    config: Config,
    cache: Arc<Mutex<HashMap<String, Cached>>>,
    key: [u8; 32],
    hashes: Arc<tokio::sync::Semaphore>,
}
impl Authenticator {
    pub fn new(config: &Config) -> Self {
        let mut key = [0; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self {
            config: config.clone(),
            cache: Arc::new(Mutex::new(HashMap::new())),
            key,
            hashes: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    pub fn required(&self) -> bool {
        self.config.auth.mode == AuthMode::Required
    }
    pub fn anonymous(&self) -> Result<Identity> {
        if self.required() {
            bail!("authentication required");
        }
        Ok(Identity {
            owner: "anonymous".into(),
            business: self.config.auth.default_business.clone(),
            session: self.config.auth.default_session.clone(),
            strategy: self.config.auth.default_strategy,
            credential_version: 0,
        })
    }
    fn user_name(username: &str) -> Result<&str> {
        if username.len() > 255 {
            bail!("route username exceeds 255 bytes");
        }
        let fields: Vec<_> = username.split('|').collect();
        let name = match fields.as_slice() {
            [name] => *name,
            ["v1", name, _, _, _] => *name,
            _ => bail!("invalid route username"),
        };
        if !valid_identifier(name) {
            bail!("invalid user identifier");
        }
        Ok(name)
    }
    fn identity(
        &self,
        username: &str,
        owner: String,
        businesses: &[String],
        version: u64,
    ) -> Result<Identity> {
        Self::user_name(username)?;
        let fields: Vec<_> = username.split('|').collect();
        let (business, session, strategy) = if fields.len() == 5 {
            (fields[2], fields[3], fields[4].parse()?)
        } else {
            (
                businesses
                    .first()
                    .context("user has no business grants")?
                    .as_str(),
                self.config.auth.default_session.as_str(),
                self.config.auth.default_strategy,
            )
        };
        if !valid_identifier(business)
            || !valid_identifier(session)
            || !businesses.iter().any(|b| b == business)
        {
            bail!("invalid or unauthorized route identity");
        }
        Ok(Identity {
            owner,
            business: business.into(),
            session: session.into(),
            strategy,
            credential_version: version,
        })
    }
    // Configuration-only verification is retained for legacy library callers. Network paths
    // use verify_live so a startup Config clone is never the online user authority.
    pub fn verify(&self, username: &str, password: &str) -> Result<Identity> {
        let name = Self::user_name(username)?;
        let user = self.config.auth.users.iter().find(|u| u.name == name);
        let expected = Sha256::digest(user.map_or("", |u| u.password.as_str()).as_bytes());
        if user.is_none() || !bool::from(expected.ct_eq(&Sha256::digest(password.as_bytes()))) {
            bail!("invalid credentials");
        }
        let user = user.unwrap();
        self.identity(username, user.name.clone(), &user.businesses, user.version)
    }
    pub async fn verify_live(
        &self,
        store: &Store,
        username: &str,
        password: &str,
    ) -> Result<Identity> {
        if password.is_empty() || password.len() > 255 {
            bail!("invalid credentials");
        }
        let name = Self::user_name(username)?;
        let user = store
            .user_credentials(name)
            .await?
            .context("invalid credentials")?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("fixed HMAC key");
        mac.update(user.id.as_bytes());
        mac.update(&[0]);
        mac.update(password.as_bytes());
        let digest: [u8; 32] = mac.finalize().into_bytes().into();
        let cached = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("authentication cache unavailable"))?
            .get(&user.id)
            .cloned();
        let valid_cache =
            cached.filter(|entry| entry.version == user.version && entry.until > Instant::now());
        if let Some(entry) = valid_cache {
            if !bool::from(entry.digest.ct_eq(&digest)) {
                bail!("invalid credentials");
            }
        } else {
            let password = password.to_owned();
            let hash = user.password_hash.clone();
            let permit = self.hashes.clone().acquire_owned().await?;
            let valid = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                PasswordHash::new(&hash).is_ok_and(|hash| {
                    Argon2::default()
                        .verify_password(password.as_bytes(), &hash)
                        .is_ok()
                })
            })
            .await
            .context("password verification worker stopped")?;
            if !valid {
                bail!("invalid credentials");
            }
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("authentication cache unavailable"))?;
            if cache.len() >= 1000 {
                cache.retain(|_, entry| entry.until > Instant::now());
                if cache.len() >= 1000 {
                    cache.clear();
                }
            }
            cache.insert(
                user.id.clone(),
                Cached {
                    version: user.version,
                    digest,
                    until: Instant::now() + Duration::from_secs(self.config.auth.ttl_seconds),
                },
            );
        }
        let current = store
            .user_credentials(name)
            .await?
            .context("invalid credentials")?;
        if current.id != user.id || current.version != user.version {
            bail!("credentials changed during verification; retry");
        }
        self.identity(username, current.id, &current.businesses, current.version)
    }
}
