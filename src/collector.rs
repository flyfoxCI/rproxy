use anyhow::{bail, Result};
use reqwest::{header, redirect::Policy, Client, Method};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use url::Url;

use crate::{
    config::CollectorConfig,
    managed_types::{CollectorResult, CollectorSource},
    store::Store,
};

const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_ADDRESSES: usize = 4096;
const BATCH_SIZE: usize = 256;
const MAX_ENV_FILE: usize = 1024 * 1024;
const MAX_ENV_KEY: usize = 128;
const MAX_ENV_VALUE: usize = 64 * 1024;
const MAX_ENV_ENTRIES: usize = 4096;
const DEFAULT_INTERVAL_SECONDS: u64 = 60;

#[derive(Clone, Copy, Debug)]
enum FailureKind {
    EnvMissing,
    MappingFailed,
    FetchFailed,
}

impl FailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::EnvMissing => "env-missing",
            Self::MappingFailed => "mapping-failed",
            Self::FetchFailed => "fetch-failed",
        }
    }
}

#[derive(Clone)]
struct SourceSpec {
    id: String,
    version: u64,
    run_generation: u64,
    config: Value,
}

impl SourceSpec {
    fn from(source: CollectorSource) -> Self {
        Self {
            id: source.id,
            version: source.version,
            run_generation: source.run_generation,
            config: source.config,
        }
    }

    fn revision(&self) -> (u64, u64) {
        (self.version, self.run_generation)
    }
}

struct ScheduledSource {
    spec: SourceSpec,
    interval: Duration,
    next_due: Instant,
}

struct InFlight {
    revision: (u64, u64),
    handle: JoinHandle<()>,
}

struct Completion {
    source_id: String,
    revision: (u64, u64),
}

#[derive(Clone, Copy)]
enum ResponseFormat {
    Text,
    Json,
}

#[derive(Clone)]
struct ResponseMapping {
    format: ResponseFormat,
    items_path: String,
    address_path: Option<String>,
    ip_path: Option<String>,
    port_path: Option<String>,
    success_path: Option<String>,
    success_value: Option<Value>,
}

struct Provider {
    url: Url,
    method: Method,
    params: Option<Value>,
    headers: reqwest::header::HeaderMap,
    body: Option<Vec<u8>>,
    mapping: ResponseMapping,
    timeout: Duration,
    attempts: usize,
    ca_certificate: Option<Vec<u8>>,
    insecure: bool,
}

