use crate::{config::Config, routing::Protocol, server::State};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::net::TcpStream;

const MAX_BODY: usize = 65536;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Import {
    protocol: Protocol,
    addresses: Vec<SocketAddr>,
    source: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    username: String,
    password: String,
}

pub fn token(config: &Config) -> Result<String> {
    let value = std::env::var(&config.admin.token_env).map_err(|_| {
        anyhow::anyhow!("management token environment variable is missing or not valid UTF-8")
    })?;
    if value.len() < 16 || value.len() > 1024 || value.bytes().any(|b| b <= 32 || b >= 127) {
        bail!("management token must contain 16..1024 printable non-space ASCII characters");
    }
    Ok(value)
}
fn response(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Cache-Control", "no-store")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}
fn error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    response(status, json!({"error":message}))
}
async fn handle(
    mut request: Request<Incoming>,
    state: Arc<State>,
    token: Arc<String>,
) -> Response<Full<Bytes>> {
    let headers: Vec<_> = request.headers().get_all("authorization").iter().collect();
    let supplied = if headers.len() == 1 {
        headers[0]
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
    } else {
        None
    };
    if !supplied.is_some_and(|s| {
        bool::from(Sha256::digest(s.as_bytes()).ct_eq(&Sha256::digest(token.as_bytes())))
    }) {
        return error(
            StatusCode::UNAUTHORIZED,
            "management authentication required",
        );
    }
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if method == Method::GET && matches!(path.as_str(), "/health" | "/v1/status") {
        return match state.router.store.status().await {
            Ok(status) => response(StatusCode::OK, serde_json::to_value(status).unwrap()),
            Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "state worker unavailable"),
        };
    }
    if method != Method::POST {
        return error(StatusCode::NOT_FOUND, "unknown management operation");
    }
    if request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request too large");
    }
    let body = tokio::time::timeout(Duration::from_secs(5), async {
        let mut body = Vec::new();
        while let Some(frame) = request.body_mut().frame().await {
            let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
            if let Ok(bytes) = frame.into_data() {
                if body.len().saturating_add(bytes.len()) > MAX_BODY {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                body.extend_from_slice(&bytes);
            }
        }
        Ok(body)
    })
    .await;
    let body = match body {
        Ok(Ok(body)) => body,
        Ok(Err(status)) => return error(status, "invalid or oversized request"),
        Err(_) => return error(StatusCode::REQUEST_TIMEOUT, "request body timed out"),
    };
    match path.as_str() {
        "/v1/proxies" => {
            let import: Import = match serde_json::from_slice(&body) {
                Ok(import) => import,
                Err(_) => return error(StatusCode::BAD_REQUEST, "invalid candidate batch"),
            };
            if import.addresses.is_empty()
                || import.addresses.len() > 256
                || import.source.is_empty()
                || import.source.len() > 64
                || import
                    .addresses
                    .iter()
                    .any(|a| a.port() == 0 || a.ip().is_unspecified())
            {
                return error(StatusCode::BAD_REQUEST, "invalid candidate batch");
            }
            match state
                .router
                .store
                .add_candidates(import.protocol, import.addresses, import.source)
                .await
            {
                Ok(accepted) => response(StatusCode::ACCEPTED, json!({"accepted":accepted})),
                Err(_) => error(
                    StatusCode::CONFLICT,
                    "candidate submission failed (capacity or state unavailable)",
                ),
            }
        }
        "/v1/verify" => {
            state.router.store.request_probe();
            response(StatusCode::ACCEPTED, json!({"scheduled":true}))
        }
        "/v1/sessions/release" => {
            let release: Release = match serde_json::from_slice(&body) {
                Ok(release) => release,
                Err(_) => return error(StatusCode::BAD_REQUEST, "invalid release request"),
            };
            if !state.auth.required() {
                return error(
                    StatusCode::FORBIDDEN,
                    "release requires entry authentication",
                );
            }
            let identity = match state
                .authenticate(Some((&release.username, &release.password)))
                .await
            {
                Ok(identity) => identity,
                Err(_) => return error(StatusCode::FORBIDDEN, "invalid entry identity"),
            };
            match state.router.release_session(&identity).await {
                Ok(()) => response(StatusCode::OK, json!({"released_or_draining":true})),
                Err(_) => error(StatusCode::CONFLICT, "session release failed"),
            }
        }
        _ => error(StatusCode::NOT_FOUND, "unknown management operation"),
    }
}
pub async fn serve(stream: TcpStream, state: Arc<State>, token: Arc<String>) -> Result<()> {
    let service = service_fn(move |request| {
        let state = state.clone();
        let token = token.clone();
        async move { Ok::<_, Infallible>(handle(request, state, token).await) }
    });
    hyper::server::conn::http1::Builder::new()
        .keep_alive(false)
        .max_buf_size(MAX_BODY)
        .serve_connection(TokioIo::new(stream), service)
        .await?;
    Ok(())
}
pub async fn command(config: &Config, path: &str, body: Option<Value>) -> Result<String> {
    if !config.admin.enabled {
        bail!("management listener is disabled");
    }
    let mut address = config.admin.bind;
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            _ => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    let url = format!("http://{address}{path}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let request = if let Some(body) = body {
        client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
    } else {
        client.get(&url)
    };
    let response = request
        .bearer_auth(token(config)?)
        .send()
        .await
        .context("cannot contact running rproxy management listener")?;
    if !response.status().is_success() {
        bail!(
            "management operation failed with status {}",
            response.status()
        );
    }
    Ok(response.text().await?)
}
