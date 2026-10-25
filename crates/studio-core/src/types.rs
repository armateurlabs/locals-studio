use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StudioConfig {
    pub api: ApiConfig,
    pub desktop: DesktopConfig,
    pub library_paths: Vec<String>,
    pub memory_budget_bytes: Option<u64>,
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            api: ApiConfig::default(),
            desktop: DesktopConfig::default(),
            library_paths: Vec::new(),
            memory_budget_bytes: None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    pub menu_bar_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_api_configuration_defaults_to_enabled() {
        let config:ApiConfig=serde_json::from_value(serde_json::json!({"host":"127.0.0.1","port":8321,"allowed_hosts":[],"allowed_origins":[]})).unwrap();
        assert!(config.enabled);
    }

    #[test]
    fn old_studio_config_keeps_menu_bar_disabled_and_new_preference_round_trips() {
        let mut config: StudioConfig = serde_json::from_value(
            serde_json::json!({"api":{"host":"127.0.0.1","port":8321},"library_paths":[]}),
        )
        .unwrap();
        assert!(!config.desktop.menu_bar_enabled);
        config.desktop.menu_bar_enabled = true;
        let restored: StudioConfig =
            serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        assert!(restored.desktop.menu_bar_enabled);
    }
}
