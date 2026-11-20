use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use reqwest::Method;
use rproxy::control_client::{resource_path, ControlError, RootClient};
use serde_json::{json, Map, Value};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, BufRead, IsTerminal, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(
    name = "rproxy-cli",
    version,
    about = "Online rproxy administration (management API v1)"
)]
struct Cli {
    #[arg(long, global = true)]
    endpoint: Option<String>,
    #[arg(long, global = true, default_value = "RPROXY_ADMIN_TOKEN")]
    token_env: String,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=300))]
    timeout: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Status,
    #[command(alias = "healthcheck")]
    Health,
    #[command(subcommand)]
    User(UserCommand),
    #[command(subcommand)]
    Source(SourceCommand),
    #[command(subcommand)]
    Proxy(ProxyCommand),
    #[command(subcommand)]
    Session(SessionCommand),
}

#[derive(Args)]
struct PasswordInput {
    #[arg(long, conflicts_with = "password_env")]
    password_stdin: bool,
    #[arg(long)]
    password_env: Option<String>,
}

#[derive(Args)]
struct Confirmation {
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    yes: bool,
}

#[derive(Args)]
struct Impact {
    #[command(flatten)]
    confirmation: Confirmation,
    #[arg(long)]
    disconnect: bool,
}

#[derive(Args)]
struct Wait {
    /// Poll until ready, rejected, or the bounded deadline expires.
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=300))]
    wait_timeout: u64,
}