/// Run the bounded, in-process acquisition scheduler.
///
/// The scheduler only talks to the typed Store seam. It never calls the
/// management listener and never reads the process environment.
pub async fn run(
    config: CollectorConfig,
    store: Store,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let environment = Arc::new(load_environment(&config)?);
    if !config.enabled {
        return Ok(());
    }

    let concurrency = config.concurrency.clamp(1, 64);
    let poll_seconds = config.poll_seconds.clamp(1, 300);
    let mut changes = store.subscribe();
    let (completion_tx, mut completion_rx) = mpsc::channel::<Completion>(256);
    let mut scheduled = HashMap::<String, ScheduledSource>::new();
    let mut in_flight = HashMap::<String, InFlight>::new();
    let mut poll = tokio::time::interval(Duration::from_secs(poll_seconds));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    refresh_plan(&store, &mut scheduled, &mut in_flight, Instant::now()).await;
    start_due(
        &store,
        &environment,
        &config.base_directory,
        concurrency,
        &mut scheduled,
        &mut in_flight,
        &completion_tx,
    );

    loop {
        if *shutdown.borrow() {
            break;
        }
        let next_due = (in_flight.len() < concurrency)
            .then(|| {
                scheduled
                    .iter()
                    .filter(|(id, _)| !in_flight.contains_key(*id))
                    .map(|(_, source)| source.next_due)
                    .min()
            })
            .flatten();
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = changes.changed() => {}
            _ = poll.tick() => {}
            _ = async {
                if let Some(deadline) = next_due {
                    tokio::time::sleep_until(deadline.into()).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
            completion = completion_rx.recv() => {
                if let Some(completion) = completion {
                    finish_job(completion, &mut scheduled, &mut in_flight).await;
                } else {
                    break;
                }
            }
        }
        // Recheck current authority before filling capacity from cached due work.
        refresh_plan(&store, &mut scheduled, &mut in_flight, Instant::now()).await;
        start_due(
            &store,
            &environment,
            &config.base_directory,
            concurrency,
            &mut scheduled,
            &mut in_flight,
            &completion_tx,
        );
    }

    for (_, job) in in_flight.drain() {
        job.handle.abort();
        let _ = job.handle.await;
    }
    Ok(())
}

async fn refresh_plan(
    store: &Store,
    scheduled: &mut HashMap<String, ScheduledSource>,
    in_flight: &mut HashMap<String, InFlight>,
    now: Instant,
) {
    let plan = match store.collector_plan().await {
        Ok(plan) => plan,
        Err(_) => {
            log::warn!("collector plan unavailable; retaining the last valid plan");
            return;
        }
    };
    let incoming: HashMap<String, SourceSpec> = plan
        .sources
        .into_iter()
        .map(|source| {
            let source = SourceSpec::from(source);
            (source.id.clone(), source)
        })
        .collect();

    let stale_jobs: Vec<String> = in_flight
        .iter()
        .filter_map(|(id, job)| match incoming.get(id) {
            Some(source) if source.revision() == job.revision => None,
            _ => Some(id.clone()),
        })
        .collect();
    for id in stale_jobs {
        if let Some(job) = in_flight.remove(&id) {
            job.handle.abort();
            let _ = job.handle.await;
        }
    }

    let previous = std::mem::take(scheduled);
    for (id, spec) in incoming {
        let interval = source_interval(&spec.config);
        let next_due = previous
            .get(&id)
            .filter(|old| old.spec.revision() == spec.revision())
            .map_or(now, |old| old.next_due);
        scheduled.insert(
            id,
            ScheduledSource {
                spec,
                interval,
                next_due,
            },
        );
    }
}

fn start_due(
    store: &Store,
    environment: &Arc<HashMap<String, String>>,
    base_directory: &Path,
    concurrency: usize,
    scheduled: &mut HashMap<String, ScheduledSource>,
    in_flight: &mut HashMap<String, InFlight>,
    completion_tx: &mpsc::Sender<Completion>,
) {
    while in_flight.len() < concurrency {
        let now = Instant::now();
        let Some(id) = scheduled
            .iter()
            .filter(|(id, source)| source.next_due <= now && !in_flight.contains_key(*id))
            .min_by_key(|(_, source)| source.next_due)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        let source = scheduled.get(&id).unwrap();
        let spec = source.spec.clone();
        let revision = spec.revision();
        let environment = environment.clone();
        let base_directory = base_directory.to_path_buf();
        let store = store.clone();
        let completion_tx = completion_tx.clone();
        let source_id = id.clone();
        let handle = tokio::spawn(async move {
            let _ = collect_source(&spec, &store, &environment, &base_directory).await;
            let _ = completion_tx
                .send(Completion {
                    source_id,
                    revision,
                })
                .await;
        });
        in_flight.insert(id, InFlight { revision, handle });
    }
}

async fn finish_job(
    completion: Completion,
    scheduled: &mut HashMap<String, ScheduledSource>,
    in_flight: &mut HashMap<String, InFlight>,
) {
    if !in_flight
        .get(&completion.source_id)
        .is_some_and(|job| job.revision == completion.revision)
    {
        return;
    }
    let job = in_flight.remove(&completion.source_id).unwrap();
    let _ = job.handle.await;
    if let Some(source) = scheduled.get_mut(&completion.source_id) {
        if source.spec.revision() == completion.revision {
            source.next_due = Instant::now() + source.interval;
        }
    }
}

async fn collect_source(
    source: &SourceSpec,
    store: &Store,
    environment: &HashMap<String, String>,
    base_directory: &Path,
) -> Result<()> {
    let provider = match prepare_provider(&source.config, environment, base_directory) {
        Ok(provider) => provider,
        Err(failure) => {
            submit_failure(store, source, failure).await?;
            return Ok(());
        }
    };
    let addresses = match fetch_addresses(&provider).await {
        Ok(addresses) => addresses,
        Err(failure) => {
            submit_failure(store, source, failure).await?;
            return Ok(());
        }
    };
    submit_addresses(store, source, addresses).await
}

async fn submit_failure(store: &Store, source: &SourceSpec, failure: FailureKind) -> Result<()> {
    store
        .collector_result(CollectorResult {
            source_id: source.id.clone(),
            version: source.version,
            run_generation: source.run_generation,
            addresses: Vec::new(),
            complete: true,
            error: Some(failure.as_str().into()),
        })
        .await?;
    Ok(())
}

async fn submit_addresses(
    store: &Store,
    source: &SourceSpec,
    addresses: Vec<SocketAddr>,
) -> Result<()> {
    for result in result_batches(source, &addresses) {
        store.collector_result(result).await?;
    }
    Ok(())
}

fn result_batches<'a>(
    source: &'a SourceSpec,
    addresses: &'a [SocketAddr],
) -> impl Iterator<Item = CollectorResult> + 'a {
    let count = addresses.len().div_ceil(BATCH_SIZE).max(1);
    addresses
        .chunks(BATCH_SIZE)
        .chain(addresses.is_empty().then_some(addresses))
        .enumerate()
        .map(move |(index, batch)| CollectorResult {
            source_id: source.id.clone(),
            version: source.version,
            run_generation: source.run_generation,
            addresses: batch.to_vec(),
            complete: index + 1 == count,
            error: None,
        })
}

