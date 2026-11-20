use rproxy::{
    auth::Identity,
    config::Config,
    routing::{Capability, Protocol, Proxy, Router, Strategy},
    store::Store,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn identity(session: &str) -> Identity {
    Identity {
        owner: "alice".into(),
        business: "shop".into(),
        session: session.into(),
        strategy: Strategy::Stick,
        credential_version: 1,
    }
}
fn proxy() -> Proxy {
    Proxy {
        id: "http:127.0.0.1:8080".into(),
        protocol: Protocol::Http,
        address: "127.0.0.1:8080".parse().unwrap(),
        egress: "1.2.3.4".parse().unwrap(),
        tunnel: true,
        stable: true,
        verified_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    }
}

#[tokio::test]
async fn sqlite_reopen_preserves_stick_but_not_dead_connections() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    {
        let store = Store::open(&config).await.unwrap();
        store.save_proxy(&proxy()).await.unwrap();
        let router = Router::new(config.clone(), store);
        let reservation = router
            .reserve(&identity("a"), Capability::Tunnel)
            .await
            .unwrap();
        assert_eq!(reservation.proxy.egress, proxy().egress);
        reservation.release().await.unwrap();
    }
    let store = Store::open(&config).await.unwrap();
    let router = Router::new(config, store);
    // Check exclusion before the original session reconnects and could recreate a lost binding.
    assert!(router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .is_err());
    let again = router
        .reserve(&identity("a"), Capability::Forward)
        .await
        .unwrap();
    assert_eq!(again.proxy.egress, proxy().egress);
    assert!(router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .is_err());
    again.release().await.unwrap();
    router.release_session(&identity("a")).await.unwrap();
    let next = router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .unwrap();
    next.release().await.unwrap();
}

#[tokio::test]
async fn status_excludes_stale_verified_metadata() {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let mut stale = proxy();
    stale.verified_at = 1;
    store.save_proxy(&stale).await.unwrap();
    assert_eq!(store.status().await.unwrap().verified, 0);
}

#[tokio::test]
async fn corrupt_persisted_binding_fails_closed() {
    for egress in ["::ffff:1.2.3.4", "0.0.0.0"] {
        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            database: directory.path().join("state.sqlite3"),
            ..Config::default()
        };
        {
            let store = Store::open(&config).await.unwrap();
            store.save_proxy(&proxy()).await.unwrap();
            let router = Router::new(config.clone(), store);
            router
                .reserve(&identity("a"), Capability::Forward)
                .await
                .unwrap()
                .release()
                .await
                .unwrap();
        }
        let database = rusqlite::Connection::open(&config.database).unwrap();
        let json: String = database
            .query_row("SELECT state FROM businesses WHERE name='shop'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut state: serde_json::Value = serde_json::from_str(&json).unwrap();
        state["sessions"]["a"]["binding"]["egress"] = egress.into();
        database
            .execute(
                "UPDATE businesses SET state=?1 WHERE name='shop'",
                [state.to_string()],
            )
            .unwrap();
        drop(database);
        assert!(
            Store::open(&config).await.is_err(),
            "accepted corrupt egress {egress}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn removing_sidecar_cannot_bypass_database_owner() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let _ = std::fs::remove_file(directory.path().join("state.sqlite3.lock"));
    assert!(
        Store::open(&config).await.is_err(),
        "deleting sidecar allowed a second owner"
    );
    store.status().await.unwrap();
}

#[tokio::test]
async fn sqlite_file_rejects_a_second_owner() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    let first = Store::open(&config).await.unwrap();
    assert!(Store::open(&config).await.is_err());
    first.status().await.unwrap();
}

#[tokio::test]
async fn candidate_capacity_is_atomic_and_stale_probe_cannot_resurrect() {
    let config = Config {
        database: ":memory:".into(),
        max_proxies: 1,
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let address = proxy().address;
    assert_eq!(
        store
            .add_candidates(Protocol::Http, vec![address], "test".into())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .add_candidates(Protocol::Http, vec![address], "test".into())
            .await
            .unwrap(),
        0
    );
    assert!(store
        .add_candidates(
            Protocol::Http,
            vec!["127.0.0.1:8081".parse().unwrap()],
            "test".into()
        )
        .await
        .is_err());
    let generation = store.candidates().await.unwrap()[0].generation.clone();
    store.remove_proxy(&proxy().id).await.unwrap();
    store
        .add_candidates(Protocol::Http, vec![address], "test".into())
        .await
        .unwrap();
    assert!(!store.update_probe(&proxy(), generation).await.unwrap());
    assert!(store.proxies().await.unwrap().is_empty());
}

#[tokio::test]
async fn probe_failure_removes_unverified_candidates_but_not_recent_verified_bindings() {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    store
        .add_candidates(Protocol::Http, vec![proxy().address], "test".into())
        .await
        .unwrap();
    let candidate = store.candidates().await.unwrap().remove(0);
    assert!(store
        .probe_failed(&proxy().id, candidate.generation)
        .await
        .unwrap());
    assert_eq!(store.status().await.unwrap().candidates, 0);
    store.save_proxy(&proxy()).await.unwrap();
    let candidate = store.candidates().await.unwrap().remove(0);
    assert!(!store
        .probe_failed(&proxy().id, candidate.generation)
        .await
        .unwrap());
    assert_eq!(store.status().await.unwrap().verified, 1);
}

#[tokio::test]
async fn failed_sqlite_commit_does_not_publish_a_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    store.save_proxy(&proxy()).await.unwrap();
    let database = rusqlite::Connection::open(&config.database).unwrap();
    database.execute_batch("CREATE TRIGGER reject_binding BEFORE INSERT ON businesses BEGIN SELECT RAISE(ABORT, 'simulated disk failure'); END;").unwrap();
    let router = Router::new(config, store.clone());
    assert!(router
        .reserve(&identity("a"), Capability::Forward)
        .await
        .is_err());
    assert_eq!(store.status().await.unwrap().sessions, 0);
    assert_eq!(store.status().await.unwrap().connections, 0);
    database
        .execute_batch("DROP TRIGGER reject_binding;")
        .unwrap();
    let reservation = router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .unwrap();
    reservation.release().await.unwrap();
}

#[tokio::test]
async fn cancel_during_sqlite_wait_does_not_leak_connection_occupancy() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        database: directory.path().join("state.sqlite3"),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    store.save_proxy(&proxy()).await.unwrap();
    let database = rusqlite::Connection::open(&config.database).unwrap();
    database.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let router = Router::new(config, store.clone());
    let pending_router = router.clone();
    let task = tokio::spawn(async move {
        let mut owner = identity("a");
        owner.strategy = Strategy::RoundRobin;
        pending_router.reserve(&owner, Capability::Forward).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    task.abort();
    let _ = task.await;
    database.execute_batch("ROLLBACK;").unwrap();
    let reservation = router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .unwrap();
    reservation.release().await.unwrap();
    assert_eq!(store.status().await.unwrap().connections, 0);
}

#[tokio::test]
async fn canceled_reserve_does_not_leak_runtime_occupancy() {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    store.save_proxy(&proxy()).await.unwrap();
    let mut nonstick = identity("a");
    nonstick.strategy = Strategy::RoundRobin;
    let router = Router::new(config, store);
    let reservation = router
        .reserve(&nonstick, Capability::Forward)
        .await
        .unwrap();
    drop(reservation);
    let next = router
        .reserve(&identity("b"), Capability::Forward)
        .await
        .unwrap();
    next.release().await.unwrap();
}
