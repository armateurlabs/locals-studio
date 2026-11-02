use crate::{
    gateway::parse_model,
    store::Store,
    supervisor::{Supervisor, WorkerLease},
    types::{ModelAlias, ServerProfile},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex;

#[derive(Debug)]
pub(crate) struct OpaqueContext;
impl std::fmt::Display for OpaqueContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Model aliases require full message history; Ollama context token IDs cannot be shared between models")
    }
}
impl std::error::Error for OpaqueContext {}

const ALIASES: &str = "model-aliases";
const BINDINGS: &str = "alias-bindings";

#[derive(Serialize, Deserialize)]
struct Binding {
    alias_id: String,
    session_hash: String,
    profile_id: String,
    model_id: String,
}

pub struct AliasRouter {
    store: Store,
    // CRUD, first assignment and failover share this lock. No SQLite connection
    // lock spans an await; supervisor selection reserves a worker lease atomically.
    cursors: Mutex<HashMap<String, usize>>,
}

impl AliasRouter {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            cursors: Mutex::new(HashMap::new()),
        }
    }

    pub fn list(&self) -> Result<Vec<ModelAlias>> {
        self.store.list(ALIASES)
    }

    pub fn name_in_use(&self, name: &str) -> Result<bool> {
        Ok(self.list()?.iter().any(|alias| alias.name == name))
    }

    pub fn ensure_profiles_unreferenced(&self, ids: &[String]) -> Result<()> {
        if let Some(alias) = self
            .list()?
            .iter()
            .find(|alias| alias.profile_ids.iter().any(|id| ids.contains(id)))
        {
            bail!(
                "Remove the server from model alias '{}' before deleting it",
                alias.name
            );
        }
        Ok(())
    }

    pub async fn save(&self, mut alias: ModelAlias, supervisor: &Supervisor) -> Result<ModelAlias> {
        let _guard = self.cursors.lock().await;
        validate_name(&alias.name)?;
        if alias.id.is_empty()
            || alias.id.len() > 128
            || !alias
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            bail!("The alias identifier is invalid");
        }
        if let Some(previous) = self.store.get::<ModelAlias>(ALIASES, &alias.id)? {
            if previous.name != alias.name {
                bail!("An alias name cannot be changed; create another alias instead");
            }
        }
        let profiles = self.store.list::<ServerProfile>("profiles")?;
        if profiles.iter().any(|profile| profile.alias == alias.name)
            || self
                .list()?
                .iter()
                .any(|other| other.id != alias.id && other.name == alias.name)
        {
            bail!("Another server or alias already uses this public model identifier");
        }
        let mut seen = HashSet::new();
        alias.profile_ids.retain(|id| seen.insert(id.clone()));
        if alias.profile_ids.is_empty() {
            bail!("Select at least one server for the alias");
        }
        if alias
            .profile_ids
            .iter()
            .any(|id| !profiles.iter().any(|profile| &profile.id == id))
        {
            bail!("The alias references an unknown server profile");
        }
        supervisor
            .with_unused_alias_name(&alias.name, || self.store.set(ALIASES, &alias.id, &alias))
            .await?;
        Ok(alias)
    }

    pub async fn remove(&self, id: &str) -> Result<()> {
        let mut cursors = self.cursors.lock().await;
        self.store
            .get::<ModelAlias>(ALIASES, id)?
            .context("Model alias not found")?;
        let bindings = self.store.list::<Binding>(BINDINGS)?;
        let keys: Vec<_> = bindings
            .iter()
            .filter(|binding| binding.alias_id == id)
            .map(|binding| binding_key(&binding.alias_id, &binding.session_hash))
            .collect();
        let mut records: Vec<_> = keys.iter().map(|key| (BINDINGS, key.as_str())).collect();
        records.push((ALIASES, id));
        self.store.delete_records(&records)?;
        cursors.remove(id);
        Ok(())
    }

    /// None means this identifier belongs to the existing direct-profile route.
    pub async fn acquire(
        &self,
        requested: &str,
        session: Option<&str>,
        supervisor: &Supervisor,
        opaque_context: bool,
    ) -> Result<Option<(WorkerLease, String)>> {
        let mut cursors = self.cursors.lock().await;
        let aliases = self.list()?;
        let profiles = self.store.list::<ServerProfile>("profiles")?;
        let (parsed, engine, mut thinking) = parse_model(requested);
        let undecorated = requested
            .strip_prefix("locals/")
            .or_else(|| requested.strip_prefix("mlx/"))
            .or_else(|| requested.strip_prefix("stock/"))
            .unwrap_or(requested);
        // Exact names win over generated thinking variants, just as for direct profiles.
        if profiles.iter().any(|profile| profile.alias == undecorated)
            || supervisor.has_ready_alias(undecorated).await
        {
            return Ok(None);
        }
        let name = if aliases.iter().any(|alias| alias.name == undecorated) {
            thinking = false;
            undecorated
        } else {
            parsed.as_str()
        };
        let Some(alias) = aliases.iter().find(|alias| alias.name == name) else {
            return Ok(None);
        };
        if opaque_context {
            return Err(OpaqueContext.into());
        }
        let members: Vec<_> = alias
            .profile_ids
            .iter()
            .filter_map(|id| profiles.iter().find(|profile| &profile.id == id).cloned())
            .collect();
        let session_hash = session_hash(session);
        let key = binding_key(&alias.id, &session_hash);
        let binding = self.store.get::<Binding>(BINDINGS, &key)?;
        let preferred = binding
            .as_ref()
            .map(|binding| (binding.profile_id.as_str(), binding.model_id.as_str()));
        let cursor = *cursors.get(&alias.id).unwrap_or(&0);
        let (lease, reused) = supervisor
            .acquire_pool(&members, preferred, engine, thinking, cursor)
            .await?;
        if !reused {
            self.store.set(
                BINDINGS,
                &key,
                &Binding {
                    alias_id: alias.id.clone(),
                    session_hash,
                    profile_id: lease.profile_id.clone(),
                    model_id: lease.activity_status.model_id.clone(),
                },
            )?;
            cursors.insert(alias.id.clone(), cursor.wrapping_add(1));
        }
        let backend = crate::gateway::rewrite_model(&lease.backend_model, engine, thinking);
        Ok(Some((lease, backend)))
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.len() > 128
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!("An alias name must use letters, numbers, hyphens, underscores or dots");
    }
    Ok(())
}

fn session_hash(session: Option<&str>) -> String {
    let mut hash = Sha256::new();
    match session {
        Some(session) => {
            hash.update([1]);
            hash.update(session.as_bytes());
        }
        None => hash.update([0]),
    }
    format!("{:x}", hash.finalize())
}

fn binding_key(alias_id: &str, session_hash: &str) -> String {
    format!("{:x}", Sha256::digest(format!("{alias_id}:{session_hash}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_slots_and_alias_names_are_unambiguous() {
        assert_ne!(session_hash(None), session_hash(Some("")));
        assert_ne!(session_hash(None), session_hash(Some("default")));
        assert_ne!(
            binding_key("a", &session_hash(Some("b"))),
            binding_key("b", &session_hash(Some("a")))
        );
        for name in ["qwen", "qwen-thinking", "qwen_27b.v1"] {
            assert!(validate_name(name).is_ok());
        }
        for name in ["", ".", "..", "mlx/qwen", "qwen x", "qwen\n"] {
            assert!(validate_name(name).is_err());
        }
    }
}