fn load_environment(config: &CollectorConfig) -> Result<HashMap<String, String>> {
    let Some(path) = &config.env_file else {
        return Ok(HashMap::new());
    };
    let path = resolve_path(path, &config.base_directory);
    let file =
        std::fs::File::open(path).map_err(|_| anyhow::anyhow!("cannot read provider env file"))?;
    let mut bytes = Vec::new();
    file.take(MAX_ENV_FILE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read provider env file"))?;
    if bytes.len() > MAX_ENV_FILE {
        bail!("provider env file is too large");
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("provider env file is not UTF-8"))?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            bail!("malformed provider env file");
        };
        let key = raw_key.trim();
        if !valid_env_name(key) || key.len() > MAX_ENV_KEY {
            bail!("malformed provider env file");
        }
        let value = parse_env_value(raw_value.trim())?;
        if value.len() > MAX_ENV_VALUE || values.len() >= MAX_ENV_ENTRIES {
            bail!("provider env file is too large");
        }
        if values.insert(key.to_owned(), value).is_some() {
            bail!("duplicate provider env key");
        }
    }
    Ok(values)
}

fn parse_env_value(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Ok(String::new());
    }
    let first = raw.as_bytes()[0] as char;
    if first != '\'' && first != '"' {
        let value = raw
            .char_indices()
            .find(|(index, character)| {
                *character == '#' && (*index == 0 || raw[..*index].ends_with(char::is_whitespace))
            })
            .map_or(raw, |(index, _)| &raw[..index])
            .trim_end();
        return Ok(value.to_owned());
    }

    let mut value = String::new();
    let mut escaped = false;
    let mut closing = None;
    for (offset, character) in raw[1..].char_indices() {
        if first == '"' && escaped {
            value.push(character);
            escaped = false;
        } else if first == '"' && character == '\\' {
            escaped = true;
        } else if character == first {
            closing = Some(1 + offset + character.len_utf8());
            break;
        } else {
            value.push(character);
        }
    }
    let Some(closing) = closing else {
        bail!("malformed provider env file");
    };
    if !raw[closing..].trim().is_empty() && !raw[closing..].trim_start().starts_with('#') {
        bail!("malformed provider env file");
    }
    Ok(value)
}

