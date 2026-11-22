use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};

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
    pub metrics_enabled: bool,
    pub host: String,
    pub port: u16,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            metrics_enabled: false,
            host: "127.0.0.1".into(),
            port: 8321,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
        }
    }
}

/// The inference engine a server profile runs on. Profiles saved before engines existed are Locals.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Locals,
    Kapitsa,
}

impl Engine {
    pub const ALL: [Engine; 2] = [Engine::Locals, Engine::Kapitsa];

    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Locals => "locals",
            Engine::Kapitsa => "kapitsa",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::Locals => "Locals",
            Engine::Kapitsa => "Kapitsa",
        }
    }

    pub fn parse(name: &str) -> anyhow::Result<Self> {
        Engine::ALL
            .into_iter()
            .find(|engine| engine.as_str() == name)
            .ok_or_else(|| anyhow::anyhow!("Unknown engine {name:?}; expected locals or kapitsa"))
    }
}

impl std::fmt::Display for Engine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerProfile {
    pub id: String,
    pub model_id: String,
    pub alias: String,
    pub engine: Engine,
    pub selected: bool,
    pub options: Map<String, Value>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelAlias {
    pub id: String,
    pub name: String,
    pub profile_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigurationPreset {
    pub id: String,
    pub name: String,
    /// The engine whose options the preset holds; presets saved before engines existed are Locals.
    pub engine: Engine,
    pub options: Map<String, Value>,
}

impl Default for ConfigurationPreset {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            engine: Engine::Locals,
            options: Map::new(),
        }
    }
}

impl Default for ModelAlias {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            profile_ids: Vec::new(),
        }
    }
}

impl Default for ServerProfile {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            model_id: String::new(),
            alias: String::new(),
            engine: Engine::Locals,
            selected: false,
            options: Map::new(),
            env: BTreeMap::new(),
        }
    }
}

pub(crate) fn studio_server_defaults() -> Map<String, Value> {
    Map::new()
}

/// Resolve explicit launch options without changing the saved profile. Missing and null values omit
/// the argument, retaining Locals' runtime default/Auto. Canonical keys win if both
/// underscore and CLI spellings of the same option were saved.
pub(crate) fn effective_server_options(profile: &ServerProfile) -> Map<String, Value> {
    let mut options = studio_server_defaults();
    let normalize = |key: &str| key.trim_start_matches("--").replace('-', "_");
    for (key, value) in &profile.options {
        options.insert(normalize(key), value.clone());
    }
    for (key, value) in &profile.options {
        if *key == normalize(key) {
            options.insert(key.clone(), value.clone());
        }
    }
    options
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerModelContext {
    pub context_length: Option<u64>,
    pub max_model_len: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub profile_id: String,
    pub alias: String,
    pub model_id: String,
    #[serde(default)]
    pub engine: Engine,
    /// The engine version this worker was started with; a newer installed version applies on restart.
    #[serde(default)]
    pub runtime_version: Option<String>,
    pub state: String,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub backend_model: Option<String>,
    #[serde(default)]
    pub backend_models: Vec<String>,
    #[serde(default)]
    pub backend_model_contexts: HashMap<String, WorkerModelContext>,
    pub error: Option<String>,
    pub active_requests: usize,
    pub started_at: Option<String>,
}

pub use studio_client::types::{RpcRequest, RpcResponse};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_launch_defaults_do_not_modify_saved_profiles_and_keep_explicit_values() {
        let profile = ServerProfile::default();
        let before = serde_json::to_value(&profile).unwrap();
        assert!(effective_server_options(&profile).is_empty());
        assert_eq!(serde_json::to_value(&profile).unwrap(), before);
        let legacy: ServerProfile = serde_json::from_value(
            serde_json::json!({"id":"legacy","model_id":"old-model","alias":"legacy"}),
        )
        .unwrap();
        assert!(effective_server_options(&legacy).is_empty());

        let mut profile = profile;
        profile.options = serde_json::from_value(serde_json::json!({
            "temp":0,"top-p":1,"top_k":0,"--max-tokens":2048,"max-context":4096,"slots":1
        }))
        .unwrap();
        let before = profile.options.clone();
        assert_eq!(
            effective_server_options(&profile),
            serde_json::from_value::<Map<String, Value>>(serde_json::json!({
                "temp":0,"top_p":1,"top_k":0,"max_tokens":2048,"max_context":4096,"slots":1
            }))
            .unwrap()
        );
        assert_eq!(profile.options, before);
    }

    #[test]
    fn explicit_null_and_canonical_spellings_override_defaults_and_cli_aliases() {
        let mut profile = ServerProfile::default();
        profile.options = serde_json::from_value(serde_json::json!({
            "temp":null,"top-p":null,"top_k":null,"max-tokens":2048,
            "max_tokens":null,"max-context":null
        }))
        .unwrap();
        let options = effective_server_options(&profile);
        for key in ["temp", "top_p", "top_k", "max_tokens", "max_context"] {
            assert_eq!(options.get(key), Some(&Value::Null), "{key}");
        }
        assert_eq!(options.len(), 5, "aliases produce only one launch argument");
    }

    #[test]
    fn legacy_worker_statuses_have_no_invented_context_limits() {
        let status: ServerStatus = serde_json::from_value(serde_json::json!({
            "profile_id":"profile","alias":"model","model_id":"model",
            "state":"ready","pid":123,"port":8322,"backend_model":"locals/model",
            "error":null,"active_requests":0,"started_at":null
        }))
        .unwrap();
        assert!(status.backend_model_contexts.is_empty());
        assert_eq!(status.engine, Engine::Locals);
        assert_eq!(status.runtime_version, None);
    }

    #[test]
    fn profiles_saved_before_engines_existed_run_on_locals() {
        let legacy: ServerProfile = serde_json::from_value(
            serde_json::json!({"id":"legacy","model_id":"old-model","alias":"legacy"}),
        )
        .unwrap();
        assert_eq!(legacy.engine, Engine::Locals);
        let kapitsa: ServerProfile =
            serde_json::from_value(serde_json::json!({"id":"k","engine":"kapitsa"})).unwrap();
        assert_eq!(kapitsa.engine, Engine::Kapitsa);
        assert!(
            serde_json::from_value::<ServerProfile>(serde_json::json!({"engine":"vllm"})).is_err()
        );
    }

    #[test]
    fn old_api_configuration_defaults_to_enabled() {
        let config:ApiConfig=serde_json::from_value(serde_json::json!({"host":"127.0.0.1","port":8321,"allowed_hosts":[],"allowed_origins":[]})).unwrap();
        assert!(config.enabled);
        assert!(!config.metrics_enabled);
    }

    #[test]
    fn prometheus_preference_round_trips_and_requires_a_boolean() {
        let mut config = StudioConfig::default();
        config.api.metrics_enabled = true;
        let restored: StudioConfig =
            serde_json::from_value(serde_json::to_value(config).unwrap()).unwrap();
        assert!(restored.api.metrics_enabled);
        assert!(
            serde_json::from_value::<ApiConfig>(serde_json::json!({"metrics_enabled":"true"}))
                .is_err()
        );
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
