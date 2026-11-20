use anyhow::{bail, Result};
use clap::Parser;
use rproxy::{
    admin,
    args::{Args, Command},
    auth::Authenticator,
    http::tls_acceptor,
    routing::Router,
    server::{Server, State},
    store::Store,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if args.quiet {
        "warn"
    } else if args.verbose > 0 {
        "debug"
    } else {
        "info"
    }))
    .init();
    let mut config = args.load_config()?;
    if matches!(args.command, Some(Command::CheckConfig)) {
        if let Some(tls) = &config.tls {
            tls_acceptor(tls)?;
        }
        println!("Configuration valid");
        return Ok(());
    }
    match args.command.unwrap_or(Command::Serve) {
        Command::CheckConfig => unreachable!(),
        Command::Healthcheck => {
            println!("{}", admin::command(&config, "/health", None).await?);
        }
        Command::VerifyPool => {
            println!(
                "{}",
                admin::command(&config, "/v1/verify", Some(json!({}))).await?
            );
        }
        Command::AddProxy { protocol, address } => {
            println!(
                "{}",
                admin::command(
                    &config,
                    "/v1/proxies",
                    Some(json!({"protocol":protocol,"addresses":[address],"source":"cli"}))
                )
                .await?
            );
        }
        Command::ReleaseSession {
            username,
            password_env,
        } => {
            if config.auth.users.is_empty() {
                bail!("release-session requires configured authentication");
            }
            let password = std::env::var(password_env).map_err(|_| {
                anyhow::anyhow!(
                    "release credential environment variable is missing or not valid UTF-8"
                )
            })?;
            println!(
                "{}",
                admin::command(
                    &config,
                    "/v1/sessions/release",
                    Some(json!({"username":username,"password":password}))
                )
                .await?
            );
        }
        Command::Serve => {
            config.resolve_secrets()?;
            let auth = Authenticator::new(&config);
            if !auth.required() {
                log::warn!("anonymous proxy mode enabled; do not expose to untrusted networks");
            }
            if config.probe.insecure {
                log::warn!("certificate verification disabled ONLY for egress probes");
            }
            // Validate management credentials before creating or taking ownership of state.
            if config.admin.enabled {
                admin::token(&config)?;
            }
            let store = Store::open(&config).await?;
            let state = Arc::new(State {
                auth,
                router: Router::new(config.clone(), store),
                config,
            });
            Server::bind(state).await?.run(async {
                #[cfg(unix)]
                {
                    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("install SIGTERM handler");
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
                }
                #[cfg(not(unix))]
                { let _ = tokio::signal::ctrl_c().await; }
            }).await?;
        }
    }
    Ok(())
}
