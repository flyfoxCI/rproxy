use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::Incoming,
    header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, HOST, PROXY_AUTHORIZATION},
    service::service_fn,
    Method, Request, Response, StatusCode, Uri,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    error::Error,
    io::BufReader,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::Mutex,
};
use tokio_rustls::TlsAcceptor;

use crate::{
    auth::Identity,
    config::TlsConfig,
    routing::{Capability, Protocol, Reservation},
    server::{relay, State},
    upstream::{connect_tunnel, ConnectError, Target},
};

type BoxError = Box<dyn Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Default)]
struct ConnectionRoute {
    identity: Mutex<Option<Identity>>,
    reservation: Mutex<Option<Reservation>>,
    forwarded: AtomicBool,
}

pub fn tls_acceptor(config: &TlsConfig) -> Result<TlsAcceptor> {
    let mut cert_file = BufReader::new(
        std::fs::File::open(&config.certificate).context("cannot open TLS certificate")?,
    );
    let certificates =
        rustls_pemfile::certs(&mut cert_file).collect::<std::io::Result<Vec<_>>>()?;
    let mut key_file =
        BufReader::new(std::fs::File::open(&config.private_key).context("cannot open TLS key")?);
    let key = rustls_pemfile::private_key(&mut key_file)?.context("missing TLS private key")?;
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(tls)))
}

pub async fn serve_http<S>(stream: S, state: Arc<State>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let route = Arc::new(ConnectionRoute::default());
    let context = route.clone();
    let service_state = state.clone();
    let service = service_fn(move |request| {
        let state = service_state.clone();
        let route = context.clone();
        async move { Ok::<_, Infallible>(handle(request, state, route).await) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(
            state.config.routing.handshake_timeout_seconds,
        ))
        .max_buf_size(65536)
        .half_close(true);
    let connection = builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    let mut revocations = state.router.store.subscribe_revocations();
    loop {
        tokio::select! {
            result = &mut connection => return result.context("HTTP client connection failed"),
            _ = revocations.changed() => {
                if let Some(reservation) = route.reservation.lock().await.clone() { reservation.renew().await?; }
            }
        }
    }
}

fn error_response(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .header(CONNECTION, "close")
        .body(
            Full::new(Bytes::from_static(message.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap();
    if status == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        response.headers_mut().insert(
            "proxy-authenticate",
            HeaderValue::from_static("Basic realm=\"rproxy\""),
        );
    }
    response
}

fn credentials(headers: &HeaderMap) -> Result<Option<(String, String)>> {
    let values: Vec<_> = headers.get_all(PROXY_AUTHORIZATION).iter().collect();
    if values.is_empty() {
        return Ok(None);
    }
    if values.len() != 1 {
        bail!("multiple proxy authorization headers");
    }
    let value = values[0].to_str()?;
    let (scheme, data) = value
        .split_once(' ')
        .context("malformed proxy authorization")?;
    if !scheme.eq_ignore_ascii_case("basic") {
        bail!("only Basic proxy authentication is supported");
    }
    let decoded = String::from_utf8(STANDARD.decode(data)?)?;
    let (username, password) = decoded.split_once(':').context("malformed credentials")?;
    Ok(Some((username.to_owned(), password.to_owned())))
}

async fn handle(
    mut request: Request<Incoming>,
    state: Arc<State>,
    route: Arc<ConnectionRoute>,
) -> Response<Body> {
    let parsed = match credentials(request.headers()) {
        Ok(credentials) => credentials,
        Err(_) => {
            return error_response(
                StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                "Proxy authentication required",
            )
        }
    };
    let identity = match state
        .authenticate(
            parsed
                .as_ref()
                .map(|(user, password)| (user.as_str(), password.as_str())),
        )
        .await
    {
        Ok(identity) => identity,
        Err(_) => {
            return error_response(
                StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                "Proxy authentication required",
            )
        }
    };
    {
        let mut current = route.identity.lock().await;
        if current
            .as_ref()
            .is_some_and(|old| !old.same_route(&identity))
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Route identity cannot change on a reused connection",
            );
        }
        *current = Some(identity.clone());
    }
    let target = match request_target(&request) {
        Ok(target) => target,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid proxy target"),
    };
    if request.method() == Method::CONNECT {
        if request.headers().contains_key("transfer-encoding")
            || request
                .headers()
                .get("content-length")
                .is_some_and(|value| value != "0")
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "CONNECT must not have a request body",
            );
        }
        let result = if let Some(reservation) = route.reservation.lock().await.clone() {
            if !reservation.proxy.tunnel {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "Bound proxy does not support target tunnels",
                );
            }
            match connect_tunnel(
                &reservation.proxy,
                &target,
                Duration::from_secs(state.config.routing.connect_timeout_seconds),
            )
            .await
            {
                Ok(stream) => Ok((stream, reservation)),
                Err(error) => Err(error.into()),
            }
        } else {
            state.tunnel(&identity, &target).await
        };
        let (upstream, reservation) = match result {
            Ok(result) => result,
            Err(_) => {
                return error_response(StatusCode::BAD_GATEWAY, "Unable to establish target tunnel")
            }
        };
        *route.reservation.lock().await = Some(reservation.clone());
        let upgrade = hyper::upgrade::on(&mut request);
        tokio::spawn(async move {
            let _ = reservation
                .guard(async {
                    if let Ok(upgraded) = upgrade.await {
                        let _ = relay(TokioIo::new(upgraded), upstream, reservation.clone()).await;
                    }
                })
                .await;
        });
        return Response::new(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
    }
    match forward(request, &identity, &target, &state, &route).await {
        Ok(response) => response,
        Err(_) => error_response(StatusCode::BAD_GATEWAY, "Upstream forwarding failed"),
    }
}