fn valid_env_name(value: &str) -> bool {
    let mut characters = value.bytes();
    matches!(characters.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && characters.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn resolve_path(path: &Path, base_directory: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_directory.join(path)
    }
}

fn source_interval(config: &Value) -> Duration {
    config
        .get("interval_seconds")
        .and_then(Value::as_u64)
        .filter(|seconds| (1..=86_400).contains(seconds))
        .map_or(
            Duration::from_secs(DEFAULT_INTERVAL_SECONDS),
            Duration::from_secs,
        )
}

fn prepare_provider(
    raw_config: &Value,
    environment: &HashMap<String, String>,
    base_directory: &Path,
) -> std::result::Result<Provider, FailureKind> {
    let config = expand_value(raw_config.clone(), environment)?;
    let object = config.as_object().ok_or(FailureKind::MappingFailed)?;
    const ALLOWED: &[&str] = &[
        "name",
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
    if object.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(FailureKind::MappingFailed);
    }

    let raw_url = object
        .get("url")
        .and_then(Value::as_str)
        .ok_or(FailureKind::MappingFailed)?;
    if raw_url.len() > 8192 || raw_url.bytes().any(|byte| byte <= 32 || byte == 127) {
        return Err(FailureKind::MappingFailed);
    }
    let url = Url::parse(raw_url).map_err(|_| FailureKind::MappingFailed)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port() == Some(0)
    {
        return Err(FailureKind::MappingFailed);
    }

    let protocol = object.get("protocol").map_or(Ok("http"), |value| {
        value.as_str().ok_or(FailureKind::MappingFailed)
    })?;
    if !matches!(protocol, "http" | "socks5") {
        return Err(FailureKind::MappingFailed);
    }
    let method = match object.get("method").map_or(Ok("GET"), |value| {
        value.as_str().ok_or(FailureKind::MappingFailed)
    })? {
        "GET" => Method::GET,
        "POST" => Method::POST,
        _ => return Err(FailureKind::MappingFailed),
    };

    let params = object.get("params").cloned();
    if params.as_ref().is_some_and(|value| !value.is_object()) {
        return Err(FailureKind::MappingFailed);
    }
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(raw_headers) = object.get("headers") {
        let values = raw_headers.as_object().ok_or(FailureKind::MappingFailed)?;
        for (key, value) in values {
            let name = header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| FailureKind::MappingFailed)?;
            let value = value
                .as_str()
                .ok_or(FailureKind::MappingFailed)
                .and_then(|value| {
                    header::HeaderValue::from_str(value).map_err(|_| FailureKind::MappingFailed)
                })?;
            headers.insert(name, value);
        }
    }

    let body = if let Some(value) = object.get("json") {
        if method != Method::POST || !value.is_object() {
            return Err(FailureKind::MappingFailed);
        }
        Some(serde_json::to_vec(value).map_err(|_| FailureKind::MappingFailed)?)
    } else {
        None
    };

    let mapping = parse_mapping(object.get("response"))?;
    positive_u64(
        object.get("interval_seconds"),
        DEFAULT_INTERVAL_SECONDS,
        86_400,
    )?;
    let timeout = positive_u64(object.get("timeout_seconds"), 10, 300)?;
    let attempts = positive_u64(object.get("attempts"), 3, 5)? as usize;
    let insecure = object.get("insecure").map_or(Ok(false), |value| {
        value.as_bool().ok_or(FailureKind::MappingFailed)
    })?;
    let ca_certificate = match object.get("ca_certificate") {
        None => None,
        Some(value) => {
            let path = value
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or(FailureKind::MappingFailed)?;
            let path = resolve_path(Path::new(path), base_directory);
            Some(std::fs::read(path).map_err(|_| FailureKind::MappingFailed)?)
        }
    };

    Ok(Provider {
        url,
        method,
        params,
        headers,
        body,
        mapping,
        timeout: Duration::from_secs(timeout),
        attempts,
        ca_certificate,
        insecure,
    })
}

fn positive_u64(
    value: Option<&Value>,
    default: u64,
    maximum: u64,
) -> std::result::Result<u64, FailureKind> {
    let value = value.map_or(Ok(default), |value| {
        value.as_u64().ok_or(FailureKind::MappingFailed)
    })?;
    if !(1..=maximum).contains(&value) {
        return Err(FailureKind::MappingFailed);
    }
    Ok(value)
}

fn parse_mapping(raw_mapping: Option<&Value>) -> std::result::Result<ResponseMapping, FailureKind> {
    let object = raw_mapping
        .and_then(Value::as_object)
        .ok_or(FailureKind::MappingFailed)?;
    const ALLOWED: &[&str] = &[
        "format",
        "items_path",
        "address_path",
        "ip_path",
        "port_path",
        "success_path",
        "success_value",
    ];
    if object.keys().any(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(FailureKind::MappingFailed);
    }
    let format = match object.get("format").map_or(Ok("text"), |value| {
        value.as_str().ok_or(FailureKind::MappingFailed)
    })? {
        "text" => ResponseFormat::Text,
        "json" => ResponseFormat::Json,
        _ => return Err(FailureKind::MappingFailed),
    };
    let path = |key: &str| -> std::result::Result<Option<String>, FailureKind> {
        let Some(value) = object.get(key) else {
            return Ok(None);
        };
        let value = value.as_str().ok_or(FailureKind::MappingFailed)?;
        if value.len() > 1024 {
            return Err(FailureKind::MappingFailed);
        }
        Ok(Some(value.to_owned()))
    };
    let items_path = path("items_path")?.unwrap_or_default();
    let address_path = path("address_path")?;
    let ip_path = path("ip_path")?;
    let port_path = path("port_path")?;
    let success_path = path("success_path")?;
    let success_value = object.get("success_value").cloned();
    if ip_path.is_some() != port_path.is_some()
        || (ip_path.is_some() && address_path.is_some())
        || success_path.is_some() != success_value.is_some()
    {
        return Err(FailureKind::MappingFailed);
    }
    Ok(ResponseMapping {
        format,
        items_path,
        address_path,
        ip_path,
        port_path,
        success_path,
        success_value,
    })
}

