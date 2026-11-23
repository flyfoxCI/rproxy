use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Request {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    pub timeout: Duration,
}
#[derive(Debug)]
pub struct Output {
    pub success: bool,
    pub status: String,
    pub stdout: String,
    pub stderr: String,
}
pub trait Runner {
    fn run(&mut self, request: &Request) -> Result<Output>;
}

pub fn environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| {
            let name = name.to_ascii_uppercase();
            !name.starts_with("RPROXY_") && !name.starts_with("COMPOSE_")
        })
        .collect()
}

pub struct OwnedChild(pub Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn read_tail(path: &Path, limit: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(limit)))?;
    let mut data = Vec::new();
    file.take(limit).read_to_end(&mut data)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

pub struct SystemRunner;
impl Runner for SystemRunner {
    fn run(&mut self, request: &Request) -> Result<Output> {
        let out = tempfile::NamedTempFile::new()?;
        let err = tempfile::NamedTempFile::new()?;
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(&request.cwd)
            .env_clear()
            .envs(&request.env)
            .stdin(if request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(out.reopen()?)
            .stderr(err.reopen()?);
        let mut child = OwnedChild(command.spawn().context("start trusted local process")?);
        if let Some(input) = &request.stdin {
            child
                .0
                .stdin
                .take()
                .context("open process stdin")?
                .write_all(input.as_bytes())?;
        }
        let deadline = Instant::now() + request.timeout;
        let (status, timed_out) = loop {
            if let Some(status) = child.0.try_wait()? {
                break (status, false);
            }
            if Instant::now() >= deadline {
                let _ = child.0.kill();
                break (
                    child.0.wait().context("reap timed-out owned process")?,
                    true,
                );
            }
            thread::sleep(Duration::from_millis(25));
        };
        Ok(Output {
            success: status.success() && !timed_out,
            status: if timed_out {
                format!(
                    "timed out after {}ms; terminated process exit cause: {status}",
                    request.timeout.as_millis()
                )
            } else {
                status.to_string()
            },
            stdout: read_tail(out.path(), 1024 * 1024)?,
            stderr: read_tail(err.path(), 64 * 1024)?,
        })
    }
}
