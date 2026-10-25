use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioConfig {
    pub api: ApiConfig,
    pub library_paths: Vec<String>,
    pub memory_budget_bytes: Option<u64>,
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            library_paths: Vec::new(),
            memory_budget_bytes: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8321,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerProfile {
    pub id: String,
    pub model_id: String,
    pub alias: String,
    pub selected: bool,
    pub options: Map<String, Value>,
    pub env: BTreeMap<String, String>,
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            model_id: String::new(),
            alias: String::new(),
            selected: false,
            options: Map::new(),
            env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub profile_id: String,
    pub alias: String,
    pub model_id: String,
    pub state: String,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub backend_model: Option<String>,
    pub error: Option<String>,
    pub active_requests: usize,
    pub started_at: Option<String>,
}

pub use studio_client::types::{RpcRequest, RpcResponse};
