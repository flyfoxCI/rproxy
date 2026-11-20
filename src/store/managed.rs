use super::*;
use crate::{
    managed_types::*,
    secret::{redact_config, Vault},
};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2,
};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AuthUser {
    pub id: String,
    pub name: String,
    pub businesses: Vec<String>,
    pub enabled: bool,
    pub deleted: bool,
    pub version: u64,
    pub(crate) password_hash: String,
}
impl AuthUser {
    fn view(&self) -> UserView {
        UserView {
            id: self.id.clone(),
            name: self.name.clone(),
            businesses: self.businesses.clone(),
            enabled: self.enabled,
            deleted: self.deleted,
            version: self.version,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Source {
    id: String,
    name: String,
    kind: String,
    enabled: bool,
    deleted: bool,
    version: u64,
    run_generation: u64,
    last_finished_generation: u64,
    last_run_at: u64,
    last_error: Option<String>,
    encrypted_config: String,
}
impl Source {
    fn view(&self, vault: &Vault, secrets: bool) -> Result<SourceView> {
        let config = vault.reveal(&self.encrypted_config)?;
        Ok(SourceView {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind.clone(),
            enabled: self.enabled,
            deleted: self.deleted,
            version: self.version,
            run_generation: self.run_generation,
            last_finished_generation: self.last_finished_generation,
            last_run_at: self.last_run_at,
            last_error: self.last_error.clone(),
            config: if secrets {
                config
            } else {
                redact_config(&config)
            },
        })
    }
}
pub(super) struct Managed {
    pub users: BTreeMap<String, AuthUser>,
    sources: BTreeMap<String, Source>,
    memberships: HashMap<String, HashSet<String>>,
    pub disabled: HashSet<String>,
    diagnostics: BTreeMap<String, Value>,
    pub revoked: HashSet<String>,
    vault: Vault,
}
fn id(prefix: &str) -> String {
    format!("{prefix}_{:032x}", rand::random::<u128>())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn identifier(name: &str) -> Result<()> {
    if !crate::config::valid_identifier(name) {
        bail!("invalid identifier");
    }
    Ok(())
}
fn grants(businesses: &[String]) -> Result<()> {
    if businesses.len() > 100
        || businesses
            .iter()
            .any(|b| !crate::config::valid_identifier(b))
    {
        bail!("at most 100 valid businesses are allowed");
    }
    Ok(())
}
fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() || password.len() > 255 {
        bail!("password must contain 1..255 UTF-8 bytes");
    }
    Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|hash| hash.to_string())
        .map_err(|_| anyhow::anyhow!("cannot hash password"))
}
async fn hash_async(password: String) -> Result<String> {
    static LIMIT: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();
    let permit = LIMIT
        .get_or_init(|| Arc::new(Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password(&password)
    })
    .await
    .context("password worker stopped")?
}
fn save_user(connection: &Connection, user: &AuthUser) -> Result<()> {
    connection.execute("INSERT INTO users(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![user.id,serde_json::to_string(user)?])?;
    Ok(())
}
fn save_source(connection: &Connection, source: &Source) -> Result<()> {
    connection.execute("INSERT INTO sources(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![source.id,serde_json::to_string(source)?])?;
    Ok(())
}
fn normalize_config(kind: &str, mut config: Value) -> Result<Value> {
    if !matches!(kind, "api" | "static") || !config.is_object() {
        bail!("source kind must be api/static and config must be an object");
    }
    let allowed = [
        "url",
        "protocol",
        "method",
        "params",
        "headers",
        "json",
        "response",
        "interval_seconds",
        "timeout_seconds",
        "attempts",
        "ca_certificate",
        "insecure",
    ];
    if config
        .as_object()
        .unwrap()
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        bail!("unknown source configuration field");
    }
    for (key, value) in [
        ("protocol", json!("http")),
        ("method", json!("GET")),
        ("interval_seconds", json!(60)),
        ("timeout_seconds", json!(10)),
        ("attempts", json!(3)),
        ("response", json!({"format":"text"})),
    ] {
        config
            .as_object_mut()
            .unwrap()
            .entry(key.to_string())
            .or_insert(value);
    }
    let protocol = config["protocol"]
        .as_str()
        .context("source protocol is required")?;
    protocol.parse::<Protocol>()?;
    if !matches!(config["method"].as_str(), Some("GET" | "POST")) {
        bail!("source method must be GET/POST");
    }
    for (field, max) in [
        ("interval_seconds", 86400),
        ("timeout_seconds", 300),
        ("attempts", 5),
    ] {
        if !config[field].as_u64().is_some_and(|v| v > 0 && v <= max) {
            bail!("invalid source timing or attempts");
        }
    }
    if kind == "api" {
        let url = config["url"]
            .as_str()
            .context("api source url is required")?;
        if url.is_empty() || url.len() > 8192 {
            bail!("invalid source url");
        }
        if !url.contains("${") {
            let parsed = url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid source URL"))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                bail!("source URL must be HTTP(S) without embedded credentials");
            }
        }
    }
    for key in ["params", "headers", "json"] {
        if config.get(key).is_some_and(|v| !v.is_object()) {
            bail!("request params/headers/json must be objects");
        }
    }
    if config["method"] == "GET" && config.get("json").is_some() {
        bail!("JSON request body requires POST");
    }
    if !config["response"].is_object()
        || !matches!(
            config["response"]
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("text"),
            "text" | "json"
        )
    {
        bail!("invalid source response mapping");
    }
    let mapping = config["response"].as_object().unwrap();
    let mapping_fields = [
        "format",
        "items_path",
        "address_path",
        "ip_path",
        "port_path",
        "success_path",
        "success_value",
    ];
    if mapping
        .keys()
        .any(|k| !mapping_fields.contains(&k.as_str()))
        || mapping
            .iter()
            .any(|(k, v)| k.ends_with("_path") && !v.is_string())
    {
        bail!("invalid source response fields");
    }
    if mapping.contains_key("ip_path") != mapping.contains_key("port_path")
        || (mapping.contains_key("ip_path") && mapping.contains_key("address_path"))
        || mapping.contains_key("success_path") != mapping.contains_key("success_value")
    {
        bail!("incomplete or conflicting response mapping");
    }
    if config.get("insecure").is_some_and(|v| !v.is_boolean())
        || config.get("ca_certificate").is_some_and(|v| !v.is_string())
    {
        bail!("invalid source TLS configuration");
    }
    if serde_json::to_vec(&config)?.len() > 32768 {
        bail!("source configuration too large");
    }
    Ok(config)
}
impl Managed {
    pub fn load(
        connection: &mut Connection,
        config: &Config,
        businesses: &mut BTreeMap<String, Business>,
        records: &BTreeMap<String, Candidate>,
    ) -> Result<Self> {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS users(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS sources(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS memberships(proxy_id TEXT NOT NULL,source_id TEXT NOT NULL,PRIMARY KEY(proxy_id,source_id));
            CREATE TABLE IF NOT EXISTS proxy_policy(id TEXT PRIMARY KEY,enabled INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS proxy_diagnostics(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS management_meta(name TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        let mut users = BTreeMap::new();
        {
            let mut statement = connection.prepare("SELECT id,record FROM users")?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, text) = row?;
                let user: AuthUser =
                    serde_json::from_str(&text).context("invalid persisted user record")?;
                identifier(&user.name)?;
                if user.id != id || !id.starts_with("u_") || user.version == 0 {
                    bail!("invalid persisted user identity");
                }
                if !user.deleted {
                    grants(&user.businesses)?;
                    argon2::PasswordHash::new(&user.password_hash)
                        .map_err(|_| anyhow::anyhow!("invalid persisted password hash"))?;
                }
                users.insert(id, user);
            }
        }
        let mut sources = BTreeMap::new();
        {
            let mut statement = connection.prepare("SELECT id,record FROM sources")?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, text) = row?;
                let source: Source =
                    serde_json::from_str(&text).context("invalid persisted source record")?;
                identifier(&source.name)?;
                if source.id != id || !id.starts_with("s_") || source.version == 0 {
                    bail!("invalid persisted source identity");
                }
                sources.insert(id, source);
            }
        }
        let vault = Vault::open(&config.database, !sources.is_empty())?;
        for source in sources.values() {
            normalize_config(&source.kind, vault.reveal(&source.encrypted_config)?)?;
        }
        let mut managed = Self {
            users,
            sources,
            memberships: HashMap::new(),
            disabled: HashSet::new(),
            diagnostics: BTreeMap::new(),
            revoked: HashSet::new(),
            vault,
        };
        {
            let mut statement = connection.prepare("SELECT proxy_id,source_id FROM memberships")?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (p, s) = row?;
                if !managed.sources.contains_key(&s) {
                    bail!("invalid source membership");
                }
                managed.memberships.entry(p).or_default().insert(s);
            }
        }
        {
            let mut statement =
                connection.prepare("SELECT id FROM proxy_policy WHERE enabled=0")?;
            for row in statement.query_map([], |r| r.get::<_, String>(0))? {
                managed.disabled.insert(row?);
            }
        }
        {
            let mut statement = connection.prepare("SELECT id,record FROM proxy_diagnostics")?;
            for row in
                statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (id, text) = row?;
                managed.diagnostics.insert(id, serde_json::from_str(&text)?);
            }
        }
        let initialized: i64 = connection.query_row(
            "SELECT count(*) FROM management_meta WHERE name='users_initialized'",
            [],
            |r| r.get(0),
        )?;
        if initialized == 0 {
            if !managed.users.is_empty() {
                bail!("managed-user initialization marker is missing");
            }
            let mut bootstrap = config.clone();
            bootstrap.resolve_secrets()?;
            for user in &bootstrap.auth.users {
                let entry = AuthUser {
                    id: id("u"),
                    name: user.name.clone(),
                    businesses: user.businesses.clone(),
                    enabled: true,
                    deleted: false,
                    version: user.version.max(1),
                    password_hash: hash_password(&user.password)?,
                };
                managed.users.insert(entry.id.clone(), entry);
            }
            let mut owners = HashMap::new();
            for (business_name, business) in businesses.iter() {
                for session in business.sessions.values() {
                    let unnamed = session.owner == "anonymous"
                        && !managed
                            .users
                            .values()
                            .any(|user| user.name == "anonymous" && !user.deleted);
                    if unnamed || owners.contains_key(&session.owner) {
                        continue;
                    }
                    let owner = managed
                        .users
                        .values()
                        .find(|u| u.name == session.owner)
                        .map(|u| u.id.clone())
                        .unwrap_or_else(|| {
                            let entry = AuthUser {
                                id: id("u"),
                                name: session.owner.clone(),
                                businesses: vec![business_name.clone()],
                                enabled: false,
                                deleted: true,
                                version: 1,
                                password_hash: String::new(),
                            };
                            let id = entry.id.clone();
                            managed.users.insert(id.clone(), entry);
                            id
                        });
                    owners.insert(session.owner.clone(), owner);
                }
            }
            for business in businesses.values_mut() {
                for session in business.sessions.values_mut() {
                    if let Some(owner) = owners.get(&session.owner) {
                        session.owner = owner.clone();
                    }
                }
            }
            // Persistence joins the source/membership migration transaction below.
        }
        let mut active_names = HashSet::new();
        for user in managed.users.values().filter(|u| !u.deleted) {
            if !active_names.insert(user.name.clone()) {
                bail!("duplicate active user names");
            }
        }
        let mut source_names = HashSet::new();
        for source in managed.sources.values().filter(|s| !s.deleted) {
            if !source_names.insert(source.name.clone()) {
                bail!("duplicate active source names");
            }
        }
        let transaction = connection.transaction()?;
        if initialized == 0 {
            for user in managed.users.values() {
                save_user(&transaction, user)?;
            }
            for (name, business) in businesses.iter() {
                transaction.execute(
                    "UPDATE businesses SET state=?2 WHERE name=?1",
                    params![name, serde_json::to_string(business)?],
                )?;
            }
            transaction.execute(
                "INSERT INTO management_meta VALUES('users_initialized','1')",
                [],
            )?;
        }
        for (proxy_id, candidate) in records {
            if managed
                .memberships
                .get(proxy_id)
                .is_some_and(|s| !s.is_empty())
            {
                continue;
            }
            let name = if crate::config::valid_identifier(&candidate.source) {
                candidate.source.clone()
            } else {
                format!("legacy-{:x}", Sha256::digest(candidate.source.as_bytes()))[..32]
                    .to_string()
            };
            let source = managed.static_source(&name)?;
            save_source(&transaction, &source)?;
            transaction.execute(
                "INSERT OR IGNORE INTO memberships VALUES(?1,?2)",
                params![proxy_id, source.id],
            )?;
            managed
                .memberships
                .entry(proxy_id.clone())
                .or_default()
                .insert(source.id.clone());
            managed.sources.insert(source.id.clone(), source);
        }
        transaction.execute_batch("PRAGMA user_version=2;")?;
        transaction.commit()?;
        Ok(managed)
    }
    fn user(&self, name: &str) -> Option<&AuthUser> {
        self.users
            .get(name)
            .or_else(|| self.users.values().find(|u| u.name == name && !u.deleted))
            .or_else(|| self.users.values().find(|u| u.name == name))
    }
    fn source(&self, name: &str) -> Option<&Source> {
        self.sources
            .get(name)
            .or_else(|| self.sources.values().find(|s| s.name == name && !s.deleted))
    }
    fn static_source(&self, name: &str) -> Result<Source> {
        identifier(name)?;
        if let Some(source) = self.source(name) {
            if source.kind != "static" {
                bail!("manual import requires a static source");
            }
            return Ok(source.clone());
        }
        Ok(Source {
            id: id("s"),
            name: name.into(),
            kind: "static".into(),
            enabled: true,
            deleted: false,
            version: 1,
            run_generation: 1,
            last_finished_generation: 0,
            last_run_at: 0,
            last_error: None,
            encrypted_config: self.vault.seal(&normalize_config("static", json!({}))?)?,
        })
    }
    pub fn validate_identity(&self, identity: &Identity) -> Result<()> {
        if identity.owner.starts_with("u_") {
            let user = self
                .users
                .get(&identity.owner)
                .context("entry user no longer exists")?;
            if user.deleted
                || !user.enabled
                || user.version != identity.credential_version
                || !user.businesses.contains(&identity.business)
            {
                bail!("entry identity was revoked or changed");
            }
        }
        Ok(())
    }
}
use sha2::{Digest, Sha256};

impl Store {
    pub async fn create_user(
        &self,
        name: String,
        password: String,
        businesses: Vec<String>,
    ) -> Result<UserView> {
        identifier(&name)?;
        grants(&businesses)?;
        let password_hash = hash_async(password).await?;
        self.call(move |db| {
            if db
                .managed
                .users
                .values()
                .any(|u| u.name == name && !u.deleted)
            {
                bail!("user already exists");
            }
            if db.managed.users.values().filter(|u| !u.deleted).count() >= 1000 {
                bail!("user limit reached");
            }
            let user = AuthUser {
                id: id("u"),
                name,
                businesses,
                enabled: true,
                deleted: false,
                version: 1,
                password_hash,
            };
            save_user(&db.connection, &user)?;
            let view = user.view();
            db.managed.users.insert(user.id.clone(), user);
            Ok(view)
        })
        .await
    }
    pub async fn users(&self) -> Result<Vec<UserView>> {
        self.call(|db| {
            Ok(db
                .managed
                .users
                .values()
                .filter(|u| !u.deleted)
                .map(AuthUser::view)
                .collect())
        })
        .await
    }
    pub async fn user(&self, name: String) -> Result<Option<UserView>> {
        self.call(move |db| Ok(db.managed.user(&name).map(AuthUser::view)))
            .await
    }
    pub(crate) async fn user_credentials(&self, name: &str) -> Result<Option<AuthUser>> {
        let name = name.to_owned();
        self.call(move |db| {
            Ok(db
                .managed
                .users
                .values()
                .find(|u| u.name == name && !u.deleted && u.enabled)
                .cloned())
        })
        .await
    }
    pub async fn update_user(&self, name: String, change: UserChange) -> Result<UserView> {
        if let Some(businesses) = &change.businesses {
            grants(businesses)?;
        }
        let password_hash = match change.password {
            Some(password) => Some(hash_async(password).await?),
            None => None,
        };
        let disconnect = change.disconnect;
        let result = self
            .call(move |db| {
                let mut user = db
                    .managed
                    .user(&name)
                    .filter(|u| !u.deleted)
                    .cloned()
                    .context("user not found")?;
                if change.expected_version.is_some_and(|v| v != user.version) {
                    bail!("user revision conflict");
                }
                if let Some(hash) = password_hash {
                    user.password_hash = hash;
                }
                if let Some(businesses) = change.businesses {
                    user.businesses = businesses;
                }
                if let Some(enabled) = change.enabled {
                    user.enabled = enabled;
                }
                user.version = user
                    .version
                    .checked_add(1)
                    .context("user revision exhausted")?;
                save_user(&db.connection, &user)?;
                if disconnect {
                    revoke_owner(db, &user.id);
                }
                let view = user.view();
                db.managed.users.insert(user.id.clone(), user);
                Ok(view)
            })
            .await;
        if result.is_ok() && disconnect {
            self.notify_revocations();
        }
        result
    }
    pub async fn remove_user(&self, name: String, disconnect: bool) -> Result<UserView> {
        let result = self
            .call(move |db| {
                let mut user = db.managed.user(&name).cloned().context("user not found")?;
                user.enabled = false;
                user.deleted = true;
                user.version = user
                    .version
                    .checked_add(1)
                    .context("user revision exhausted")?;
                save_user(&db.connection, &user)?;
                if disconnect {
                    revoke_owner(db, &user.id);
                }
                let view = user.view();
                db.managed.users.insert(user.id.clone(), user);
                Ok(view)
            })
            .await;
        if result.is_ok() && disconnect {
            self.notify_revocations();
        }
        result
    }
    pub async fn create_source(&self, input: SourceInput) -> Result<SourceView> {
        identifier(&input.name)?;
        let config = normalize_config(&input.kind, input.config)?;
        self.call(move |db| {
            if db.managed.source(&input.name).is_some() {
                bail!("source already exists");
            }
            if db.managed.sources.values().filter(|s| !s.deleted).count() >= 1000 {
                bail!("source limit reached");
            }
            let source = Source {
                id: id("s"),
                name: input.name,
                kind: input.kind,
                enabled: input.enabled,
                deleted: false,
                version: 1,
                run_generation: 1,
                last_finished_generation: 0,
                last_run_at: 0,
                last_error: None,
                encrypted_config: db.managed.vault.seal(&config)?,
            };
            save_source(&db.connection, &source)?;
            let view = source.view(&db.managed.vault, false)?;
            db.managed.sources.insert(source.id.clone(), source);
            Ok(view)
        })
        .await
    }
    pub async fn sources(&self) -> Result<Vec<SourceView>> {
        self.call(|db| {
            db.managed
                .sources
                .values()
                .filter(|s| !s.deleted)
                .map(|s| s.view(&db.managed.vault, false))
                .collect()
        })
        .await
    }
    pub async fn source(&self, name: String) -> Result<Option<SourceView>> {
        self.call(move |db| {
            db.managed
                .source(&name)
                .map(|s| s.view(&db.managed.vault, false))
                .transpose()
        })
        .await
    }
    pub async fn update_source(&self, name: String, change: SourceChange) -> Result<SourceView> {
        self.call(move |db| {
            let mut source = db
                .managed
                .source(&name)
                .cloned()
                .context("source not found")?;
            if change.expected_version.is_some_and(|v| v != source.version) {
                bail!("source revision conflict");
            }
            if let Some(config) = change.config {
                source.encrypted_config = db
                    .managed
                    .vault
                    .seal(&normalize_config(&source.kind, config)?)?;
            }
            if let Some(enabled) = change.enabled {
                source.enabled = enabled;
            }
            source.version = source
                .version
                .checked_add(1)
                .context("source revision exhausted")?;
            source.run_generation = source
                .run_generation
                .checked_add(1)
                .context("source run counter exhausted")?;
            save_source(&db.connection, &source)?;
            let view = source.view(&db.managed.vault, false)?;
            db.managed.sources.insert(source.id.clone(), source);
            Ok(view)
        })
        .await
    }
    pub async fn remove_source(&self, name: String) -> Result<SourceView> {
        self.call(move |db| {
            let mut source = db
                .managed
                .source(&name)
                .cloned()
                .context("source not found")?;
            source.enabled = false;
            source.deleted = true;
            source.version = source
                .version
                .checked_add(1)
                .context("source revision exhausted")?;
            save_source(&db.connection, &source)?;
            let view = source.view(&db.managed.vault, false)?;
            db.managed.sources.insert(source.id.clone(), source);
            Ok(view)
        })
        .await
    }
    pub async fn run_source(&self, name: String) -> Result<Value> {
        self.call(move|db|{
            let mut source=db.managed.source(&name).cloned().context("source not found")?;
            if !source.enabled||source.kind!="api"{bail!("source is paused or has no acquisition job");}
            source.run_generation=source.run_generation.checked_add(1).context("source run counter exhausted")?;
            save_source(&db.connection,&source)?;
            let result=json!({"source_id":source.id,"version":source.version,"run_generation":source.run_generation,"job_id":format!("{}:{}:{}",source.id,source.version,source.run_generation)});
            db.managed.sources.insert(source.id.clone(),source);Ok(result)
        }).await
    }
    pub async fn export_sources(&self, secrets: bool) -> Result<Value> {
        self.call(move |db| {
            let sources = db
                .managed
                .sources
                .values()
                .filter(|s| !s.deleted)
                .map(|s| {
                    let view = s.view(&db.managed.vault, secrets)?;
                    Ok(json!({"name":view.name,"kind":view.kind,"enabled":view.enabled,"config":view.config}))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"schema_version":1,"sources":sources}))
        })
        .await
    }
    pub async fn collector_plan(&self) -> Result<CollectorPlan> {
        self.call(|db| {
            let sources = db
                .managed
                .sources
                .values()
                .filter(|s| s.enabled && !s.deleted && s.kind == "api")
                .map(|s| {
                    let mut config = db.managed.vault.reveal(&s.encrypted_config)?;
                    config["name"] = s.name.clone().into();
                    Ok(CollectorSource {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        version: s.version,
                        run_generation: s.run_generation,
                        config,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(CollectorPlan {
                version: env!("CARGO_PKG_VERSION"),
                sources,
            })
        })
        .await
    }
    pub async fn collector_result(&self, result: CollectorResult) -> Result<usize> {
        let added = self
            .call(move |db| {
                let mut source = db
                    .managed
                    .sources
                    .get(&result.source_id)
                    .cloned()
                    .context("source not found")?;
                if source.deleted
                    || !source.enabled
                    || source.kind != "api"
                    || source.version != result.version
                    || source.run_generation != result.run_generation
                {
                    bail!("stale or inactive source work");
                }
                if result.error.as_deref().is_some_and(|e| {
                    !matches!(e, "fetch-failed" | "mapping-failed" | "env-missing")
                }) {
                    bail!("unknown collector error category");
                }
                if result.error.is_some() && !result.addresses.is_empty() {
                    bail!("failed collection cannot submit addresses");
                }
                let config = db.managed.vault.reveal(&source.encrypted_config)?;
                let protocol = config["protocol"]
                    .as_str()
                    .context("source protocol missing")?
                    .parse::<Protocol>()?;
                if result.complete || result.error.is_some() {
                    source.last_finished_generation = result.run_generation;
                    source.last_run_at = now();
                    source.last_error = result.error;
                }
                match import_into(db, protocol, result.addresses, source.clone()) {
                    Ok(count) => Ok(count),
                    Err(error) => {
                        source.last_run_at = now();
                        source.last_finished_generation = result.run_generation;
                        source.last_error = Some("submission-failed".into());
                        save_source(&db.connection, &source)?;
                        db.managed.sources.insert(source.id.clone(), source);
                        Err(error)
                    }
                }
            })
            .await?;
        if added > 0 {
            self.request_probe();
        }
        Ok(added)
    }
    pub(super) async fn import_manual(
        &self,
        protocol: Protocol,
        addresses: Vec<SocketAddr>,
        source: String,
    ) -> Result<usize> {
        let added = self
            .call(move |db| {
                let source = db.managed.static_source(&source)?;
                import_into(db, protocol, addresses, source)
            })
            .await?;
        if added > 0 {
            self.request_probe();
        }
        Ok(added)
    }
    pub async fn managed_sessions(&self) -> Result<Vec<Value>> {
        self.call(|db|{
            let mut result=Vec::new();
            for(business_name,business)in &db.businesses{for(name,session)in &business.sessions{
                let owner_name=db.managed.users.get(&session.owner).map(|u|u.name.clone()).unwrap_or_else(||session.owner.clone());
                result.push(json!({"business":business_name,"session":name,"owner":session.owner,"owner_name":owner_name,"strategy":session.strategy,"proxy_id":session.binding.as_ref().map(|p|&p.id),"egress":session.binding.as_ref().map(|p|p.egress),"connections":session.active.len(),"releasing":session.releasing}));
            }}Ok(result)
        }).await
    }
    pub async fn admin_release(&self, request: AdminRelease) -> Result<Impact> {
        identifier(&request.business)?;
        identifier(&request.session)?;
        let disconnect = request.disconnect;
        let result = self
            .call(move |db| {
                let mut business = db
                    .businesses
                    .get(&request.business)
                    .cloned()
                    .unwrap_or_default();
                let session = business.sessions.get_mut(&request.session);
                let mut impact = Impact {
                    applied: false,
                    affected_sessions: usize::from(session.is_some()),
                    active_connections: session.as_ref().map_or(0, |s| s.active.len()),
                };
                if request.dry_run {
                    return Ok(impact);
                }
                let mut tokens = Vec::new();
                if let Some(session) = session {
                    session.releasing = true;
                    session.binding = None;
                    if disconnect {
                        tokens.extend(session.active.keys().cloned());
                    }
                    if session.active.is_empty() {
                        business.sessions.remove(&request.session);
                    }
                }
                db.save_business(&request.business, business)?;
                db.managed.revoked.extend(tokens);
                impact.applied = true;
                Ok(impact)
            })
            .await;
        if result.is_ok() && disconnect {
            self.notify_revocations();
        }
        result
    }
    pub async fn proxy_views(&self) -> Result<Vec<Value>> {
        self.call(|db|{
        let mut rows=BTreeMap::new();
        for(id,candidate)in &db.records{
            let mut row=json!({"id":id,"protocol":candidate.protocol,"address":candidate.address,"sources":source_names(db,id),"enabled":!db.managed.disabled.contains(id),"state":"pending","reason":null,"egress":null,"tunnel":false,"verified_at":null});
            if db.managed.disabled.contains(id){row["state"]="disabled".into();}
            else if let Some(proxy)=&candidate.verified{row["state"]=if !proxy.stable{"unstable"}else if proxy.verified_at.saturating_add(db.max_age)<now(){"stale"}else{"ready"}.into();row["egress"]=json!(proxy.egress);row["tunnel"]=proxy.tunnel.into();row["verified_at"]=proxy.verified_at.into();}
            let impact=proxy_impact(db,id);row["active_connections"]=impact.active_connections.into();row["affected_sessions"]=impact.affected_sessions.into();rows.insert(id.clone(),row);
        }
        for(id,diagnostic)in &db.managed.diagnostics{rows.entry(id.clone()).or_insert_with(||diagnostic.clone());}
        Ok(rows.into_values().collect())
    }).await
    }
    pub async fn proxy_action(&self, action: ProxyAction) -> Result<Impact> {
        let disconnect = action.disconnect;
        let result=self.call(move|db|{
            let impact=proxy_impact(db,&action.id);
            if !db.records.contains_key(&action.id)&&!db.managed.diagnostics.contains_key(&action.id)&&!db.managed.disabled.contains(&action.id){bail!("proxy not found");}
            if action.dry_run{return Ok(impact);}
            if (action.remove||action.enabled==Some(false))&&impact.affected_sessions>0&&!action.confirm{bail!("proxy has session impact; preview and confirm first");}
            let mut record=db.records.get(&action.id).cloned();
            if action.enabled==Some(true)&&record.is_none(){
                let(protocol,address)=action.id.split_once(':').context("invalid proxy id")?;
                let protocol=protocol.parse::<Protocol>()?;let address:SocketAddr=address.parse()?;
                if address.port()==0||address.ip().is_unspecified(){bail!("invalid proxy address");}
                if db.records.len()>=db.capacity{bail!("proxy capacity reached");}
                record=Some(Candidate{protocol,address,source:"manual".into(),generation:id("g"),verified:None});
            }
            let disabled=action.remove||action.enabled==Some(false);
            let transaction=db.connection.transaction()?;
            transaction.execute("INSERT INTO proxy_policy VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET enabled=excluded.enabled",params![action.id,i32::from(!disabled)])?;
            if action.remove{transaction.execute("DELETE FROM proxies WHERE id=?1",[&action.id])?;}
            else if let Some(record)=&record{transaction.execute("INSERT INTO proxies VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![action.id,serde_json::to_string(record)?])?;}
            transaction.commit()?;
            if disabled{db.managed.disabled.insert(action.id.clone());}else{db.managed.disabled.remove(&action.id);}
            if action.remove{db.records.remove(&action.id);}else if let Some(record)=record{db.records.insert(action.id.clone(),record);}
            if disconnect{for business in db.businesses.values(){for session in business.sessions.values(){for(token,active)in &session.active{if active.proxy_id==action.id{db.managed.revoked.insert(token.clone());}}}}}
            Ok(Impact{applied:true,..impact})
        }).await;
        if result.is_ok() {
            self.request_probe();
            if disconnect {
                self.notify_revocations();
            }
        }
        result
    }
}
fn revoke_owner(db: &mut Database, owner: &str) {
    for business in db.businesses.values() {
        for session in business.sessions.values().filter(|s| s.owner == owner) {
            db.managed.revoked.extend(session.active.keys().cloned());
        }
    }
}
fn source_names(db: &Database, id: &str) -> Vec<String> {
    db.managed
        .memberships
        .get(id)
        .into_iter()
        .flatten()
        .filter_map(|id| db.managed.sources.get(id).map(|s| s.name.clone()))
        .collect()
}
fn proxy_impact(db: &Database, id: &str) -> Impact {
    let mut impact = Impact {
        applied: false,
        affected_sessions: 0,
        active_connections: 0,
    };
    for business in db.businesses.values() {
        for session in business.sessions.values() {
            let active = session.active.values().filter(|a| a.proxy_id == id).count();
            if session.binding.as_ref().is_some_and(|p| p.id == id) || active > 0 {
                impact.affected_sessions += 1;
            }
            impact.active_connections += active;
        }
    }
    impact
}
fn import_into(
    db: &mut Database,
    protocol: Protocol,
    addresses: Vec<SocketAddr>,
    source: Source,
) -> Result<usize> {
    if addresses.len() > 256
        || addresses
            .iter()
            .any(|a| a.port() == 0 || a.ip().is_unspecified())
    {
        bail!("invalid candidate batch");
    }
    let mut entries = BTreeMap::new();
    let mut ids = HashSet::new();
    for address in addresses {
        let key = format!("{protocol}:{address}");
        ids.insert(key.clone());
        if !db.records.contains_key(&key) && !db.managed.disabled.contains(&key) {
            entries.insert(
                key,
                Candidate {
                    protocol,
                    address,
                    source: source.name.clone(),
                    generation: id("g"),
                    verified: None,
                },
            );
        }
    }
    if db.records.len().saturating_add(entries.len()) > db.capacity {
        bail!("proxy capacity reached");
    }
    let transaction = db.connection.transaction()?;
    save_source(&transaction, &source)?;
    for (key, record) in &entries {
        transaction.execute(
            "INSERT INTO proxies VALUES(?1,?2)",
            params![key, serde_json::to_string(record)?],
        )?;
    }
    for key in &ids {
        transaction.execute(
            "INSERT OR IGNORE INTO memberships VALUES(?1,?2)",
            params![key, source.id],
        )?;
    }
    transaction.commit()?;
    let added = entries.len();
    db.records.extend(entries);
    for key in ids {
        db.managed
            .memberships
            .entry(key)
            .or_default()
            .insert(source.id.clone());
    }
    db.managed.sources.insert(source.id.clone(), source);
    Ok(added)
}
pub(super) fn retire(db: &mut Database, id: &str, reason: &str) -> Result<()> {
    let Some(candidate) = db.records.get(id) else {
        return Ok(());
    };
    let diagnostic = json!({"id":id,"protocol":candidate.protocol,"address":candidate.address,"sources":source_names(db,id),"enabled":false,"state":"rejected","reason":reason,"checked_at":now(),"egress":candidate.verified.as_ref().map(|p|p.egress),"active_connections":proxy_impact(db,id).active_connections});
    let transaction = db.connection.transaction()?;
    transaction.execute("INSERT INTO proxy_diagnostics VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![id,diagnostic.to_string()])?;
    transaction.execute("DELETE FROM proxies WHERE id=?1", [id])?;
    transaction.execute("DELETE FROM proxy_diagnostics WHERE id NOT IN (SELECT id FROM proxy_diagnostics ORDER BY rowid DESC LIMIT 1000)",[])?;
    transaction.commit()?;
    db.records.remove(id);
    db.managed.diagnostics.insert(id.to_owned(), diagnostic);
    if db.managed.diagnostics.len() > 1000 {
        let retained: HashSet<String> = db
            .connection
            .prepare("SELECT id FROM proxy_diagnostics")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        db.managed.diagnostics.retain(|id, _| retained.contains(id));
    }
    Ok(())
}
