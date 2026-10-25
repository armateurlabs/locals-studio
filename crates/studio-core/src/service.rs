use crate::{
    chat::ChatManager,
    events::{EventSender, emit},
    gateway::{self, GatewayHandle},
    hub::HubManager,
    models::{self, ModelRecord},
    paths::AppPaths,
    runtime::RuntimeManager,
    store::Store,
    supervisor::Supervisor,
    types::{ApiConfig, RpcRequest, RpcResponse, ServerProfile, ServerStatus, StudioConfig},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, RwLock, watch},
};

struct StoppingGuard<'a>(&'a AtomicUsize);
impl<'a> StoppingGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}
impl Drop for StoppingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn wait_for_generation_change(receiver: &mut watch::Receiver<u64>, epoch: u64) {
    loop {
        if *receiver.borrow() != epoch {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

pub struct Studio {
    pub paths: AppPaths,
    pub store: Store,
    pub events: EventSender,
    pub supervisor: Supervisor,
    pub hub: HubManager,
    pub runtime: RuntimeManager,
    pub chat: ChatManager,
    pub config: RwLock<StudioConfig>,
    pub api_state: RwLock<Value>,
    gateway: Mutex<Option<GatewayHandle>>,
    mutations: Arc<Mutex<()>>,
    server_operations: Mutex<()>,
    stop_generation: watch::Sender<u64>,
    stopping: AtomicUsize,
    pending_starts: Mutex<HashMap<String, Vec<watch::Sender<bool>>>>,
    shutting_down: AtomicBool,
}

impl Studio {
    pub async fn new(paths: AppPaths) -> Result<Arc<Self>> {
        paths.prepare()?;
        let store = Store::open(&paths.database_path)?;
        let config = store
            .get::<StudioConfig>("settings", "config")?
            .unwrap_or_default();
        let (events, _) = tokio::sync::broadcast::channel(2048);
        let mutations = Arc::new(Mutex::new(()));
        let (stop_generation, _) = watch::channel(0u64);
        let studio = Arc::new(Self {
            supervisor: Supervisor::new(paths.clone(), store.clone(), events.clone()),
            runtime: RuntimeManager::new(paths.clone(), events.clone()),
            hub: HubManager::new(paths.clone(), store.clone(), events.clone()),
            chat: ChatManager::with_mutations(store.clone(), events.clone(), mutations.clone()),
            paths,
            store,
            events,
            config: RwLock::new(config),
            api_state: RwLock::new(json!({"running":false})),
            gateway: Mutex::new(None),
            mutations,
            server_operations: Mutex::new(()),
            stop_generation,
            stopping: AtomicUsize::new(0),
            pending_starts: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        });
        studio.watch_downloaded_models();
        studio.supervisor.initialize().await?;
        if !studio.config.read().await.api.enabled {
            // A crash after persisting OFF must not resurrect workers whose shutdown was incomplete.
            studio.stop_live_workers().await?;
        }
        studio.hub.recover().await?;
        studio.restart_gateway().await;
        Ok(studio)
    }

    pub async fn restart_gateway(self: &Arc<Self>) {
        let mut slot = self.gateway.lock().await;
        if let Some(old) = slot.take() {
            old.shutdown().await;
        }
        let config = self.config.read().await.api.clone();
        if !config.enabled || self.shutting_down.load(Ordering::Acquire) {
            *self.api_state.write().await = gateway::description(&config, false, None);
            emit(&self.events, "api.changed", &*self.api_state.read().await);
            return;
        }
        match gateway::start(self.clone(), config.clone()).await {
            Ok(handle) => {
                *self.api_state.write().await = handle.description.clone();
                *slot = Some(handle);
            }
            Err(error) => {
                *self.api_state.write().await =
                    gateway::description(&config, false, Some(format!("{error:#}")));
            }
        }
        emit(&self.events, "api.changed", &*self.api_state.read().await);
    }

    pub async fn snapshot(&self) -> Result<Value> {
        Ok(
            json!({"config":*self.config.read().await,"api":*self.api_state.read().await,"models":models::list(&self.store)?,"profiles":self.store.list::<ServerProfile>("profiles")?,"servers":self.supervisor.list().await,"downloads":self.hub.list()?,"runtime":self.runtime.status().await?,"telemetry":self.supervisor.telemetry().await?,"folders":self.store.list::<Value>("folders")?,"conversations":self.store.list::<Value>("conversations")?}),
        )
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.shutting_down.store(true, Ordering::Release);
        self.stop_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        self.cancel_pending(None).await;
        let _operation = self.server_operations.lock().await;
        let mut gateway = self.gateway.lock().await.take();
        if let Some(gateway) = gateway.as_mut() {
            gateway.begin_shutdown();
        }
        // A management-service exit is not a persisted global OFF command.
        let api = self.config.read().await.api.clone();
        *self.api_state.write().await = gateway::description(&api, false, None);
        let closing = async {
            if let Some(gateway) = gateway {
                gateway.shutdown().await;
            }
        };
        let (_, result) = tokio::join!(closing, self.stop_live_workers());
        result.map(|_| ())
    }

    pub fn server_is_stopping(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire) || self.stopping.load(Ordering::Acquire) > 0
    }

    async fn estimate_with_active(&self, selected: &[ServerProfile]) -> Result<Value> {
        let mut profiles = selected.to_vec();
        for record in self.store.list::<Value>("workers")? {
            if record["status"]["pid"].is_null() {
                continue;
            }
            if let Ok(active) = serde_json::from_value::<ServerProfile>(record["profile"].clone()) {
                if !profiles.iter().any(|p| p.id == active.id) {
                    profiles.push(active);
                }
            }
        }
        self.supervisor
            .estimate(&profiles, &models::list(&self.store)?)
            .await
    }

    fn worker_origins_require_restart(&self, api: &ApiConfig) -> Result<bool> {
        let profiles = self.store.list::<ServerProfile>("profiles")?;
        for record in self.store.list::<Value>("workers")? {
            if record["status"]["pid"].is_null() {
                continue;
            }
            let active: ServerProfile = serde_json::from_value(record["profile"].clone())?;
            let profile = profiles
                .iter()
                .find(|p| p.id == active.id)
                .unwrap_or(&active);
            if active.env.get("LOCALS_ORIGINS") != Some(&effective_origins(profile, api)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn set_api_enabled(&self, enabled: bool) -> Result<()> {
        let _guard = self.mutations.lock().await;
        let mut config = self.config.write().await;
        if config.api.enabled != enabled {
            let mut next = config.clone();
            next.api.enabled = enabled;
            self.store.set("settings", "config", &next)?;
            *config = next;
            emit(&self.events, "config.changed", &*config);
        }
        Ok(())
    }

    async fn cancel_pending(&self, profiles: Option<&HashSet<String>>) -> bool {
        let mut pending = self.pending_starts.lock().await;
        let mut cancelled = false;
        pending.retain(|id, senders| {
            senders.retain(|sender| !sender.is_closed());
            if profiles.is_none_or(|profiles| profiles.contains(id)) {
                for sender in senders.iter() {
                    sender.send_replace(true);
                    cancelled = true;
                }
            }
            !senders.is_empty()
        });
        cancelled
    }

    async fn stop_live_workers(&self) -> Result<Vec<ServerStatus>> {
        let mut tasks = tokio::task::JoinSet::new();
        for status in self.supervisor.list().await {
            if status.pid.is_some() {
                let supervisor = self.supervisor.clone();
                tasks.spawn(async move { supervisor.stop(&status.profile_id).await });
            }
        }
        let mut statuses = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(status)) => statuses.push(status),
                Ok(Err(error)) => errors.push(format!("{error:#}")),
                Err(error) => errors.push(error.to_string()),
            }
        }
        if !errors.is_empty() {
            bail!("Some workers could not be stopped: {}", errors.join("; "));
        }
        Ok(statuses)
    }

    async fn stop_all(self: &Arc<Self>) -> Result<Value> {
        // Invalidate active AND queued starts before the first await. New start requests are
        // cancelled while any global stop is in progress; they cannot re-enable this listener.
        let _stopping = StoppingGuard::new(&self.stopping);
        self.stop_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        self.cancel_pending(None).await;
        let _operation = self.server_operations.lock().await;
        self.set_api_enabled(false).await?;
        let mut gateway = self.gateway.lock().await.take();
        if let Some(gateway) = gateway.as_mut() {
            gateway.begin_shutdown();
        }
        let config = self.config.read().await.api.clone();
        *self.api_state.write().await = gateway::description(&config, false, None);
        emit(&self.events, "api.changed", &*self.api_state.read().await);
        let closing = async {
            if let Some(gateway) = gateway {
                gateway.shutdown().await;
            }
        };
        // Drain listener connections and worker jobs together, rather than two consecutive 30s waits.
        let (_, servers) = tokio::join!(closing, self.stop_live_workers());
        let servers = servers?;
        Ok(json!({"servers":servers,"api":*self.api_state.read().await}))
    }

    async fn start_servers(self: &Arc<Self>, params: Value) -> Result<Value> {
        let mut generation = self.stop_generation.subscribe();
        let epoch = *generation.borrow();
        if self.server_is_stopping() {
            return Ok(json!({"cancelled":true,"servers":[],"api":*self.api_state.read().await}));
        }
        let all = params["all"].as_bool().unwrap_or(false);
        let id = params["id"].as_str();
        let selected: Vec<_> = self
            .store
            .list::<ServerProfile>("profiles")?
            .into_iter()
            .filter(|profile| {
                if all {
                    profile.selected
                } else {
                    id == Some(profile.id.as_str())
                        || id == Some(profile.alias.as_str())
                        || id == Some(profile.model_id.as_str())
                }
            })
            .collect();
        if selected.is_empty() && !all {
            bail!(
                "No server selected. Select profiles in Model Server or pass a profile identifier"
            );
        }
        let mut tickets = Vec::new();
        {
            let mut pending = self.pending_starts.lock().await;
            pending.retain(|_, senders| {
                senders.retain(|sender| !sender.is_closed());
                !senders.is_empty()
            });
            for profile in &selected {
                let (sender, receiver) = watch::channel(false);
                pending.entry(profile.id.clone()).or_default().push(sender);
                tickets.push((profile.clone(), receiver));
            }
        }
        let _operation = tokio::select! {
            biased;
            _=wait_for_generation_change(&mut generation,epoch)=>return Ok(json!({"cancelled":true,"servers":[],"api":*self.api_state.read().await})),
            guard=self.server_operations.lock()=>guard,
        };
        if *generation.borrow() != epoch || self.server_is_stopping() {
            return Ok(json!({"cancelled":true,"servers":[],"api":*self.api_state.read().await}));
        }
        if !tickets.is_empty() && tickets.iter().all(|(_, receiver)| *receiver.borrow()) {
            return Ok(json!({"cancelled":true,"servers":[],"api":*self.api_state.read().await}));
        }
        self.set_api_enabled(true).await?;
        if self.api_state.read().await["running"] != true {
            self.restart_gateway().await;
        }
        if self.api_state.read().await["running"] != true {
            bail!(
                "The Studio API is unavailable. Resolve its port or bind address in settings before loading a model"
            );
        }
        let estimate = self.estimate_with_active(&selected).await?;
        if *generation.borrow() != epoch || self.server_is_stopping() {
            return Ok(json!({"cancelled":true,"servers":[],"api":*self.api_state.read().await}));
        }
        let allow = params["allow_memory_overcommit"].as_bool().unwrap_or(false);
        if !selected.is_empty() && estimate["requires_override"].as_bool() == Some(true) && !allow {
            bail!("MEMORY_CONFIRMATION_REQUIRED: {}", estimate);
        }
        let models = models::list(&self.store)?;
        let api = self.config.read().await.api.clone();
        let mut servers = Vec::new();
        let mut cancelled = false;
        for (mut profile, receiver) in tickets {
            if *generation.borrow() != epoch || self.server_is_stopping() {
                cancelled = true;
                break;
            }
            if *receiver.borrow() {
                cancelled = true;
                continue;
            }
            let Some(current) = self.store.get::<ServerProfile>("profiles", &profile.id)? else {
                cancelled = true;
                continue;
            };
            profile = current;
            let origins = effective_origins(&profile, &api);
            profile.env.insert("LOCALS_ORIGINS".into(), origins);
            let model = models
                .iter()
                .find(|model| model.id == profile.model_id)
                .context("Model files are not registered")?;
            match self
                .supervisor
                .start_cancellable(profile, model.clone(), allow, receiver)
                .await?
            {
                Some(status) => servers.push(status),
                None => cancelled = true,
            }
        }
        cancelled |= *generation.borrow() != epoch;
        Ok(
            json!({"servers":servers,"estimate":estimate,"api":*self.api_state.read().await,"cancelled":cancelled}),
        )
    }

    async fn stop_servers(self: &Arc<Self>, params: Value) -> Result<Value> {
        if params["all"].as_bool() == Some(true) {
            return self.stop_all().await;
        }
        let id = params["id"].as_str();
        let profiles = self.store.list::<ServerProfile>("profiles")?;
        let statuses = self.supervisor.list().await;
        let mut ids: HashSet<String> = profiles
            .into_iter()
            .filter(|profile| {
                id == Some(profile.id.as_str())
                    || id == Some(profile.alias.as_str())
                    || id == Some(profile.model_id.as_str())
            })
            .map(|profile| profile.id)
            .collect();
        ids.extend(
            statuses
                .iter()
                .filter(|status| {
                    id == Some(status.profile_id.as_str())
                        || id == Some(status.alias.as_str())
                        || id == Some(status.model_id.as_str())
                })
                .map(|status| status.profile_id.clone()),
        );
        let cancelled = self.cancel_pending(Some(&ids)).await;
        let mut servers = Vec::new();
        for status in statuses
            .into_iter()
            .filter(|status| ids.contains(&status.profile_id))
        {
            servers.push(self.supervisor.stop(&status.profile_id).await?);
        }
        Ok(json!({"servers":servers,"cancelled":cancelled,"api":*self.api_state.read().await}))
    }

    pub async fn handle(self: &Arc<Self>, method: &str, params: Value) -> Result<Value> {
        match method {
            "ping" => Ok(json!({"service":"locals-studio","version":env!("CARGO_PKG_VERSION")})),
            "options.schema" => Ok(crate::supervisor::options_schema()),
            "snapshot" | "status" => self.snapshot().await,
            "models.list" => Ok(serde_json::to_value(models::list(&self.store)?)?),
            "models.scan" => {
                let defaults = self.config.read().await.library_paths.clone();
                let extra: Vec<PathBuf> = params["paths"]
                    .as_array()
                    .map(|v| {
                        v.iter()
                            .filter_map(Value::as_str)
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_else(|| defaults.iter().map(PathBuf::from).collect());
                let found = models::scan(&self.paths, &self.store, &extra)?;
                let _guard = self.mutations.lock().await;
                for model in &found {
                    self.ensure_profile(model)?;
                }
                emit(&self.events, "models.changed", &found);
                Ok(serde_json::to_value(found)?)
            }
            "models.import" => {
                let model = models::import(
                    &self.store,
                    std::path::Path::new(required(&params, "path")?),
                )?;
                let _guard = self.mutations.lock().await;
                self.ensure_profile(&model)?;
                emit(&self.events, "models.changed", &model);
                Ok(serde_json::to_value(model)?)
            }
            "models.remove" => {
                let _guard = self.mutations.lock().await;
                let id = required(&params, "id")?;
                let profiles: Vec<_> = self
                    .store
                    .list::<ServerProfile>("profiles")?
                    .into_iter()
                    .filter(|p| p.model_id == id)
                    .collect();
                let servers = self.supervisor.list().await;
                if servers.iter().any(|s| s.model_id == id && s.pid.is_some()) {
                    bail!("Stop the model before removing its registration");
                }
                // Include stale records from profiles removed by older Studio versions.
                let mut worker_ids: Vec<_> = profiles.iter().map(|p| p.id.clone()).collect();
                worker_ids.extend(
                    servers
                        .iter()
                        .filter(|s| s.model_id == id)
                        .map(|s| s.profile_id.clone()),
                );
                worker_ids.sort();
                worker_ids.dedup();
                self.cancel_pending(Some(
                    &worker_ids
                        .iter()
                        .cloned()
                        .chain(profiles.iter().map(|profile| profile.id.clone()))
                        .collect(),
                ))
                .await;
                for worker_id in worker_ids {
                    self.supervisor.forget_profile(&worker_id).await?;
                }
                for profile in profiles {
                    self.store.delete("profiles", &profile.id)?;
                }
                models::remove(&self.store, id)?;
                self.store.delete("model-profile-suppressed", id)?;
                emit(&self.events, "models.changed", &json!({"removed":id}));
                Ok(json!({"removed":id,"files_deleted":false}))
            }
            "profiles.list" => Ok(serde_json::to_value(
                self.store.list::<ServerProfile>("profiles")?,
            )?),
            "profiles.save" => {
                let _guard = self.mutations.lock().await;
                let profile: ServerProfile =
                    serde_json::from_value(params.get("profile").cloned().unwrap_or(params))?;
                validate_profile(&profile)?;
                if !models::list(&self.store)?
                    .iter()
                    .any(|m| m.id == profile.model_id)
                {
                    bail!("The profile references an unknown model");
                }
                if self
                    .store
                    .list::<ServerProfile>("profiles")?
                    .iter()
                    .any(|p| p.id != profile.id && p.alias == profile.alias)
                {
                    bail!("Another profile already uses this public model identifier");
                }
                self.store.set("profiles", &profile.id, &profile)?;
                emit(&self.events, "profiles.changed", &profile);
                Ok(serde_json::to_value(profile)?)
            }
            "profiles.remove" => {
                let _guard = self.mutations.lock().await;
                let id = required(&params, "id")?;
                if self
                    .supervisor
                    .list()
                    .await
                    .iter()
                    .any(|s| s.profile_id == id && s.pid.is_some())
                {
                    bail!("Stop the server before removing its profile");
                }
                self.cancel_pending(Some(&HashSet::from([id.to_owned()])))
                    .await;
                self.supervisor.forget_profile(id).await?;
                if let Some(profile) = self.store.get::<ServerProfile>("profiles", id)? {
                    // Record the user's choice before deleting: scans and download events must
                    // not restore a default profile after the last one was explicitly removed.
                    self.store
                        .set("model-profile-suppressed", &profile.model_id, &true)?;
                }
                self.store.delete("profiles", id)?;
                emit(&self.events, "profiles.changed", &json!({"removed":id}));
                Ok(json!({"removed":id}))
            }
            "memory.estimate" => {
                let profiles = self.store.list::<ServerProfile>("profiles")?;
                let id = params["id"].as_str();
                let selected: Vec<_> = profiles
                    .into_iter()
                    .filter(|p| {
                        if let Some(id) = id {
                            p.id == id || p.alias == id || p.model_id == id
                        } else {
                            p.selected
                        }
                    })
                    .collect();
                self.estimate_with_active(&selected).await
            }
            "server.start" => self.start_servers(params).await,
            "server.stop" => self.stop_servers(params).await,
            "server.status" => Ok(serde_json::to_value(self.supervisor.list().await)?),
            "server.logs" => self.supervisor.logs(required(&params, "id")?).await,
            "config.get" => {
                let value = serde_json::to_value(&*self.config.read().await)?;
                Ok(if let Some(key) = params["key"].as_str() {
                    get_key(&value, key)
                        .context("Unknown configuration key")?
                        .clone()
                } else {
                    value
                })
            }
            "config.set" => {
                if params["key"].as_str() == Some("api.enabled") {
                    bail!(
                        "Use server start --all or server stop --all to change the global server state"
                    );
                }
                // Match lifecycle lock order. In particular an old Settings snapshot must not
                // re-enable the API while a global stop is draining connections.
                let _operation = self.server_operations.lock().await;
                let _guard = self.mutations.lock().await;
                let old = self.config.read().await.clone();
                let mut value = serde_json::to_value(&old)?;
                if let Some(config) = params.get("config") {
                    value = config.clone();
                } else {
                    set_key(
                        &mut value,
                        required(&params, "key")?,
                        params.get("value").cloned().context("value is required")?,
                    )?;
                }
                let mut config: StudioConfig = serde_json::from_value(value)?;
                config.api.enabled = old.api.enabled;
                if config.api.port == 0 {
                    bail!("The public API port must be between 1 and 65535");
                }
                config
                    .api
                    .host
                    .parse::<std::net::IpAddr>()
                    .context("api.host must be an IPv4 or IPv6 address")?;
                let api_changed =
                    serde_json::to_value(&old.api)? != serde_json::to_value(&config.api)?;
                let worker_restart_required = old.api.allowed_origins != config.api.allowed_origins
                    || self.worker_origins_require_restart(&config.api)?;
                self.store.set("settings", "config", &config)?;
                *self.config.write().await = config.clone();
                if api_changed {
                    self.restart_gateway().await;
                }
                emit(&self.events, "config.changed", &config);
                Ok(
                    json!({"config":config,"api":*self.api_state.read().await,"worker_restart_required":worker_restart_required}),
                )
            }
            "search" => {
                self.hub
                    .search(params["query"].as_str().unwrap_or(""))
                    .await
            }
            "model.info" => self.hub.info(required(&params, "repo_id")?).await,
            "download.add" => {
                self.hub
                    .add(required(&params, "repo_id")?, params["revision"].as_str())
                    .await
            }
            "download.list" => Ok(json!(self.hub.list()?)),
            "download.pause" => self.hub.pause(required(&params, "id")?).await,
            "download.resume" => self.hub.resume(required(&params, "id")?).await,
            "download.cancel" => self.hub.cancel(required(&params, "id")?).await,
            "runtime.status" => self.runtime.status().await,
            "runtime.install" => self.runtime.install().await,
            "runtime.update" => self.runtime.update().await,
            "folders.list" => Ok(json!(self.store.list::<Value>("folders")?)),
            "folders.save" => self.save_document("folders", "folder", params).await,
            "folders.remove" => {
                let _guard = self.mutations.lock().await;
                let id = required(&params, "id")?;
                for mut conversation in self.store.list::<Value>("conversations")? {
                    if conversation["folder_id"].as_str() == Some(id) {
                        conversation["folder_id"] = Value::Null;
                        self.store.set(
                            "conversations",
                            conversation["id"]
                                .as_str()
                                .context("Invalid conversation")?,
                            &conversation,
                        )?;
                    }
                }
                self.store.delete("folders", id)?;
                emit(&self.events, "folders.changed", &params);
                Ok(json!({"removed":id}))
            }
            "conversations.list" => Ok(json!(self.store.list::<Value>("conversations")?)),
            "conversations.save" => {
                self.save_document("conversations", "conversation", params)
                    .await
            }
            "conversations.remove" => {
                let _guard = self.mutations.lock().await;
                let id = required(&params, "id")?;
                if self.chat.is_active(id).await {
                    bail!("Stop the running reply before deleting this conversation");
                }
                self.store.delete("conversations", id)?;
                emit(&self.events, "conversations.changed", &params);
                Ok(json!({"removed":id}))
            }
            "chat.start" => {
                let _guard = self.mutations.lock().await;
                if self.api_state.read().await["running"] != true {
                    bail!("The Studio API is unavailable. Check the API port in settings");
                }
                let config = self.config.read().await.api.clone();
                let host = match config.host.as_str() {
                    "0.0.0.0" => "127.0.0.1",
                    "::" => "::1",
                    other => other,
                };
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.into()
                };
                self.chat
                    .start(params, format!("http://{host}:{}", config.port))
                    .await
            }
            "chat.cancel" => self.chat.cancel(required(&params, "id")?).await,
            _ => bail!("Unknown method: {method}"),
        }
    }

    fn watch_downloaded_models(self: &Arc<Self>) {
        // Subscribe before recovery starts, so even a download completed during startup creates
        // its default profile. Snapshots remain read-only and never repair deleted profiles.
        let mut events = self.events.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let model_id = match events.recv().await {
                    Ok(event) if event["type"] == "models.updated" => {
                        let Some(id) = event["data"]["id"].as_str() else {
                            continue;
                        };
                        Some(id.to_owned())
                    }
                    Ok(_) => continue,
                    // Recover missed completion events without changing explicit deletions.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(studio) = weak.upgrade() else {
                    break;
                };
                let _guard = studio.mutations.lock().await;
                let result = (|| -> Result<()> {
                    let models = if let Some(id) = model_id {
                        studio
                            .store
                            .get::<ModelRecord>("models", &id)?
                            .into_iter()
                            .collect()
                    } else {
                        models::list(&studio.store)?
                    };
                    for model in models {
                        studio.ensure_profile(&model)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    tracing::warn!(%error, "Could not create a downloaded model's default profile");
                }
            }
        });
    }

    fn ensure_profile(&self, model: &ModelRecord) -> Result<()> {
        if !model.compatible
            || self
                .store
                .get::<bool>("model-profile-suppressed", &model.id)?
                .unwrap_or(false)
            || self
                .store
                .get::<ModelRecord>("models", &model.id)?
                .is_none()
        {
            return Ok(());
        }
        let profiles = self.store.list::<ServerProfile>("profiles")?;
        if profiles.iter().any(|p| p.model_id == model.id) {
            return Ok(());
        }
        let base: String = model
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let base = if base.trim_matches('-').is_empty() {
            "model".to_string()
        } else {
            base
        };
        let mut alias = base.clone();
        let mut suffix = 2;
        while profiles.iter().any(|p| p.alias == alias) {
            alias = format!("{base}-{suffix}");
            suffix += 1;
        }
        let profile = ServerProfile {
            model_id: model.id.clone(),
            alias,
            ..Default::default()
        };
        self.store.set("profiles", &profile.id, &profile)?;
        emit(&self.events, "profiles.changed", &profile);
        Ok(())
    }

    async fn save_document(&self, namespace: &str, field: &str, params: Value) -> Result<Value> {
        let _guard = self.mutations.lock().await;
        let mut value = params.get(field).cloned().unwrap_or(params);
        if !value.is_object() {
            bail!("Expected an object");
        }
        let id = value["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if namespace == "conversations" && self.chat.is_active(&id).await {
            bail!("Wait for the running reply before editing this conversation");
        }
        value["id"] = json!(id);
        value["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
        if namespace == "conversations" && !value["messages"].is_array() {
            value["messages"] = json!([]);
        }
        self.store.set(namespace, &id, &value)?;
        emit(&self.events, &format!("{namespace}.changed"), &value);
        Ok(value)
    }
}

fn effective_origins(profile: &ServerProfile, api: &ApiConfig) -> String {
    let mut origins: Vec<String> = if api.allowed_origins.is_empty() {
        vec!["*".into()]
    } else {
        api.allowed_origins
            .iter()
            .map(|origin| origin.trim().to_owned())
            .filter(|origin| !origin.is_empty())
            .collect()
    };
    if let Some(extra) = profile.env.get("LOCALS_ORIGINS") {
        for origin in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !origins.iter().any(|present| present == origin) {
                origins.push(origin.to_owned());
            }
        }
    }
    origins.join(",")
}

pub fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{key} is required"))
}

fn validate_profile(profile: &ServerProfile) -> Result<()> {
    if profile.id.is_empty()
        || profile.alias.is_empty()
        || !profile
            .alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        bail!(
            "Use a public model identifier containing letters, digits, dots, hyphens or underscores"
        );
    }
    for key in profile.options.keys() {
        if matches!(
            key.replace('-', "_").as_str(),
            "host" | "port" | "ollama_port" | "model"
        ) {
            bail!("{key} is managed by the Studio API, not by individual profiles");
        }
    }
    Ok(())
}