fn expand_value(
    value: Value,
    environment: &HashMap<String, String>,
) -> std::result::Result<Value, FailureKind> {
    match value {
        Value::String(value) => Ok(Value::String(substitute(&value, environment)?)),
        Value::Array(values) => values
            .into_iter()
            .map(|value| expand_value(value, environment))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, expand_value(value, environment)?)))
            .collect::<std::result::Result<serde_json::Map<_, _>, FailureKind>>()
            .map(Value::Object),
        value => Ok(value),
    }
}

fn substitute(
    input: &str,
    environment: &HashMap<String, String>,
) -> std::result::Result<String, FailureKind> {
    let mut result = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(relative) = input[cursor..].find("${") {
        let start = cursor + relative;
        result.push_str(&input[cursor..start]);
        let name_start = start + 2;
        let Some(relative_end) = input[name_start..].find('}') else {
            return Err(FailureKind::MappingFailed);
        };
        let end = name_start + relative_end;
        let name = &input[name_start..end];
        if !valid_env_name(name) {
            return Err(FailureKind::MappingFailed);
        }
        let value = environment
            .get(name)
            .filter(|value| !value.is_empty())
            .ok_or(FailureKind::EnvMissing)?;
        result.push_str(value);
        cursor = end + 1;
    }
    result.push_str(&input[cursor..]);
    Ok(result)
}

fn build_client(provider: &Provider) -> std::result::Result<Client, FailureKind> {
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .danger_accept_invalid_certs(provider.insecure);
    if let Some(certificate) = &provider.ca_certificate {
        let certificate =
            reqwest::Certificate::from_pem(certificate).map_err(|_| FailureKind::MappingFailed)?;
        builder = builder.add_root_certificate(certificate);
    }
    builder.build().map_err(|_| FailureKind::MappingFailed)
}

async fn fetch_addresses(provider: &Provider) -> std::result::Result<Vec<SocketAddr>, FailureKind> {
    let client = build_client(provider)?;
    for attempt in 0..provider.attempts {
        let result = tokio::time::timeout(provider.timeout, fetch_attempt(&client, provider)).await;
        match result {
            Ok(Ok(body)) => return extract_addresses(&body, &provider.mapping),
            Ok(Err(FailureKind::MappingFailed)) => return Err(FailureKind::MappingFailed),
            Ok(Err(FailureKind::FetchFailed)) | Err(_) => {}
            Ok(Err(FailureKind::EnvMissing)) => return Err(FailureKind::EnvMissing),
        }
        if attempt + 1 < provider.attempts {
            let shift = attempt.min(3) as u32;
            let delay = Duration::from_millis((250_u64 << shift).min(2_000));
            tokio::time::sleep(delay).await;
        }
    }
    Err(FailureKind::FetchFailed)
}

async fn fetch_attempt(
    client: &Client,
    provider: &Provider,
) -> std::result::Result<String, FailureKind> {
    let mut request = client.request(provider.method.clone(), provider.url.clone());
    if let Some(params) = &provider.params {
        request = request.query(params);
    }
    request = request.headers(provider.headers.clone());
    if let Some(body) = &provider.body {
        if !provider.headers.contains_key(header::CONTENT_TYPE) {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        request = request.body(body.clone());
    }
    let mut response = request.send().await.map_err(|_| FailureKind::FetchFailed)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(FailureKind::FetchFailed);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| FailureKind::FetchFailed)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            return Err(FailureKind::FetchFailed);
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).map_err(|_| FailureKind::MappingFailed)?;
    Ok(body.strip_prefix('\u{feff}').unwrap_or(&body).to_owned())
}

fn extract_addresses(
    body: &str,
    mapping: &ResponseMapping,
) -> std::result::Result<Vec<SocketAddr>, FailureKind> {
    let mut values = Vec::new();
    match mapping.format {
        ResponseFormat::Text => {
            for value in body.split(|character: char| {
                character.is_whitespace() || matches!(character, ',' | ';')
            }) {
                if value.is_empty() {
                    continue;
                }
                values.push(parse_endpoint(value)?);
                if values.len() > MAX_ADDRESSES {
                    return Err(FailureKind::MappingFailed);
                }
            }
        }
        ResponseFormat::Json => {
            let payload: Value =
                serde_json::from_str(body).map_err(|_| FailureKind::MappingFailed)?;
            if let (Some(path), Some(expected)) = (&mapping.success_path, &mapping.success_value) {
                if value_at(&payload, path)? != expected {
                    return Err(FailureKind::MappingFailed);
                }
            }
            let items = value_at(&payload, &mapping.items_path)?
                .as_array()
                .ok_or(FailureKind::MappingFailed)?;
            for item in items {
                let address = if let (Some(ip_path), Some(port_path)) =
                    (&mapping.ip_path, &mapping.port_path)
                {
                    let ip = value_at(item, ip_path)?
                        .as_str()
                        .ok_or(FailureKind::MappingFailed)?;
                    let port = value_at(item, port_path)?;
                    parse_ip_port(ip, port)?
                } else {
                    let path = mapping.address_path.as_deref().unwrap_or("");
                    let address = value_at(item, path)?
                        .as_str()
                        .ok_or(FailureKind::MappingFailed)?;
                    parse_endpoint(address)?
                };
                values.push(address);
                if values.len() > MAX_ADDRESSES {
                    return Err(FailureKind::MappingFailed);
                }
            }
        }
    }

    let mut unique = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        if seen.insert(value) {
            unique.push(value);
        }
    }
    Ok(unique)
}

