use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use std::{fs::OpenOptions, io::Write, path::Path};

pub(crate) struct Vault {
    key: [u8; 32],
}
impl Vault {
    pub fn open(database: &Path, existing: bool) -> Result<Self> {
        if let Some(value) = std::env::var_os("RPROXY_SOURCE_KEY") {
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("source key is not valid UTF-8"))?;
            let bytes = STANDARD
                .decode(value)
                .map_err(|_| anyhow::anyhow!("source key must be base64 encoded"))?;
            let key = bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("source key must be 32 bytes"))?;
            return Ok(Self { key });
        }
        let mut key = [0; 32];
        if database == Path::new(":memory:") {
            rand::thread_rng().fill_bytes(&mut key);
            return Ok(Self { key });
        }
        let mut name = database.as_os_str().to_owned();
        name.push(".key");
        let path = std::path::PathBuf::from(name);
        if path.exists() {
            key = std::fs::read(&path)
                .context("cannot read source key file")?
                .try_into()
                .map_err(|_| anyhow::anyhow!("invalid source key file length"))?;
        } else {
            if existing {
                bail!("source encryption key is missing; restore the key or set RPROXY_SOURCE_KEY");
            }
            rand::thread_rng().fill_bytes(&mut key);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&path)
                .context("cannot create source key file")?;
            file.write_all(&key)?;
            file.sync_all()?;
        }
        Ok(Self { key })
    }
    pub fn seal(&self, value: &serde_json::Value) -> Result<String> {
        let mut nonce = [0; 24];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = XChaCha20Poly1305::new(&self.key.into());
        let encrypted = cipher
            .encrypt(&XNonce::from(nonce), serde_json::to_vec(value)?.as_slice())
            .map_err(|_| anyhow::anyhow!("cannot encrypt source config"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        Ok(STANDARD.encode(bytes))
    }
    pub fn reveal(&self, value: &str) -> Result<serde_json::Value> {
        let bytes = STANDARD
            .decode(value)
            .map_err(|_| anyhow::anyhow!("invalid encrypted source config"))?;
        if bytes.len() < 40 {
            bail!("invalid encrypted source config");
        }
        let nonce: [u8; 24] = bytes[..24].try_into()?;
        let decrypted = XChaCha20Poly1305::new(&self.key.into())
            .decrypt(&XNonce::from(nonce), &bytes[24..])
            .map_err(|_| anyhow::anyhow!("cannot decrypt source config; check the source key"))?;
        serde_json::from_slice(&decrypted)
            .map_err(|_| anyhow::anyhow!("invalid decrypted source config"))
    }
}

pub(crate) fn redact_config(config: &serde_json::Value) -> serde_json::Value {
    fn env_ref(value: &str) -> bool {
        value
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .is_some_and(|s| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
    }
    fn redact(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) if !env_ref(s) => *s = "<redacted>".into(),
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                *value = "<redacted>".into()
            }
            serde_json::Value::Object(object) => {
                for value in object.values_mut() {
                    redact(value);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    redact(item);
                }
            }
            _ => {}
        }
    }
    let mut output = config.clone();
    for key in ["url", "params", "headers", "json"] {
        if let Some(value) = output.get_mut(key) {
            redact(value);
        }
    }
    output
}
