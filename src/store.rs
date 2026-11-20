use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    net::SocketAddr,
    path::Path,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{oneshot, watch, Semaphore};

use crate::{
    auth::Identity,
    config::Config,
    pool_state::Business,
    routing::{Capability, Protocol, Proxy},
};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn worker_failure_does_not_release_process_ownership_while_handles_exist() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            database: directory.path().join("state.sqlite3"),
            ..Config::default()
        };
        let store = Store::open(&config).await.unwrap();
        store
            .worker
            .sender
            .as_ref()
            .unwrap()
            .send(Box::new(|_| panic!("simulated state worker failure")))
            .unwrap();
        assert!(store.status().await.is_err());
        assert!(
            Store::open(&config).await.is_err(),
            "worker failure unlocked live process state"
        );
    }
}

mod managed;

type Job = Box<dyn FnOnce(&mut Database) + Send>;
struct Worker {
    sender: Option<mpsc::Sender<Job>>,
    thread: Option<thread::JoinHandle<()>>,
    permits: Arc<Semaphore>,
    _lock: Option<Arc<File>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}
#[derive(Clone)]
pub struct Store {
    worker: Arc<Worker>,
    changes: Arc<watch::Sender<u64>>,
    revocations: Arc<watch::Sender<u64>>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub protocol: Protocol,
    pub address: SocketAddr,
    pub source: String,
    pub generation: String,
    pub verified: Option<Proxy>,
}
#[derive(Serialize)]
pub struct Status {
    pub candidates: usize,
    pub verified: usize,
    pub sessions: usize,
    pub connections: usize,
}
struct Database {
    connection: Connection,
    _lock: Option<Arc<File>>,
    records: BTreeMap<String, Candidate>,
    businesses: BTreeMap<String, Business>,
    cooldown: HashMap<String, Instant>,
    auth: HashMap<String, (u64, Instant)>,
    capacity: usize,
    max_age: u64,
    managed: managed::Managed,
}
impl Database {
    fn open(config: &Config) -> Result<Self> {
        let memory = config.database == Path::new(":memory:");
        let lock = if memory {
            None
        } else {
            if let Some(parent) = config
                .database
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).context("cannot create database directory")?;
            }
            // Unix flock is independent of SQLite's fcntl locks: lock the database inode,
            // not a removable sidecar. Windows byte locks would block SQLite itself.
            #[cfg(unix)]
            let lock_name = config.database.clone();
            #[cfg(not(unix))]
            let lock_name = {
                let path = std::fs::canonicalize(&config.database)
                    .unwrap_or_else(|_| config.database.clone());
                let mut name = path.into_os_string();
                name.push(".lock");
                name
            };
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                // FILE_SHARE_READ | FILE_SHARE_WRITE, deliberately excluding FILE_SHARE_DELETE.
                options.share_mode(3);
            }
            let file = options
                .open(lock_name)
                .context("cannot open database ownership lock")?;
            file.try_lock_exclusive()
                .context("database is already owned by another rproxy process")?;
            Some(Arc::new(file))
        };
        let mut connection =
            Connection::open(&config.database).context("cannot open SQLite database")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > 2 {
            bail!("unsupported database schema version {version}");
        }
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS proxies (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS businesses (name TEXT PRIMARY KEY, state TEXT NOT NULL);",
        )?;
        let mut records = BTreeMap::new();
        {
            let mut statement = connection.prepare("SELECT id, record FROM proxies")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, json) = row?;
                let record: Candidate =
                    serde_json::from_str(&json).context("corrupt proxy state")?;
                if id != format!("{}:{}", record.protocol, record.address) {
                    bail!("invalid persisted proxy identity");
                }
                if let Some(proxy) = &record.verified {
                    proxy.validate()?;
                    if proxy.id != id {
                        bail!("persisted metadata belongs to another proxy");
                    }
                }
                records.insert(id, record);
            }
        }
        let mut businesses = BTreeMap::new();
        {
            let mut statement = connection.prepare("SELECT name, state FROM businesses")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (name, json) = row?;
                let mut business: Business =
                    serde_json::from_str(&json).context("corrupt routing state")?;
                if !crate::config::valid_identifier(&name) {
                    bail!("invalid persisted business identity");
                }
                business.validate()?;
                business.sessions.retain(|_, session| !session.releasing);
                businesses.insert(name, business);
            }
        }
        let managed = managed::Managed::load(&mut connection, config, &mut businesses, &records)?;
        Ok(Self {
            managed,
            connection,
            _lock: lock,
            records,
            businesses,
            cooldown: HashMap::new(),
            auth: HashMap::new(),
            capacity: config.max_proxies,
            max_age: config.probe.max_age_seconds,
        })
    }
    fn save_business(&mut self, name: &str, business: Business) -> Result<()> {
        self.connection.execute(
            "INSERT INTO businesses(name,state) VALUES (?1,?2)
            ON CONFLICT(name) DO UPDATE SET state=excluded.state",
            params![name, serde_json::to_string(&business)?],
        )?;
        self.businesses.insert(name.to_owned(), business);
        Ok(())
    }
    fn save_record(&mut self, id: String, record: Candidate) -> Result<()> {
        self.connection.execute(
            "INSERT INTO proxies(id,record) VALUES (?1,?2)
            ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, serde_json::to_string(&record)?],
        )?;
        self.records.insert(id, record);
        Ok(())
    }
}
impl Store {
    pub async fn open(config: &Config) -> Result<Self> {
        let config = config.clone();
        let (sender, receiver) = mpsc::channel::<Job>();
        let (ready, wait) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("rproxy-state".into())
            .spawn(move || {
                let mut database = match Database::open(&config) {
                    Ok(database) => {
                        let _ = ready.send(Ok(database._lock.clone()));
                        database
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Ok(job) = receiver.recv() {
                    job(&mut database);
                }
            })?;
        let lock = wait
            .await
            .context("state worker stopped during initialization")??;
        let (changes, _) = watch::channel(0);
        let (revocations, _) = watch::channel(0);
        Ok(Self {
            worker: Arc::new(Worker {
                sender: Some(sender),
                thread: Some(thread),
                permits: Arc::new(Semaphore::new(256)),
                _lock: lock,
            }),
            changes: Arc::new(changes),
            revocations: Arc::new(revocations),
        })
    }
    async fn call<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Database) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = self.worker.permits.clone().acquire_owned().await?;
        let (reply, result) = oneshot::channel();
        self.worker
            .sender
            .as_ref()
            .unwrap()
            .send(Box::new(move |database| {
                let _permit = permit;
                let _ = reply.send(job(database));
            }))
            .map_err(|_| anyhow::anyhow!("state worker stopped"))?;
        tokio::time::timeout(Duration::from_secs(10), result)
            .await
            .context("state operation timed out")?
            .context("state worker stopped")?
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
    pub fn subscribe_revocations(&self) -> watch::Receiver<u64> {
        self.revocations.subscribe()
    }
    fn notify_revocations(&self) {
        self.revocations
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub fn request_probe(&self) {
        self.changes
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub async fn status(&self) -> Result<Status> {
        self.call(|db| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            Ok(Status {
                candidates: db.records.len(),
                verified: db
                    .records
                    .values()
                    .filter(|r| {
                        r.verified.as_ref().is_some_and(|p| {
                            p.stable && p.verified_at.saturating_add(db.max_age) >= now
                        })
                    })
                    .count(),
                sessions: db.businesses.values().map(|b| b.sessions.len()).sum(),
                connections: db
                    .businesses
                    .values()
                    .flat_map(|b| b.sessions.values())
                    .map(|s| s.active.len())
                    .sum(),
            })
        })
        .await
    }
    pub async fn add_candidates(
        &self,
        protocol: Protocol,
        addresses: Vec<SocketAddr>,
        source: String,
    ) -> Result<usize> {
        self.import_manual(protocol, addresses, source).await
    }
    pub async fn candidates(&self) -> Result<Vec<Candidate>> {
        self.call(|db| {
            Ok(db
                .records
                .iter()
                .filter(|(id, _)| !db.managed.disabled.contains(*id))
                .map(|(_, r)| r.clone())
                .collect())
        })
        .await
    }
    pub async fn pool_addresses(&self) -> Result<Vec<(Protocol, SocketAddr)>> {
        Ok(self
            .candidates()
            .await?
            .into_iter()
            .map(|c| (c.protocol, c.address))
            .collect())
    }
    pub async fn save_proxy(&self, proxy: &Proxy) -> Result<()> {
        let proxy = proxy.normalized();
        proxy.validate()?;
        self.call(move |db| {
            let id = proxy.id.clone();
            if !db.records.contains_key(&id) && db.records.len() >= db.capacity {
                bail!("proxy capacity reached");
            }
            let mut record = db.records.get(&id).cloned().unwrap_or(Candidate {
                protocol: proxy.protocol,
                address: proxy.address,
                source: "local".into(),
                generation: format!("{:032x}", rand::random::<u128>()),
                verified: None,
            });
            record.verified = Some(proxy);
            db.save_record(id, record)
        })
        .await
    }
    pub async fn update_probe(&self, proxy: &Proxy, generation: String) -> Result<bool> {
        let proxy = proxy.normalized();
        proxy.validate()?;
        self.call(move |db| {
            let Some(mut record) = db
                .records
                .get(&proxy.id)
                .filter(|r| r.generation == generation)
                .cloned()
            else {
                return Ok(false);
            };
            let id = proxy.id.clone();
            if !proxy.stable {
                managed::retire(db, &id, "unstable-egress")?;
                return Ok(true);
            }
            record.verified = Some(proxy);
            db.save_record(id, record)?;
            Ok(true)
        })
        .await
    }
    pub async fn probe_failed(&self, id: &str, generation: String) -> Result<bool> {
        let id = id.to_owned();
        self.call(move |db| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let removable = db.records.get(&id).is_some_and(|r| {
                r.generation == generation
                    && r.verified
                        .as_ref()
                        .is_none_or(|p| !p.stable || p.verified_at.saturating_add(db.max_age) < now)
            });
            if removable {
                managed::retire(db, &id, "probe-failed")?;
            }
            Ok(removable)
        })
        .await
    }
    pub async fn remove_proxy(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.call(move |db| managed::retire(db, &id, "removed"))
            .await
    }
    pub async fn metadata(&self) -> Result<Vec<Proxy>> {
        self.call(|db| {
            Ok(db
                .records
                .values()
                .filter_map(|r| r.verified.clone())
                .collect())
        })
        .await
    }
    pub async fn proxies(&self) -> Result<Vec<Proxy>> {
        Ok(self
            .metadata()
            .await?
            .into_iter()
            .filter(|p| p.stable)
            .collect())
    }
    pub async fn reserve(
        &self,
        identity: &Identity,
        token: String,
        capability: Capability,
    ) -> Result<(Proxy, bool)> {
        let identity = identity.clone();
        self.call(move |db| {
            db.managed.validate_identity(&identity)?;
            let mut business = db
                .businesses
                .get(&identity.business)
                .cloned()
                .unwrap_or_default();
            let proxies: Vec<_> = db
                .records
                .iter()
                .filter(|(id, _)| !db.managed.disabled.contains(*id))
                .filter_map(|(_, r)| r.verified.clone())
                .collect();
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let result = business.reserve(
                &identity,
                token,
                capability,
                &proxies,
                &db.cooldown,
                now.saturating_sub(db.max_age),
            )?;
            db.save_business(&identity.business, business)?;
            Ok(result)
        })
        .await
    }
    pub async fn release(&self, identity: &Identity, token: &str) -> Result<()> {
        let identity = identity.clone();
        let token = token.to_owned();
        self.call(move |db| Self::release_in(db, &identity, &token))
            .await
    }
    fn release_in(db: &mut Database, identity: &Identity, token: &str) -> Result<()> {
        // Occupancy is runtime-only: cleanup must still work if the disk becomes unwritable.
        db.managed.revoked.remove(token);
        if let Some(business) = db.businesses.get_mut(&identity.business) {
            business.release(identity, token)?;
        }
        Ok(())
    }
    pub fn release_now(&self, identity: Identity, token: String) {
        let _ = self
            .worker
            .sender
            .as_ref()
            .unwrap()
            .send(Box::new(move |db| {
                if let Err(error) = Self::release_in(db, &identity, &token) {
                    log::warn!("connection cleanup failed: {error}");
                }
            }));
    }
    pub async fn active(&self, identity: &Identity, token: &str) -> Result<()> {
        let identity = identity.clone();
        let token = token.to_owned();
        self.call(move |db| {
            if db.managed.revoked.contains(&token) {
                bail!("connection revoked by administrator");
            }
            let session = db
                .businesses
                .get_mut(&identity.business)
                .map(|b| b.session(&identity))
                .transpose()?
                .flatten();
            if !session.is_some_and(|s| s.active.contains_key(&token)) {
                bail!("connection occupancy released");
            }
            Ok(())
        })
        .await
    }
    pub async fn fail(
        &self,
        identity: &Identity,
        proxy_id: &str,
        cooldown: Duration,
    ) -> Result<()> {
        let identity = identity.clone();
        let proxy_id = proxy_id.to_owned();
        self.call(move |db| {
            let mut business = db
                .businesses
                .get(&identity.business)
                .cloned()
                .unwrap_or_default();
            if let Some(session) = business.session(&identity)? {
                if session.binding.as_ref().is_some_and(|p| p.id == proxy_id) {
                    session.previous = session.binding.take();
                }
            }
            db.save_business(&identity.business, business)?;
            db.cooldown.insert(proxy_id, Instant::now() + cooldown);
            Ok(())
        })
        .await
    }
    pub async fn release_session(&self, identity: &Identity) -> Result<()> {
        let identity = identity.clone();
        self.call(move |db| {
            let mut business = db
                .businesses
                .get(&identity.business)
                .cloned()
                .unwrap_or_default();
            if let Some(session) = business.session(&identity)? {
                session.releasing = true;
                session.binding = None;
                if session.active.is_empty() {
                    business.sessions.remove(&identity.session);
                }
            }
            db.save_business(&identity.business, business)
        })
        .await
    }
    pub async fn authenticate(&self, identity: &Identity, ttl: u64) -> Result<bool> {
        let identity = identity.clone();
        self.call(move |db| {
            let now = Instant::now();
            if db
                .auth
                .get(&identity.owner)
                .is_some_and(|(version, until)| {
                    *version == identity.credential_version && *until > now
                })
            {
                return Ok(false);
            }
            db.auth.insert(
                identity.owner,
                (identity.credential_version, now + Duration::from_secs(ttl)),
            );
            Ok(true)
        })
        .await
    }
}