#[derive(Subcommand)]
enum UserCommand {
    Add {
        name: String,
        #[arg(long = "business", alias = "businesses", value_delimiter = ',')]
        businesses: Vec<String>,
        #[command(flatten)]
        password: PasswordInput,
    },
    List,
    Show {
        name: String,
    },
    Passwd {
        name: String,
        #[command(flatten)]
        password: PasswordInput,
        #[arg(long)]
        disconnect: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Replace the complete business grant list (repeat --business or use commas).
    Grant {
        name: String,
        #[arg(long = "business", alias = "businesses", value_delimiter = ',')]
        businesses: Vec<String>,
    },
    Disable {
        name: String,
        #[command(flatten)]
        impact: Impact,
    },
    Enable {
        name: String,
    },
    Remove {
        name: String,
        #[command(flatten)]
        impact: Impact,
    },
}

#[derive(Subcommand)]
enum SourceCommand {
    Import {
        file: PathBuf,
        #[arg(long)]
        replace: bool,
        #[command(flatten)]
        wait: Wait,
    },
    List,
    Show {
        name: String,
    },
    Pause {
        name: String,
    },
    Resume {
        name: String,
    },
    Run {
        name: String,
        #[command(flatten)]
        wait: Wait,
    },
    Remove {
        name: String,
        #[command(flatten)]
        confirmation: Confirmation,
    },
    Export {
        #[arg(long, requires_all = ["output", "yes"])]
        include_secrets: bool,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ProxyCommand {
    Add {
        #[arg(value_parser = ["http", "socks5"])]
        protocol: String,
        #[arg(required = true)]
        addresses: Vec<String>,
        #[arg(long, default_value = "manual")]
        source: String,
        #[command(flatten)]
        wait: Wait,
    },
    Import {
        #[arg(value_parser = ["http", "socks5"])]
        protocol: String,
        /// One numeric IP:port per line; '-' reads standard input.
        file: PathBuf,
        #[arg(long, default_value = "manual")]
        source: String,
        #[command(flatten)]
        wait: Wait,
    },
    List {
        #[arg(long, value_parser = ["http", "socks5"])]
        protocol: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Show {
        id: String,
    },
    Verify {
        #[command(flatten)]
        wait: Wait,
    },
    Disable {
        id: String,
        #[command(flatten)]
        impact: Impact,
    },
    Enable {
        id: String,
    },
    Remove {
        id: String,
        #[command(flatten)]
        impact: Impact,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    List {
        #[arg(long)]
        business: Option<String>,
        #[arg(long)]
        owner: Option<String>,
    },
    Release {
        business: String,
        session: String,
        #[command(flatten)]
        impact: Impact,
    },
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return;
        }
        Err(_) => {
            eprintln!("error: invalid command arguments; use rproxy-cli --help (argument values withheld)");
            std::process::exit(2);
        }
    };
    match run(&cli).await {
        Ok(value) => {
            if let Err(error) = print_value(&redact(&value), cli.json) {
                eprintln!("error: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: &Cli) -> Result<Value> {
    let endpoint = cli
        .endpoint
        .clone()
        .or_else(|| std::env::var("RPROXY_ADMIN_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:9100".to_owned());
    let client = RootClient::new(&endpoint, &cli.token_env, Duration::from_secs(cli.timeout))?;
    let capability = match cli.command {
        Command::User(_) => Some("users"),
        Command::Source(_) => Some("sources"),
        Command::Proxy(_) => Some("proxies"),
        Command::Session(_) => Some("sessions"),
        _ => None,
    };
    client.check_management(capability).await?;
    match &cli.command {
        Command::Status => get(&client, "/v1/status").await,
        Command::Health => get(&client, "/health").await,
        Command::User(command) => user(&client, command).await,
        Command::Source(command) => source(&client, command).await,
        Command::Proxy(command) => proxy(&client, command).await,
        Command::Session(command) => session(&client, command).await,
    }
}

async fn get(client: &RootClient, path: &str) -> Result<Value> {
    Ok(client.request(Method::GET, path, &[], None).await?)
}

async fn send(client: &RootClient, method: Method, path: &str, body: Value) -> Result<Value> {
    Ok(client.request(method, path, &[], Some(&body)).await?)
}

async fn user(client: &RootClient, command: &UserCommand) -> Result<Value> {
    match command {
        UserCommand::List => get(client, "/v1/users").await,
        UserCommand::Show { name } => get(client, &resource_path("users", name)?).await,
        UserCommand::Add {
            name,
            businesses,
            password,
        } => {
            resource_path("users", name)?;
            send(
                client,
                Method::POST,
                "/v1/users",
                json!({"name":name,"password":read_password(password)?,"businesses":businesses}),
            )
            .await
        }
        UserCommand::Passwd {
            name,
            password,
            disconnect,
            yes,
        } => {
            let path = resource_path("users", name)?;
            if *disconnect {
                confirm(*yes)?;
            }
            send(
                client,
                Method::PATCH,
                &path,
                json!({"password":read_password(password)?,"disconnect":disconnect}),
            )
            .await
        }
        UserCommand::Grant { name, businesses } => {
            send(
                client,
                Method::PATCH,
                &resource_path("users", name)?,
                json!({"businesses":businesses}),
            )
            .await
        }
        UserCommand::Enable { name } => {
            send(
                client,
                Method::PATCH,
                &resource_path("users", name)?,
                json!({"enabled":true}),
            )
            .await
        }
        UserCommand::Disable { name, impact } | UserCommand::Remove { name, impact } => {
            let path = resource_path("users", name)?;
            let current = get(client, &path).await?;
            if impact.confirmation.dry_run {
                return Ok(json!({"applied":false,"user":current,"disconnect":impact.disconnect}));
            }
            // UserView has no runtime impact counts; conservatively confirm every revocation.
            confirm(impact.confirmation.yes)?;
            if matches!(command, UserCommand::Remove { .. }) {
                Ok(client
                    .request(
                        Method::DELETE,
                        &path,
                        &[("disconnect".into(), impact.disconnect.to_string())],
                        None,
                    )
                    .await?)
            } else {
                send(
                    client,
                    Method::PATCH,
                    &path,
                    json!({"enabled":false,"disconnect":impact.disconnect}),
                )
                .await
            }
        }
    }
}

fn read_password(input: &PasswordInput) -> Result<String> {
    let password = if input.password_stdin {
        let mut password = String::new();
        io::stdin()
            .lock()
            .take(16 * 1024 + 1)
            .read_to_string(&mut password)
            .map_err(|_| anyhow::anyhow!("could not read password from stdin"))?;
        if password.ends_with('\n') {
            password.pop();
            if password.ends_with('\r') {
                password.pop();
            }
        }
        password
    } else if let Some(variable) = &input.password_env {
        std::env::var(variable)
            .map_err(|_| anyhow::anyhow!("password environment variable is missing or invalid"))?
    } else {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!("password input requires a TTY; use --password-stdin or --password-env");
        }
        let password = rpassword::prompt_password("Password: ")
            .map_err(|_| anyhow::anyhow!("could not read hidden password"))?;
        let repeated = rpassword::prompt_password("Confirm password: ")
            .map_err(|_| anyhow::anyhow!("could not read hidden password confirmation"))?;
        if password != repeated {
            bail!("password confirmation did not match");
        }
        password
    };
    if password.is_empty() || password.len() > 16 * 1024 || password.contains(['\r', '\n', '\0']) {
        bail!("password must be one nonempty line of at most 16384 bytes");
    }
    Ok(password)
}

fn confirm(yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("operation requires confirmation; inspect --dry-run and pass --yes");
    }
    eprint!("Apply the previewed change? Type yes: ");
    io::stderr()
        .flush()
        .context("could not write confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .take(64)
        .read_line(&mut answer)
        .map_err(|_| anyhow::anyhow!("could not read confirmation"))?;
    if answer.trim() != "yes" {
        bail!("operation cancelled; no change applied");
    }
    Ok(())
}

struct SourceImport {
    name: String,
    kind: String,
    config: Value,
    enabled: Option<bool>,
}

fn read_file(path: &Path) -> Result<String> {
    let input: Box<dyn Read> = if path == Path::new("-") {
        Box::new(io::stdin())
    } else {
        Box::new(fs::File::open(path).map_err(|_| anyhow::anyhow!("could not open import file"))?)
    };
    let mut text = String::new();
    input
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| anyhow::anyhow!("could not read UTF-8 import file"))?;
    if text.len() > 4 * 1024 * 1024 {
        bail!("import file exceeds the 4 MiB limit");
    }
    Ok(text)
}

fn source_imports(path: &Path) -> Result<Vec<SourceImport>> {
    let text = read_file(path)?;
    let value: Value = if text.trim_start().starts_with('{') {
        serde_json::from_str(&text)
            .map_err(|_| anyhow::anyhow!("invalid source JSON; content withheld"))?
    } else {
        let value: toml::Value = toml::from_str(&text)
            .map_err(|_| anyhow::anyhow!("invalid source TOML; content withheld"))?;
        serde_json::to_value(value).map_err(|_| anyhow::anyhow!("invalid source TOML structure"))?
    };
    if value
        .get("schema_version")
        .is_some_and(|version| version.as_u64() != Some(1))
    {
        bail!("unsupported source schema version; expected 1");
    }
    let legacy = value.get("providers").is_some();
    if legacy && value.get("sources").is_some() {
        bail!("source import cannot mix providers and sources");
    }
    let entries = value
        .get(if legacy { "providers" } else { "sources" })
        .and_then(Value::as_array)
        .context("source import requires a sources or providers array")?;
    if entries.is_empty() {
        bail!("source import is empty");
    }
    let mut names = HashSet::new();
    let mut imports = Vec::new();
    for entry in entries {
        let mut fields = entry
            .as_object()
            .cloned()
            .context("invalid source entry; expected object")?;
        let name = fields
            .remove("name")
            .and_then(|name| name.as_str().map(str::to_owned))
            .context("source entry requires a name")?;
        resource_path("sources", &name)?;
        if !names.insert(name.clone()) {
            bail!("duplicate source name in import");
        }
        let enabled = fields
            .remove("enabled")
            .map(|value| value.as_bool().context("source enabled must be boolean"))
            .transpose()?;
        let kind = fields
            .remove("kind")
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .context("invalid source kind")
            })
            .transpose()?
            .unwrap_or_else(|| "api".into());
        if !matches!(kind.as_str(), "api" | "static") {
            bail!("source kind must be api or static");
        }
        let config = if legacy {
            Value::Object(fields)
        } else {
            fields
                .remove("config")
                .context("source entry requires config object")?
        };
        if !config.is_object() {
            bail!("source config must be an object");
        }
        imports.push(SourceImport {
            name,
            kind,
            config,
            enabled,
        });
    }
    Ok(imports)
}

async fn source(client: &RootClient, command: &SourceCommand) -> Result<Value> {
    match command {
        SourceCommand::List => get(client, "/v1/sources").await,
        SourceCommand::Show { name } => get(client, &resource_path("sources", name)?).await,
        SourceCommand::Pause { name } | SourceCommand::Resume { name } => {
            send(
                client,
                Method::PATCH,
                &resource_path("sources", name)?,
                json!({"enabled":matches!(command, SourceCommand::Resume { .. })}),
            )
            .await
        }
        SourceCommand::Run { name, wait } => run_source(client, name, wait).await,
        SourceCommand::Remove { name, confirmation } => {
            let path = resource_path("sources", name)?;
            let current = get(client, &path).await?;
            if confirmation.dry_run {
                return Ok(json!({"applied":false,"source":current,"keeps_proxies":true}));
            }
            confirm(confirmation.yes)?;
            Ok(client.request(Method::DELETE, &path, &[], None).await?)
        }
        SourceCommand::Import {
            file,
            replace,
            wait,
        } => {
            let imports = source_imports(file)?;
            let mut items = Vec::new();
            for import in imports {
                let path = resource_path("sources", &import.name)?;
                let existing = if *replace {
                    match client.request(Method::GET, &path, &[], None).await {
                        Ok(value) => Some(value),
                        Err(ControlError::Http(404)) => None,
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    None
                };
                let mut item = if let Some(existing) = existing {
                    if existing
                        .get("kind")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != import.kind)
                    {
                        bail!("--replace cannot change a source kind; use a new name");
                    }
                    let version = existing.get("version").and_then(Value::as_u64).context(
                        "source response omitted version; refusing unchecked replacement",
                    )?;
                    let mut body = json!({"config":import.config,"expected_version":version});
                    if let Some(enabled) = import.enabled {
                        body["enabled"] = json!(enabled);
                    }
                    send(client, Method::PATCH, &path, body).await?
                } else {
                    let mut body =
                        json!({"name":import.name,"kind":import.kind,"config":import.config});
                    if let Some(enabled) = import.enabled {
                        body["enabled"] = json!(enabled);
                    }
                    let created = send(client, Method::POST, "/v1/sources", body).await?;
                    if let Some(enabled) = import
                        .enabled
                        .filter(|enabled| created.get("enabled") != Some(&Value::Bool(*enabled)))
                    {
                        let version = created.get("version").and_then(Value::as_u64).context(
                            "source response omitted version; refusing unchecked enabled update",
                        )?;
                        send(
                            client,
                            Method::PATCH,
                            &path,
                            json!({"enabled":enabled,"expected_version":version}),
                        )
                        .await?
                    } else {
                        created
                    }
                };
                if wait.wait && import.kind == "api" {
                    if import.enabled == Some(false)
                        || item.get("enabled") == Some(&Value::Bool(false))
                    {
                        bail!("source saved but paused; resume it before waiting for acquisition");
                    }
                    let result = run_source(client, &import.name, wait).await?;
                    item["verification"] = result["verification"].clone();
                } else {
                    item["verification"] = json!("saved");
                }
                items.push(item);
            }
            Ok(json!({"total":items.len(),"items":items}))
        }
        SourceCommand::Export {
            include_secrets,
            output,
            yes,
        } => {
            if *include_secrets && (output.is_none() || !yes) {
                bail!("secret export requires --output and --yes");
            }
            let value = client
                .request(
                    Method::GET,
                    "/v1/sources/export",
                    &[("include_secrets".into(), include_secrets.to_string())],
                    None,
                )
                .await?;
            let value = if *include_secrets {
                value
            } else {
                redact(&value)
            };
            if let Some(path) = output {
                write_export(path, &value)?;
                Ok(json!({"exported":true,"include_secrets":include_secrets}))
            } else {
                Ok(value)
            }
        }
    }
}

fn write_export(path: &Path, value: &Value) -> Result<()> {
    // Never overwrite a credential backup or follow a pre-existing symlink.
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| {
        anyhow::anyhow!("could not create export file; destination must not already exist")
    })?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|_| anyhow::anyhow!("could not write export file"))?;
    file.write_all(b"\n")
        .map_err(|_| anyhow::anyhow!("could not finish export file"))?;
    file.sync_all()
        .map_err(|_| anyhow::anyhow!("could not flush export file"))?;
    Ok(())
}

async fn run_source(client: &RootClient, name: &str, wait: &Wait) -> Result<Value> {
    let path = resource_path("sources", name)?;
    let mut result = send(client, Method::POST, &format!("{path}/run"), json!({})).await?;
    result["verification"] = json!("pending");
    if !wait.wait {
        return Ok(result);
    }
    let generation = result
        .get("run_generation")
        .and_then(Value::as_u64)
        .context("source run response omitted generation")?;
    let deadline = Instant::now() + Duration::from_secs(wait.wait_timeout);
    loop {
        let source = client
            .request_before(Method::GET, &path, &[], None, Some(deadline))
            .await?;
        if source.get("deleted") == Some(&Value::Bool(true))
            || source.get("enabled") == Some(&Value::Bool(false))
        {
            bail!("source wait failed: source was paused or removed");
        }
        if source
            .get("last_finished_generation")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= generation
        {
            if source
                .get("last_error")
                .is_some_and(|error| !error.is_null())
            {
                bail!(
                    "source acquisition failed; inspect source status (provider details withheld)"
                );
            }
            result["verification"] = json!("ready");
            return Ok(result);
        }
        poll_delay(deadline).await?;
    }
}

async fn proxy(client: &RootClient, command: &ProxyCommand) -> Result<Value> {
    match command {
        ProxyCommand::Add { protocol, addresses, source, wait } => add_proxies(client, protocol, addresses.clone(), source, wait).await,
        ProxyCommand::Import { protocol, file, source, wait } => {
            let text = if file == Path::new("-") {
                let mut text = String::new();
                io::stdin().lock().take(4 * 1024 * 1024 + 1).read_to_string(&mut text)
                    .map_err(|_| anyhow::anyhow!("could not read proxy input"))?;
                if text.len() > 4 * 1024 * 1024 { bail!("proxy import exceeds 4 MiB"); }
                text
            } else { read_file(file)? };
            let addresses = text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).map(str::to_owned).collect();
            add_proxies(client, protocol, addresses, source, wait).await
        }
        ProxyCommand::List { protocol, source, state, limit, offset } => {
            let mut query = vec![("limit".into(),limit.to_string()),("offset".into(),offset.to_string())];
            for (key, value) in [("protocol",protocol),("source",source),("state",state)] {
                if let Some(value) = value { query.push((key.into(),value.clone())); }
            }
            Ok(client.request(Method::GET,"/v1/proxies",&query,None).await?)
        }
        ProxyCommand::Show { id } => proxy_show(client, id, None).await,
        ProxyCommand::Verify { wait } => {
            let mut result = send(client,Method::POST,"/v1/verify",json!({})).await?;
            result["verification"] = json!("pending");
            if wait.wait {
                // Snapshot a bounded batch, then poll each ID rather than calling verify again.
                let list = client.request(Method::GET,"/v1/proxies",&[("limit".into(),"1000".into()),("offset".into(),"0".into())],None).await?;
                let rows = list.get("items").and_then(Value::as_array).context("proxy list response omitted items")?;
                if list.get("total").and_then(Value::as_u64).unwrap_or(0) > 1000 { bail!("verification scheduled; --wait supports at most 1000 proxies; inspect filtered proxy list"); }
                let ids = rows.iter().map(|row| row.get("id").and_then(Value::as_str).map(str::to_owned).context("proxy response omitted id")).collect::<Result<Vec<_>>>()?;
                wait_proxies(client,&ids,wait).await?;
                result["verification"] = json!("ready");
            }
            Ok(result)
        }
        ProxyCommand::Enable { id } => send(client,Method::POST,"/v1/proxies/action",json!({"id":id,"enabled":true,"remove":false,"disconnect":false,"dry_run":false,"confirm":false})).await,
        ProxyCommand::Disable { id, impact } | ProxyCommand::Remove { id, impact } => {
            let mut body = json!({"id":id,"remove":matches!(command,ProxyCommand::Remove { .. }),"disconnect":impact.disconnect,"dry_run":true,"confirm":false});
            if matches!(command,ProxyCommand::Disable { .. }) { body["enabled"] = json!(false); }
            let preview = send(client,Method::POST,"/v1/proxies/action",body.clone()).await?;
            if impact.confirmation.dry_run { return Ok(preview); }
            let impacting = has_impact(&preview);
            if impacting { show_impact(&preview); confirm(impact.confirmation.yes)?; }
            body["dry_run"] = json!(false);
            // A zero-impact preview is not approval for impact acquired in a race.
            body["confirm"] = json!(impacting || impact.confirmation.yes);
            send(client,Method::POST,"/v1/proxies/action",body).await
        }
    }
}

async fn proxy_show(client: &RootClient, id: &str, deadline: Option<Instant>) -> Result<Value> {
    Ok(client
        .request_before(
            Method::GET,
            "/v1/proxies/show",
            &[("id".into(), id.to_owned())],
            None,
            deadline,
        )
        .await?)
}

async fn add_proxies(
    client: &RootClient,
    protocol: &str,
    addresses: Vec<String>,
    source: &str,
    wait: &Wait,
) -> Result<Value> {
    resource_path("sources", source)?;
    if addresses.is_empty() {
        bail!("proxy input is empty");
    }
    let mut unique = HashSet::new();
    let mut validated = Vec::new();
    for address in addresses {
        let endpoint: SocketAddr = address.parse().map_err(|_| {
            anyhow::anyhow!("proxy addresses must be numeric IP:port without credentials or URLs")
        })?;
        if endpoint.port() == 0 {
            bail!("proxy port must be nonzero");
        }
        let address = endpoint.to_string();
        if unique.insert(address.clone()) {
            validated.push(address);
        }
    }
    let mut accepted = 0;
    let mut ids = Vec::new();
    for batch in validated.chunks(256) {
        let result = send(
            client,
            Method::POST,
            "/v1/proxies",
            json!({"protocol":protocol,"addresses":batch,"source":source}),
        )
        .await?;
        accepted += result
            .get("accepted")
            .and_then(Value::as_u64)
            .context("proxy response omitted accepted count")?;
        if let Some(batch_ids) = result.get("ids").and_then(Value::as_array) {
            for id in batch_ids {
                ids.push(id.as_str().context("invalid proxy ID response")?.to_owned());
            }
        } else if wait.wait {
            bail!("server omitted proxy IDs; cannot wait for admission");
        }
    }
    let mut result = json!({"accepted":accepted,"ids":ids,"verification":"pending"});
    if wait.wait {
        wait_proxies(client, &ids, wait).await?;
        result["verification"] = json!("ready");
    }
    Ok(result)
}

async fn wait_proxies(client: &RootClient, ids: &[String], wait: &Wait) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(wait.wait_timeout);
    let mut pending: HashSet<_> = ids.iter().cloned().collect();
    while !pending.is_empty() {
        for id in pending.clone() {
            let row = proxy_show(client, &id, Some(deadline)).await?;
            match row.get("state").and_then(Value::as_str) {
                Some("ready" | "active" | "healthy" | "verified" | "admitted" | "available") => {
                    pending.remove(&id);
                }
                Some("failed" | "rejected" | "retired" | "removed" | "disabled" | "dead") => {
                    let reason = row
                        .get("reason")
                        .or_else(|| row.get("last_error"))
                        .and_then(Value::as_str)
                        .unwrap_or("rejected");
                    let reason = match reason {
                        "probe-failed" | "timeout" | "capacity" | "unsupported" | "unreachable" => {
                            reason
                        }
                        _ => "rejected (details withheld)",
                    };
                    bail!("proxy admission failed: {reason}");
                }
                _ => {}
            }
        }
        if !pending.is_empty() {
            poll_delay(deadline).await?;
        }
    }
    Ok(())
}