fn get_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |v, k| v.get(k))
}
fn set_key(value: &mut Value, key: &str, replacement: Value) -> Result<()> {
    let mut keys = key.split('.').peekable();
    let mut cursor = value;
    while let Some(key) = keys.next() {
        if keys.peek().is_none() {
            if cursor.get(key).is_none() {
                bail!("Unknown configuration key: {key}");
            }
            cursor[key] = replacement;
            return Ok(());
        }
        cursor = cursor.get_mut(key).context("Unknown configuration key")?;
    }
    bail!("A configuration key is required")
}

pub async fn run() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let paths = AppPaths::discover()?;
    paths.prepare()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.data_dir.join("daemon.lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    if paths.socket_path.exists() {
        std::fs::remove_file(&paths.socket_path)?;
    }
    let listener = UnixListener::bind(&paths.socket_path)?;
    std::fs::set_permissions(&paths.socket_path, std::fs::Permissions::from_mode(0o600))?;
    let studio = Studio::new(paths.clone()).await?;
    let discovery = studio.clone();
    tokio::spawn(async move {
        let paths = discovery.paths.clone();
        let store = discovery.store.clone();
        let extra: Vec<PathBuf> = discovery
            .config
            .read()
            .await
            .library_paths
            .iter()
            .map(PathBuf::from)
            .collect();
        match tokio::task::spawn_blocking(move || models::scan(&paths, &store, &extra)).await {
            Ok(Ok(found)) => {
                let _guard = discovery.mutations.lock().await;
                for model in &found {
                    let _ = discovery.ensure_profile(model);
                }
                emit(&discovery.events, "models.changed", &found);
            }
            result => tracing::warn!(?result, "Initial model discovery did not complete"),
        }
    });
    let weak = Arc::downgrade(&studio);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            let Some(studio) = weak.upgrade() else {
                break;
            };
            if let Ok(value) = studio.supervisor.telemetry().await {
                emit(&studio.events, "telemetry", &value);
            }
            emit(
                &studio.events,
                "servers.changed",
                &studio.supervisor.list().await,
            );
        }
    });
    loop {
        tokio::select! {
            connection=listener.accept()=>{let (socket,_)=connection?;let state=studio.clone();tokio::spawn(async move {if let Err(error)=serve_connection(socket,state).await {tracing::debug!(%error,"IPC connection ended");}});},
            _=tokio::signal::ctrl_c()=>break,
        }
    }
    let shutdown = studio.shutdown().await;
    let _ = std::fs::remove_file(&paths.socket_path);
    drop(lock);
    shutdown
}

