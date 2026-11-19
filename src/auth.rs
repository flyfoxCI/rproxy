use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    config::{valid_identifier, Config},
    routing::Strategy,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub owner: String,
    pub business: String,
    pub session: String,
    pub strategy: Strategy,
    pub credential_version: u64,
}

#[derive(Clone)]
pub struct Authenticator {
    config: Config,
}

impl Authenticator {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
        }
    }
    pub fn required(&self) -> bool {
        !self.config.auth.users.is_empty()
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

    pub fn verify(&self, username: &str, password: &str) -> Result<Identity> {
        if username.len() > 255 {
            bail!("route username exceeds 255 bytes");
        }
        let fields: Vec<_> = username.split('|').collect();
        let user_name = match fields.as_slice() {
            [user] => *user,
            ["v1", user, _, _, _] => *user,
            _ => bail!("invalid route username"),
        };
        if !valid_identifier(user_name) {
            bail!("invalid user identifier");
        }
        let user = self.config.auth.users.iter().find(|u| u.name == user_name);
        let expected = Sha256::digest(user.map_or("", |u| u.password.as_str()).as_bytes());
        let supplied = Sha256::digest(password.as_bytes());
        if user.is_none() || !bool::from(expected.ct_eq(&supplied)) {
            bail!("invalid credentials");
        }
        let user = user.unwrap();
        let (business, session, strategy) = if fields.len() == 5 {
            (fields[2], fields[3], fields[4].parse()?)
        } else {
            (
                user.businesses[0].as_str(),
                self.config.auth.default_session.as_str(),
                self.config.auth.default_strategy,
            )
        };
        if !valid_identifier(business)
            || !valid_identifier(session)
            || !user.businesses.iter().any(|b| b == business)
        {
            bail!("invalid or unauthorized route identity");
        }
        Ok(Identity {
            owner: user.name.clone(),
            business: business.into(),
            session: session.into(),
            strategy,
            credential_version: user.version,
        })
    }
}
