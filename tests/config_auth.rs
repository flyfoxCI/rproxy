use rproxy::{auth::Authenticator, config::Config, routing::Strategy};

fn config() -> Config {
    Config::parse(
        r#"
        [auth]
        ttl_seconds = 60
        [[auth.users]]
        name = "alice"
        password = "secret"
        businesses = ["shop", "news"]
    "#,
    )
    .unwrap()
}

#[test]
fn route_identity_separates_authentication_business_session_and_strategy() {
    let auth = Authenticator::new(&config());
    let id = auth
        .verify("v1|alice|shop|account-1|stick", "secret")
        .unwrap();
    assert_eq!(id.owner, "alice");
    assert_eq!(id.business, "shop");
    assert_eq!(id.session, "account-1");
    assert_eq!(id.strategy, Strategy::Stick);
}

#[test]
fn wrong_password_unknown_strategy_and_unauthorized_business_are_rejected() {
    let auth = Authenticator::new(&config());
    for (name, password) in [
        ("v1|alice|shop|a|stick", "wrong"),
        ("v1|alice|bank|a|stick", "secret"),
        ("v1|alice|shop|a|unknown", "secret"),
        ("v1|alice|shop||stick", "secret"),
        ("v1|alice|shop|a|stick|extra", "secret"),
    ] {
        assert!(auth.verify(name, password).is_err(), "accepted {name}");
    }
    assert!(auth.anonymous().is_err());
}

#[test]
fn anonymous_requires_explicit_unauthenticated_configuration() {
    assert!(Authenticator::new(&Config::default()).anonymous().is_err());
    let config = Config::parse("[auth]\nmode='anonymous'").unwrap();
    let id = Authenticator::new(&config).anonymous().unwrap();
    assert_eq!(id.strategy, Strategy::Random);
    assert_eq!(id.business, "default");
}

#[test]
fn configuration_rejects_insecure_ambiguities() {
    for text in [
        "[[auth.users]]\nname='alice'\npassword=''\nbusinesses=['shop']",
        "[tls]\ncertificate='cert.pem'",
        "[routing]\nlease_seconds=3\nrenew_seconds=3",
        "[auth]\nttl_seconds=0",
    ] {
        assert!(Config::parse(text).is_err(), "accepted {text}");
    }
}

#[test]
fn four_strategies_and_ordinary_username_defaults_work() {
    for name in ["stick", "random", "round-robin", "shuffle"] {
        assert!(name.parse::<Strategy>().is_ok());
    }
    let id = Authenticator::new(&config())
        .verify("alice", "secret")
        .unwrap();
    assert_eq!(id.session, "default");
    assert_eq!(id.business, "shop");
}
