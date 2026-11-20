use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::IpAddr,
    time::Instant,
};

use crate::{
    auth::Identity,
    routing::{Capability, Proxy, Strategy},
};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Business {
    pub sessions: BTreeMap<String, Session>,
    pub cursor: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    pub owner: String,
    pub strategy: Strategy,
    pub binding: Option<Proxy>,
    pub previous: Option<Proxy>,
    pub last: Option<IpAddr>,
    pub seen: HashSet<IpAddr>,
    pub releasing: bool,
    #[serde(skip)]
    pub active: HashMap<String, IpAddr>,
}
impl Business {
    pub fn validate(&self) -> Result<()> {
        let mut occupied = HashSet::new();
        if self.sessions.len() > 10000 {
            bail!("invalid persisted session count");
        }
        for (name, session) in &self.sessions {
            if !crate::config::valid_identifier(name)
                || !crate::config::valid_identifier(&session.owner)
            {
                bail!("invalid persisted session identity");
            }
            for proxy in session.binding.iter().chain(session.previous.iter()) {
                proxy.validate()?;
            }
            if let Some(proxy) = &session.binding {
                if session.strategy != Strategy::Stick
                    || session.releasing
                    || !occupied.insert(proxy.egress)
                {
                    bail!("invalid or conflicting persisted binding");
                }
            }
            for ip in session.last.iter().chain(session.seen.iter()) {
                if ip.is_unspecified()
                    || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some())
                {
                    bail!("invalid persisted strategy history");
                }
            }
        }
        Ok(())
    }
    pub fn session(&mut self, identity: &Identity) -> Result<Option<&mut Session>> {
        let session = self.sessions.get_mut(&identity.session);
        if session.as_ref().is_some_and(|s| s.owner != identity.owner) {
            bail!("session belongs to another identity");
        }
        Ok(session)
    }
    pub fn release(&mut self, identity: &Identity, token: &str) -> Result<()> {
        if let Some(session) = self.session(identity)? {
            session.active.remove(token);
            if session.releasing && session.active.is_empty() {
                self.sessions.remove(&identity.session);
            }
        }
        Ok(())
    }
    pub fn reserve(
        &mut self,
        identity: &Identity,
        token: String,
        capability: Capability,
        proxies: &[Proxy],
        cooldown: &HashMap<String, Instant>,
        verified_after: u64,
    ) -> Result<(Proxy, bool)> {
        if let Some(session) = self.session(identity)? {
            if session.releasing {
                bail!("session is draining");
            }
            if session.strategy != identity.strategy {
                bail!("release session before changing strategy");
            }
        } else {
            if self.sessions.len() >= 10000 {
                bail!("business session limit reached");
            }
            self.sessions.insert(
                identity.session.clone(),
                Session {
                    owner: identity.owner.clone(),
                    strategy: identity.strategy,
                    binding: None,
                    previous: None,
                    last: None,
                    seen: HashSet::new(),
                    releasing: false,
                    active: HashMap::new(),
                },
            );
        }
        let mut occupied = HashMap::new();
        for (name, session) in &self.sessions {
            if let Some(proxy) = &session.binding {
                occupied.insert(proxy.egress, name);
            }
            for egress in session.active.values() {
                occupied.insert(*egress, name);
            }
        }
        let fresh = |proxy: &&Proxy| {
            proxy.stable
                && proxy.verified_at >= verified_after
                && !cooldown
                    .get(&proxy.id)
                    .is_some_and(|until| *until > Instant::now())
        };
        let mut exits = HashSet::new();
        let candidates: Vec<_> = proxies
            .iter()
            .filter(fresh)
            .filter(|p| {
                occupied
                    .get(&p.egress)
                    .is_none_or(|owner| **owner == identity.session)
                    && (!matches!(capability, Capability::Tunnel) || p.tunnel)
                    && exits.insert(p.egress)
            })
            .collect();
        let session = self.sessions.get_mut(&identity.session).unwrap();
        let mut chosen = None;
        if identity.strategy == Strategy::Stick {
            if let Some(bound) = &session.binding {
                if let Some(existing) = proxies
                    .iter()
                    .find(|p| p.id == bound.id && p.egress == bound.egress && p.stable)
                {
                    if !fresh(&existing) {
                        bail!("bound proxy is cooling down or awaiting fresh verification");
                    }
                    if matches!(capability, Capability::Tunnel) && !existing.tunnel {
                        bail!("bound proxy cannot create target tunnel");
                    }
                    chosen = Some(existing.clone());
                } else {
                    session.previous = session.binding.take();
                }
            }
        }
        let mut changed = false;
        let proxy = if let Some(proxy) = chosen {
            proxy
        } else {
            if candidates.is_empty() {
                bail!("no unoccupied verified egress available");
            }
            let eligible: Vec<_> = match identity.strategy {
                Strategy::Random => candidates
                    .iter()
                    .copied()
                    .filter(|p| Some(p.egress) != session.last)
                    .collect(),
                Strategy::Shuffle => {
                    let remaining: Vec<_> = candidates
                        .iter()
                        .copied()
                        .filter(|p| !session.seen.contains(&p.egress))
                        .collect();
                    if remaining.is_empty() {
                        session.seen.clear();
                        candidates
                            .iter()
                            .copied()
                            .filter(|p| candidates.len() == 1 || Some(p.egress) != session.last)
                            .collect()
                    } else {
                        remaining
                    }
                }
                _ => candidates,
            };
            if eligible.is_empty() {
                bail!("no different egress available for random");
            }
            let index = if identity.strategy == Strategy::RoundRobin {
                let index = (self.cursor % eligible.len() as u64) as usize;
                self.cursor = self.cursor.wrapping_add(1);
                index
            } else {
                (rand::random::<u64>() % eligible.len() as u64) as usize
            };
            let proxy = eligible[index].clone();
            if identity.strategy == Strategy::Stick {
                changed = session.previous.take().is_some();
                session.binding = Some(proxy.clone());
            }
            proxy
        };
        session.last = Some(proxy.egress);
        session.seen.insert(proxy.egress);
        session.active.insert(token, proxy.egress);
        Ok((proxy, changed))
    }
}
