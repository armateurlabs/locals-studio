use std::{
    collections::{HashMap, HashSet},
    net::TcpListener,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::{Child, Command},
    sync::{Mutex, watch},
    time::Instant,
};

use crate::{
    events::{self, EventSender},
    models::ModelRecord,
    paths::AppPaths,
    runtime::{self, RuntimeManager},
    store::Store,
    telemetry::{self, TelemetryCollector},
    types::{
        Engine, ModelAlias, ServerProfile, ServerStatus, StudioConfig, WorkerModelContext,
        effective_server_options, studio_server_defaults,
    },
};

#[derive(Clone)]
pub struct Supervisor {
    paths: AppPaths,
    store: Store,
    events: EventSender,
    runtime: RuntimeManager,
    client: reqwest::Client,
    workers: Arc<Mutex<HashMap<String, Worker>>>,
    lifecycle: Arc<Mutex<()>>,
    stop_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    collector: TelemetryCollector,
}

struct Worker {
    record: WorkerRecord,
    child: Option<Child>,
    active: Arc<AtomicUsize>,
    accepting: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct WorkerRecord {
    profile: ServerProfile,
    status: ServerStatus,
    identity: Option<ProcessIdentity>,
    log_path: PathBuf,
    model_path: PathBuf,
    /// The installed engine version this worker runs from, kept while the worker lives.
    #[serde(default)]
    runtime_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProcessIdentity {
    pid: u32,
    start_time: u64,
    command: Vec<String>,
}

pub struct WorkerLease {
    pub profile_id: String,
    pub base_url: String,
    pub backend_model: String,
    pub(crate) activity_status: ServerStatus,
    active: Arc<AtomicUsize>,
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Supervisor {
    pub fn new(paths: AppPaths, store: Store, events: EventSender) -> Self {
        Self {
            runtime: RuntimeManager::new(paths.clone(), events.clone()),
            paths,
            store,
            events: events.clone(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .no_proxy()
                .build()
                .expect("local HTTP client"),
            workers: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(())),
            stop_locks: Arc::new(Mutex::new(HashMap::new())),
            collector: TelemetryCollector::with_events(events.clone()),
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.paths.data_dir.join("logs")).await?;
        let records = self.store.list::<WorkerRecord>("workers")?;
        for mut record in records {
            // Model limits are worker observations, never inherited from an old record or profile.
            record.status.backend_model_contexts.clear();
            let owned = record.identity.as_ref().is_some_and(identity_matches);
            let mut accepting = false;
            if owned {
                if record.status.state == "draining" {
                    // A daemon restart must not turn a deliberately stopped server back into a ready one.
                    if let Some(identity) = record.identity.as_ref() {
                        signal_owned(identity, libc::SIGTERM)?;
                    }
                    record.status.state = "stopping".into();
                } else if let Ok((backend, models, contexts)) = self.probe(&record).await {
                    record.status.backend_model = Some(backend);
                    record.status.backend_models = models;
                    record.status.backend_model_contexts = contexts;
                    record.status.state = "ready".into();
                    record.status.error = None;
                    accepting = true;
                } else if record.status.state != "starting" {
                    record.status.state = "error".into();
                    record.status.error = Some("The owned worker is running but failed its health/model check; inspect its logs.".into());
                }
            } else {
                record.status.pid = None;
                record.status.port = None;
                record.status.backend_model = None;
                record.status.backend_models.clear();
                record.status.backend_model_contexts.clear();
                record.identity = None;
                if !matches!(record.status.state.as_str(), "stopped" | "error") {
                    record.status.state = "error".into();
                    record.status.error = Some("The previously recorded worker has exited or its PID belongs to another process.".into());
                }
            }
            if !owned
                && record.status.pid.is_none()
                && self
                    .store
                    .get::<ServerProfile>("profiles", &record.profile.id)?
                    .is_none()
            {
                // Old versions left stopped worker records behind after profile deletion.
                self.store.delete("workers", &record.profile.id)?;
                self.workers.lock().await.remove(&record.profile.id);
                continue;
            }
            self.persist(&record)?;
            let resume = record.status.state == "starting" && owned;
            let id = record.profile.id.clone();
            self.workers.lock().await.insert(
                id.clone(),
                Worker {
                    record,
                    child: None,
                    active: Arc::new(AtomicUsize::new(0)),
                    accepting,
                },
            );
            if resume {
                let supervisor = self.clone();
                tokio::spawn(async move {
                    let _ = supervisor.wait_ready(&id).await;
                });
            }
        }
        let supervisor = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                supervisor.reconcile_exits().await;
            }
        });
        Ok(())
    }

    pub async fn start(
        &self,
        profile: ServerProfile,
        model: ModelRecord,
        allow_memory_overcommit: bool,
    ) -> Result<ServerStatus> {
        self.start_inner(profile, model, allow_memory_overcommit, None)
            .await?
            .context("Server start was cancelled")
    }

    pub async fn start_cancellable(
        &self,
        profile: ServerProfile,
        model: ModelRecord,
        allow_memory_overcommit: bool,
        cancellation: watch::Receiver<bool>,
    ) -> Result<Option<ServerStatus>> {
        self.start_inner(profile, model, allow_memory_overcommit, Some(cancellation))
            .await
    }

    async fn start_inner(
        &self,
        profile: ServerProfile,
        model: ModelRecord,
        allow_memory_overcommit: bool,
        mut cancellation: Option<watch::Receiver<bool>>,
    ) -> Result<Option<ServerStatus>> {
        // Serialize model loads: overlapping loads independently size against the same GPU capacity.
        let _guard = tokio::select! {
            biased;
            _=wait_start_cancel(&mut cancellation)=>return Ok(None),
            guard=self.lifecycle.lock()=>guard,
        };
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        // A daemon may have reattached a worker that is still loading. Include that load in serialization.
        let pending: Vec<String> = self
            .workers
            .lock()
            .await
            .values()
            .filter(|worker| {
                worker.record.status.state == "starting" && worker.record.profile.id != profile.id
            })
            .map(|worker| worker.record.profile.id.clone())
            .collect();
        for id in pending {
            tokio::select! {
                biased;
                _=wait_start_cancel(&mut cancellation)=>return Ok(None),
                _=self.wait_ready(&id)=>{},
            }
        }
        if profile.alias.trim().is_empty() {
            bail!("A server profile needs a model alias");
        }
        if self
            .store
            .list::<ModelAlias>("model-aliases")?
            .iter()
            .any(|alias| alias.name == profile.alias)
        {
            bail!("A model alias already uses this server's public identifier");
        }
        if !model.runs_on(profile.engine) {
            bail!(
                "{} cannot run this model: it is incomplete or not a model type this engine supports",
                profile.engine.label()
            );
        }
        if !model.path.is_dir() {
            bail!(
                "The model directory no longer exists: {}",
                model.path.display()
            );
        }
        {
            let workers = self.workers.lock().await;
            if let Some(existing) = workers.get(&profile.id) {
                if existing.record.status.pid.is_some() {
                    if existing.record.profile.model_id != profile.model_id
                        || existing.record.profile.options != profile.options
                        || existing.record.profile.env != profile.env
                        || existing.record.profile.alias != profile.alias
                    {
                        bail!("Stop this worker before applying a changed profile");
                    }
                    return Ok(Some(existing.status()));
                }
            }
            if workers.values().any(|worker| {
                worker.record.status.pid.is_some() && worker.record.profile.alias == profile.alias
            }) {
                bail!(
                    "Another running profile already uses alias {}",
                    profile.alias
                );
            }
        }
        let (mut profiles, mut models) = self.active_profiles_and_models().await?;
        profiles.push(profile.clone());
        models.retain(|m| m.id != model.id);
        models.push(model.clone());
        let estimate = self.estimate(&profiles, &models).await?;
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        if estimate["requires_override"].as_bool() == Some(true) && !allow_memory_overcommit {
            bail!(
                "Memory confirmation required. Configure explicit context/cache limits or retry with allow_memory_overcommit=true. {}",
                estimate
            );
        }
        if allow_memory_overcommit {
            events::emit(
                &self.events,
                "server.memory_override",
                &json!({"profile_id":profile.id,"estimate":estimate}),
            );
        }
        let args = server_options(&profile)?;
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        // Provisioning owns its staging/cleanup lifecycle. Cancelling a model start detaches this
        // task rather than abandoning an installation half committed.
        let runtime = self.runtime.clone();
        let engine = profile.engine;
        let mut installation = tokio::spawn(async move { runtime.launcher(engine).await });
        let launcher = tokio::select! {
            biased;
            _=wait_start_cancel(&mut cancellation)=>return Ok(None),
            result=&mut installation=>result.context("Managed runtime task failed")??,
        };
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        let model_path = model.path.canonicalize()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let logs = self.paths.data_dir.join("logs");
        tokio::fs::create_dir_all(&logs).await?;
        let log_path = logs.join(format!(
            "server-{:x}.log",
            Sha256::digest(profile.id.as_bytes())
        ));
        rotate_log_if_needed(&log_path).await?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut command = match engine {
            Engine::Locals => {
                let mut command = Command::new(&launcher.executable);
                command
                    .args([
                        "-u",
                        "-c",
                        crate::worker_logging::BOOTSTRAP,
                        "-m",
                        "locals.server",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        &port.to_string(),
                        "--model",
                    ])
                    .arg(&model_path)
                    .args(&args)
                    .env("PYTHONUNBUFFERED", "1")
                    .env("HF_HUB_OFFLINE", "1");
                command
            }
            Engine::Kapitsa => {
                let mut command = runtime::kapitsa::command(&launcher.executable);
                command
                    .args([
                        "serve",
                        "--private",
                        "--host",
                        "127.0.0.1",
                        "--port",
                        &port.to_string(),
                        "--model-id",
                        &profile.alias,
                    ])
                    .args(self.kapitsa_memory_budget(&profile).await)
                    .arg("--model")
                    .arg(&model_path)
                    .args(&args);
                command
            }
        };
        command
            .envs(&profile.env)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        command.as_std_mut().process_group(0);
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        // Workers survive a UI or daemon exit and are reconciled from their recorded process identity.
        let mut child = command
            .spawn()
            .with_context(|| format!("Could not launch the {} worker", engine.label()))?;
        let pid = child.id().context("The worker did not have a PID")?;
        let identity = match wait_for_identity(&mut child, pid, port, &model_path).await {
            Ok(identity) => identity,
            Err(error) => {
                let _ = child.kill().await;
                if start_cancelled(&cancellation) {
                    return Ok(None);
                }
                return Err(error.context(format!(
                    "Could not record the worker process identity; inspect {}",
                    log_path.display()
                )));
            }
        };
        if start_cancelled(&cancellation) {
            let _ = signal_owned(&identity, libc::SIGTERM);
            let _ = child.kill().await;
            return Ok(None);
        }
        let record = WorkerRecord {
            status: ServerStatus {
                profile_id: profile.id.clone(),
                alias: profile.alias.clone(),
                model_id: profile.model_id.clone(),
                engine,
                runtime_version: Some(launcher.version.clone()),
                state: "starting".into(),
                pid: Some(pid),
                port: Some(port),
                backend_model: None,
                backend_models: Vec::new(),
                backend_model_contexts: HashMap::new(),
                error: None,
                active_requests: 0,
                started_at: Some(chrono::Utc::now().to_rfc3339()),
            },
            profile,
            identity: Some(identity),
            log_path,
            model_path,
            runtime_root: Some(launcher.root.clone()),
        };
        let id = record.profile.id.clone();
        let mut workers = self.workers.lock().await;
        // Provisioning may have overlapped a profile rename and creation of a pool
        // using the old name. Claim the name and publish this worker atomically
        // with pool creation, without holding this lock during any process wait.
        let publication = (|| -> Result<()> {
            if self
                .store
                .list::<ModelAlias>("model-aliases")?
                .iter()
                .any(|alias| alias.name == record.profile.alias)
            {
                bail!(
                    "A model alias claimed this server's public identifier while it was starting"
                );
            }
            self.persist(&record)
        })();
        if let Err(error) = publication {
            drop(workers);
            let _ = child.kill().await;
            return Err(error);
        }
        events::emit(&self.events, "server.state", &record.status);
        workers.insert(
            id.clone(),
            Worker {
                record,
                child: Some(child),
                active: Arc::new(AtomicUsize::new(0)),
                accepting: false,
            },
        );
        drop(workers);
        tokio::select! {
            biased;
            _=wait_start_cancel(&mut cancellation)=>{
                self.stop(&id).await?;
                Ok(None)
            },
            result=self.wait_ready(&id)=>result.map(Some),
        }
    }

