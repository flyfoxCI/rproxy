use rproxy::{
    admin,
    auth::Authenticator,
    config::Config,
    routing::{Capability, Protocol, Proxy, Router},
    server::State,
    store::Store,
};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpListener;

async fn fixture() -> (String, Arc<State>) {
    let config = Config {
        database: ":memory:".into(),
        ..Config::default()
    };
    let store = Store::open(&config).await.unwrap();
    let state = Arc::new(State {
        auth: Authenticator::new(&config),
        router: Router::new(config.clone(), store),
        config,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let running = state.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let state = running.clone();
            tokio::spawn(async move {
                let _ = admin::serve_scoped(
                    stream,
                    state,
                    Arc::new("root-test-token-1234".into()),
                    Some(Arc::new("worker-test-token-1234".into())),
                )
                .await;
            });
        }
    });
    (format!("http://{address}"), state)
}
async fn request(
    url: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut request = client
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        request = request
            .body(body.to_string())
            .header("Content-Type", "application/json");
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let body = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    (status, body)
}
#[tokio::test]
async fn one_shot_management_responses_explicitly_close_connections() {
    let (url, _) = fixture().await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{url}/v1/info"))
        .bearer_auth("root-test-token-1234")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response
            .headers()
            .get("connection")
            .and_then(|value| value.to_str().ok()),
        Some("close")
    );
}

#[tokio::test]
async fn root_user_management_is_live_and_worker_cannot_access_root_routes() {
    let (url, state) = fixture().await;
    assert_eq!(
        request(&url, "GET", "/v1/info", "worker-test-token-1234", None)
            .await
            .0,
        403
    );
    assert_eq!(request(&url, "GET", "/v1/users", "", None).await.0, 401);
    let (status, user) = request(
        &url,
        "POST",
        "/v1/users",
        "root-test-token-1234",
        Some(json!({"name":"alice","password":"private-password-value","businesses":["shop"]})),
    )
    .await;
    assert_eq!(status, 201);
    assert!(user["id"].as_str().unwrap().starts_with("u_"));
    let credentials = state
        .authenticate(Some(("v1|alice|shop|a|stick", "private-password-value")))
        .await
        .unwrap();
    assert_eq!(credentials.owner, user["id"]);
    let (status, users) = request(&url, "GET", "/v1/users", "root-test-token-1234", None).await;
    assert_eq!(status, 200);
    assert!(!users.to_string().contains("private-password-value"));
    assert!(!users.to_string().contains("argon2"));
    assert_eq!(
        request(
            &url,
            "PATCH",
            "/v1/users/alice",
            "root-test-token-1234",
            Some(json!({"password":"new-password"}))
        )
        .await
        .0,
        200
    );
    assert!(state
        .authenticate(Some(("alice", "private-password-value")))
        .await
        .is_err());
    state
        .authenticate(Some(("alice", "new-password")))
        .await
        .unwrap();
    assert_eq!(
        request(
            &url,
            "PATCH",
            "/v1/users/alice",
            "root-test-token-1234",
            Some(json!({"businesses":[]}))
        )
        .await
        .0,
        200
    );
    assert!(state
        .authenticate(Some(("alice", "new-password")))
        .await
        .is_err());
    assert_eq!(
        request(
            &url,
            "DELETE",
            "/v1/users/alice",
            "root-test-token-1234",
            None
        )
        .await
        .0,
        200
    );
    assert!(state.authenticate(None).await.is_err());
}
#[tokio::test]
async fn source_role_version_and_protocol_are_enforced_at_candidate_commit() {
    let (url, state) = fixture().await;
    let(status,source)=request(&url,"POST","/v1/sources","root-test-token-1234",Some(json!({"name":"vendor","kind":"api","config":{"url":"${SUPPLIER_URL}","protocol":"socks5"}}))).await;
    assert_eq!(status, 201);
    let paused = request(&url, "POST", "/v1/sources", "root-test-token-1234", Some(json!({"name":"paused-vendor","kind":"api","enabled":false,"config":{"url":"https://supplier.example"}}))).await;
    assert_eq!(
        paused.0, 201,
        "paused source must be created atomically, not enabled then patched later"
    );
    let (status, plan) = request(
        &url,
        "GET",
        "/v1/collector/plan",
        "worker-test-token-1234",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(plan["sources"][0]["config"]["protocol"], "socks5");
    let result = json!({"source_id":source["id"],"version":source["version"],"run_generation":source["run_generation"],"addresses":["127.0.0.1:1080"],"complete":true,"error":null});
    assert_eq!(
        request(
            &url,
            "POST",
            "/v1/collector/results",
            "worker-test-token-1234",
            Some(result.clone())
        )
        .await
        .0,
        202
    );
    assert_eq!(
        state.router.store.candidates().await.unwrap()[0].protocol,
        Protocol::Socks5
    );
    assert_eq!(
        request(
            &url,
            "PATCH",
            "/v1/sources/vendor",
            "root-test-token-1234",
            Some(json!({"enabled":false}))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(
            &url,
            "POST",
            "/v1/collector/results",
            "worker-test-token-1234",
            Some(result)
        )
        .await
        .0,
        409
    );
    assert_eq!(
        request(
            &url,
            "POST",
            "/v1/proxies",
            "worker-test-token-1234",
            Some(json!({"protocol":"http","addresses":["127.0.0.1:80"],"source":"spoofed"}))
        )
        .await
        .0,
        403
    );
}
#[tokio::test]
async fn force_disconnect_is_not_undone_by_rapid_reenable_and_dry_run_is_readonly() {
    let (url, state) = fixture().await;
    request(
        &url,
        "POST",
        "/v1/users",
        "root-test-token-1234",
        Some(json!({"name":"alice","password":"password","businesses":["shop"]})),
    )
    .await;
    let identity = state
        .authenticate(Some(("v1|alice|shop|a|stick", "password")))
        .await
        .unwrap();
    let address = "127.0.0.1:8080".parse().unwrap();
    state
        .router
        .store
        .save_proxy(&Proxy {
            id: format!("http:{address}"),
            protocol: Protocol::Http,
            address,
            egress: "192.0.2.1".parse().unwrap(),
            stable: true,
            tunnel: true,
            verified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        })
        .await
        .unwrap();
    let reservation = state
        .router
        .reserve(&identity, Capability::Tunnel)
        .await
        .unwrap();
    let guarded = reservation.clone();
    let future = tokio::spawn(async move { guarded.guard(std::future::pending::<()>()).await });
    let (status, impact) = request(
        &url,
        "POST",
        "/v1/proxies/action",
        "root-test-token-1234",
        Some(json!({"id":reservation.proxy.id,"remove":true,"dry_run":true})),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(impact["applied"], false);
    assert_eq!(state.router.store.status().await.unwrap().candidates, 1);
    request(
        &url,
        "PATCH",
        "/v1/users/alice",
        "root-test-token-1234",
        Some(json!({"enabled":false,"disconnect":true})),
    )
    .await;
    request(
        &url,
        "PATCH",
        "/v1/users/alice",
        "root-test-token-1234",
        Some(json!({"enabled":true})),
    )
    .await;
    assert!(tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    reservation.release().await.unwrap();
    let renewed = state
        .authenticate(Some(("v1|alice|shop|a|stick", "password")))
        .await
        .unwrap();
    let next = state
        .router
        .reserve(&renewed, Capability::Tunnel)
        .await
        .unwrap();
    next.release().await.unwrap();
}