fn value_at<'a>(value: &'a Value, path: &str) -> std::result::Result<&'a Value, FailureKind> {
    let mut current = value;
    if path.is_empty() {
        return Ok(current);
    }
    for part in path.split('.') {
        if part.is_empty() {
            return Err(FailureKind::MappingFailed);
        }
        current = match current {
            Value::Object(values) => values.get(part).ok_or(FailureKind::MappingFailed)?,
            Value::Array(values) if part.bytes().all(|byte| byte.is_ascii_digit()) => {
                let index = part
                    .parse::<usize>()
                    .map_err(|_| FailureKind::MappingFailed)?;
                values.get(index).ok_or(FailureKind::MappingFailed)?
            }
            _ => return Err(FailureKind::MappingFailed),
        };
    }
    Ok(current)
}

fn parse_endpoint(value: &str) -> std::result::Result<SocketAddr, FailureKind> {
    if value.starts_with('[') {
        let close = value.find(']').ok_or(FailureKind::MappingFailed)?;
        if !value[close + 1..].starts_with(':') || value[close + 2..].contains(':') {
            return Err(FailureKind::MappingFailed);
        }
        let ip = parse_ip(&value[1..close])?;
        let port = parse_port(&value[close + 2..])?;
        if !ip.is_ipv6() {
            return Err(FailureKind::MappingFailed);
        }
        return Ok(SocketAddr::new(ip, port));
    }
    let (host, port) = value.rsplit_once(':').ok_or(FailureKind::MappingFailed)?;
    if host.contains(':') {
        return Err(FailureKind::MappingFailed);
    }
    Ok(SocketAddr::new(parse_ip(host)?, parse_port(port)?))
}

fn parse_ip_port(ip: &str, port: &Value) -> std::result::Result<SocketAddr, FailureKind> {
    let port = match port {
        Value::Number(number) => number.as_u64().ok_or(FailureKind::MappingFailed)?,
        Value::String(value) => u64::from(parse_port(value)?),
        _ => return Err(FailureKind::MappingFailed),
    };
    if port == 0 || port > u16::MAX as u64 {
        return Err(FailureKind::MappingFailed);
    }
    Ok(SocketAddr::new(parse_ip(ip)?, port as u16))
}

fn parse_ip(value: &str) -> std::result::Result<IpAddr, FailureKind> {
    let ip = IpAddr::from_str(value).map_err(|_| FailureKind::MappingFailed)?;
    if ip.is_unspecified() {
        return Err(FailureKind::MappingFailed);
    }
    Ok(ip)
}