async fn poll_delay(deadline: Instant) -> Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("wait timed out; operation remains pending")?;
    if remaining.is_zero() {
        bail!("wait timed out; operation remains pending");
    }
    tokio::time::sleep(Duration::from_millis(200).min(remaining)).await;
    Ok(())
}

fn has_impact(value: &Value) -> bool {
    let impact = value.get("impact").unwrap_or(value);
    // Missing or unfamiliar impact data must not silently authorize a mutation.
    let sessions = impact.get("affected_sessions").and_then(Value::as_u64);
    let connections = impact
        .get("active_connections")
        .or_else(|| impact.get("connections"))
        .and_then(Value::as_u64);
    sessions.is_none() || connections.is_none() || sessions != Some(0) || connections != Some(0)
}

fn show_impact(value: &Value) {
    let impact = value.get("impact").unwrap_or(value);
    let sessions = impact
        .get("affected_sessions")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    let connections = impact
        .get("active_connections")
        .or_else(|| impact.get("connections"))
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into());
    eprintln!("Preview: affected sessions={sessions}, active connections={connections}");
}

async fn session(client: &RootClient, command: &SessionCommand) -> Result<Value> {
    match command {
        SessionCommand::List { business, owner } => {
            let mut query = Vec::new();
            for (key, value) in [("business", business), ("owner", owner)] {
                if let Some(value) = value {
                    query.push((key.into(), value.clone()));
                }
            }
            Ok(client
                .request(Method::GET, "/v1/sessions", &query, None)
                .await?)
        }
        SessionCommand::Release {
            business,
            session,
            impact,
        } => {
            let mut body = json!({"business":business,"session":session,"disconnect":impact.disconnect,"dry_run":true});
            let preview = send(client, Method::POST, "/v1/sessions/release", body.clone()).await?;
            if impact.confirmation.dry_run {
                return Ok(preview);
            }
            if has_impact(&preview) {
                show_impact(&preview);
                confirm(impact.confirmation.yes)?;
            }
            body["dry_run"] = json!(false);
            send(client, Method::POST, "/v1/sessions/release", body).await
        }
    }
}

