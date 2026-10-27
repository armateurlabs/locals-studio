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
    runtime::RuntimeManager,
    store::Store,
    telemetry::{self, TelemetryCollector},
    types::{ServerProfile, ServerStatus, StudioConfig},
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
            let owned = record.identity.as_ref().is_some_and(identity_matches);
            let mut accepting = false;
            if owned {
                if record.status.state == "draining" {
                    // A daemon restart must not turn a deliberately stopped server back into a ready one.
                    if let Some(identity) = record.identity.as_ref() {
                        signal_owned(identity, libc::SIGTERM)?;
                    }
                    record.status.state = "stopping".into();
                } else if let Ok((backend, models)) = self.probe(&record).await {
                    record.status.backend_model = Some(backend);
                    record.status.backend_models = models;
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
        if !model.compatible {
            bail!("This model is incomplete or is not a supported text safetensors model");
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
        // task rather than abandoning uv or leaving an installation half committed.
        let runtime = self.runtime.clone();
        let mut installation = tokio::spawn(async move { runtime.python().await });
        let python = tokio::select! {
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
        let mut command = Command::new(&python);
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
            .args(args)
            .envs(&profile.env)
            .env("PYTHONUNBUFFERED", "1")
            .env("HF_HUB_OFFLINE", "1")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        command.as_std_mut().process_group(0);
        if start_cancelled(&cancellation) {
            return Ok(None);
        }
        // Workers survive a UI or daemon exit and are reconciled from their recorded process identity.
        let mut child = command
            .spawn()
            .context("Could not launch the Locals worker")?;
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
                state: "starting".into(),
                pid: Some(pid),
                port: Some(port),
                backend_model: None,
                backend_models: Vec::new(),
                error: None,
                active_requests: 0,
                started_at: Some(chrono::Utc::now().to_rfc3339()),
            },
            profile,
            identity: Some(identity),
            log_path,
            model_path,
        };
        let id = record.profile.id.clone();
        if let Err(error) = self.persist(&record) {
            let _ = child.kill().await;
            return Err(error);
        }
        events::emit(&self.events, "server.state", &record.status);
        self.workers.lock().await.insert(
            id.clone(),
            Worker {
                record,
                child: Some(child),
                active: Arc::new(AtomicUsize::new(0)),
                accepting: false,
            },
        );
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
            if let Ok((backend, models)) = self.probe(&record).await {
                let mut workers = self.workers.lock().await;
                let worker = workers.get_mut(id).context("Unknown server profile")?;
                if worker.record.status.state != "starting" {
                    return Ok(worker.status());
                }
                worker.record.status.backend_model = Some(backend);
                worker.record.status.backend_models = models;
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

    async fn probe(&self, record: &WorkerRecord) -> Result<(String, Vec<String>)> {
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
        if health["status"] != "ok" || health["version"] != crate::runtime::LOCALS_VERSION {
            bail!("Worker health/version did not match Locals");
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
        let models: Value = self
            .client
            .get(format!("{base}/v1/models"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut ids = Vec::new();
        for id in models["data"]
            .as_array()
            .context("Worker model list is invalid")?
            .iter()
            .filter_map(|model| model["id"].as_str())
            .filter(|id| !id.is_empty())
        {
            if !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_owned());
            }
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
        Ok((backend, ids))
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
        let (identity, active, port) = {
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
            )
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let backend_active = if let Some(port) = port {
                self.client
                    .get(format!("http://127.0.0.1:{port}/stats"))
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
                    .and_then(|s| s["active"].as_array().map(Vec::len))
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
                signal_owned(identity, libc::SIGTERM)?;
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
        worker.record.status.active_requests = 0;
        worker.record.status.error = None;
        worker.record.identity = None;
        self.persist(&worker.record)?;
        events::emit(&self.events, "server.state", &worker.record.status);
        Ok(worker.status())
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
                tasks.spawn(async move {
                    let result = client
                        .get(format!("http://127.0.0.1:{port}/stats"))
                        .send()
                        .await;
                    let value = match result {
                        Ok(response) if response.status().is_success() => {
                            response.json::<Value>().await.ok()
                        }
                        _ => None,
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
            let module = identity
                .command
                .windows(2)
                .any(|p| p[0] == "-m" && p[1] == "locals.server");
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
            && actual
                .command
                .windows(2)
                .any(|pair| pair[0] == "-m" && pair[1] == "locals.server")
    })
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

/// Public form metadata for the pinned Locals runtime. Null means omit the argument.
pub fn options_schema() -> Value {
    json!({
        "locals_version":"0.2.1","restart_required":true,"null_behavior":"omit",
        "fields":[
            {"key":"engines","label":"Engines","type":"string","default":"locals","description":"Allowed engines, separated by commas: locals, mlx. Two engines load two copies of the same model."},
            {"key":"preload","label":"Preload","type":"string","default":"locals","description":"Engines loaded at startup, separated by commas. none defers loading until the first request."},
            {"key":"slots","label":"Concurrent requests","type":"integer","default":16,"min":1,"description":"Maximum batch size for each engine; affects memory requirements."},
            {"key":"temp","label":"Temperature","type":"number","default":0.2,"min":0,"description":"Default generation temperature. Zero enables deterministic generation."},
            {"key":"top_p","label":"Top-p","type":"number","default":0.0,"min":0,"max":1,"description":"Nucleus sampling when temperature is positive. Zero or one disables this filter."},
            {"key":"top_k","label":"Top-k","type":"integer","default":0,"min":0,"description":"Number of candidates retained when temperature is positive. Zero disables this filter."},
            {"key":"max_tokens","label":"Response tokens","type":"integer","default":4096,"min":0,"description":"Default response token budget for each request."},
            {"key":"cache_gb","label":"Prompt cache (GiB)","type":"number","default":null,"min":0,"exclusive_min":true,"auto":true,"description":"Auto uses Locals' automatic sizing. An explicit value must be strictly positive; zero does not disable the cache and must be replaced with Auto."},
            {"key":"max_context","label":"Maximum context (tokens)","type":"integer","default":null,"min":1,"auto":true,"description":"Combined prompt and response tokens per request. Auto lets Locals size the context based on the model and available memory."},
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

fn server_options(profile: &ServerProfile) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for (key, value) in &profile.options {
        if value.is_null() {
            continue;
        }
        let key = key.trim_start_matches("--").replace('_', "-");
        if matches!(key.as_str(), "host" | "port" | "model" | "ollama-port") {
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
            "ollama-port"
                | "engines"
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
    for key in profile.env.keys() {
        if key.contains('=') || key.contains('\0') || key.is_empty() {
            return Err(anyhow!("Invalid environment variable name"));
        }
    }
    Ok(args)
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
time.sleep(float(os.environ.get('FAKE_LOADING_DELAY','0')))
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  data={'/health':{'status':'ok','version':'0.2.1'},'/stats':{'model':args.model,'active':[],'memory':{'gpu_active_mb':1,'gpu_cache_mb':2,'gpu_peak_mb':3,'rss_mb':4}},'/v1/models':{'data':[{'id':model} for model in model_ids]}}.get(self.path,{})
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
            locals_source: root.join("source"),
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
                state: "stopped".into(),
                pid: None,
                port: None,
                backend_model: None,
                backend_models: Vec::new(),
                error: None,
                active_requests: 0,
                started_at: None,
            },
            profile,
            identity: None,
            log_path,
            model_path: model.path.clone(),
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
        let stopped = recovered.stop(&profile.id).await.unwrap();
        assert!(stopped.backend_models.is_empty());
        assert!(stopped.backend_model.is_none());

        profile
            .env
            .insert("FAKE_MODEL_IDS".into(), json!(["locals/fake"]).to_string());
        let restarted = recovered
            .start(profile.clone(), model, false)
            .await
            .unwrap();
        assert_eq!(restarted.backend_models, ["locals/fake"]);
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
