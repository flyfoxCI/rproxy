use anyhow::{bail, Context, Result};
use clap::Parser;
use rproxy::{
    args::{Args, Command},
    auth::Authenticator,
    http::tls_acceptor,
    routing::Router,
    server::{Server, State},
    store::Store,
    upstream::verify_pool,
};
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
    let config = args.load_config()?;
    if matches!(args.command, Some(Command::CheckConfig)) {
        if let Some(tls) = &config.tls {
            tls_acceptor(tls)?;
        }
        println!("Configuration valid");
        return Ok(());
    }
    let auth = Authenticator::new(&config);
    let release_identity = if let Some(Command::ReleaseSession {
        username,
        password_env,
    }) = &args.command
    {
        if !auth.required() {
            bail!("release-session requires configured authentication");
        }
        let password = std::env::var(password_env)
            .context("release credential environment variable is missing")?;
        Some(
            auth.verify(username, &password)
                .context("release authentication failed")?,
        )
    } else {
        None
    };
    let store = Store::connect(&config).await?;
    match args.command.unwrap_or(Command::Serve) {
        Command::CheckConfig => unreachable!(),
        Command::VerifyPool => {
            let count = verify_pool(&config, &store).await?;
            println!("Verified {count} stable upstreams");
        }
        Command::ReleaseSession { .. } => {
            let identity = release_identity.unwrap();
            store
                .authenticate(&identity, config.auth.ttl_seconds)
                .await?;
            Router::new(config, store)
                .release_session(&identity)
                .await?;
            println!("Session released or draining");
        }
        Command::Serve => {
            if !auth.required() {
                log::warn!("anonymous proxy mode enabled; do not expose to untrusted networks");
            }
            if config.probe.insecure {
                log::warn!("certificate verification disabled ONLY for egress probes");
            }
            let state = Arc::new(State {
                auth,
                router: Router::new(config.clone(), store),
                config,
            });
            Server::bind(state)
                .await?
                .run(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}