    async fn wait_ready(&self, id: &str) -> Result<ServerStatus> {
        let deadline = Instant::now() + Duration::from_secs(3600);
        loop {
            let record = self
                .workers
                .lock()
                .await
                .get(id)
                .context("Unknown server profile")?
                .record
                .clone();
            if matches!(record.status.state.as_str(), "draining" | "stopping") {
                // stop() runs independently, but the next load waits until this cancelled load has exited.
                if record.status.pid.is_some()
                    && record.identity.as_ref().is_some_and(identity_matches)
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                return Ok(record.status);
            }
            if record.status.state != "starting" {
                return Ok(record.status);
            }
            if !record.identity.as_ref().is_some_and(identity_matches) {
                return self
                    .failed(
                        id,
                        "The worker exited while loading. Inspect the server log.",
                    )
                    .await;
            }
            if let Ok((backend, models, contexts)) = self.probe(&record).await {
                let mut workers = self.workers.lock().await;
                let worker = workers.get_mut(id).context("Unknown server profile")?;
                if worker.record.status.state != "starting"
                    || worker.record.status.pid != record.status.pid
                    || worker.record.status.port != record.status.port
                    || worker.record.status.started_at != record.status.started_at
                {
                    return Ok(worker.status());
                }
                worker.record.status.backend_model = Some(backend);
                worker.record.status.backend_models = models;
                worker.record.status.backend_model_contexts = contexts;
                worker.record.status.state = "ready".into();
                worker.record.status.error = None;
                worker.accepting = true;
                self.persist(&worker.record)?;
                events::emit(&self.events, "server.state", &worker.record.status);
                return Ok(worker.status());
            }
            if Instant::now() >= deadline {
                return self.failed(id,"Loading exceeded one hour. The owned worker remains available to stop; inspect its log.").await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn probe(
        &self,
        record: &WorkerRecord,
    ) -> Result<(String, Vec<String>, HashMap<String, WorkerModelContext>)> {
        let port = record.status.port.context("No worker port")?;
        let base = format!("http://127.0.0.1:{port}");
        let health: Value = self
            .client
            .get(format!("{base}/health"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if health["status"] != "ok" {
            bail!("Worker health check failed");
        }
        match record.status.engine {
            Engine::Locals => {
                let reported = health["version"].as_str().and_then(runtime::Version::parse);
                let expected = record
                    .status
                    .runtime_version
                    .as_deref()
                    .and_then(runtime::Version::parse);
                let matches = match expected {
                    Some(expected) => reported == Some(expected),
                    None => {
                        reported.is_some_and(|version| version >= runtime::locals::MINIMUM_VERSION)
                    }
                };
                if !matches {
                    bail!(
                        "Worker health/version did not match the Locals version it was started with"
                    );
                }
                let stats: Value = self
                    .client
                    .get(format!("{base}/stats"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if stats["model"].as_str() != record.model_path.to_str() {
                    bail!("The port is serving another model");
                }
            }
            // Kapitsa listens only once its model is loaded; the model list below names it.
            Engine::Kapitsa => {}
        }
        let models: Value = self
            .client
            .get(format!("{base}/v1/models"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut ids = Vec::new();
        let mut contexts = HashMap::new();
        for model in models["data"]
            .as_array()
            .context("Worker model list is invalid")?
        {
            let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
                continue;
            };
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_owned());
                let context = worker_model_context(model);
                if context.context_length.is_some() || context.max_model_len.is_some() {
                    contexts.insert(id.to_owned(), context);
                }
            }
        }
        if record.status.engine == Engine::Kapitsa {
            // One model per process, named after the profile with --model-id.
            let backend = ids
                .iter()
                .find(|id| **id == record.profile.alias)
                .context("The port is serving another model")?
                .clone();
            return Ok((backend, ids, contexts));
        }
        // A real model name may itself end in -thinking. Only an ID whose base is
        // also announced is a reasoning variant, rather than a canonical model.
        let is_variant = |id: &&String| {
            id.strip_suffix("-thinking")
                .is_some_and(|base| ids.iter().any(|other| other == base))
        };
        let backend = ids
            .iter()
            .filter(|id| !is_variant(id))
            .find(|id| id.starts_with("locals/"))
            .or_else(|| ids.iter().find(|id| !is_variant(id)))
            .context("Worker has no canonical model")?
            .clone();
        Ok((backend, ids, contexts))
    }

    async fn failed(&self, id: &str, message: &str) -> Result<ServerStatus> {
        let mut workers = self.workers.lock().await;
        let worker = workers.get_mut(id).context("Unknown server profile")?;
        if matches!(
            worker.record.status.state.as_str(),
            "draining" | "stopping" | "stopped"
        ) {
            return Ok(worker.status());
        }
        worker.accepting = false;
        worker.record.status.backend_model_contexts.clear();
        worker.record.status.state = "error".into();
        worker.record.status.error = Some(message.into());
        if !worker
            .record
            .identity
            .as_ref()
            .is_some_and(identity_matches)
        {
            worker.record.status.pid = None;
            worker.record.status.port = None;
            worker.record.identity = None;
            if let Some(child) = worker.child.as_mut() {
                let _ = child.try_wait();
            }
        }
        self.persist(&worker.record)?;
        events::emit(&self.events, "server.state", &worker.record.status);
        bail!("{message}")
    }

    pub async fn stop(&self, profile_id: &str) -> Result<ServerStatus> {
        let initial = self
            .workers
            .lock()
            .await
            .get(profile_id)
            .context("Unknown server profile")?
            .status();
        let lock = self
            .stop_locks
            .lock()
            .await
            .entry(profile_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _stop_guard = lock.lock().await;
        // Do not wait for the model-loading lock: users must be able to cancel a load.
        let (identity, active, port, engine) = {
            let mut workers = self.workers.lock().await;
            let worker = workers
                .get_mut(profile_id)
                .context("Unknown server profile")?;
            if worker.record.status.pid != initial.pid
                || worker.record.status.started_at != initial.started_at
            {
                return Ok(worker.status());
            }
            if worker.record.status.pid.is_none() {
                return Ok(worker.status());
            }
            worker.accepting = false;
            worker.record.status.state = "draining".into();
            self.persist(&worker.record)?;
            events::emit(&self.events, "server.state", &worker.record.status);
            (
                worker.record.identity.clone(),
                worker.active.clone(),
                worker.record.status.port,
                worker.record.status.engine,
            )
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let backend_active = if let Some(port) = port {
                stats_request(&self.client, engine, port)
                    .send()
                    .await
                    .ok()
                    .filter(|r| r.status().is_success())
                    .map(|r| async move { r.json::<Value>().await.ok() })
            } else {
                None
            };
            let backend_active = match backend_active {
                Some(future) => future
                    .await
                    .and_then(|s| worker_requests(engine, &s).map(Vec::len))
                    .unwrap_or(0),
                None => 0,
            };
            if active.load(Ordering::Acquire) == 0 && backend_active == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut signalled_owned = false;
        if let Some(identity) = identity.as_ref() {
            if identity_matches(identity) {
                // Kapitsa stops cleanly on an interrupt, as after Ctrl+C in a terminal.
                let signal = match engine {
                    Engine::Locals => libc::SIGTERM,
                    Engine::Kapitsa => libc::SIGINT,
                };
                signal_owned(identity, signal)?;
                signalled_owned = true;
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while identity_matches(identity) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if identity_matches(identity) {
                signal_owned(identity, libc::SIGKILL)?;
            }
        }
        let mut workers = self.workers.lock().await;
        let worker = workers
            .get_mut(profile_id)
            .context("Unknown server profile")?;
        if let Some(child) = worker.child.as_mut() {
            if child.try_wait()?.is_none() {
                // A terminating process can lose its command snapshot before waitpid reports its
                // exit. An already verified signal permits bounded reaping of our Child handle.
                if !signalled_owned && !identity.as_ref().is_some_and(identity_matches) {
                    worker.record.status.state = "error".into();
                    worker.record.status.error=Some("The worker is alive but its ownership could not be verified; no unrelated process was signalled.".into());
                    self.persist(&worker.record)?;
                    bail!("Worker ownership could not be verified; stopping was cancelled");
                }
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(result) => {
                        result?;
                    }
                    Err(_) => {
                        worker.record.status.state = "error".into();
                        worker.record.status.error = Some(
                            "The worker did not exit after termination; retry stopping it.".into(),
                        );
                        self.persist(&worker.record)?;
                        bail!("The worker did not exit after termination");
                    }
                }
            }
        }
        worker.child.take();
        worker.record.status.state = "stopped".into();
        worker.record.status.pid = None;
        worker.record.status.port = None;
        worker.record.status.backend_model = None;
        worker.record.status.backend_models.clear();
        worker.record.status.backend_model_contexts.clear();
        worker.record.status.active_requests = 0;
        worker.record.status.error = None;
        worker.record.identity = None;
        self.persist(&worker.record)?;
        events::emit(&self.events, "server.state", &worker.record.status);
        Ok(worker.status())
    }

    pub(crate) async fn has_ready_alias(&self, alias: &str) -> bool {
        self.workers.lock().await.values().any(|worker| {
            worker.record.profile.alias == alias
                && worker.accepting
                && matches!(worker.record.status.state.as_str(), "ready" | "running")
        })
    }

    pub async fn acquire(&self, alias: &str) -> Result<WorkerLease> {
        let workers = self.workers.lock().await;
        let worker = workers.values().find(|worker| {
            worker.record.profile.alias == alias
                && worker.accepting
                && worker.record.status.state == "ready"
        });
        let Some(worker) = worker else {
            if workers
                .values()
                .any(|worker| worker.record.profile.alias == alias)
            {
                bail!("The requested model is not ready");
            }
            bail!("Unknown model alias");
        };
        let port = worker.record.status.port.context("Worker has no port")?;
        let backend = worker
            .record
            .status
            .backend_model
            .clone()
            .context("Worker has no canonical model")?;
        worker.active.fetch_add(1, Ordering::AcqRel);
        Ok(WorkerLease {
            profile_id: worker.record.profile.id.clone(),
            base_url: format!("http://127.0.0.1:{port}"),
            backend_model: backend,
            activity_status: worker.status(),
            active: worker.active.clone(),
        })
    }

    pub(crate) async fn with_unused_alias_name<T>(
        &self,
        name: &str,
        save: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let workers = self.workers.lock().await;
        if workers
            .values()
            .any(|worker| worker.record.profile.alias == name && worker.record.status.pid.is_some())
        {
            bail!(
                "A running or starting server already uses this public model identifier; stop it before creating the alias"
            );
        }
        save()
    }

    pub(crate) async fn acquire_pool(
        &self,
        members: &[ServerProfile],
        preferred: Option<(&str, &str)>,
        engine: Option<&str>,
        thinking: bool,
        cursor: usize,
    ) -> Result<(WorkerLease, bool)> {
        let workers = self.workers.lock().await;
        let eligible: Vec<_> = members
            .iter()
            .filter_map(|profile| {
                workers.get(&profile.id).filter(|worker| {
                    worker.accepting
                        && worker.record.status.state == "ready"
                        && worker.record.profile.model_id == profile.model_id
                        && worker.record.status.model_id == profile.model_id
                        && worker.record.status.port.is_some()
                        && advertised_backend(&worker.record.status, engine, thinking).is_some()
                })
            })
            .collect();
        let pinned = preferred.and_then(|(profile_id, model_id)| {
            eligible.iter().copied().find(|worker| {
                worker.record.profile.id == profile_id && worker.record.status.model_id == model_id
            })
        });
        let reused = pinned.is_some();
        let worker = pinned
            .or_else(|| eligible.get(cursor % eligible.len().max(1)).copied())
            .context("No server in this model alias is ready for the requested model mode")?;
        let port = worker.record.status.port.context("Worker has no port")?;
        let backend = worker
            .record
            .status
            .backend_model
            .clone()
            .context("Worker has no canonical model")?;
        worker.active.fetch_add(1, Ordering::AcqRel);
        Ok((
            WorkerLease {
                profile_id: worker.record.profile.id.clone(),
                base_url: format!("http://127.0.0.1:{port}"),
                backend_model: backend,
                activity_status: worker.status(),
                active: worker.active.clone(),
            },
            reused,
        ))
    }

    /// Guard destructive file removal against live workers, including stale model IDs.
    pub(crate) async fn with_model_files_unused<T>(
        &self,
        model_id: &str,
        path: &std::path::Path,
        remove: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let _lifecycle = self
            .lifecycle
            .try_lock()
            .map_err(|_| anyhow!("A model is loading; wait before deleting model files"))?;
        let workers = self.workers.lock().await;
        let records = self.store.list::<WorkerRecord>("workers")?;
        for record in records
            .iter()
            .chain(workers.values().map(|worker| &worker.record))
        {
            let live = record.status.pid.is_some()
                || matches!(
                    record.status.state.as_str(),
                    "starting" | "loading" | "ready" | "running" | "draining" | "stopping"
                )
                || record.identity.as_ref().is_some_and(identity_matches)
                || workers.get(&record.profile.id).is_some_and(|worker| {
                    worker.accepting || worker.active.load(Ordering::Acquire) > 0
                });
            if live
                && (record.status.model_id == model_id
                    || crate::models::paths_overlap(path, &record.model_path))
            {
                bail!(
                    "Stop every server using this model directory before deleting its local files"
                );
            }
        }
        // Keep the worker/lifecycle locks through the synchronous purge, so a new
        // lease or an independently requested supervisor start cannot race it.
        remove()
    }

    /// Forget only an inactive profile's supervisor record; model weights and log files remain.
    pub async fn forget_profile(&self, id: &str) -> Result<()> {
        // A start can be provisioning its interpreter before it has a worker/PID to record.
        let _lifecycle = self.lifecycle.try_lock().map_err(|_| {
            anyhow!("A model is loading; wait for it before removing a server profile")
        })?;
        let mut workers = self.workers.lock().await;
        let saved = self.store.get::<WorkerRecord>("workers", id)?;
        for record in workers
            .get(id)
            .map(|worker| &worker.record)
            .into_iter()
            .chain(saved.as_ref())
        {
            if record.status.pid.is_some()
                || matches!(
                    record.status.state.as_str(),
                    "starting" | "draining" | "stopping"
                )
                || record.identity.as_ref().is_some_and(identity_matches)
            {
                bail!("Stop the server before removing its profile");
            }
        }
        if workers
            .get(id)
            .is_some_and(|worker| worker.accepting || worker.active.load(Ordering::Acquire) > 0)
        {
            bail!("Stop the server before removing its profile");
        }
        self.store.delete("workers", id)?;
        workers.remove(id);
        Ok(())
    }

    /// The engine versions running workers use, or None when a worker started before versions were
    /// recorded is alive: then nothing proves an older version unused.
    pub async fn runtimes_in_use(&self) -> Option<HashSet<PathBuf>> {
        let workers = self.workers.lock().await;
        let mut roots = HashSet::new();
        for worker in workers.values() {
            let live = worker.record.status.pid.is_some()
                || worker
                    .record
                    .identity
                    .as_ref()
                    .is_some_and(identity_matches);
            if !live {
                continue;
            }
            roots.insert(worker.record.runtime_root.clone()?);
        }
        Some(roots)
    }

    /// Kapitsa sizes itself to the GPU memory macOS recommends, which would not leave room for
    /// workers already running. Beside them it gets what is free now, less a reserve for macOS;
    /// an explicit `mem_budget` option wins.
    async fn kapitsa_memory_budget(&self, profile: &ServerProfile) -> Vec<String> {
        if effective_server_options(profile)
            .get("mem_budget")
            .is_some_and(|value| !value.is_null())
        {
            return Vec::new();
        }
        let others = self.workers.lock().await.values().any(|worker| {
            worker.record.profile.id != profile.id && worker.record.status.pid.is_some()
        });
        if !others {
            return Vec::new();
        }
        let available = self.collector.available_memory();
        let budget = available.saturating_sub(2 << 30) as f64 / 1e9;
        vec!["--mem-budget".into(), format!("{:.1}", budget.max(1.0))]
    }

    pub async fn list(&self) -> Vec<ServerStatus> {
        let mut statuses: Vec<_> = self
            .workers
            .lock()
            .await
            .values()
            .map(Worker::status)
            .collect();
        statuses.sort_by(|a, b| a.alias.cmp(&b.alias));
        statuses
    }

    pub async fn logs(&self, profile_id: &str) -> Result<Value> {
        let path = self
            .workers
            .lock()
            .await
            .get(profile_id)
            .map(|worker| worker.record.log_path.clone());
        let Some(path) = path else {
            if self
                .store
                .get::<ServerProfile>("profiles", profile_id)?
                .is_some()
            {
                return Ok(
                    json!({"profile_id":profile_id,"text":"","entries":[],"path":null,"truncated":false}),
                );
            }
            bail!("Unknown server profile");
        };
        let mut file = tokio::fs::File::open(&path)
            .await
            .context("The server log is not available yet")?;
        let length = file.metadata().await?.len();
        let offset = length.saturating_sub(64 * 1024);
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut data = Vec::new();
        file.take(64 * 1024).read_to_end(&mut data).await?;
        let text = String::from_utf8_lossy(&data);
        let entries = crate::worker_logging::entries(&text, offset > 0);
        Ok(
            json!({"profile_id":profile_id,"path":path,"text":text,"entries":entries,"truncated":offset>0}),
        )
    }

    pub async fn telemetry(&self) -> Result<Value> {
        let statuses = self.list().await;
        let mut tasks = tokio::task::JoinSet::new();
        for status in &statuses {
            if let Some(port) = status.port {
                let client = self.client.clone();
                let id = status.profile_id.clone();
                let engine = status.engine;
                tasks.spawn(async move {
                    let result = stats_request(&client, engine, port).send().await;
                    let value = match result {
                        Ok(response) if response.status().is_success() => {
                            response.json::<Value>().await.ok()
                        }
                        _ => None,
                    };
                    let value = match engine {
                        Engine::Locals => value,
                        Engine::Kapitsa => value.map(|status| telemetry::kapitsa_stats(&status)),
                    };
                    (id, value)
                });
            }
        }
        let mut stats = HashMap::new();
        while let Some(result) = tasks.join_next().await {
            if let Ok((id, Some(value))) = result {
                stats.insert(id, value);
            }
        }
        let current = self.list().await;
        stats.retain(|id, _| {
            statuses
                .iter()
                .find(|status| &status.profile_id == id)
                .is_some_and(|previous| {
                    current
                        .iter()
                        .any(|status| crate::activity::same_worker(previous, status))
                })
        });
        Ok(self.collector.snapshot(&current, &stats))
    }

    pub(crate) fn begin_activity(&self, lease: &WorkerLease) -> crate::activity::ActivityGuard {
        self.collector.begin_activity(lease.activity_status.clone())
    }

    pub async fn estimate(
        &self,
        profiles: &[ServerProfile],
        models: &[ModelRecord],
    ) -> Result<Value> {
        let configured = self
            .store
            .get::<StudioConfig>("settings", "config")?
            .and_then(|c| c.memory_budget_bytes);
        let recommended = self.runtime.recommended_memory().await;
        let physical = self.collector.default_budget();
        let budget = configured
            .unwrap_or_else(|| recommended.map(|gpu| gpu.min(physical)).unwrap_or(physical));
        let mut estimate = telemetry::estimate_memory(profiles, models, budget).await?;
        estimate["budget_source"] = json!(if configured.is_some() {
            "user"
        } else if recommended.is_some() {
            "metal_recommended_with_system_reserve"
        } else {
            "physical_memory_with_system_reserve"
        });
        estimate["recommended_gpu_bytes"] = json!(recommended);
        let active_profiles: HashSet<String> = self
            .workers
            .lock()
            .await
            .values()
            .filter(|worker| {
                worker.record.status.pid.is_some()
                    && worker
                        .record
                        .identity
                        .as_ref()
                        .is_some_and(identity_matches)
            })
            .map(|worker| worker.record.profile.id.clone())
            .collect();
        telemetry::account_available_memory(
            &mut estimate,
            &active_profiles,
            self.collector.available_memory(),
        );
        estimate["available_memory_sampled_at"] = json!(chrono::Utc::now().to_rfc3339());
        Ok(estimate)
    }

    async fn active_profiles_and_models(&self) -> Result<(Vec<ServerProfile>, Vec<ModelRecord>)> {
        let profiles = self
            .workers
            .lock()
            .await
            .values()
            .filter(|w| w.record.status.pid.is_some())
            .map(|w| w.record.profile.clone())
            .collect();
        Ok((profiles, self.store.list::<ModelRecord>("models")?))
    }

    fn persist(&self, record: &WorkerRecord) -> Result<()> {
        self.store.set("workers", &record.profile.id, record)
    }

    async fn reconcile_exits(&self) {
        let mut workers = self.workers.lock().await;
        let mut log_paths = Vec::new();
        for worker in workers.values_mut() {
            if worker.record.status.pid.is_some() {
                log_paths.push(worker.record.log_path.clone());
            }
            if let Some(child) = worker.child.as_mut() {
                let _ = child.try_wait();
            }
            if worker.record.status.pid.is_some()
                && !worker
                    .record
                    .identity
                    .as_ref()
                    .is_some_and(identity_matches)
            {
                let was_stopping =
                    matches!(worker.record.status.state.as_str(), "draining" | "stopping");
                worker.accepting = false;
                worker.record.status.state = if was_stopping { "stopped" } else { "error" }.into();
                worker.record.status.error = if was_stopping {
                    None
                } else {
                    Some("The worker exited unexpectedly. Inspect its log.".into())
                };
                worker.record.status.pid = None;
                worker.record.status.port = None;
                worker.record.status.backend_model = None;
                worker.record.status.backend_models.clear();
                worker.record.status.backend_model_contexts.clear();
                worker.record.identity = None;
                let _ = self.persist(&worker.record);
                events::emit(&self.events, "server.state", &worker.record.status);
            }
        }
        drop(workers);
        for path in log_paths {
            if let Err(error) = rotate_log_if_needed(&path).await {
                events::emit(
                    &self.events,
                    "server.log_error",
                    &json!({"path":path,"error":error.to_string()}),
                );
            }
        }
    }
}

impl Worker {
    fn status(&self) -> ServerStatus {
        let mut status = self.record.status.clone();
        status.active_requests = self.active.load(Ordering::Acquire);
        status
    }
}

fn start_cancelled(cancellation: &Option<watch::Receiver<bool>>) -> bool {
    cancellation
        .as_ref()
        .is_some_and(|receiver| *receiver.borrow())
}

async fn wait_start_cancel(cancellation: &mut Option<watch::Receiver<bool>>) {
    let Some(receiver) = cancellation else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

fn read_identity(pid: u32) -> Option<ProcessIdentity> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .without_tasks(),
    );
    let process = system.process(Pid::from_u32(pid))?;
    if process.status() == sysinfo::ProcessStatus::Zombie {
        return None;
    }
    Some(ProcessIdentity {
        pid,
        start_time: process.start_time(),
        command: process
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
    })
}

async fn wait_for_identity(
    child: &mut Child,
    pid: u32,
    port: u16,
    model: &std::path::Path,
) -> Result<ProcessIdentity> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let port = port.to_string();
    let model = model.to_string_lossy();
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("Worker exited before its identity was available ({status})");
        }
        if let Some(identity) = read_identity(pid) {
            let module = is_worker_command(&identity.command);
            let correct_port = identity
                .command
                .windows(2)
                .any(|p| p[0] == "--port" && p[1] == port);
            let correct_model = identity
                .command
                .windows(2)
                .any(|p| p[0] == "--model" && p[1].as_str() == model.as_ref());
            if module && correct_port && correct_model && identity_matches(&identity) {
                return Ok(identity);
            }
        }
        if Instant::now() >= deadline {
            bail!("Timed out waiting for the spawned worker's command and birth time");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn identity_matches(identity: &ProcessIdentity) -> bool {
    read_identity(identity.pid).is_some_and(|actual| {
        actual.start_time == identity.start_time
            // macOS's Python launcher may exec Python.app and replace argv[0]. The module and all
            // arguments (including our unique private port and model path) must remain identical.
            && actual.command.get(1..)==identity.command.get(1..)
            && is_worker_command(&actual.command)
    })
}

/// A worker's report of its requests and memory. Kapitsa's needs the key of the run that holds
/// its control socket; any other Kapitsa process refuses it, and then has no report.
fn stats_request(client: &reqwest::Client, engine: Engine, port: u16) -> reqwest::RequestBuilder {
    match engine {
        Engine::Locals => client.get(format!("http://127.0.0.1:{port}/stats")),
        Engine::Kapitsa => {
            let request = client.get(format!("http://127.0.0.1:{port}/engine/status"));
            match runtime::kapitsa::engine_key() {
                Some(key) => request.bearer_auth(key),
                None => request,
            }
        }
    }
}

fn worker_requests(engine: Engine, stats: &Value) -> Option<&Vec<Value>> {
    match engine {
        Engine::Locals => stats["active"].as_array(),
        Engine::Kapitsa => stats["requests"].as_array(),
    }
}

/// A Locals worker runs `-m locals.server`; a Kapitsa worker is `kapitsa serve`.
fn is_worker_command(command: &[String]) -> bool {
    command
        .windows(2)
        .any(|pair| pair[0] == "-m" && pair[1] == "locals.server")
        || (command.get(1).is_some_and(|argument| argument == "serve")
            && command.first().is_some_and(|program| {
                std::path::Path::new(program)
                    .file_name()
                    .is_some_and(|name| name == "kapitsa")
            }))
}

fn signal_owned(identity: &ProcessIdentity, signal: i32) -> Result<()> {
    if !identity_matches(identity) {
        bail!("Worker ownership could not be verified; no signal was sent");
    }
    // A group created for this worker includes its own children, never the daemon or its siblings.
    let pid = identity.pid as libc::pid_t;
    let group = unsafe { libc::getpgid(pid) };
    let target = if group == pid { -pid } else { pid };
    if unsafe { libc::kill(target, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

/// Public form metadata includes Studio launch defaults and the pinned runtime defaults.
pub(crate) fn advertised_backend(
    status: &ServerStatus,
    engine: Option<&str>,
    thinking: bool,
) -> Option<String> {
    let canonical = status.backend_model.as_deref()?;
    let backend = crate::gateway::rewrite_model(canonical, engine, thinking);
    if (engine.is_none() && !thinking) || status.backend_models.contains(&backend) {
        Some(backend)
    } else {
        None
    }
}

/// The free-text field every engine has, for options this Studio does not describe yet.
const EXTRA_ARGS: &str = "extra_args";

pub fn options_schema(engine: Engine) -> Value {
    let mut schema = match engine {
        Engine::Locals => locals_options_schema(),
        Engine::Kapitsa => kapitsa_options_schema(),
    };
    schema["engine"] = json!(engine);
    schema["fields"]
        .as_array_mut()
        .expect("static options schema")
        .push(json!({
            "key":EXTRA_ARGS,"label":"Additional options","type":"text","default":null,
            "description":format!("Further {} command-line options, as typed in a terminal (for example --flag value). Use it for options this Studio does not list yet. Options Studio manages are refused.", engine.label()),
        }));
    schema
}

fn locals_options_schema() -> Value {
    let defaults = studio_server_defaults();
    json!({
        "version":crate::runtime::locals::MINIMUM_VERSION.to_string(),"restart_required":true,"null_behavior":"omit",
        "studio_defaults":defaults,
        "missing_options_behavior":"Absent and null options omit the argument and use the Locals runtime default or Auto. Model-specific recommendations are explicit configuration presets.",
        "fields":[
            {"key":"engines","label":"Engines","type":"string","default":"locals","description":"Allowed engines, separated by commas: locals, mlx. Two engines load two copies of the same model."},
            {"key":"preload","label":"Preload","type":"string","default":"locals","description":"Engines loaded at startup, separated by commas. none defers loading until the first request."},
            {"key":"slots","label":"Concurrent requests","type":"integer","default":16,"min":1,"description":"Maximum batch size for each engine; affects memory requirements."},
            {"key":"temp","label":"Temperature","type":"number","default":0.2,"runtime_default":0.2,"default_source":"runtime","min":0,"description":"Locals defaults to 0.2 when unset. Zero enables deterministic generation. Apply a model-specific preset only when appropriate for that model."},
            {"key":"top_p","label":"Top-p","type":"number","default":0.0,"runtime_default":0.0,"default_source":"runtime","min":0,"max":1,"description":"Locals defaults to zero (disabled). Nucleus sampling when temperature is positive; zero or one disables this filter."},
            {"key":"top_k","label":"Top-k","type":"integer","default":0,"runtime_default":0,"default_source":"runtime","min":0,"description":"Locals defaults to zero (disabled). Number of candidates retained when temperature is positive."},
            {"key":"max_tokens","label":"Response tokens","type":"integer","default":4096,"runtime_default":4096,"default_source":"runtime","min":0,"description":"Locals defaults to 4,096 completion tokens per request. Explicit profile values, requests and remaining context can change the effective limit."},
            {"key":"cache_gb","label":"Prompt cache (GiB)","type":"number","default":null,"min":0,"exclusive_min":true,"auto":true,"description":"Auto uses Locals' automatic sizing. An explicit value must be strictly positive; zero does not disable the cache and must be replaced with Auto."},
            {"key":"max_context","label":"Maximum context (tokens)","type":"integer","default":null,"runtime_default":null,"default_source":"runtime","min":1,"auto":true,"description":"Auto lets Locals size context for the model and available memory. Set an explicit limit or apply an appropriate model preset when needed."},
            {"key":"thinking","label":"Default thinking","type":"boolean","default":false,"description":"Default reasoning for compatible models after the next server start. Override it for the next reply in the chat’s Inference settings. Thinking model variants remain available regardless of this default."},
            {"key":"seed","label":"Random seed","type":"integer","default":0,"description":"Default generation seed; requests can provide their own seed."},
            {"key":"max_calls","label":"Tool calls per response","type":"integer","default":6,"min":0,"description":"Stops the response after this many completed tool calls."},
            {"key":"walk","label":"Walk constraints","type":"boolean","default":false,"description":"Uses the Walk constraint engine, without token tables, for large grammars."},
            {"key":"typed_tools","label":"Tool parameter types","type":"boolean","default":false,"description":"Constrains tool parameter names and types. Use with Walk for large tool sets."}
        ],
        "managed_fields":[
            {"key":"host","label":"Listen address","default":"127.0.0.1","description":"Configured in the shared Studio API. Workers stay on loopback."},
            {"key":"port","label":"Port","default":null,"description":"The public port is configured in the Studio API; worker ports are assigned automatically."},
            {"key":"model","label":"Model","default":null,"description":"Selected from the library for this profile; its local path is passed to the worker."},
            {"key":"ollama_port","label":"Ollama port","default":0,"description":"Managed by the shared Studio API; workers do not open a separate Ollama port."}
        ]
    })
}

fn kapitsa_options_schema() -> Value {
    json!({
        "version":crate::runtime::kapitsa::MINIMUM_VERSION.to_string(),"restart_required":true,"null_behavior":"omit",
        "studio_defaults":{},
        "missing_options_behavior":"Absent and null options omit the argument and use Kapitsa's default for this model and Mac.",
        "fields":[
            {"key":"context","numeric":true,"label":"Context per conversation","type":"string","default":null,"auto":true,"description":"Tokens per conversation: a number (131072), k or m of them (128k, 1m), or max for the model's limit. Auto: the model's limit, or less where memory is tight."},
            {"key":"slots","label":"Concurrent requests","type":"integer","default":null,"min":1,"auto":true,"description":"Requests that run at once, each keeping its conversation ready for follow-ups. Auto: up to 4, as memory allows."},
            {"key":"profile","label":"Profile","type":"string","default":"precise","choices":["precise","fast"],"description":"precise runs the exact outputs; fast also turns on speedups measured to move outputs no more than the fastest other engine."},
            {"key":"mem_budget","label":"GPU memory budget (GB)","type":"number","default":null,"min":0.1,"auto":true,"description":"GPU memory Kapitsa may hold. Auto: what macOS recommends, or what is free beside the servers already running."},
            {"key":"dflash","label":"Draft model","type":"string","default":"auto","description":"auto uses the drafter downloaded with the model; none decodes with the model's MTP head; or a drafter folder."},
            {"key":"spec","label":"MTP draft depth","type":"integer","default":null,"min":0,"auto":true,"description":"Draft depth without a drafter; 0 turns speculation off. Auto: per model."},
            {"key":"disk_cache","numeric":true,"label":"SSD prefix cache","type":"string","default":"auto","description":"auto: a quarter of free space beyond 10 GiB, at most 32 GiB; a size such as 16G caps it; 0 turns it off."},
            {"key":"prefill_chunk","label":"Prefill chunk (tokens)","type":"integer","default":null,"min":1,"auto":true,"description":"Prompt tokens per prefill pass. Auto: 2048, or 1024 when memory is short."},
            {"key":"idle_unload","numeric":true,"label":"Unload when idle","type":"string","default":"auto","description":"Minutes without a request before the model leaves the GPU, auto (after 10 minutes under memory pressure) or 0 (never)."},
            {"key":"image_max_pixels","label":"Image size limit (pixels)","type":"integer","default":null,"min":0,"auto":true,"description":"Larger images are resized to about this many pixels; 0 uses the model's own limit."},
            {"key":"timing","label":"First-token timing","type":"string","default":"both","choices":["ttft","prefill","both"],"description":"What the request log shows of the wait before the first token."},
            {"key":"suppress_tokens","label":"Suppressed tokens file","type":"string","default":null,"description":"A file of output token IDs never to generate (JSON array or one ID per line)."},
            {"key":"loops","label":"Dory loops","type":"integer","default":null,"min":1,"max":4,"auto":true,"description":"Dory only: times its shared block runs per token (1-4)."},
            {"key":"loop_kv","label":"Dory loop cache","type":"string","default":"last-loop","choices":["last-loop","per-loop"],"description":"Dory only: one KV cache per layer, or one per loop (more memory)."},
            {"key":"split","label":"Split across Macs","type":"string","default":null,"description":"Qwen3.8-Flash-Next only: HOST:PORT of each kapitsa stage holding later layers, comma separated."}
        ],
        "managed_fields":[
            {"key":"host","label":"Listen address","default":"127.0.0.1","description":"Workers stay on this Mac; the shared Studio API is the public address."},
            {"key":"port","label":"Port","default":null,"description":"Worker ports are assigned automatically."},
            {"key":"private","label":"Private","default":true,"description":"Workers never join or serve other Macs on the network."},
            {"key":"model","label":"Model","default":null,"description":"Selected from the library for this profile."},
            {"key":"model_id","label":"API model name","default":null,"description":"The profile's alias."},
            {"key":"api_key","label":"API key","default":null,"description":"Configure access on the shared Studio API."},
            {"key":"allow_switch","label":"Model switching","default":false,"description":"One model per profile; start another profile instead."}
        ]
    })
}

/// Options Studio sets itself, refused in profiles and in the additional options field.
fn managed_option(engine: Engine, key: &str) -> bool {
    let key = key.trim_start_matches("--").replace('_', "-");
    let key = key.split('=').next().unwrap_or_default();
    match engine {
        Engine::Locals => matches!(key, "host" | "port" | "model" | "ollama-port"),
        Engine::Kapitsa => matches!(
            key,
            "host"
                | "port"
                | "model"
                | "model-id"
                | "private"
                | "api-key"
                | "allow-switch"
                | "config"
                | "yes"
                | "verbose"
                | "ctx"
        ),
    }
}

pub(crate) fn validate_server_options(
    engine: Engine,
    options: &serde_json::Map<String, Value>,
) -> Result<()> {
    let schema = options_schema(engine);
    let fields = schema["fields"].as_array().expect("static options schema");
    // Validate keys before launch serialization skips null values. A preset must
    // never smuggle a managed/unknown option merely by setting it to null.
    for (key, value) in options {
        let normalized = key.trim_start_matches("--").replace('-', "_");
        let field = fields
            .iter()
            .find(|field| field["key"] == normalized)
            .with_context(|| {
                format!(
                    "Unsupported or Studio-managed {} server option: {key}",
                    engine.label()
                )
            })?;
        if value.is_null() {
            continue;
        }
        if field["type"] == "text" && !value.is_string() {
            bail!("{key} must be text");
        }
        if field["type"] == "boolean" && !value.is_boolean() {
            bail!("{key} must be a boolean");
        }
        if field["type"] == "string" {
            let text_or_list = value.is_string()
                || (value.is_number() && field["numeric"] == true)
                || value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string));
            if !text_or_list {
                bail!("{key} must be text, a number, or null");
            }
            if let (Some(choices), Some(text)) = (field["choices"].as_array(), value.as_str())
                && !choices.iter().any(|choice| choice == text)
            {
                bail!(
                    "{key} must be one of {}",
                    choices
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if matches!(field["type"].as_str(), Some("integer" | "number")) {
            if field["type"] == "integer"
                && !(value.is_i64()
                    || value.is_u64()
                    || value
                        .as_str()
                        .is_some_and(|text| text.trim().parse::<i128>().is_ok()))
            {
                bail!("{key} must be an integer or null/Auto");
            }
            let number = value
                .as_f64()
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|text| text.trim().parse::<f64>().ok())
                })
                .filter(|number| number.is_finite())
                .with_context(|| format!("{key} must be a finite number or null/Auto"))?;
            if field["min"].as_f64().is_some_and(|minimum| {
                number < minimum || (field["exclusive_min"] == true && number == minimum)
            }) || field["max"]
                .as_f64()
                .is_some_and(|maximum| number > maximum)
            {
                bail!(
                    "{key} is outside the allowed range in the {} options schema",
                    engine.label()
                );
            }
        }
    }
    server_options(&ServerProfile {
        engine,
        options: options.clone(),
        ..Default::default()
    })?;
    Ok(())
}

fn server_options(profile: &ServerProfile) -> Result<Vec<String>> {
    let options = effective_server_options(profile);
    let mut args = match profile.engine {
        Engine::Locals => locals_arguments(&options)?,
        Engine::Kapitsa => kapitsa_arguments(&options)?,
    };
    if let Some(extra) = options.get(EXTRA_ARGS).and_then(Value::as_str) {
        let words = split_arguments(extra)?;
        if let Some(managed) = words
            .iter()
            .find(|word| word.starts_with("--") && managed_option(profile.engine, word))
        {
            bail!("{managed} is managed by Studio and cannot be set in additional options");
        }
        args.extend(words);
    }
    for key in profile.env.keys() {
        if key.contains('=') || key.contains('\0') || key.is_empty() {
            return Err(anyhow!("Invalid environment variable name"));
        }
    }
    Ok(args)
}

fn kapitsa_arguments(options: &serde_json::Map<String, Value>) -> Result<Vec<String>> {
    let schema = kapitsa_options_schema();
    let fields = schema["fields"].as_array().expect("static options schema");
    let mut args = Vec::new();
    for (key, value) in options {
        if value.is_null() || key == EXTRA_ARGS {
            continue;
        }
        if managed_option(Engine::Kapitsa, key) {
            bail!("{key} is managed by Studio");
        }
        if !fields.iter().any(|field| field["key"] == key.as_str()) {
            bail!("Unknown Kapitsa server option: {key}");
        }
        let text = match value {
            Value::String(text) => text.trim().to_owned(),
            Value::Number(number) => number.to_string(),
            _ => bail!("Invalid value for Kapitsa option {key}"),
        };
        if text.is_empty() {
            continue;
        }
        if text.starts_with("--") {
            bail!("Invalid option value for {key}");
        }
        args.extend([format!("--{}", key.replace('_', "-")), text]);
    }
    Ok(args)
}

fn locals_arguments(options: &serde_json::Map<String, Value>) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for (key, value) in options {
        if value.is_null() || key == EXTRA_ARGS {
            continue;
        }
        let key = key.trim_start_matches("--").replace('_', "-");
        if managed_option(Engine::Locals, &key) {
            bail!(
                "{key} is managed by Studio; configure the common API address or the profile's model instead"
            );
        }
        if key == "cache-gb" {
            let cache = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()));
            if !cache.is_some_and(|cache| cache.is_finite() && cache > 0.) {
                bail!("cache_gb must be positive; use null/Auto to keep Locals automatic sizing");
            }
        }
        if matches!(key.as_str(), "thinking" | "walk" | "typed-tools") {
            match value.as_bool() {
                Some(true) => args.push(format!("--{key}")),
                Some(false) => {}
                None => bail!("{key} must be a boolean"),
            }
            continue;
        }
        if !matches!(
            key.as_str(),
            "engines"
                | "preload"
                | "slots"
                | "temp"
                | "top-p"
                | "top-k"
                | "max-tokens"
                | "cache-gb"
                | "max-context"
                | "seed"
                | "max-calls"
        ) {
            bail!("Unknown Locals server option: {key}");
        }
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Array(values) if matches!(key.as_str(), "engines" | "preload") => values
                .iter()
                .map(|v| v.as_str().context("Engine names must be strings"))
                .collect::<Result<Vec<_>>>()?
                .join(","),
            _ => bail!("Invalid value for Locals option {key}"),
        };
        if text.starts_with("--") {
            bail!("Invalid option value for {key}");
        }
        args.extend([format!("--{key}"), text]);
    }
    Ok(args)
}

/// Split options typed as in a shell: whitespace separates words, quotes group them, a backslash
/// escapes the next character. Nothing is expanded or run.
fn split_arguments(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                word.push(
                    chars
                        .next()
                        .context("Additional options end with a backslash")?,
                );
                started = true;
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        bail!("Additional options have an unclosed quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn worker_model_context(model: &Value) -> WorkerModelContext {
    let positive = |key| model[key].as_u64().filter(|value| *value > 0);
    WorkerModelContext {
        context_length: positive("context_length"),
        max_model_len: positive("max_model_len"),
    }
}

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Copy-and-truncate preserves the descriptor inherited by workers across daemon restarts.
/// Keep one bounded backup; writes at the rotation boundary may be lost, as with logrotate copytruncate.
async fn rotate_log_if_needed(path: &std::path::Path) -> Result<()> {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(());
    };
    if metadata.len() <= MAX_LOG_BYTES {
        return Ok(());
    }
    let mut source = tokio::fs::File::open(path).await?;
    source
        .seek(std::io::SeekFrom::Start(
            metadata.len().saturating_sub(MAX_LOG_BYTES),
        ))
        .await?;
    let mut backup = tokio::fs::File::create(path.with_extension("log.1")).await?;
    tokio::io::copy(&mut source.take(MAX_LOG_BYTES), &mut backup).await?;
    let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
    file.set_len(0).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_arguments_inherit_runtime_defaults_and_preserve_explicit_options() {
        let mut profile = ServerProfile::default();
        let args = server_options(&profile).unwrap();
        assert!(
            args.is_empty(),
            "missing options must inherit the Locals runtime instead of model-specific settings"
        );
        assert!(
            profile.options.is_empty(),
            "launch defaults must not be saved as user overrides"
        );
        profile.options = serde_json::from_value(json!({
            "temp":0,"top-p":1,"top-k":0,"max-tokens":512,"max-context":null
        }))
        .unwrap();
        let args = server_options(&profile).unwrap();
        assert_eq!(args.iter().filter(|arg| *arg == "--max-tokens").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["--max-tokens", "512"]));
        assert!(args.windows(2).any(|pair| pair == ["--temp", "0"]));
        assert!(args.windows(2).any(|pair| pair == ["--top-p", "1"]));
        assert!(args.windows(2).any(|pair| pair == ["--top-k", "0"]));
        assert!(
            !args.iter().any(|arg| arg == "--max-context"),
            "explicit null selects the runtime Auto setting"
        );
    }

    #[test]
    fn kapitsa_profiles_launch_with_their_options_and_additional_ones_but_never_managed_flags() {
        let profile = |options: Value| ServerProfile {
            engine: Engine::Kapitsa,
            options: options.as_object().unwrap().clone(),
            ..Default::default()
        };
        let args = server_options(&profile(json!({
            "context":"128k","slots":2,"profile":"fast","mem_budget":null,
            "extra_args":"--peer 'mac 2:8000' --new-flag=on"
        })))
        .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("--context 128k"));
        assert!(joined.contains("--slots 2"));
        assert!(joined.contains("--profile fast"));
        assert!(!joined.contains("--mem-budget"));
        assert!(args.ends_with(&["--peer".into(), "mac 2:8000".into(), "--new-flag=on".into()]));

        for extra in [
            "--port 9000",
            "--model-id=x",
            "--host 0.0.0.0",
            "--allow-switch",
        ] {
            let error = server_options(&profile(json!({"extra_args":extra}))).unwrap_err();
            assert!(error.to_string().contains("managed by Studio"), "{extra}");
        }
        assert!(server_options(&profile(json!({"extra_args":"--x 'open"}))).is_err());
        assert!(
            server_options(&profile(json!({"temp":0.2}))).is_err(),
            "a Locals option on Kapitsa"
        );
        assert!(
            validate_server_options(
                Engine::Kapitsa,
                &profile(json!({"profile":"turbo"})).options
            )
            .is_err()
        );
        assert!(
            validate_server_options(Engine::Kapitsa, &profile(json!({"context":131072})).options)
                .is_ok()
        );
        assert!(
            validate_server_options(
                Engine::Locals,
                &profile(json!({"extra_args":"--ollama-port 1"})).options
            )
            .is_err()
        );
        let locals = ServerProfile {
            options: json!({"slots":4,"extra_args":"--future-option 1"})
                .as_object()
                .unwrap()
                .clone(),
            ..Default::default()
        };
        assert_eq!(
            server_options(&locals).unwrap().join(" "),
            "--slots 4 --future-option 1"
        );
    }

    #[test]
    fn worker_ownership_recognizes_locals_and_kapitsa_commands_only() {
        let words = |text: &str| text.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert!(is_worker_command(&words(
            "/env/bin/python -u -c x -m locals.server --port 1"
        )));
        assert!(is_worker_command(&words(
            "/data/engines/kapitsa/versions/v/kapitsa serve --port 1"
        )));
        assert!(!is_worker_command(&words(
            "/usr/bin/kapitsa-systemone serve"
        )));
        assert!(!is_worker_command(&words("/data/kapitsa pull 4b")));
        assert!(!is_worker_command(&words(
            "/usr/bin/python3 -m http.server"
        )));
    }

    #[test]
    fn kapitsa_engine_status_reads_as_worker_stats() {
        let stats = crate::telemetry::kapitsa_stats(&json!({
            "state":"ready","model":"qwen3.5-4b",
            "requests":[{"request_id":"req-1","phase":"decoding","prompt_tokens":120,"cached_tokens":100,"completion_tokens":7,"tokens_per_second":41.5}],
            "engine":{"memory":{"budget_bytes":20_000_000_000u64,"allocated_bytes":6_000_000_000u64,"kv_bytes":1_000_000_000u64}}
        }));
        assert_eq!(stats["active"][0]["tokens"], 7);
        assert_eq!(stats["active"][0]["prompt_tokens"], 120);
        assert_eq!(stats["memory"]["gpu_active_mb"], 6000.0);
        assert_eq!(
            worker_requests(Engine::Kapitsa, &json!({"requests":[1,2]})).map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn schema_reports_runtime_defaults_and_auto_without_universal_model_tuning() {
        let schema = options_schema(Engine::Locals);
        assert_eq!(schema["null_behavior"], "omit");
        let fields = schema["fields"].as_array().unwrap();
        for (key, runtime) in [
            ("temp", json!(0.2)),
            ("top_p", json!(0.0)),
            ("top_k", json!(0)),
            ("max_tokens", json!(4096)),
            ("max_context", Value::Null),
        ] {
            let field = fields.iter().find(|field| field["key"] == key).unwrap();
            assert_eq!(field["default"], runtime, "{key}");
            assert_eq!(field["runtime_default"], runtime, "{key}");
            assert_eq!(field["default_source"], "runtime");
        }
        assert_eq!(schema["studio_defaults"], json!({}));
    }

    #[test]
    fn worker_context_limits_require_positive_integer_metadata() {
        let context = worker_model_context(&json!({"context_length":8192,"max_model_len":32768}));
        assert_eq!(context.context_length, Some(8192));
        assert_eq!(context.max_model_len, Some(32768));
        for invalid in [
            Value::Null,
            json!(0),
            json!(-1),
            json!(1.5),
            json!("8192"),
            json!(true),
            json!({}),
        ] {
            let context =
                worker_model_context(&json!({"context_length":invalid,"max_model_len":invalid}));
            assert_eq!(context.context_length, None);
            assert_eq!(context.max_model_len, None);
        }
        let context = worker_model_context(&json!({"context_length":4096,"max_model_len":0}));
        assert_eq!(context.context_length, Some(4096));
        assert_eq!(context.max_model_len, None);
    }

    const FAKE_WORKER: &str = r#"
import argparse,json,os,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from socketserver import TCPServer
if os.environ.get('FAKE_FAIL_REVERSE_DNS') == '1':
 import socket
 def reject_reverse_dns(*args): raise AssertionError('Fake workers must not perform reverse DNS')
 socket.getfqdn=reject_reverse_dns
class LocalHTTPServer(ThreadingHTTPServer):
 def server_bind(self):
  # HTTPServer.server_bind performs reverse DNS even for our numeric loopback host.
  TCPServer.server_bind(self)
  self.server_name='localhost';self.server_port=self.server_address[1]
p=argparse.ArgumentParser()
p.add_argument('--host');p.add_argument('--port',type=int);p.add_argument('--model')
args,_=p.parse_known_args()
model_ids=json.loads(os.environ.get('FAKE_MODEL_IDS','["locals/fake"]'))
model_contexts=json.loads(os.environ.get('FAKE_MODEL_CONTEXTS','{}'))
time.sleep(float(os.environ.get('FAKE_LOADING_DELAY','0')))
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  data={'/health':{'status':'ok','version':'0.2.1'},'/stats':{'model':args.model,'active':[],'memory':{'gpu_active_mb':1,'gpu_cache_mb':2,'gpu_peak_mb':3,'rss_mb':4}},'/v1/models':{'data':[{'id':model,**model_contexts.get(model,{})} for model in model_ids]}}.get(self.path,{})
  body=json.dumps(data).encode();self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
LocalHTTPServer((args.host,args.port),Handler).serve_forever()
"#;

    async fn fixture() -> (tempfile::TempDir, Supervisor, ServerProfile, ModelRecord) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = AppPaths {
            data_dir: root.join("data"),
            database_path: root.join("data/db"),
            socket_path: root.join("data/sock"),
            models_dir: root.join("models"),
            locals_source: None,
        };
        paths.prepare().unwrap();
        let runtime = paths.data_dir.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let python = Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .await
            .unwrap();
        assert!(
            python.status.success(),
            "python3 is required for the fake-worker test"
        );
        let python = String::from_utf8_lossy(&python.stdout).trim().to_owned();
        std::fs::write(runtime.join("current.json"),serde_json::to_vec(&json!({"python_path":python,"python_version":"3.12","locals_version":"0.2.1","installed_at":"test","source_path":root})).unwrap()).unwrap();
        let package = root.join("fake/locals");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("__init__.py"), "").unwrap();
        std::fs::write(package.join("server.py"), FAKE_WORKER).unwrap();
        let model_path = root.join("model");
        std::fs::create_dir_all(&model_path).unwrap();
        let model = ModelRecord {
            id: "model".into(),
            name: "fake".into(),
            source: "import".into(),
            repo_id: None,
            revision: None,
            path: model_path,
            weights_bytes: 1024,
            compatible: true,
            engines: vec![crate::types::Engine::Locals],
            architecture: Some("test".into()),
            quantization: None,
            context_length: Some(1024),
        };
        let store = Store::open(&paths.database_path).unwrap();
        store.set("models", &model.id, &model).unwrap();
        store
            .set(
                "settings",
                "config",
                &StudioConfig {
                    memory_budget_bytes: Some(64 << 30),
                    ..Default::default()
                },
            )
            .unwrap();
        let (events, _) = tokio::sync::broadcast::channel(64);
        let supervisor = Supervisor::new(paths, store, events);
        let mut profile = ServerProfile {
            id: "first".into(),
            alias: "first".into(),
            model_id: model.id.clone(),
            ..Default::default()
        };
        profile.options =
            serde_json::from_value(json!({"max_context":1024,"cache_gb":0.25,"slots":1})).unwrap();
        profile.env.insert(
            "PYTHONPATH".into(),
            root.join("fake").to_string_lossy().into_owned(),
        );
        (temp, supervisor, profile, model)
    }

    #[tokio::test]
    async fn logs_are_empty_for_a_saved_profile_that_has_never_started_but_unknown_ids_fail() {
        let (_temp, supervisor, profile, _model) = fixture().await;
        supervisor
            .store
            .set("profiles", &profile.id, &profile)
            .unwrap();
        let logs = supervisor.logs(&profile.id).await.unwrap();
        assert_eq!(
            logs,
            json!({"profile_id":profile.id,"text":"","entries":[],"path":null,"truncated":false})
        );
        assert!(supervisor.logs("unknown-profile").await.is_err());
    }

    fn inactive_record(
        profile: ServerProfile,
        model: &ModelRecord,
        log_path: PathBuf,
    ) -> WorkerRecord {
        WorkerRecord {
            status: ServerStatus {
                profile_id: profile.id.clone(),
                alias: profile.alias.clone(),
                model_id: profile.model_id.clone(),
                engine: Default::default(),
                runtime_version: None,
                state: "stopped".into(),
                pid: None,
                port: None,
                backend_model: None,
                backend_models: Vec::new(),
                backend_model_contexts: HashMap::new(),
                error: None,
                active_requests: 0,
                started_at: None,
            },
            profile,
            identity: None,
            log_path,
            model_path: model.path.clone(),
            runtime_root: None,
        }
    }

    #[tokio::test]
    async fn ready_alias_routes_past_a_stopped_legacy_profile_and_forgetting_removes_only_inactive_records()
     {
        let (_temp, supervisor, profile, model) = fixture().await;
        let mut second = profile.clone();
        second.id = "second".into();
        let mut workers = supervisor.workers.lock().await;
        for profile in [profile, second] {
            let record = inactive_record(
                profile,
                &model,
                supervisor.paths.data_dir.join("historical.log"),
            );
            workers.insert(
                record.profile.id.clone(),
                Worker {
                    record,
                    child: None,
                    active: Arc::new(AtomicUsize::new(0)),
                    accepting: false,
                },
            );
        }
        // Assign roles in actual HashMap iteration order so the old find(alias) implementation
        // deterministically selects the stopped record and fails this regression.
        let ids: Vec<_> = workers.keys().cloned().collect();
        let legacy_id = ids[0].clone();
        let ready_id = ids[1].clone();
        let ready = workers.get_mut(&ready_id).unwrap();
        ready.record.status.state = "ready".into();
        ready.record.status.pid = Some(u32::MAX);
        ready.record.status.port = Some(12345);
        ready.record.status.backend_model = Some("locals/fake-new".into());
        ready.accepting = true;
        for worker in workers.values() {
            supervisor.persist(&worker.record).unwrap();
        }
        assert!(
            !workers
                .values()
                .find(|worker| worker.record.profile.alias == "first")
                .unwrap()
                .accepting
        );
        drop(workers);
        let lease = supervisor.acquire("first").await.unwrap();
        assert_eq!(lease.profile_id, ready_id);
        assert_eq!(lease.backend_model, "locals/fake-new");
        drop(lease);
        supervisor.forget_profile(&legacy_id).await.unwrap();
        assert!(
            supervisor
                .store
                .get::<WorkerRecord>("workers", &legacy_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(supervisor.list().await.len(), 1);
        let lease = supervisor.acquire("first").await.unwrap();
        assert_eq!(lease.profile_id, ready_id);
        drop(lease);
        assert!(supervisor.forget_profile(&ready_id).await.is_err());
        assert!(
            supervisor
                .store
                .get::<WorkerRecord>("workers", &ready_id)
                .unwrap()
                .is_some()
        );
        let mut workers = supervisor.workers.lock().await;
        let loading = workers.get_mut(&ready_id).unwrap();
        loading.record.status.pid = None;
        loading.record.status.state = "starting".into();
        loading.accepting = false;
        supervisor.persist(&loading.record).unwrap();
        drop(workers);
        assert!(supervisor.forget_profile(&ready_id).await.is_err());
    }

    #[tokio::test]
    async fn initialize_discards_only_inactive_records_without_a_saved_profile() {
        let (_temp, supervisor, profile, model) = fixture().await;
        let ghost = inactive_record(
            profile.clone(),
            &model,
            supervisor.paths.data_dir.join("legacy.log"),
        );
        supervisor.persist(&ghost).unwrap();
        let mut retained_profile = profile;
        retained_profile.id = "retained".into();
        let retained = inactive_record(
            retained_profile.clone(),
            &model,
            supervisor.paths.data_dir.join("retained.log"),
        );
        supervisor
            .store
            .set("profiles", &retained_profile.id, &retained_profile)
            .unwrap();
        supervisor.persist(&retained).unwrap();
        supervisor.initialize().await.unwrap();
        assert!(
            supervisor
                .store
                .get::<WorkerRecord>("workers", &ghost.profile.id)
                .unwrap()
                .is_none()
        );
        let remaining = supervisor.list().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].profile_id, "retained");
    }

    #[tokio::test]
    async fn fake_worker_readiness_never_requires_reverse_dns() {
        let (_temp, supervisor, mut profile, model) = fixture().await;
        profile
            .env
            .insert("FAKE_FAIL_REVERSE_DNS".into(), "1".into());
        let ready = tokio::time::timeout(
            Duration::from_secs(10),
            supervisor.start(profile.clone(), model, false),
        )
        .await
        .expect("fake worker startup must not wait for reverse DNS")
        .unwrap();
        assert_eq!(ready.state, "ready");
        supervisor.stop(&profile.id).await.unwrap();
    }

    #[tokio::test]
    async fn recovery_reprobes_legacy_model_ids_and_restart_clears_old_variants() {
        let (_temp, supervisor, mut profile, model) = fixture().await;
        let ids = json!([
            "locals/fake-thinking",
            "mlx/fake",
            "mlx/fake-thinking",
            "locals/fake"
        ]);
        profile.env.insert("FAKE_MODEL_IDS".into(), ids.to_string());
        let contexts = json!({
            "locals/fake":{"context_length":8192,"max_model_len":8192},
            "locals/fake-thinking":{"context_length":8192},
            "mlx/fake":{"max_model_len":4096},
            "mlx/fake-thinking":{"context_length":-1,"max_model_len":"4096"}
        });
        profile
            .env
            .insert("FAKE_MODEL_CONTEXTS".into(), contexts.to_string());
        supervisor
            .store
            .set("profiles", &profile.id, &profile)
            .unwrap();
        let ready = supervisor
            .start(profile.clone(), model.clone(), false)
            .await
            .unwrap();
        assert_eq!(ready.backend_model.as_deref(), Some("locals/fake"));
        assert_eq!(serde_json::to_value(&ready.backend_models).unwrap(), ids);
        assert_eq!(ready.backend_model_contexts.len(), 3);
        assert_eq!(
            ready.backend_model_contexts["locals/fake"].context_length,
            Some(8192)
        );
        assert_eq!(
            ready.backend_model_contexts["mlx/fake"].max_model_len,
            Some(4096)
        );
        assert!(
            !ready
                .backend_model_contexts
                .contains_key("mlx/fake-thinking")
        );

        // A daemon upgrade reads old records without cached IDs, then probes the
        // still-running worker rather than trusting its historical canonical ID.
        let mut legacy = supervisor
            .store
            .get::<Value>("workers", &profile.id)
            .unwrap()
            .unwrap();
        legacy["status"]["backend_model"] = json!("locals/stale");
        legacy["status"]
            .as_object_mut()
            .unwrap()
            .remove("backend_models");
        legacy["status"]
            .as_object_mut()
            .unwrap()
            .remove("backend_model_contexts");
        supervisor
            .store
            .set("workers", &profile.id, &legacy)
            .unwrap();
        let recovered = Supervisor::new(
            supervisor.paths.clone(),
            supervisor.store.clone(),
            supervisor.events.clone(),
        );
        recovered.initialize().await.unwrap();
        let ready = recovered.list().await.pop().unwrap();
        assert_eq!(ready.backend_model.as_deref(), Some("locals/fake"));
        assert_eq!(serde_json::to_value(&ready.backend_models).unwrap(), ids);
        assert_eq!(
            ready.backend_model_contexts["locals/fake"].context_length,
            Some(8192),
            "recovery re-probes the worker rather than using the profile context of 1024"
        );
        let stopped = recovered.stop(&profile.id).await.unwrap();
        assert!(stopped.backend_models.is_empty());
        assert!(stopped.backend_model.is_none());
        assert!(stopped.backend_model_contexts.is_empty());

        profile
            .env
            .insert("FAKE_MODEL_IDS".into(), json!(["locals/fake"]).to_string());
        profile.env.remove("FAKE_MODEL_CONTEXTS");
        let restarted = recovered
            .start(profile.clone(), model, false)
            .await
            .unwrap();
        assert_eq!(restarted.backend_models, ["locals/fake"]);
        assert!(
            restarted.backend_model_contexts.is_empty(),
            "a restarted worker without limit metadata must not inherit old values"
        );
        recovered.stop(&profile.id).await.unwrap();
    }

    #[tokio::test]
    async fn workers_get_distinct_ports_and_leases_track_activity_and_stop_is_scoped() {
        let (_temp, supervisor, profile, model) = fixture().await;
        let first = supervisor
            .start(profile.clone(), model.clone(), false)
            .await
            .unwrap();
        assert_eq!(first.state, "ready");
        let mut second_profile = profile.clone();
        second_profile.id = "second".into();
        second_profile.alias = "second".into();
        let second = supervisor
            .start(second_profile, model, false)
            .await
            .unwrap();
        assert_ne!(first.port, second.port);
        let lease = supervisor.acquire("first").await.unwrap();
        assert_eq!(lease.backend_model, "locals/fake");
        assert_eq!(
            supervisor
                .list()
                .await
                .into_iter()
                .find(|s| s.profile_id == "first")
                .unwrap()
                .active_requests,
            1
        );
        drop(lease);
        let stopped = supervisor.stop("first").await.unwrap();
        assert_eq!(stopped.state, "stopped");
        assert!(supervisor.acquire("first").await.is_err());
        assert!(identity_matches(
            supervisor.workers.lock().await["second"]
                .record
                .identity
                .as_ref()
                .unwrap()
        ));
        supervisor.stop("second").await.unwrap();
    }

    #[tokio::test]
    async fn ownership_check_rejects_an_unrelated_process_and_profile_cannot_rebind_worker() {
        let output = Command::new("python3")
            .args(["-c", "import time;time.sleep(30)"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let identity = read_identity(output.id().unwrap()).unwrap();
        assert!(
            identity.command.iter().any(|argument| argument == "-c"),
            "Identity snapshots must explicitly refresh process arguments"
        );
        assert!(!identity_matches(&identity));
        assert!(signal_owned(&identity, libc::SIGTERM).is_err());
        assert!(read_identity(identity.pid).is_some());
        let mut profile = ServerProfile::default();
        profile.options.insert("host".into(), json!("0.0.0.0"));
        assert!(server_options(&profile).is_err());
        drop(output);
    }

    #[tokio::test]
    async fn stopping_a_loading_worker_cancels_it_without_waiting_for_the_load_lock() {
        let (_temp, supervisor, mut profile, model) = fixture().await;
        profile.env.insert("FAKE_LOADING_DELAY".into(), "30".into());
        let starter = supervisor.clone();
        let loading = tokio::spawn(async move { starter.start(profile, model, false).await });
        let deadline = Instant::now() + Duration::from_secs(5);
        while supervisor
            .list()
            .await
            .first()
            .is_none_or(|s| s.state != "starting")
        {
            assert!(
                Instant::now() < deadline,
                "The fake worker was not launched"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let stopped = tokio::time::timeout(Duration::from_secs(5), supervisor.stop("first"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stopped.state, "stopped");
        let result = tokio::time::timeout(Duration::from_secs(3), loading)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_ne!(result.state, "ready");
    }

    #[tokio::test]
    async fn log_rotation_retains_a_bounded_backup_and_keeps_the_inherited_file_usable() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("server.log");
        let mut inherited = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        inherited
            .write_all(&vec![b'x'; (MAX_LOG_BYTES + 1024) as usize])
            .unwrap();
        rotate_log_if_needed(&path).await.unwrap();
        assert_eq!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES
        );
        inherited.write_all(b"after rotation\n").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "after rotation\n");
    }
}