fn request_target(request: &Request<Incoming>) -> Result<Target> {
    if request.method() == Method::CONNECT {
        if request.uri().scheme().is_some() || request.uri().path_and_query().is_some() {
            bail!("CONNECT requires authority form");
        }
        let authority = request
            .uri()
            .authority()
            .context("missing CONNECT authority")?;
        return Target::new(
            authority.host(),
            authority.port_u16().context("CONNECT requires port")?,
        );
    }
    if request
        .uri()
        .scheme_str()
        .is_some_and(|scheme| scheme != "http")
    {
        bail!("HTTPS targets require CONNECT");
    }
    let host;
    let authority = if let Some(authority) = request.uri().authority() {
        authority
    } else {
        host = request
            .headers()
            .get(HOST)
            .context("missing target host")?
            .to_str()?
            .parse::<hyper::http::uri::Authority>()?;
        &host
    };
    Target::new(authority.host(), authority.port_u16().unwrap_or(80))
}

fn strip_hop_headers(headers: &mut HeaderMap, upgrade: bool) {
    let nominated: Vec<_> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| name.trim().parse::<HeaderName>().ok())
        .collect();
    for name in nominated {
        if !(upgrade && name == "upgrade") {
            headers.remove(name);
        }
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        headers.remove(name);
    }
    if upgrade {
        headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    } else {
        headers.remove("upgrade");
    }
}

async fn forward(
    mut request: Request<Incoming>,
    identity: &Identity,
    target: &Target,
    state: &State,
    route: &Arc<ConnectionRoute>,
) -> Result<Response<Body>> {
    let timeout = Duration::from_secs(state.config.routing.connect_timeout_seconds);
    let mut connection = None;
    for _ in 0..state.config.routing.max_attempts {
        let reservation = {
            let mut current = route.reservation.lock().await;
            if current.is_none() {
                *current = Some(state.router.reserve(identity, Capability::Forward).await?);
            }
            current.as_ref().unwrap().clone()
        };
        let result = match reservation.proxy.protocol {
            Protocol::Http => {
                match tokio::time::timeout(timeout, TcpStream::connect(reservation.proxy.address))
                    .await
                {
                    Ok(Ok(stream)) => Ok(stream),
                    Ok(Err(error)) => Err(ConnectError::Proxy(error.into())),
                    Err(error) => Err(ConnectError::Proxy(error.into())),
                }
            }
            Protocol::Socks5 => connect_tunnel(&reservation.proxy, target, timeout).await,
        };
        match result {
            Ok(stream) => {
                connection = Some((stream, reservation));
                break;
            }
            Err(ConnectError::Target(code)) => return Err(ConnectError::Target(code).into()),
            Err(ConnectError::Proxy(_)) => {
                state
                    .router
                    .fail(&reservation, "upstream connection/handshake failure")
                    .await?;
                if route.forwarded.load(Ordering::Acquire) {
                    bail!("upstream failed on pinned HTTP connection; reconnect to change route");
                }
                reservation.release().await?;
                *route.reservation.lock().await = None;
            }
        }
    }
    let (stream, reservation) = connection.context("no upstream connection available")?;
    let upgrade = request.headers().contains_key("upgrade")
        && request
            .headers()
            .get_all(CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .any(|value| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
            });
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));
    strip_hop_headers(request.headers_mut(), upgrade);
    request
        .headers_mut()
        .insert(HOST, HeaderValue::from_str(&target.authority())?);
    let path = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    *request.uri_mut() = match reservation.proxy.protocol {
        Protocol::Http if request.uri().scheme().is_some() => request.uri().clone(),
        Protocol::Http => format!("http://{}{}", target.authority(), path).parse::<Uri>()?,
        Protocol::Socks5 => path.parse::<Uri>()?,
    };
    let (mut sender, connection) = match tokio::time::timeout(
        timeout,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    {
        Ok(Ok(connection)) => connection,
        _ => {
            state
                .router
                .fail(
                    &reservation,
                    "HTTP transport initialization failed before request",
                )
                .await?;
            bail!("unable to initialize HTTP transport");
        }
    };
    let driver_lease = reservation.clone();
    tokio::spawn(async move {
        let _ = driver_lease.guard(connection.with_upgrades()).await;
    });
    route.forwarded.store(true, Ordering::Release);
    let mut response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(error) => {
            if reservation.proxy.protocol == Protocol::Http
                && (error.is_closed() || error.is_incomplete_message() || error.is_parse())
            {
                state
                    .router
                    .fail(
                        &reservation,
                        "upstream HTTP transport failed; request not replayed",
                    )
                    .await?;
            }
            return Err(error.into());
        }
    };
    if response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        state
            .router
            .fail(&reservation, "upstream requires separate credentials")
            .await?;
        bail!("upstream authentication unsupported");
    }
    let upgraded = response.status() == StatusCode::SWITCHING_PROTOCOLS;
    if upgraded {
        let client_upgrade = client_upgrade.context("unexpected upstream protocol upgrade")?;
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        let lease = reservation.clone();
        tokio::spawn(async move {
            let _ = lease
                .guard(async move {
                    if let (Ok(client), Ok(upstream)) =
                        tokio::join!(client_upgrade, upstream_upgrade)
                    {
                        let _ = tokio::io::copy_bidirectional(
                            &mut TokioIo::new(client),
                            &mut TokioIo::new(upstream),
                        )
                        .await;
                    }
                })
                .await;
        });
    }
    strip_hop_headers(response.headers_mut(), upgraded);
    Ok(response.map(|body| {
        body.map_err(|error| -> BoxError { Box::new(error) })
            .boxed_unsync()
    }))
}