fn redact(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let name = key.to_ascii_lowercase();
                    let sensitive = name.contains("password")
                        || name.contains("token")
                        || name.contains("secret")
                        || name.contains("hash")
                        || name.contains("credential")
                        || name.ends_with("_key")
                        || matches!(name.as_str(), "key" | "cookie")
                        || name == "authorization"
                        || name == "phc"
                        || matches!(
                            name.as_str(),
                            "url" | "headers" | "params" | "json" | "body" | "last_error" | "error"
                        );
                    (
                        key.clone(),
                        if sensitive && !value.is_null() && !value.is_boolean() {
                            json!("[redacted]")
                        } else {
                            redact(value)
                        },
                    )
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        _ => value.clone(),
    }
}

fn cell(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Null => "-".into(),
        _ => value.to_string(),
    };
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn print_value(value: &Value, json_output: bool) -> Result<()> {
    let mut output = io::stdout().lock();
    if json_output {
        serde_json::to_writer_pretty(&mut output, value)
            .map_err(|_| anyhow::anyhow!("could not write JSON output"))?;
        writeln!(output).context("could not write output")?;
        return Ok(());
    }
    if let Some(items) = value.get("items").and_then(Value::as_array) {
        if items.is_empty() {
            writeln!(output, "No items.")?;
        } else {
            let preferred = [
                "id",
                "name",
                "kind",
                "business",
                "session",
                "owner_name",
                "protocol",
                "address",
                "state",
                "enabled",
                "version",
                "verification",
                "active_connections",
                "connections",
                "releasing",
            ];
            let columns: Vec<_> = preferred
                .into_iter()
                .filter(|key| items.iter().any(|item| item.get(key).is_some()))
                .collect();
            if columns.is_empty() {
                for item in items {
                    writeln!(output, "{}", cell(item))?;
                }
            } else {
                writeln!(
                    output,
                    "{}",
                    columns
                        .iter()
                        .map(|name| name.to_uppercase())
                        .collect::<Vec<_>>()
                        .join("\t")
                )?;
                for item in items {
                    writeln!(
                        output,
                        "{}",
                        columns
                            .iter()
                            .map(|key| cell(&item[key]))
                            .collect::<Vec<_>>()
                            .join("\t")
                    )?;
                }
            }
        }
        if let Some(total) = value.get("total") {
            writeln!(output, "Total: {}", cell(total))?;
        }
    } else if let Some(fields) = value.as_object() {
        for (key, value) in fields {
            writeln!(output, "{}: {}", cell(&json!(key)), cell(value))?;
        }
    } else {
        writeln!(output, "{}", cell(value))?;
    }
    Ok(())
}