fn parse_port(value: &str) -> std::result::Result<u16, FailureKind> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FailureKind::MappingFailed);
    }
    let port = value
        .parse::<u32>()
        .map_err(|_| FailureKind::MappingFailed)?;
    if !(1..=u16::MAX as u32).contains(&port) {
        return Err(FailureKind::MappingFailed);
    }
    Ok(port as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn old_completion_does_not_remove_or_wait_for_replacement_job() {
        let handle = tokio::spawn(std::future::pending());
        let abort = handle.abort_handle();
        let mut in_flight = HashMap::from([(
            "source".into(),
            InFlight {
                revision: (2, 3),
                handle,
            },
        )]);
        let mut scheduled = HashMap::new();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            finish_job(
                Completion {
                    source_id: "source".into(),
                    revision: (1, 2),
                },
                &mut scheduled,
                &mut in_flight,
            ),
        )
        .await;
        abort.abort();
        assert!(
            result.is_ok(),
            "old completion waited for a new network job"
        );
        assert!(in_flight.contains_key("source"));
    }

    #[test]
    fn mapped_port_strings_reject_signs_and_nondecimal_characters() {
        for port in ["+80", "-80", " 80", "80 ", "８０", "0", "65536"] {
            assert!(parse_ip_port("1.2.3.4", &json!(port)).is_err(), "{port}");
        }
    }

    #[test]
    fn mappings_cover_root_arrays_address_paths_and_success_criteria() {
        for (body, raw, expected) in [
            (
                r#"["1.2.3.4:80","[::1]:1080","1.2.3.4:80"]"#,
                json!({"format":"json"}),
                vec!["1.2.3.4:80", "[::1]:1080"],
            ),
            (
                r#"{"data":[{"endpoint":"[::1]:80"}]}"#,
                json!({"format":"json","items_path":"data","address_path":"endpoint"}),
                vec!["[::1]:80"],
            ),
            (
                r#"{"code":0,"data":{"items":[{"host":"1.2.3.4","port":80}]}}"#,
                json!({"format":"json","items_path":"data.items","ip_path":"host","port_path":"port","success_path":"code","success_value":0}),
                vec!["1.2.3.4:80"],
            ),
        ] {
            let mapping = parse_mapping(Some(&raw)).unwrap();
            let actual = extract_addresses(body, &mapping).unwrap();
            assert_eq!(
                actual.iter().map(ToString::to_string).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn mappings_reject_bad_paths_payloads_and_unsafe_endpoints() {
        let text = parse_mapping(Some(&json!({"format":"text"}))).unwrap();
        for body in [
            "http://user:password@1.2.3.4:80",
            "1.2.3.4:0",
            "supplier.example:80",
            "0.0.0.0:80",
            "[::]:80",
            "[fe80::1%eth0]:80",
            "not a proxy",
            "::1:80",
            "1.2.3.4:65536",
            "[1.2.3.4]:80",
            "1.2.3.4:+80",
        ] {
            assert!(extract_addresses(body, &text).is_err(), "{body}");
        }
        for (body, raw) in [
            (
                r#"{"code":1,"data":[]}"#,
                json!({"format":"json","items_path":"data","success_path":"code","success_value":0}),
            ),
            (
                r#"{"data":{}}"#,
                json!({"format":"json","items_path":"data"}),
            ),
            (
                r#"{"data":[]}"#,
                json!({"format":"json","items_path":"data.2"}),
            ),
            (
                r#"{"data":["1.2.3.4:80"]}"#,
                json!({"format":"json","items_path":"data..items"}),
            ),
            ("not json", json!({"format":"json"})),
            (r#"["1.2.3.4:80",false]"#, json!({"format":"json"})),
        ] {
            let mapping = parse_mapping(Some(&raw)).unwrap();
            assert!(extract_addresses(body, &mapping).is_err(), "{body}");
        }
        let addresses = (1..=4096)
            .map(|port| format!("1.2.3.4:{port}"))
            .collect::<Vec<_>>()
            .join(";");
        assert_eq!(extract_addresses(&addresses, &text).unwrap().len(), 4096);
        assert!(extract_addresses(&(addresses + ";1.2.3.4:80"), &text).is_err());
    }

    #[test]
    fn provider_environment_supports_quotes_crlf_and_literal_nonexpanding_values() {
        let directory = tempfile::TempDir::new().unwrap();
        std::fs::write(
            directory.path().join("providers.env"),
            concat!(
                "# isolated variables\r\n\r\nTOKEN='private#value'\r\n",
                "QUOTED=\"value with spaces\" # comment\r\n",
                "LITERAL=${TOKEN}\r\nSHELL=$(not-executed)\r\nEMPTY=\r\n",
                "PLAIN=value # comment\r\nHASH=value#suffix\r\n"
            ),
        )
        .unwrap();
        let config = CollectorConfig {
            env_file: Some("providers.env".into()),
            base_directory: directory.path().into(),
            ..CollectorConfig::default()
        };
        let values = load_environment(&config).unwrap();
        assert_eq!(values["TOKEN"], "private#value");
        assert_eq!(values["QUOTED"], "value with spaces");
        assert_eq!(values["LITERAL"], "${TOKEN}");
        assert_eq!(values["SHELL"], "$(not-executed)");
        assert_eq!(values["EMPTY"], "");
        assert_eq!(values["PLAIN"], "value");
        assert_eq!(values["HASH"], "value#suffix");
        assert_eq!(
            substitute("Bearer ${TOKEN}", &values).unwrap(),
            "Bearer private#value"
        );
        assert_eq!(substitute("${LITERAL}", &values).unwrap(), "${TOKEN}");
        assert!(matches!(
            substitute("${EMPTY}", &values),
            Err(FailureKind::EnvMissing)
        ));
        assert!(matches!(
            substitute("${UNKNOWN}", &values),
            Err(FailureKind::EnvMissing)
        ));
        assert!(load_environment(&CollectorConfig::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn provider_environment_errors_are_bounded_and_value_free() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("private-file.env");
        let config = CollectorConfig {
            env_file: Some(path.clone()),
            ..CollectorConfig::default()
        };
        let error = load_environment(&config).unwrap_err().to_string();
        assert!(!error.contains("private-file"));
        for bytes in [
            b"secret-malformed-line".to_vec(),
            b"PRIVATE_KEY=secret\nPRIVATE_KEY=second".to_vec(),
            b"PRIVATE_KEY='secret-unclosed".to_vec(),
            b"PRIVATE_KEY=\"secret\"invalid".to_vec(),
            b"9_PRIVATE=secret".to_vec(),
            vec![0xff],
            vec![b'x'; MAX_ENV_FILE + 1],
            format!("{}=secret", "K".repeat(MAX_ENV_KEY + 1)).into_bytes(),
            format!("PRIVATE_KEY={}", "x".repeat(MAX_ENV_VALUE + 1)).into_bytes(),
            (0..=MAX_ENV_ENTRIES)
                .map(|index| format!("KEY_{index}=secret\n"))
                .collect::<String>()
                .into_bytes(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            let error = load_environment(&config).unwrap_err().to_string();
            for secret in ["secret", "PRIVATE_KEY", "private-file"] {
                assert!(!error.contains(secret));
            }
        }
    }

    #[test]
    fn result_batches_preserve_revision_and_only_finish_the_final_batch() {
        let source = SourceSpec {
            id: "s_batch".into(),
            version: 4,
            run_generation: 7,
            config: json!({}),
        };
        let addresses: Vec<_> = (1..=513)
            .map(|port| SocketAddr::from(([1, 2, 3, 4], port)))
            .collect();
        let batches: Vec<_> = result_batches(&source, &addresses).collect();
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.addresses.len())
                .collect::<Vec<_>>(),
            vec![256, 256, 1]
        );
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.complete)
                .collect::<Vec<_>>(),
            vec![false, false, true]
        );
        assert!(batches.iter().all(|batch| batch.source_id == "s_batch"
            && batch.version == 4
            && batch.run_generation == 7
            && batch.error.is_none()));
        let empty: Vec<_> = result_batches(&source, &[]).collect();
        assert_eq!(empty.len(), 1);
        assert!(empty[0].addresses.is_empty());
        assert!(empty[0].complete);
        assert!(empty[0].error.is_none());
    }

    #[tokio::test]
    async fn stale_batches_do_not_admit_candidates_or_finish_a_new_generation() {
        use crate::{
            config::Config,
            managed_types::{SourceChange, SourceInput},
        };
        let directory = tempfile::TempDir::new().unwrap();
        let config = Config {
            database: directory.path().join("state.sqlite3"),
            ..Config::default()
        };
        let store = Store::open(&config).await.unwrap();
        store
            .create_source(SourceInput {
                name: "stale".into(),
                kind: "api".into(),
                enabled: true,
                config: json!({"url":"http://127.0.0.1:1"}),
            })
            .await
            .unwrap();
        let spec = SourceSpec::from(store.collector_plan().await.unwrap().sources.remove(0));
        store
            .update_source(
                "stale".into(),
                SourceChange {
                    enabled: Some(true),
                    ..SourceChange::default()
                },
            )
            .await
            .unwrap();
        let addresses = (1..=513)
            .map(|port| SocketAddr::from(([1, 2, 3, 4], port)))
            .collect();
        assert!(submit_addresses(&store, &spec, addresses).await.is_err());
        assert!(store.candidates().await.unwrap().is_empty());
        let source = store.source("stale".into()).await.unwrap().unwrap();
        assert_eq!(source.last_finished_generation, 0);
        assert_eq!(source.run_generation, 2);
    }

    #[test]
    fn expanded_provider_configuration_remains_strict() {
        for change in [
            json!({"protocol": 42}),
            json!({"method": 42}),
            json!({"interval_seconds": 0}),
            json!({"response": {"format": 42}}),
            json!({"url": "http://localhost/secret\npath"}),
        ] {
            let mut config = json!({"url": "http://127.0.0.1:80", "response": {"format": "text"}});
            config
                .as_object_mut()
                .unwrap()
                .extend(change.as_object().unwrap().clone());
            assert!(
                prepare_provider(&config, &HashMap::new(), Path::new(".")).is_err(),
                "{change}"
            );
        }
    }
}
