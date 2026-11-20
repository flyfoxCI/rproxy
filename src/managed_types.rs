use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::SocketAddr;

#[derive(Clone, Serialize, Deserialize)]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub businesses: Vec<String>,
    pub enabled: bool,
    pub deleted: bool,
    pub version: u64,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserChange {
    pub password: Option<String>,
    pub businesses: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub disconnect: bool,
    pub expected_version: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub name: String,
    #[serde(default = "api_kind")]
    pub kind: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "empty_object")]
    pub config: Value,
}
fn api_kind() -> String {
    "api".into()
}
fn empty_object() -> Value {
    serde_json::json!({})
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceChange {
    pub config: Option<Value>,
    pub enabled: Option<bool>,
    pub expected_version: Option<u64>,
}
#[derive(Clone, Serialize)]
pub struct SourceView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub deleted: bool,
    pub version: u64,
    pub run_generation: u64,
    pub last_finished_generation: u64,
    pub last_run_at: u64,
    pub last_error: Option<String>,
    pub config: Value,
}
#[derive(Serialize)]
pub struct CollectorPlan {
    pub version: &'static str,
    pub sources: Vec<CollectorSource>,
}
#[derive(Serialize)]
pub struct CollectorSource {
    pub id: String,
    pub name: String,
    pub version: u64,
    pub run_generation: u64,
    pub config: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectorResult {
    pub source_id: String,
    pub version: u64,
    pub run_generation: u64,
    pub addresses: Vec<SocketAddr>,
    #[serde(default = "yes")]
    pub complete: bool,
    pub error: Option<String>,
}
fn yes() -> bool {
    true
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyAction {
    pub id: String,
    pub enabled: Option<bool>,
    pub remove: bool,
    pub disconnect: bool,
    pub dry_run: bool,
    pub confirm: bool,
}
#[derive(Serialize)]
pub struct Impact {
    pub applied: bool,
    pub affected_sessions: usize,
    pub active_connections: usize,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminRelease {
    pub business: String,
    pub session: String,
    pub disconnect: bool,
    pub dry_run: bool,
}
