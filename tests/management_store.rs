use rproxy::{
    auth::Authenticator,
    config::Config,
    managed_types::{CollectorResult, SourceChange, SourceInput, UserChange},
    routing::{Capability, Protocol, Proxy, Router},
    store::Store,
};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn config() -> Config {
    Config {
        database: ":memory:".into(),
        ..Config::default()
    }
}
fn proxy() -> Proxy {
    Proxy {
        id: "http:127.0.0.1:8080".into(),
        protocol: Protocol::Http,
        address: "127.0.0.1:8080".parse().unwrap(),
        egress: "192.0.2.1".parse().unwrap(),
        stable: true,
        tunnel: true,
        verified_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    }
}

#[tokio::test]
async fn managed_password_rotation_keeps_stick_and_delete_last_never_enables_anonymous() {
    let config = config();
    let store = Store::open(&config).await.unwrap();
    let auth = Authenticator::new(&config);
    let user = store
        .create_user("alice".into(), "old-password".into(), vec!["shop".into()])
        .await
        .unwrap();
    assert!(user.id.starts_with("u_"));
    assert!(
        auth.verify_live(&store, &user.id, "old-password")
            .await
            .is_err(),
        "entry login accepted an internal management ID as a username"
    );
    let identity = auth
        .verify_live(&store, "v1|alice|shop|account-a|stick", "old-password")
        .await
        .unwrap();
    store.save_proxy(&proxy()).await.unwrap();
    let router = Router::new(config.clone(), store.clone());
    let first = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    first.release().await.unwrap();
    store
        .update_user(
            "alice".into(),
            UserChange {
                password: Some("new-password".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(auth
        .verify_live(&store, "alice", "old-password")
        .await
        .is_err());
    let renewed = auth
        .verify_live(&store, "v1|alice|shop|account-a|stick", "new-password")
        .await
        .unwrap();
    let next = router.reserve(&renewed, Capability::Tunnel).await.unwrap();
    assert_eq!(first.proxy.egress, next.proxy.egress);
    next.release().await.unwrap();
    store.remove_user("alice".into(), false).await.unwrap();
    assert!(auth.anonymous().is_err());
    assert!(auth
        .verify_live(&store, "alice", "new-password")
        .await
        .is_err());
    let replacement = store
        .create_user("alice".into(), "replacement".into(), vec!["shop".into()])
        .await
        .unwrap();
    assert_ne!(replacement.id, user.id);
    let replacement = auth
        .verify_live(&store, "v1|alice|shop|account-a|stick", "replacement")
        .await
        .unwrap();
    assert!(router
        .reserve(&replacement, Capability::Tunnel)
        .await
        .is_err());
    let public = serde_json::to_string(&store.users().await.unwrap()).unwrap();
    assert!(!public.contains("password") && !public.contains("argon2"));
}

#[tokio::test]
async fn migration_bootstraps_once_and_preserves_existing_owner_binding() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.sqlite3");
    let database = rusqlite::Connection::open(&path).unwrap();
    database.execute_batch("CREATE TABLE proxies(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE businesses(name TEXT PRIMARY KEY,state TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    database.execute("INSERT INTO proxies VALUES(?1,?2)", rusqlite::params![proxy().id, json!({"protocol":"http","address":"127.0.0.1:8080","source":"legacy","generation":"old","verified":proxy()}).to_string()]).unwrap();
    database.execute("INSERT INTO businesses VALUES('shop',?1)", [json!({"cursor":0,"sessions":{"account-a":{"owner":"alice","strategy":"stick","binding":proxy(),"previous":null,"last":"192.0.2.1","seen":[],"releasing":false}}}).to_string()]).unwrap();
    drop(database);
    let mut config =
        Config::parse("[[auth.users]]\nname='alice'\npassword='old-password'\nbusinesses=['shop']")
            .unwrap();
    config.database = path;
    let user_id;
    {
        let store = Store::open(&config).await.unwrap();
        user_id = store.users().await.unwrap()[0].id.clone();
        store
            .update_user(
                "alice".into(),
                UserChange {
                    password: Some("new-password".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let store = Store::open(&config).await.unwrap();
    let auth = Authenticator::new(&config);
    assert!(auth
        .verify_live(&store, "alice", "old-password")
        .await
        .is_err());
    let owner = auth
        .verify_live(&store, "v1|alice|shop|account-a|stick", "new-password")
        .await
        .unwrap();
    assert_eq!(owner.owner, user_id);
    let router = Router::new(config, store);
    let reservation = router.reserve(&owner, Capability::Tunnel).await.unwrap();
    assert_eq!(reservation.proxy.egress, proxy().egress);
    reservation.release().await.unwrap();
}

#[tokio::test]
async fn source_versions_reject_paused_work_and_encrypt_literal_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let source = store.create_source(SourceInput { name: "vendor-a".into(), kind: "api".into(), enabled: true, config: json!({"url":"https://supplier.example/extract?key=source-secret-value","protocol":"http","headers":{"Authorization":"Bearer private-source-value"},"params":{"api_key":12345678}}) }).await.unwrap();
    assert_eq!(source.config["params"]["api_key"], "<redacted>");
    let public = serde_json::to_string(&source).unwrap();
    assert!(!public.contains("source-secret-value") && !public.contains("private-source-value"));
    let database = rusqlite::Connection::open(&config.database).unwrap();
    let stored: String = database
        .query_row(
            "SELECT record FROM sources WHERE id=?1",
            [&source.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains("source-secret-value") && !stored.contains("private-source-value"));
    let plan = store.collector_plan().await.unwrap();
    assert_eq!(plan.sources.len(), 1);
    assert!(plan.sources[0].config["url"]
        .as_str()
        .unwrap()
        .contains("source-secret-value"));
    store
        .update_source(
            "vendor-a".into(),
            SourceChange {
                enabled: Some(false),
                expected_version: Some(source.version),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let exported = store.export_sources(false).await.unwrap();
    assert_eq!(
        exported["sources"][0]["enabled"], false,
        "export omitted a paused source's enabled state"
    );
    let old = CollectorResult {
        source_id: source.id,
        version: source.version,
        run_generation: source.run_generation,
        addresses: vec!["127.0.0.1:8080".parse().unwrap()],
        complete: true,
        error: None,
    };
    assert!(store.collector_result(old).await.is_err());
    assert!(store.collector_plan().await.unwrap().sources.is_empty());
    assert_eq!(store.status().await.unwrap().candidates, 0);
}

#[tokio::test]
async fn deleted_bootstrap_user_is_not_recreated_on_restart() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::parse(
        "[[auth.users]]\nname='alice'\npassword='bootstrap-password'\nbusinesses=['shop']",
    )
    .unwrap();
    config.database = directory.path().join("state.sqlite3");
    {
        let store = Store::open(&config).await.unwrap();
        store.remove_user("alice".into(), false).await.unwrap();
    }
    config.auth.users[0].password.clear();
    config.auth.users[0].password_env = Some(format!("RPROXY_ABSENT_{}", rand::random::<u64>()));
    let store = Store::open(&config).await.unwrap();
    assert!(store.users().await.unwrap().is_empty());
    let auth = Authenticator::new(&config);
    assert!(auth.anonymous().is_err());
    assert!(auth
        .verify_live(&store, "alice", "bootstrap-password")
        .await
        .is_err());
}

#[tokio::test]
async fn missing_source_key_fails_closed_without_generating_a_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    {
        let store = Store::open(&config).await.unwrap();
        store
            .create_source(SourceInput {
                name: "vendor".into(),
                kind: "api".into(),
                enabled: true,
                config: json!({"url":"https://supplier.example/?token=private-source-value"}),
            })
            .await
            .unwrap();
    }
    let key = directory.path().join("state.sqlite3.key");
    std::fs::remove_file(&key).unwrap();
    assert!(Store::open(&config).await.is_err());
    assert!(!key.exists(), "missing key was silently regenerated");
}

#[tokio::test]
async fn migration_distinguishes_a_named_anonymous_user_from_anonymous_mode() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::parse(
        "[[auth.users]]\nname='anonymous'\npassword='named-user-password'\nbusinesses=['shop']",
    )
    .unwrap();
    config.database = directory.path().join("state.sqlite3");
    let database = rusqlite::Connection::open(&config.database).unwrap();
    database.execute_batch("CREATE TABLE proxies(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE businesses(name TEXT PRIMARY KEY,state TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    database.execute("INSERT INTO proxies VALUES(?1,?2)",rusqlite::params![proxy().id,json!({"protocol":"http","address":"127.0.0.1:8080","source":"legacy","generation":"old","verified":proxy()}).to_string()]).unwrap();
    database.execute("INSERT INTO businesses VALUES('shop',?1)",[json!({"cursor":0,"sessions":{"a":{"owner":"anonymous","strategy":"stick","binding":proxy(),"previous":null,"last":null,"seen":[],"releasing":false}}}).to_string()]).unwrap();
    drop(database);
    let store = Store::open(&config).await.unwrap();
    let identity = Authenticator::new(&config)
        .verify_live(&store, "v1|anonymous|shop|a|stick", "named-user-password")
        .await
        .unwrap();
    let router = Router::new(config, store);
    let reservation = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    assert_eq!(reservation.proxy.egress, proxy().egress);
    reservation.release().await.unwrap();
}

#[tokio::test]
async fn failed_source_migration_rolls_back_user_and_binding_migration() {
    let directory = tempfile::tempdir().unwrap();
    let mut config =
        Config::parse("[[auth.users]]\nname='alice'\npassword='password'\nbusinesses=['shop']")
            .unwrap();
    config.database = directory.path().join("state.sqlite3");
    let database = rusqlite::Connection::open(&config.database).unwrap();
    database.execute_batch("CREATE TABLE proxies(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE businesses(name TEXT PRIMARY KEY,state TEXT NOT NULL); CREATE TABLE sources(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TRIGGER reject_source BEFORE INSERT ON sources BEGIN SELECT RAISE(ABORT, 'simulated migration failure'); END; PRAGMA user_version=1;").unwrap();
    database.execute("INSERT INTO proxies VALUES(?1,?2)",rusqlite::params![proxy().id,json!({"protocol":"http","address":"127.0.0.1:8080","source":"legacy","generation":"old","verified":proxy()}).to_string()]).unwrap();
    database.execute("INSERT INTO businesses VALUES('shop',?1)",[json!({"cursor":0,"sessions":{"a":{"owner":"alice","strategy":"stick","binding":proxy(),"previous":null,"last":null,"seen":[],"releasing":false}}}).to_string()]).unwrap();
    assert!(Store::open(&config).await.is_err());
    let state: String = database
        .query_row("SELECT state FROM businesses WHERE name='shop'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&state).unwrap()["sessions"]["a"]["owner"],
        "alice"
    );
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        database
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn two_sources_share_one_candidate_without_destructive_source_removal() {
    let config = config();
    let store = Store::open(&config).await.unwrap();
    store
        .add_candidates(Protocol::Http, vec![proxy().address], "source-a".into())
        .await
        .unwrap();
    store
        .add_candidates(Protocol::Http, vec![proxy().address], "source-b".into())
        .await
        .unwrap();
    assert_eq!(store.status().await.unwrap().candidates, 1);
    store.remove_source("source-a".into()).await.unwrap();
    assert_eq!(store.status().await.unwrap().candidates, 1);
    let sources = store.sources().await.unwrap();
    assert!(sources
        .iter()
        .any(|source| source.name == "source-b" && !source.deleted));
}
