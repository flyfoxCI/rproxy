use rproxy::{
    auth::Identity,
    config::Config,
    routing::{Capability, Protocol, Proxy, Router, Strategy},
    store::Store,
};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

fn id(business: &str, session: &str, strategy: Strategy) -> Identity {
    Identity {
        owner: "alice".into(),
        business: business.into(),
        session: session.into(),
        strategy,
        credential_version: 1,
    }
}

async fn fixture(ips: &[&str]) -> (Config, Store, Router) {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config)
        .await
        .expect("isolated SQLite state worker required");
    for (index, ip) in ips.iter().enumerate() {
        let proxy = Proxy {
            id: format!("http:127.0.0.1:{}", 20000 + index),
            protocol: Protocol::Http,
            address: format!("127.0.0.1:{}", 20000 + index).parse().unwrap(),
            egress: ip.parse().unwrap(),
            tunnel: true,
            stable: true,
            verified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        };
        store.save_proxy(&proxy).await.unwrap();
    }
    let router = Router::new(config.clone(), store.clone());
    (config, store, router)
}

#[tokio::test]
async fn stick_survives_connection_release_auth_renewal_and_router_restart() {
    let (config, store, router) = fixture(&["1.1.1.1", "2.2.2.2"]).await;
    let identity = id("shop", "a", Strategy::Stick);
    let first = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    let ip = first.proxy.egress;
    first.release().await.unwrap();
    let restarted = Router::new(config, store);
    let mut renewed = identity;
    renewed.credential_version = 2;
    let second = restarted
        .reserve(&renewed, Capability::Forward)
        .await
        .unwrap();
    assert_eq!(second.proxy.egress, ip);
    second.release().await.unwrap();
}

#[tokio::test]
async fn egress_aliases_are_exclusive_within_business_but_shared_between_businesses() {
    let (_, _, router) = fixture(&["1.1.1.1", "1.1.1.1"]).await;
    let a = router
        .reserve(&id("shop", "a", Strategy::Stick), Capability::Tunnel)
        .await
        .unwrap();
    assert!(router
        .reserve(&id("shop", "b", Strategy::Stick), Capability::Tunnel)
        .await
        .is_err());
    let b = router
        .reserve(&id("news", "b", Strategy::Stick), Capability::Tunnel)
        .await
        .unwrap();
    assert_eq!(a.proxy.egress, b.proxy.egress);
    a.release().await.unwrap();
    b.release().await.unwrap();
}

#[tokio::test]
async fn concurrent_instances_cannot_double_assign() {
    let (config, store, a) = fixture(&["1.1.1.1"]).await;
    let b = Router::new(config, store);
    let ia = id("shop", "a", Strategy::Stick);
    let ib = id("shop", "b", Strategy::Stick);
    let (a, b) = tokio::join!(
        a.reserve(&ia, Capability::Tunnel),
        b.reserve(&ib, Capability::Tunnel)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
}

#[tokio::test]
async fn parallel_stick_connections_share_one_binding() {
    let (_, _, router) = fixture(&["1.1.1.1", "2.2.2.2"]).await;
    let identity = id("shop", "a", Strategy::Stick);
    let (a, b) = tokio::join!(
        router.reserve(&identity, Capability::Tunnel),
        router.reserve(&identity, Capability::Tunnel)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.proxy.id, b.proxy.id);
    a.release().await.unwrap();
    b.release().await.unwrap();
}

#[tokio::test]
async fn random_never_repeats_previous_and_does_not_silently_degrade() {
    let (_, _, router) = fixture(&["1.1.1.1", "2.2.2.2"]).await;
    let identity = id("shop", "a", Strategy::Random);
    let a = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    let b = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    assert_ne!(a.proxy.egress, b.proxy.egress);
    a.release().await.unwrap();
    b.release().await.unwrap();
    let (_, _, single) = fixture(&["1.1.1.1"]).await;
    let a = single.reserve(&identity, Capability::Tunnel).await.unwrap();
    a.release().await.unwrap();
    assert!(single.reserve(&identity, Capability::Tunnel).await.is_err());
}

#[tokio::test]
async fn round_robin_and_shuffle_cover_unique_exits() {
    for strategy in [Strategy::RoundRobin, Strategy::Shuffle] {
        let (_, _, router) = fixture(&["1.1.1.1", "2.2.2.2", "3.3.3.3"]).await;
        let identity = id("shop", "a", strategy);
        let mut ips = HashSet::new();
        for _ in 0..3 {
            let r = router
                .reserve(&identity, Capability::Forward)
                .await
                .unwrap();
            assert!(ips.insert(r.proxy.egress));
            r.release().await.unwrap();
        }
        let next = router
            .reserve(&identity, Capability::Forward)
            .await
            .unwrap();
        assert!(ips.contains(&next.proxy.egress));
        next.release().await.unwrap();
    }
}

#[tokio::test]
async fn session_owner_and_strategy_cannot_be_changed_implicitly() {
    let (_, _, router) = fixture(&["1.1.1.1", "2.2.2.2"]).await;
    let identity = id("shop", "a", Strategy::Stick);
    let r = router
        .reserve(&identity, Capability::Forward)
        .await
        .unwrap();
    let mut other = identity.clone();
    other.owner = "bob".into();
    assert!(router.reserve(&other, Capability::Forward).await.is_err());
    other = identity;
    other.strategy = Strategy::Random;
    assert!(router.reserve(&other, Capability::Forward).await.is_err());
    r.release().await.unwrap();
}

#[tokio::test]
async fn explicit_release_waits_for_old_connections_to_drain() {
    let (_, _, router) = fixture(&["1.1.1.1"]).await;
    let identity = id("shop", "a", Strategy::Stick);
    let r = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    router.release_session(&identity).await.unwrap();
    assert!(router
        .reserve(&id("shop", "b", Strategy::Stick), Capability::Tunnel)
        .await
        .is_err());
    r.release().await.unwrap();
    let next = router
        .reserve(&id("shop", "b", Strategy::Stick), Capability::Tunnel)
        .await
        .unwrap();
    next.release().await.unwrap();
}

#[tokio::test]
async fn failed_stick_switches_once_while_old_egress_stays_reserved() {
    let (_, _, router) = fixture(&["1.1.1.1", "2.2.2.2"]).await;
    let identity = id("shop", "a", Strategy::Stick);
    let old = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    router.fail(&old, "connection refused").await.unwrap();
    let a = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    let b = router.reserve(&identity, Capability::Tunnel).await.unwrap();
    assert_ne!(old.proxy.egress, a.proxy.egress);
    assert_eq!(a.proxy.egress, b.proxy.egress);
    assert!(router
        .reserve(&id("shop", "other", Strategy::Stick), Capability::Tunnel)
        .await
        .is_err());
    old.release().await.unwrap();
    a.release().await.unwrap();
    b.release().await.unwrap();
}

#[tokio::test]
async fn live_nonstick_occupancy_is_reclaimed_only_after_release() {
    let (config, store, _) = fixture(&["1.1.1.1"]).await;
    let router = Router::new(config, store);
    let old = router
        .reserve(&id("shop", "a", Strategy::RoundRobin), Capability::Forward)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1150)).await;
    old.renew().await.unwrap();
    assert!(router
        .reserve(&id("shop", "b", Strategy::Stick), Capability::Forward)
        .await
        .is_err());
    old.release().await.unwrap();
    assert!(old.renew().await.is_err());
    let r = router
        .reserve(&id("shop", "b", Strategy::Stick), Capability::Forward)
        .await
        .unwrap();
    r.release().await.unwrap();
}