async fn serve_connection(socket: UnixStream, studio: Arc<Studio>) -> Result<()> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader).take(16 * 1024 * 1024 + 1);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    )
    .await??;
    if line.len() > 16 * 1024 * 1024 {
        bail!("IPC request too large");
    }
    let request: RpcRequest = serde_json::from_str(&line)?;
    if request.method == "subscribe" {
        let mut events = studio.events.subscribe();
        writer
            .write_all(b"{\"type\":\"connected\",\"data\":{}}\n")
            .await?;
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    json!({"type":"resync","data":{}})
                }
                Err(_) => break,
            };
            let mut bytes = serde_json::to_vec(&event)?;
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
        }
    } else {
        let response =
            RpcResponse::from_result(studio.handle(&request.method, request.params).await);
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path, time::Duration};

    async fn test_studio(root: &Path) -> Arc<Studio> {
        let paths = AppPaths {
            data_dir: root.join("data"),
            database_path: root.join("data/store.sqlite3"),
            socket_path: root.join("data/studio.sock"),
            models_dir: root.join("models"),
            locals_source: root.join("source"),
        };
        paths.prepare().unwrap();
        let store = Store::open(&paths.database_path).unwrap();
        let mut config = StudioConfig::default();
        // The OS chooses an isolated test port. No workers or inference runtimes are started.
        config.api.port = 0;
        store.set("settings", "config", &config).unwrap();
        Studio::new(paths).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_stop_invalidates_all_start_requests_queued_before_it() {
        let directory = tempfile::tempdir().unwrap();
        let studio = test_studio(directory.path()).await;
        let gate = studio.server_operations.lock().await;
        let first = studio.clone();
        let second = studio.clone();
        let a =
            tokio::spawn(async move { first.handle("server.start", json!({"all":true})).await });
        let b =
            tokio::spawn(async move { second.handle("server.start", json!({"all":true})).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while studio.stop_generation.receiver_count() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let stopping = studio.clone();
        let stop =
            tokio::spawn(async move { stopping.handle("server.stop", json!({"all":true})).await });
        for task in [a, b] {
            let result = tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(result["cancelled"], true);
        }
        drop(gate);
        let result = tokio::time::timeout(Duration::from_secs(3), stop)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["api"]["running"], false);
        assert_eq!(
            studio
                .store
                .get::<StudioConfig>("settings", "config")
                .unwrap()
                .unwrap()
                .api
                .enabled,
            false
        );
        assert!(studio.supervisor.list().await.is_empty());
        studio.shutdown().await.unwrap();
    }

    fn write_model(root: &Path) -> PathBuf {
        let path = root.join("models/test-model");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("config.json"), br#"{"model_type":"llama"}"#).unwrap();
        fs::write(path.join("tokenizer.json"), b"{}").unwrap();
        fs::write(path.join("model.safetensors"), b"test weights").unwrap();
        path
    }

    fn profiles_for(studio: &Studio, model_id: &str) -> Vec<ServerProfile> {
        studio
            .store
            .list::<ServerProfile>("profiles")
            .unwrap()
            .into_iter()
            .filter(|profile| profile.model_id == model_id)
            .collect()
    }

    #[tokio::test]
    async fn empty_origin_policy_preserves_profile_extras_and_detects_legacy_workers() {
        let directory = tempfile::tempdir().unwrap();
        let studio = test_studio(directory.path()).await;
        let mut profile = ServerProfile::default();
        profile.env.insert(
            "LOCALS_ORIGINS".into(),
            "https://profile.example, https://profile.example".into(),
        );
        studio.store.set("profiles", &profile.id, &profile).unwrap();
        let mut api = ApiConfig::default();
        assert_eq!(
            effective_origins(&profile, &api),
            "*,https://profile.example"
        );
        api.allowed_origins = vec!["https://client.example".into()];
        assert_eq!(
            effective_origins(&profile, &api),
            "https://client.example,https://profile.example"
        );
        api.allowed_origins.clear();

        // This is metadata only: no process with this PID is created or signalled.
        let mut active = profile.clone();
        active
            .env
            .insert("LOCALS_ORIGINS".into(), "https://profile.example".into());
        studio
            .store
            .set(
                "workers",
                &profile.id,
                &json!({
                    "profile":active,"status":{"pid":u32::MAX}
                }),
            )
            .unwrap();
        assert!(studio.worker_origins_require_restart(&api).unwrap());
        active
            .env
            .insert("LOCALS_ORIGINS".into(), effective_origins(&profile, &api));
        studio
            .store
            .set(
                "workers",
                &profile.id,
                &json!({
                    "profile":active,"status":{"pid":u32::MAX}
                }),
            )
            .unwrap();
        assert!(!studio.worker_origins_require_restart(&api).unwrap());
        assert_eq!(
            studio
                .store
                .get::<ServerProfile>("profiles", &profile.id)
                .unwrap()
                .unwrap()
                .env["LOCALS_ORIGINS"],
            "https://profile.example, https://profile.example"
        );
        studio.store.delete("workers", &profile.id).unwrap();
        studio.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn snapshots_do_not_create_profiles_but_download_completion_does() {
        let directory = tempfile::tempdir().unwrap();
        let studio = test_studio(directory.path()).await;
        let model = models::import(&studio.store, &write_model(directory.path())).unwrap();
        studio.snapshot().await.unwrap();
        assert!(profiles_for(&studio, &model.id).is_empty());

        let mut events = studio.events.subscribe();
        emit(&studio.events, "models.updated", &model);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.unwrap();
                if event["type"] == "profiles.changed"
                    && event["data"]["model_id"].as_str() == Some(model.id.as_str())
                {
                    break;
                }
            }
        })
        .await
        .expect("download completion creates a profile without requiring a snapshot");
        assert_eq!(profiles_for(&studio, &model.id).len(), 1);
        studio.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn deleted_default_profile_stays_deleted_after_scan_and_reload() {
        let directory = tempfile::tempdir().unwrap();
        let studio = test_studio(directory.path()).await;
        let path = write_model(directory.path());
        let model = studio
            .handle("models.import", json!({"path":path}))
            .await
            .unwrap();
        let model_id = model["id"].as_str().unwrap();
        let profiles = profiles_for(&studio, model_id);
        assert_eq!(profiles.len(), 1);
        studio
            .handle("profiles.remove", json!({"id":profiles[0].id}))
            .await
            .unwrap();
        studio.snapshot().await.unwrap();
        studio
            .handle("models.scan", json!({"paths":[path]}))
            .await
            .unwrap();
        studio
            .handle("models.import", json!({"path":path}))
            .await
            .unwrap();
        assert!(profiles_for(&studio, model_id).is_empty());
        studio.shutdown().await.unwrap();
        drop(studio);

        let studio = test_studio(directory.path()).await;
        studio
            .handle("models.scan", json!({"paths":[path]}))
            .await
            .unwrap();
        studio.snapshot().await.unwrap();
        assert!(profiles_for(&studio, model_id).is_empty());

        // Explicitly unregistering and adding the model again is a fresh addition.
        studio
            .handle("models.remove", json!({"id":model_id}))
            .await
            .unwrap();
        assert!(path.join("model.safetensors").is_file());
        studio
            .handle("models.import", json!({"path":path}))
            .await
            .unwrap();
        assert_eq!(profiles_for(&studio, model_id).len(), 1);
        studio.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn removing_profiles_and_models_forgets_stopped_worker_records() {
        let directory = tempfile::tempdir().unwrap();
        let studio = test_studio(directory.path()).await;
        let path = write_model(directory.path());
        let model = studio
            .handle("models.import", json!({"path":path}))
            .await
            .unwrap();
        let model_id = model["id"].as_str().unwrap();
        let first = profiles_for(&studio, model_id).pop().unwrap();
        let second = ServerProfile {
            model_id: model_id.to_owned(),
            alias: "second-profile".into(),
            ..Default::default()
        };
        studio
            .handle("profiles.save", json!({"profile":second}))
            .await
            .unwrap();
        for profile in [&first, &second] {
            studio
                .store
                .set(
                    "workers",
                    &profile.id,
                    &json!({
                        "profile":profile,
                        "status":{
                            "profile_id":profile.id,"alias":profile.alias,
                            "model_id":model_id,"state":"stopped","pid":null,
                            "port":null,"backend_model":null,"error":null,
                            "active_requests":0,"started_at":null
                        },
                        "identity":null,
                        "log_path":studio.paths.data_dir.join("logs/test.log"),
                        "model_path":path
                    }),
                )
                .unwrap();
        }
        studio.shutdown().await.unwrap();
        drop(studio);

        let studio = test_studio(directory.path()).await;
        assert_eq!(studio.supervisor.list().await.len(), 2);
        studio
            .handle("profiles.remove", json!({"id":first.id}))
            .await
            .unwrap();
        assert!(
            studio
                .store
                .get::<Value>("workers", &first.id)
                .unwrap()
                .is_none()
        );
        assert!(
            studio
                .supervisor
                .list()
                .await
                .iter()
                .all(|s| s.profile_id != first.id)
        );

        studio
            .handle("models.remove", json!({"id":model_id}))
            .await
            .unwrap();
        assert!(studio.store.list::<Value>("workers").unwrap().is_empty());
        assert!(studio.supervisor.list().await.is_empty());
        assert!(profiles_for(&studio, model_id).is_empty());
        assert!(path.join("model.safetensors").is_file());
        studio.shutdown().await.unwrap();
    }
}
