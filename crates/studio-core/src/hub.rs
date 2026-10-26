//! Hugging Face discovery and durable, checkpointed model downloads.
//!
//! Studio uses the Hub HTTP download endpoint, owns a durable staging manifest, and never edits
//! the shared Hugging Face cache. Checkpoints survive proxies that ignore Range: a full response
//! is identified by length and ETag, and its replayed prefix is compared before appending.
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Notify, Semaphore};

use crate::{
    events::{EventSender, emit},
    models,
    paths::AppPaths,
    store::Store,
};

const RANGE_BYTES: u64 = 64 * 1024 * 1024;
const JOBS: &str = "downloads";
const LOGOS: &str = "publisher-logos-v1";
const MAX_LOGO_BYTES: usize = 256 * 1024;
const LOGO_TTL: u64 = 7 * 24 * 60 * 60;
const LOGO_RETRY_TTL: u64 = 60 * 60;
const MAX_MODEL_JSON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct Compatibility {
    status: &'static str,
    reason: Option<String>,
    revision: Option<String>,
}

impl Compatibility {
    fn new(info: &Value, status: &'static str, reason: Option<String>) -> Self {
        Self {
            status,
            reason,
            revision: immutable_revision(info).map(str::to_owned),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CachedLogo {
    data_url: Option<String>,
    expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadFile {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_blob_sha1: Option<String>,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadJob {
    pub id: String,
    pub repo_id: String,
    pub revision: String,
    pub status: String,
    pub files: Vec<DownloadFile>,
    pub manifest_sha256: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub bytes_per_second: f64,
    pub eta_seconds: Option<u64>,
    #[serde(default)]
    pub replayed_bytes: u64,
    #[serde(default)]
    pub transport: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub destination: PathBuf,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct HubManager {
    inner: Arc<Inner>,
}

struct Inner {
    paths: AppPaths,
    store: Store,
    events: EventSender,
    runner: Mutex<()>,
    active: Mutex<Option<String>>,
    state: std::sync::Mutex<()>,
    control: Notify,
    endpoint: String,
    range_bytes: u64,
    logo_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    logo_requests: Semaphore,
}

impl HubManager {
    pub fn new(paths: AppPaths, store: Store, events: EventSender) -> Self {
        Self {
            inner: Arc::new(Inner {
                paths,
                store,
                events,
                runner: Mutex::new(()),
                active: Mutex::new(None),
                state: std::sync::Mutex::new(()),
                control: Notify::new(),
                endpoint: std::env::var("HF_ENDPOINT")
                    .unwrap_or_else(|_| "https://huggingface.co".to_owned()),
                range_bytes: RANGE_BYTES,
                logo_locks: Mutex::new(HashMap::new()),
                logo_requests: Semaphore::new(4),
            }),
        }
    }

    /// Recover interrupted jobs on daemon startup. A paused job remains paused.
    pub async fn recover(&self) -> Result<()> {
        fs::create_dir_all(self.staging_root())?;
        fs::create_dir_all(&self.inner.paths.models_dir)?;
        for mut job in self.jobs()? {
            if job.status == "running" || job.status == "verifying" {
                job.status = "queued".to_owned();
                job.bytes_per_second = 0.0;
                job.eta_seconds = None;
                self.save(&mut job)?;
            }
        }
        self.kick();
        Ok(())
    }

    pub async fn search(&self, query: &str) -> Result<Value> {
        let query = query.trim();
        if query.len() > 256 {
            bail!("Search query is too long (256 characters maximum)");
        }
        let mut url = self.api_url(&["api", "models"])?;
        url.query_pairs_mut()
            .append_pair("search", query)
            .append_pair("filter", "mlx")
            .append_pair("sort", "downloads")
            .append_pair("direction", "-1")
            .append_pair("limit", "50")
            // The list API supplies the revision and file names, but currently omits file sizes
            // even with blobs=true. Keep discovery to one request; visible cards can inspect a
            // pinned revision through model.info instead of estimating bytes from parameter count.
            .append_pair("full", "true");
        let mut results = self.get_json(url).await?;
        let results = results
            .as_array_mut()
            .context("Invalid Hugging Face model list")?;
        for model in results.iter_mut() {
            attach_size_metadata(model);
            let compatibility = discovery_compatibility(model);
            model["studio_compatibility"] = serde_json::to_value(compatibility)?;
        }
        results.retain(|model| model["studio_compatibility"]["status"] != "incompatible");
        Ok(json!(results))
    }

    pub async fn info(&self, repo_id: &str) -> Result<Value> {
        self.info_at(repo_id, None).await
    }

    /// Publisher avatars come from Hugging Face's official namespace avatar endpoint only.
    /// Return a local data URL so the desktop renderer never needs remote image permissions.
    pub async fn logo(&self, repo_id: &str) -> Result<Value> {
        validate_repo(repo_id)?;
        let Some((publisher, _)) = repo_id.split_once('/') else {
            return Ok(json!({"data_url":null}));
        };
        let publisher = publisher.to_ascii_lowercase();
        let lock = self
            .inner
            .logo_locks
            .lock()
            .await
            .entry(publisher.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let data_url = cached_publisher_logo(&self.inner.store, &publisher, now(), || async {
            let _permit = self.inner.logo_requests.acquire().await?;
            tokio::time::timeout(Duration::from_secs(8), fetch_publisher_logo(&publisher))
                .await
                .context("Publisher avatar request timed out")?
        })
        .await?;
        Ok(json!({"data_url":data_url}))
    }

    pub async fn info_at(&self, repo_id: &str, revision: Option<&str>) -> Result<Value> {
        validate_repo(repo_id)?;
        let mut info = self.metadata(repo_id, revision.unwrap_or("main")).await?;
        attach_size_metadata(&mut info);
        let compatibility = self.check_compatibility(repo_id, &info).await?;
        info["studio_compatibility"] = serde_json::to_value(compatibility)?;
        Ok(info)
    }

    pub async fn add(&self, repo_id: &str, revision: Option<&str>) -> Result<Value> {
        validate_repo(repo_id)?;
        let info = self.metadata(repo_id, revision.unwrap_or("main")).await?;
        let sha = immutable_revision(&info)
            .context("Hugging Face did not provide a valid immutable revision")?
            .to_owned();
        let compatibility = self.check_compatibility(repo_id, &info).await?;
        if compatibility.status != "compatible" {
            bail!(
                "{}",
                compatibility
                    .reason
                    .as_deref()
                    .unwrap_or("Model compatibility could not be verified")
            );
        }
        let _state = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("Download state is unavailable"))?;
        for job in self.jobs()? {
            if job.repo_id == repo_id
                && job.revision == sha
                && (matches!(
                    job.status.as_str(),
                    "queued" | "running" | "verifying" | "paused"
                ) || (job.status == "completed" && job.destination.is_dir()))
            {
                if job.status == "completed" {
                    models::register_download(&self.inner.store, &job.destination, repo_id, &sha)?;
                }
                return Ok(serde_json::to_value(job)?);
            }
        }
        let files = build_manifest(&info)?;
        let total_bytes = files
            .iter()
            .try_fold(0u64, |sum, f| sum.checked_add(f.size))
            .context("Total model size is too large")?;
        let manifest_sha256 = manifest_hash(&files)?;
        let safe_repo = repo_id.replace('/', "--");
        let destination = self
            .inner
            .paths
            .models_dir
            .join(format!("{safe_repo}--{}", &sha[..12]));
        let mut job = DownloadJob {
            id: uuid::Uuid::new_v4().to_string(),
            repo_id: repo_id.to_owned(),
            revision: sha,
            status: "queued".to_owned(),
            files,
            manifest_sha256,
            total_bytes,
            downloaded_bytes: 0,
            bytes_per_second: 0.0,
            eta_seconds: None,
            replayed_bytes: 0,
            transport: "http".to_owned(),
            created_at: now(),
            updated_at: now(),
            destination,
            error: None,
        };
        // Persist before starting so closing the GUI or a process crash cannot lose the queue.
        self.save(&mut job)?;
        self.announce(&job);
        self.kick();
        Ok(serde_json::to_value(job)?)
    }

    pub fn list(&self) -> Result<Vec<Value>> {
        self.jobs()?
            .into_iter()
            .map(|job| serde_json::to_value(job).map_err(Into::into))
            .collect()
    }

    pub async fn pause(&self, id: &str) -> Result<Value> {
        let job = self.change(id, |job| {
            if matches!(job.status.as_str(), "queued" | "running" | "verifying") {
                job.status = "paused".to_owned();
                job.downloaded_bytes = job.files.iter().map(|file| file.downloaded).sum();
                job.bytes_per_second = 0.0;
                job.eta_seconds = None;
            }
            Ok(())
        })?;
        self.inner.control.notify_waiters();
        self.announce(&job);
        Ok(serde_json::to_value(job)?)
    }

    pub async fn resume(&self, id: &str) -> Result<Value> {
        let job = self.change(id, |job| {
            if !matches!(job.status.as_str(), "paused" | "failed" | "queued") {
                bail!("This download cannot be resumed in its current state");
            }
            job.status = "queued".to_owned();
            job.error = None;
            Ok(())
        })?;
        self.announce(&job);
        self.inner.control.notify_waiters();
        self.kick();
        Ok(serde_json::to_value(job)?)
    }

    pub async fn cancel(&self, id: &str) -> Result<Value> {
        let job = self.change(id, |job| {
            if job.status == "completed" {
                bail!("A completed download cannot be cancelled");
            }
            job.status = "cancelled".to_owned();
            job.bytes_per_second = 0.0;
            job.eta_seconds = None;
            Ok(())
        })?;
        self.inner.control.notify_waiters();
        if self.inner.active.lock().await.as_deref() != Some(id) {
            remove_staging(&self.staging_dir(id))?;
        }
        self.announce(&job);
        Ok(serde_json::to_value(job)?)
    }

    fn jobs(&self) -> Result<Vec<DownloadJob>> {
        let mut jobs = self.inner.store.list::<DownloadJob>(JOBS)?;
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(jobs)
    }

    fn job(&self, id: &str) -> Result<DownloadJob> {
        self.inner
            .store
            .get(JOBS, id)?
            .ok_or_else(|| anyhow!("Unknown download: {id}"))
    }

    fn save(&self, job: &mut DownloadJob) -> Result<()> {
        job.updated_at = now();
        self.inner.store.set(JOBS, &job.id, job)
    }

    fn change(
        &self,
        id: &str,
        update: impl FnOnce(&mut DownloadJob) -> Result<()>,
    ) -> Result<DownloadJob> {
        let _guard = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("Download state is unavailable"))?;
        let mut job = self.job(id)?;
        update(&mut job)?;
        self.save(&mut job)?;
        Ok(job)
    }

    fn announce(&self, job: &DownloadJob) {
        emit(&self.inner.events, "download.updated", job);
    }

    fn kick(&self) {
        let manager = self.clone();
        tokio::spawn(async move {
            if let Err(error) = manager.run_queue().await {
                emit(
                    &manager.inner.events,
                    "download.error",
                    &json!({"message": error.to_string()}),
                );
            }
        });
    }

    async fn run_queue(&self) -> Result<()> {
        // Waiting instead of try_lock avoids losing an add/resume racing with an empty runner.
        let _runner = self.inner.runner.lock().await;
        loop {
            let Some(candidate) = self.jobs()?.into_iter().find(|j| j.status == "queued") else {
                break;
            };
            let mut job = self.change(&candidate.id, |job| {
                if job.status == "queued" {
                    job.status = "running".to_owned();
                }
                Ok(())
            })?;
            if job.status != "running" {
                continue;
            }
            *self.inner.active.lock().await = Some(job.id.clone());
            self.announce(&job);
            let result = self.download(&mut job).await;
            *self.inner.active.lock().await = None;
            let persisted = self.job(&job.id)?;
            if persisted.status == "cancelled" {
                remove_staging(&self.staging_dir(&job.id))?;
            } else if matches!(persisted.status.as_str(), "paused" | "queued") {
                // A control action has priority over the worker's in-memory status.
            } else if let Err(error) = result {
                let persisted = self.change(&job.id, |persisted| {
                    if !matches!(persisted.status.as_str(), "paused" | "queued" | "cancelled") {
                        persisted.status = "failed".to_owned();
                        persisted.error = Some(error.to_string());
                        persisted.bytes_per_second = 0.0;
                        persisted.eta_seconds = None;
                    }
                    Ok(())
                })?;
                self.announce(&persisted);
            }
        }
        Ok(())
    }

    fn interrupted(&self, id: &str) -> Result<bool> {
        Ok(!matches!(
            self.job(id)?.status.as_str(),
            "running" | "verifying"
        ))
    }

    async fn download(&self, job: &mut DownloadJob) -> Result<()> {
        if manifest_hash(&job.files)? != job.manifest_sha256 {
            bail!("The download manifest has been modified");
        }
        if job.destination.is_dir() {
            verify_directory(&job.destination, &job.files)?;
            self.complete(job)?;
            return Ok(());
        }
        let staging = self.staging_dir(&job.id);
        fs::create_dir_all(&staging)?;
        let remaining = job
            .files
            .iter()
            .map(|f| f.size.saturating_sub(f.downloaded))
            .sum::<u64>();
        if fs2::available_space(&staging)? < remaining {
            bail!("Insufficient disk space: {remaining} bytes remain to be downloaded");
        }
        job.replayed_bytes = 0;
        job.transport = "http".to_owned();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(90))
            .build()?;
        let started = Instant::now();
        let mut transferred = 0u64;
        let mut last_event = Instant::now() - Duration::from_secs(1);
        for index in 0..job.files.len() {
            if self.interrupted(&job.id)? {
                return Ok(());
            }
            let spec = job.files[index].clone();
            let path = staging.join(&spec.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            let actual = file.metadata()?.len();
            let checkpoint = spec.downloaded.min(actual).min(spec.size);
            let checkpoint = if checkpoint == spec.size {
                checkpoint
            } else {
                checkpoint / self.inner.range_bytes * self.inner.range_bytes
            };
            file.set_len(checkpoint)?;
            file.seek(SeekFrom::Start(checkpoint))?;
            job.files[index].downloaded = checkpoint;
            job.files[index].verified = false;
            self.checkpoint(job)?;
            if checkpoint < spec.size {
                let url = self.file_url(&job.repo_id, &job.revision, &spec.path)?;
                let mut request = client
                    .get(url)
                    .header(
                        reqwest::header::RANGE,
                        format!("bytes={}-{}", checkpoint, spec.size - 1),
                    )
                    .header(reqwest::header::ACCEPT_ENCODING, "identity");
                if let Some(token) = ambient_token() {
                    request = request.bearer_auth(token);
                }
                let sending = request.send();
                tokio::pin!(sending);
                let response = loop {
                    tokio::select! {
                        response = &mut sending => break response.map_err(|error| anyhow::Error::new(error.without_url()))?,
                        _ = self.inner.control.notified() => { if self.interrupted(&job.id)? { return Ok(()); } },
                        _ = tokio::time::sleep(Duration::from_secs(1)) => { if self.interrupted(&job.id)? { return Ok(()); } },
                    }
                };
                let replay = validate_http_response(
                    response.status(),
                    response.headers(),
                    checkpoint,
                    spec.size,
                )?;
                let etag_key = format!("{}:{}", job.id, spec.path);
                let etag = response
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                if let Some(saved) = self
                    .inner
                    .store
                    .get::<String>("download-etags", &etag_key)?
                {
                    if etag.as_deref() != Some(saved.as_str()) {
                        bail!(
                            "ETag changed for {}; resume refused without modifying the saved bytes",
                            spec.path
                        );
                    }
                } else if let Some(etag) = &etag {
                    self.inner.store.set("download-etags", &etag_key, etag)?;
                } else if replay {
                    bail!(
                        "Full HTTP response has no ETag for {}; unsafe resume refused",
                        spec.path
                    );
                }
                let mut prefix = fs::File::open(&path)?;
                let mut skip = if replay { checkpoint } else { 0 };
                let mut position = checkpoint;
                let mut stream = response.bytes_stream();
                job.transport = if skip > 0 { "http_replay" } else { "http" }.to_owned();
                loop {
                    let chunk = tokio::select! {
                        chunk = stream.next() => chunk,
                        _ = self.inner.control.notified() => {
                            if self.interrupted(&job.id)? { return Ok(()); }
                            continue;
                        },
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {
                            if self.interrupted(&job.id)? { return Ok(()); }
                            continue;
                        },
                    };
                    let Some(chunk) = chunk else {
                        break;
                    };
                    let chunk = chunk.map_err(|error| anyhow::Error::new(error.without_url()))?;
                    transferred = transferred.saturating_add(chunk.len() as u64);
                    let mut bytes = chunk.as_ref();
                    if skip > 0 {
                        let count = skip.min(bytes.len() as u64) as usize;
                        let mut saved = vec![0; count];
                        prefix.read_exact(&mut saved)?;
                        if saved != bytes[..count] {
                            bail!(
                                "HTTP prefix mismatch for {}; resume refused, no new bytes saved",
                                spec.path
                            );
                        }
                        skip -= count as u64;
                        job.replayed_bytes = job.replayed_bytes.saturating_add(count as u64);
                        bytes = &bytes[count..];
                    }
                    if skip == 0 {
                        job.transport = "http".to_owned();
                    }
                    if position.saturating_add(bytes.len() as u64) > spec.size {
                        bail!(
                            "HTTP response is too long for {}; publication refused",
                            spec.path
                        );
                    }
                    while !bytes.is_empty() {
                        let until_checkpoint =
                            self.inner.range_bytes - position % self.inner.range_bytes;
                        let count = until_checkpoint.min(bytes.len() as u64) as usize;
                        file.write_all(&bytes[..count])?;
                        position += count as u64;
                        bytes = &bytes[count..];
                        if position % self.inner.range_bytes == 0 || position == spec.size {
                            file.sync_data()?;
                            job.files[index].downloaded = position;
                            self.checkpoint(job)?;
                            if self.interrupted(&job.id)? {
                                return Ok(());
                            }
                        }
                    }
                    if last_event.elapsed() >= Duration::from_millis(200) {
                        self.progress(
                            job,
                            position - job.files[index].downloaded,
                            transferred,
                            started.elapsed(),
                        );
                        last_event = Instant::now();
                    }
                }
                if skip != 0 || position != spec.size {
                    bail!(
                        "Incomplete HTTP response for {}; resume is available from the last checkpoint",
                        spec.path
                    );
                }
                file.sync_data()?;
                job.files[index].downloaded = position;
                self.checkpoint(job)?;
                self.progress(job, 0, transferred, started.elapsed());
            }
            drop(file);
            if let Err(error) = verify_file(&path, &spec) {
                fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)?
                    .sync_all()?;
                job.files[index].downloaded = 0;
                self.checkpoint(job)?;
                return Err(error);
            }
            job.files[index].verified = true;
            self.checkpoint(job)?;
        }
        if self.interrupted(&job.id)? {
            return Ok(());
        }
        job.status = "verifying".to_owned();
        self.checkpoint(job)?;
        self.announce(job);
        self.publish(job, &staging)
    }

    fn complete(&self, job: &mut DownloadJob) -> Result<()> {
        let _guard = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("Download state is unavailable"))?;
        if self.interrupted(&job.id)? {
            return Ok(());
        }
        self.complete_inner(job)
    }

    fn publish(&self, job: &mut DownloadJob, staging: &Path) -> Result<()> {
        // Controls and publication share this lock: a completed publication wins cleanly over a
        // later cancel, and an earlier pause/cancel prevents publication entirely.
        let _guard = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("Download state is unavailable"))?;
        if self.interrupted(&job.id)? {
            return Ok(());
        }
        if let Some(parent) = job.destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(staging, &job.destination).context("Could not atomically publish the model")?;
        if let Some(parent) = job.destination.parent() {
            if let Ok(directory) = fs::File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        self.complete_inner(job)
    }

    fn complete_inner(&self, job: &mut DownloadJob) -> Result<()> {
        let model = models::register_download(
            &self.inner.store,
            &job.destination,
            &job.repo_id,
            &job.revision,
        )?;
        job.status = "completed".to_owned();
        job.downloaded_bytes = job.total_bytes;
        job.bytes_per_second = 0.0;
        job.eta_seconds = Some(0);
        job.error = None;
        for file in &mut job.files {
            file.downloaded = file.size;
            file.verified = true;
        }
        self.save(job)?;
        self.announce(job);
        emit(&self.inner.events, "models.updated", &model);
        Ok(())
    }

    fn checkpoint(&self, job: &mut DownloadJob) -> Result<()> {
        job.downloaded_bytes = job.files.iter().map(|f| f.downloaded).sum();
        let mut next = job.clone();
        let updated = self.change(&job.id, |persisted| {
            if matches!(persisted.status.as_str(), "paused" | "cancelled" | "queued") {
                next.status = persisted.status.clone();
                next.bytes_per_second = 0.0;
                next.eta_seconds = None;
            }
            *persisted = next;
            Ok(())
        })?;
        *job = updated;
        Ok(())
    }

    fn progress(&self, job: &DownloadJob, in_flight: u64, transferred: u64, elapsed: Duration) {
        // The top-level counters are a live view; per-file offsets remain durable checkpoints.
        // Persisting the view also serves CLI polling and GUI snapshots between 64 MiB checkpoints.
        let result = self.change(&job.id, |progress| {
            if matches!(progress.status.as_str(), "running" | "verifying") {
                progress.downloaded_bytes = job
                    .files
                    .iter()
                    .map(|f| f.downloaded)
                    .sum::<u64>()
                    .saturating_add(in_flight)
                    .min(progress.total_bytes);
                progress.replayed_bytes = job.replayed_bytes;
                progress.transport = job.transport.clone();
                progress.bytes_per_second = transferred as f64 / elapsed.as_secs_f64().max(0.001);
                progress.eta_seconds =
                    if progress.bytes_per_second > 0.0 && job.transport != "http_replay" {
                        Some(
                            ((progress.total_bytes - progress.downloaded_bytes) as f64
                                / progress.bytes_per_second)
                                .ceil() as u64,
                        )
                    } else {
                        None
                    };
            }
            Ok(())
        });
        if let Ok(progress) = result {
            self.announce(&progress);
        }
    }

    fn staging_root(&self) -> PathBuf {
        self.inner
            .paths
            .models_dir
            .parent()
            .unwrap_or(&self.inner.paths.models_dir)
            .join(".downloads")
    }
    fn staging_dir(&self, id: &str) -> PathBuf {
        self.staging_root().join(id)
    }

    fn file_url(&self, repo_id: &str, revision: &str, filename: &str) -> Result<reqwest::Url> {
        let mut url = self.api_url(&[])?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Invalid endpoint"))?;
            for part in repo_id.split('/') {
                path.push(part);
            }
            path.push("resolve").push(revision);
            for part in filename.split('/') {
                path.push(part);
            }
        }
        Ok(url)
    }

    fn api_url(&self, components: &[&str]) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.inner.endpoint)?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| anyhow!("Invalid Hugging Face endpoint"))?;
        segments.pop_if_empty();
        for component in components {
            segments.push(component);
        }
        drop(segments);
        Ok(url)
    }

    async fn metadata(&self, repo_id: &str, revision: &str) -> Result<Value> {
        if revision.is_empty() || revision.len() > 256 {
            bail!("Invalid revision");
        }
        let mut url = self.api_url(&["api", "models"])?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Invalid endpoint"))?;
            for part in repo_id.split('/') {
                path.push(part);
            }
            path.push("revision").push(revision);
        }
        url.query_pairs_mut().append_pair("blobs", "true");
        self.get_json(url).await
    }

    async fn check_compatibility(&self, repo_id: &str, info: &Value) -> Result<Compatibility> {
        if let Some(reason) = repository_incompatibility(info) {
            return Ok(Compatibility::new(info, "incompatible", Some(reason)));
        }
        let Some(revision) = immutable_revision(info) else {
            return Ok(Compatibility::new(
                info,
                "unknown",
                Some("Hugging Face did not provide an immutable model revision".into()),
            ));
        };
        let files = match build_manifest(info) {
            Ok(files) => files,
            Err(error) => {
                return Ok(Compatibility::new(
                    info,
                    "incompatible",
                    Some(error.to_string()),
                ));
            }
        };
        // Inspect only small JSON assets at the exact revision advertised to the user. A network
        // or gated-repository error is returned to the caller, never labelled incompatibility.
        let config = self.model_json(repo_id, revision, "config.json").await?;
        if let Some(reason) = config_incompatibility(&config) {
            return Ok(Compatibility::new(info, "incompatible", Some(reason)));
        }
        if !mlx_evidence(info, Some(&config)) {
            return Ok(Compatibility::new(info, "incompatible", Some("This repository does not declare MLX weights or native MLX quantization; its name alone does not establish compatibility".into())));
        }
        if files
            .iter()
            .any(|file| file.path == "model.safetensors.index.json")
        {
            let index = self
                .model_json(repo_id, revision, "model.safetensors.index.json")
                .await?;
            if let Some(reason) = index_incompatibility(&index, &files) {
                return Ok(Compatibility::new(info, "incompatible", Some(reason)));
            }
        }
        Ok(Compatibility::new(info, "compatible", None))
    }

    async fn model_json(&self, repo_id: &str, revision: &str, filename: &str) -> Result<Value> {
        let response = self
            .get_response(self.file_url(repo_id, revision, filename)?)
            .await?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_MODEL_JSON_BYTES as u64)
        {
            bail!("Model metadata {filename} exceeds the 4 MiB inspection limit");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                anyhow::Error::new(error.without_url()).context("Could not read model metadata")
            })?;
            if bytes.len().saturating_add(chunk.len()) > MAX_MODEL_JSON_BYTES {
                bail!("Model metadata {filename} exceeds the 4 MiB inspection limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid model metadata in {filename}"))
    }

    async fn get_json(&self, url: reqwest::Url) -> Result<Value> {
        self.get_response(url)
            .await?
            .json()
            .await
            .context("Invalid Hugging Face JSON response")
    }

    async fn get_response(&self, url: reqwest::Url) -> Result<reqwest::Response> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(45))
            .user_agent("locals-studio/0.1")
            .build()?;
        let mut request = client.get(url);
        if let Some(token) = ambient_token() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            // Keep the TLS/connect/proxy cause for diagnostics while removing signed URLs or
            // sensitive query parameters. Authentication headers are never formatted or stored.
            .map_err(|error| {
                anyhow::Error::new(error.without_url())
                    .context("Hugging Face is unreachable (TLS connection, proxy, or network)")
            })?;
        match response.status().as_u16() {
            401 | 403 => {
                bail!("Hugging Face denied access; check your authentication and the model's terms")
            }
            404 => bail!("Model or revision not found on Hugging Face"),
            429 => bail!("Hugging Face rate limit reached; try again later"),
            status if !(200..300).contains(&status) => {
                bail!("Hugging Face returned status {status}")
            }
            _ => {}
        }
        Ok(response)
    }
}

async fn cached_publisher_logo<F, Fut>(
    store: &Store,
    publisher: &str,
    timestamp: u64,
    fetch: F,
) -> Result<Option<String>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let cached = store.get::<CachedLogo>(LOGOS, publisher)?;
    if let Some(cached) = &cached {
        if cached.expires_at > timestamp {
            return Ok(cached.data_url.clone());
        }
    }
    let (data_url, ttl) = match fetch().await {
        Ok(data_url) => (Some(data_url), LOGO_TTL),
        // Missing avatars and temporary network errors are decorative failures. Retain a
        // previously downloaded logo offline, and back off before trying again.
        Err(_) => (cached.and_then(|cached| cached.data_url), LOGO_RETRY_TTL),
    };
    store.set(
        LOGOS,
        publisher,
        &CachedLogo {
            data_url: data_url.clone(),
            expires_at: timestamp.saturating_add(ttl),
        },
    )?;
    Ok(data_url)
}

fn allowed_avatar_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("huggingface.co" | "cdn-avatars.huggingface.co")
        )
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
}

async fn fetch_publisher_logo(publisher: &str) -> Result<String> {
    // This namespace endpoint covers both user and organization avatars, as documented
    // in Hugging Face's OpenAPI specification. Never inspect model-card image URLs.
    let mut url = reqwest::Url::parse("https://huggingface.co/api/avatars/")?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("Invalid publisher avatar endpoint"))?
        .pop_if_empty()
        .push(publisher);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("locals-studio/0.1")
        .build()?;
    for _ in 0..4 {
        if !allowed_avatar_url(&url) {
            bail!("Publisher avatar URL is outside Hugging Face");
        }
        // Publisher avatars are public. Do not forward ambient Hub authentication.
        let response = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Publisher avatar redirect has no destination")?
                .to_str()?;
            url = url.join(location)?;
            continue;
        }
        let response = response.error_for_status()?;
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            bail!("Publisher avatar is not a supported raster image");
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_LOGO_BYTES as u64)
        {
            bail!("Publisher avatar is too large");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if bytes.len().saturating_add(chunk.len()) > MAX_LOGO_BYTES {
                bail!("Publisher avatar is too large");
            }
            bytes.extend_from_slice(&chunk);
        }
        return avatar_data_url(&mime, &bytes);
    }
    bail!("Publisher avatar redirected too many times")
}

fn avatar_data_url(mime: &str, bytes: &[u8]) -> Result<String> {
    let valid = match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    };
    if !valid || bytes.len() > MAX_LOGO_BYTES {
        bail!("Invalid publisher avatar image");
    }
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Sizes describe this returned revision, never the parameter count or the repository's history.
/// A partial sum is a lower bound; absence of any known file size stays null rather than zero.
fn attach_size_metadata(info: &mut Value) {
    let Some(object) = info.as_object_mut() else {
        return;
    };
    let siblings = object.get("siblings").and_then(Value::as_array);
    let weights = file_sizes(siblings, |path| path.ends_with(".safetensors"));
    let download = file_sizes(siblings, model_file);
    let repository = file_sizes(siblings, |_| true);
    let revision = object
        .get("sha")
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let size = json!({
        "revision":revision,
        "weights_bytes":weights.0,
        "weights_complete":weights.1,
        "download_bytes":download.0,
        "download_complete":download.1,
        "repository_bytes":repository.0,
        "repository_complete":repository.1,
        // usedStorage is Hub storage accounting, not necessarily this revision's download size.
        "hub_storage_bytes":object.get("usedStorage").and_then(Value::as_u64),
    });
    object.insert("studio_size".into(), size);
}

fn file_sizes(
    siblings: Option<&Vec<Value>>,
    include: impl Fn(&str) -> bool,
) -> (Option<u64>, bool) {
    let Some(siblings) = siblings else {
        return (None, false);
    };
    let mut files = std::collections::BTreeMap::new();
    let mut complete = true;
    for sibling in siblings {
        let Some(path) = sibling.get("rfilename").and_then(Value::as_str) else {
            complete = false;
            continue;
        };
        if !models::safe_relative(path) {
            complete = false;
            continue;
        }
        if include(path) {
            // Match the downloader's accepted files and byte lengths, without counting duplicate
            // sibling entries twice. No model-file contents are fetched for discovery.
            files.entry(path).or_insert_with(|| {
                sibling
                    .get("size")
                    .and_then(Value::as_u64)
                    .or_else(|| sibling.pointer("/lfs/size").and_then(Value::as_u64))
            });
        }
    }
    if files.is_empty() {
        return (None, false);
    }
    let mut bytes = 0u64;
    let mut known = false;
    for size in files.values() {
        match size {
            Some(size) => {
                let Some(sum) = bytes.checked_add(*size) else {
                    return (None, false);
                };
                bytes = sum;
                known = true;
            }
            None => complete = false,
        }
    }
    (known.then_some(bytes), complete && known)
}

fn immutable_revision(info: &Value) -> Option<&str> {
    info.get("sha")
        .and_then(Value::as_str)
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn mlx_evidence(info: &Value, config: Option<&Value>) -> bool {
    info["library_name"].as_str() == Some("mlx")
        || info["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("mlx")))
        || config.is_some_and(|config| native_mlx_quantization(config).is_some())
}

fn native_mlx_quantization(config: &Value) -> Option<&Value> {
    let quantization = config
        .get("quantization")
        .filter(|value| !value.is_null())?;
    (quantization["bits"].as_u64().is_some_and(|bits| bits > 0)
        && quantization["group_size"]
            .as_u64()
            .is_some_and(|size| size > 0)
        && quantization["quant_method"]
            .as_str()
            .is_none_or(|method| method == "mlx"))
    .then_some(quantization)
}

fn discovery_compatibility(info: &Value) -> Compatibility {
    match repository_incompatibility(info) {
        Some(reason) => Compatibility::new(info, "incompatible", Some(reason)),
        None => Compatibility::new(info, "unknown", Some("Checking the complete model files and supported MLX configuration at this revision".into())),
    }
}

fn root_weights(path: &str) -> bool {
    !path.contains('/') && path.starts_with("model") && path.ends_with(".safetensors")
}

fn repository_incompatibility(info: &Value) -> Option<String> {
    if info["disabled"].as_bool() == Some(true) {
        return Some("This repository is disabled on Hugging Face".into());
    }
    if info["library_name"].as_str() == Some("sentence-transformers")
        || info["tags"].as_array().is_some_and(|tags| {
            tags.iter().any(|tag| {
                matches!(
                    tag.as_str(),
                    Some(
                        "sentence-transformers"
                            | "feature-extraction"
                            | "text-embeddings-inference"
                    )
                )
            })
        })
    {
        return Some("Embedding models are not supported by Locals text generation".into());
    }
    // Multimodal Gemma/Qwen repositories can expose a supported text decoder. In particular,
    // transformers/any-to-any metadata is not evidence that their MLX conversion is unusable.
    if let Some(task) = info["pipeline_tag"]
        .as_str()
        .filter(|task| !task.is_empty())
    {
        if !matches!(
            task,
            "text-generation" | "conversational" | "image-text-to-text" | "any-to-any"
        ) {
            return Some(format!(
                "The {task} task is not supported by Locals text generation"
            ));
        }
    }
    let siblings = info["siblings"].as_array()?;
    let paths: HashSet<&str> = siblings
        .iter()
        .filter_map(|file| file["rfilename"].as_str())
        .collect();
    if paths
        .iter()
        .any(|path| model_file(path) && !models::safe_relative(path))
    {
        return Some("Unsafe file path in the repository".into());
    }
    let mut missing = Vec::new();
    if !paths.contains("config.json") {
        missing.push("root config.json");
    }
    if !paths.iter().any(|path| root_weights(path)) {
        missing.push("root model*.safetensors weights");
    }
    if !paths.contains("tokenizer.json") && !paths.contains("tokenizer.model") {
        missing.push("tokenizer.json or tokenizer.model");
    }
    if !missing.is_empty() {
        return Some(format!(
            "Incomplete MLX model: missing {}",
            missing.join(", ")
        ));
    }
    for path in paths.iter().filter(|path| root_weights(path)) {
        let Some((prefix, total)) = path
            .strip_suffix(".safetensors")
            .and_then(|name| name.rsplit_once("-of-"))
        else {
            continue;
        };
        let Some((stem, index)) = prefix.rsplit_once('-') else {
            continue;
        };
        let (Ok(total), Ok(index)) = (total.parse::<usize>(), index.parse::<usize>()) else {
            continue;
        };
        if total == 0 || total > 100_000 || index == 0 || index > total {
            return Some("Invalid safetensors shard numbering".into());
        }
        // Match numeric indices rather than formatting widths, which differ between publishers.
        let indices: HashSet<usize> = paths
            .iter()
            .filter_map(|candidate| {
                let (left, right) = candidate
                    .strip_suffix(".safetensors")?
                    .rsplit_once("-of-")?;
                let (candidate_stem, number) = left.rsplit_once('-')?;
                (candidate_stem == stem && right.parse::<usize>().ok() == Some(total))
                    .then(|| number.parse().ok())
                    .flatten()
            })
            .collect();
        if indices.len() != total || !(1..=total).all(|index| indices.contains(&index)) {
            return Some("Incomplete MLX model: one or more safetensors shards are missing".into());
        }
    }
    None
}

fn config_incompatibility(config: &Value) -> Option<String> {
    if config.get("model_file").is_some_and(|file| !file.is_null()) {
        return Some("This model requires custom Python architecture code, which the managed Locals runtime does not enable".into());
    }
    let architectures: Vec<&str> = match config.get("architectures") {
        Some(Value::String(architecture)) => vec![architecture.as_str()],
        Some(Value::Array(architectures)) => {
            architectures.iter().filter_map(Value::as_str).collect()
        }
        _ => Vec::new(),
    };
    if architectures.iter().any(|architecture| {
        [
            "ForSequenceClassification",
            "ForTokenClassification",
            "ForQuestionAnswering",
            "ForMaskedLM",
            "ForEmbedding",
            "ForTextEncoding",
        ]
        .iter()
        .any(|suffix| architecture.ends_with(suffix))
    }) {
        return Some("This configuration defines an embedding, encoder or classification model rather than a text generation model".into());
    }
    let quantization = config.get("quantization").filter(|value| !value.is_null());
    if quantization.is_some() && native_mlx_quantization(config).is_none() {
        return Some(
            "The quantization field is not a native MLX configuration with bits and group_size"
                .into(),
        );
    }
    let legacy = config
        .get("quantization_config")
        .filter(|value| !value.is_null())
        .or_else(|| config.pointer("/text_config/quantization_config"));
    if quantization.is_none() {
        if let Some(legacy) = legacy {
            let Some(method) = legacy["quant_method"].as_str() else {
                return Some("quantization_config does not identify its weight format".into());
            };
            if !matches!(
                method,
                "bitnet" | "mxfp4" | "mxfp8" | "fp8" | "compressed-tensors"
            ) {
                return Some(format!(
                    "The {method} weights are not a native MLX conversion; choose an MLX repository"
                ));
            }
        }
    }
    let Some(model_type) = config["model_type"]
        .as_str()
        .filter(|kind| !kind.is_empty())
    else {
        return Some("config.json does not identify a model_type".into());
    };
    // Loader modules and aliases from the managed, locked mlx-lm==0.32.0 package:
    // mlx_lm/utils.py::_get_classes and mlx_lm/models/*.py. Locals 0.2.1 additionally registers
    // its bundled Flint loader in locals/models/__init__.py. Keep in sync with the runtime lock.
    const SUPPORTED: &str = "Klear afm7 afmoe apertus baichuan_m1 bailing_hybrid bailing_moe bailing_moe_linear bailing_moe_v3 bitnet cohere cohere2 dbrx deepseek deepseek_v2 deepseek_v3 deepseek_v32 deepseek_v41 dots1 ernie4_5 ernie4_5_moe exaone exaone4 exaone_moe falcon_h1 gear gemma gemma2 gemma3 gemma3_text gemma3n gemma4 gemma4_text glm glm4 glm4_moe glm4_moe_lite glm_moe_dsa gpt2 gpt_bigcode gpt_neox gpt_oss gptj granite granitemoe granitemoehybrid helium hunyuan hunyuan_v1_dense internlm2 internlm3 iquestloopcoder jamba kimi_k25 kimi_k3 kimi_linear kimi_vl laguna lfm2-vl lfm2 lfm2_moe lille-130m llama llama4 llama4_text longcat_flash longcat_flash_ngram mamba mamba2 mellum mimo mimo_v2_flash minicpm minicpm3 minimax minimax_m3_vl ministral3 mistral3 mistral4 mixtral muse_glimmer nanbeige nanochat nemotron-nas nemotron nemotron_h olmo olmo2 olmo3 olmo_hybrid olmoe openelm phi phi3 phi3small phimoe phixtral pixtral plamo plamo2 plamo3 qwen qwen2 qwen2_moe qwen2_vl qwen3 qwen3_5 qwen3_5_moe qwen3_moe qwen3_next qwen3_vl qwen3_vl_moe recurrent_gemma rwkv7 seed_oss smollm3 solar_open spark2_5 stablelm starcoder2 step3p5 talkie telechat3 youtu_llm mistral llava phi-msft falcon_mamba joyai_llm_flash kimi_k2 qwen2_5_vl minimax_m2 iquestcoder xverse gemma4_unified deepseek_v41_text";
    if model_type != "flint"
        && !SUPPORTED
            .split_ascii_whitespace()
            .any(|kind| kind == model_type)
    {
        return Some(format!(
            "Model type {model_type} is not supported by the managed mlx-lm 0.32.0 runtime"
        ));
    }
    if !config["text_config"].is_object()
        && (config
            .get("vision_config")
            .is_some_and(|value| !value.is_null())
            || config
                .get("audio_config")
                .is_some_and(|value| !value.is_null()))
    {
        return Some(
            "This multimodal model does not expose a supported text_config for Locals".into(),
        );
    }
    None
}

fn index_incompatibility(index: &Value, files: &[DownloadFile]) -> Option<String> {
    let paths: HashSet<&str> = files
        .iter()
        .filter(|file| file.size > 0 && root_weights(&file.path))
        .map(|file| file.path.as_str())
        .collect();
    let Some(weights) = index["weight_map"]
        .as_object()
        .filter(|weights| !weights.is_empty())
    else {
        return Some("The safetensors index has no valid weight_map".into());
    };
    if weights
        .values()
        .any(|file| file.as_str().is_none_or(|path| !paths.contains(path)))
    {
        return Some("Incomplete MLX model: the safetensors index references missing or unsupported root weights".into());
    }
    None
}

fn build_manifest(info: &Value) -> Result<Vec<DownloadFile>> {
    if let Some(reason) = repository_incompatibility(info) {
        bail!("{reason}");
    }
    let siblings = info
        .get("siblings")
        .and_then(Value::as_array)
        .context("Missing file list")?;
    let mut files = Vec::new();
    for sibling in siblings {
        let Some(path) = sibling.get("rfilename").and_then(Value::as_str) else {
            continue;
        };
        if !model_file(path) {
            continue;
        }
        if !models::safe_relative(path) {
            bail!("Unsafe file path in the repository");
        }
        let size = sibling
            .get("size")
            .and_then(Value::as_u64)
            .or_else(|| sibling.pointer("/lfs/size").and_then(Value::as_u64))
            .with_context(|| format!("Missing size for {path}"))?;
        if size == 0
            && (root_weights(path)
                || matches!(path, "config.json" | "tokenizer.json" | "tokenizer.model"))
        {
            bail!("Incomplete MLX model: {path} is empty");
        }
        let sha256 = sibling
            .pointer("/lfs/sha256")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let git_blob_sha1 = if sibling.get("lfs").is_none_or(Value::is_null) {
            sibling
                .get("blobId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|hash| {
                    hash.len() == 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
        } else {
            None
        };
        if path.ends_with(".safetensors") && sha256.is_none() {
            bail!("Missing SHA-256 hash for weights {path}");
        }
        files.push(DownloadFile {
            path: path.to_owned(),
            size,
            sha256,
            git_blob_sha1,
            downloaded: 0,
            verified: false,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    if !files.iter().any(|f| f.path == "config.json")
        || !files.iter().any(|f| root_weights(&f.path))
        || !files
            .iter()
            .any(|f| f.path == "tokenizer.json" || f.path == "tokenizer.model")
    {
        bail!(
            "The repository does not contain a complete MLX model (config, safetensors weights, and tokenizer required)"
        );
    }
    Ok(files)
}

fn model_file(path: &str) -> bool {
    // Download inference assets, not GGUF/PyTorch duplicates, images, demos or source code.
    path.ends_with(".safetensors")
        || path.ends_with(".json")
        || path.ends_with(".jinja")
        || path.ends_with(".jinja2")
        || path.ends_with(".tiktoken")
        || path.rsplit('/').next().is_some_and(|name| {
            matches!(
                name,
                "tokenizer.model"
                    | "sentencepiece.bpe.model"
                    | "vocab.txt"
                    | "merges.txt"
                    | "LICENSE"
                    | "LICENSE.txt"
                    | "NOTICE"
            )
        })
}

fn manifest_hash(files: &[DownloadFile]) -> Result<String> {
    if files.iter().any(|file| file.git_blob_sha1.is_some()) {
        let immutable: Vec<_> = files
            .iter()
            .map(|file| (&file.path, file.size, &file.sha256, &file.git_blob_sha1))
            .collect();
        return Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&immutable)?)
        ));
    }
    // Retain the existing manifest representation for queued jobs written before Git hashes
    // were tracked; absent optional metadata must not invalidate their durable checkpoints.
    let immutable: Vec<_> = files.iter().map(|f| (&f.path, f.size, &f.sha256)).collect();
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&immutable)?)
    ))
}

fn verify_directory(root: &Path, files: &[DownloadFile]) -> Result<()> {
    for spec in files {
        verify_file(&root.join(&spec.path), spec)?;
    }
    Ok(())
}

fn verify_file(path: &Path, spec: &DownloadFile) -> Result<()> {
    let mut file = fs::File::open(path).with_context(|| format!("Missing file: {}", spec.path))?;
    if file.metadata()?.len() != spec.size {
        bail!("Incorrect size for {}", spec.path);
    }
    if spec.sha256.is_some() || spec.git_blob_sha1.is_some() {
        let mut hash = Sha256::new();
        let mut git_hash = sha1::Sha1::new();
        if spec.git_blob_sha1.is_some() {
            git_hash.update(format!("blob {}\0", spec.size).as_bytes());
        }
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            if spec.git_blob_sha1.is_some() {
                git_hash.update(&buffer[..count]);
            }
        }
        if spec.sha256.as_ref().is_some_and(|expected| {
            format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase()
        }) {
            bail!(
                "SHA-256 verification failed for {}; remove the download and start again",
                spec.path
            );
        }
        if spec.git_blob_sha1.as_ref().is_some_and(|expected| {
            format!("{:x}", git_hash.finalize()) != expected.to_ascii_lowercase()
        }) {
            bail!(
                "Git blob SHA-1 verification failed for {}; publication refused",
                spec.path
            );
        }
    }
    Ok(())
}

fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.is_empty()
        || parts.len() > 2
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 96
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        bail!("Invalid Hugging Face identifier (expected author/model)");
    }
    Ok(())
}

/// Returns true when the response starts at byte zero and its saved prefix must be compared.
fn validate_http_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    checkpoint: u64,
    file_size: u64,
) -> Result<bool> {
    if headers
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
    {
        bail!("HTTP encoding is incompatible with an exact resume");
    }
    let length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    match status {
        reqwest::StatusCode::OK => {
            if length != Some(file_size) {
                bail!(
                    "Full HTTP response has an incorrect size; resume refused before writing any bytes"
                );
            }
            Ok(true)
        }
        reqwest::StatusCode::PARTIAL_CONTENT => {
            let expected = format!("bytes {}-{}/{}", checkpoint, file_size - 1, file_size);
            if headers
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                != Some(expected.as_str())
                || length.is_some_and(|length| length != file_size - checkpoint)
            {
                bail!("Content-Range does not match the requested resume; no new bytes saved");
            }
            Ok(false)
        }
        _ => bail!(
            "HTTP download failed (status {}); resume is available",
            status.as_u16()
        ),
    }
}

fn ambient_token() -> Option<String> {
    if std::env::var("HF_HUB_DISABLE_IMPLICIT_TOKEN")
        .ok()
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
    {
        return None;
    }
    if let Ok(token) = std::env::var("HF_TOKEN") {
        if !token.trim().is_empty() {
            return Some(token.trim().to_owned());
        }
    }
    let token_path = std::env::var_os("HF_TOKEN_PATH")
        .map(PathBuf::from)
        .or_else(|| models::huggingface_home().map(|path| path.join("token")));
    token_path
        .and_then(|path| fs::read_to_string(path).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn remove_staging(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_logos_accept_only_official_https_origins_and_bounded_raster_images() {
        for url in [
            "https://huggingface.co/api/avatars/mlx-community",
            "https://cdn-avatars.huggingface.co/v1/production/uploads/avatar.png",
        ] {
            assert!(allowed_avatar_url(&url.parse().unwrap()));
        }
        for url in [
            "http://huggingface.co/avatar.png",
            "https://huggingface.co.evil.example/avatar.png",
            "https://example.com/avatar.png",
            "https://localhost/avatar.png",
            "https://user:password@huggingface.co/avatar.png",
            "https://huggingface.co:444/avatar.png",
        ] {
            assert!(!allowed_avatar_url(&url.parse().unwrap()), "{url}");
        }
        let png = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aBz8AAAAASUVORK5CYII=")
            .unwrap();
        assert!(
            avatar_data_url("image/png", &png)
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(avatar_data_url("image/png", b"<html>not an image</html>").is_err());
        assert!(avatar_data_url("image/svg+xml", b"<svg></svg>").is_err());
        assert!(avatar_data_url("text/html", &png).is_err());
        let mut oversized = png;
        oversized.resize(MAX_LOGO_BYTES + 1, 0);
        assert!(avatar_data_url("image/png", &oversized).is_err());
    }

    #[tokio::test]
    async fn publisher_logo_cache_survives_reopening_and_reuses_the_publishers_image() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("logos.sqlite3");
        let calls = AtomicUsize::new(0);
        let data = "data:image/png;base64,cached";
        let store = Store::open(&path).unwrap();
        let first = cached_publisher_logo(&store, "publisher", 100, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(data.to_owned())
        })
        .await
        .unwrap();
        assert_eq!(first.as_deref(), Some(data));
        drop(store);
        let store = Store::open(&path).unwrap();
        let second = cached_publisher_logo(&store, "publisher", 101, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected second fetch".to_owned())
        })
        .await
        .unwrap();
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn publisher_logo_failures_back_off_and_keep_previously_cached_images_offline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("logos.sqlite3")).unwrap();
        let calls = AtomicUsize::new(0);
        let missing = cached_publisher_logo(&store, "publisher", 100, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            bail!("offline")
        })
        .await
        .unwrap();
        assert!(missing.is_none());
        let still_missing = cached_publisher_logo(&store, "publisher", 101, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("unexpected fetch".to_owned())
        })
        .await
        .unwrap();
        assert!(still_missing.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let recovered_at = 100 + LOGO_RETRY_TTL;
        let recovered = cached_publisher_logo(&store, "publisher", recovered_at, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("data:image/png;base64,recovered".to_owned())
        })
        .await
        .unwrap();
        let offline =
            cached_publisher_logo(&store, "publisher", recovered_at + LOGO_TTL, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                bail!("offline again")
            })
            .await
            .unwrap();
        assert_eq!(offline, recovered);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{Request, Response, StatusCode},
    };
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicBool, AtomicU16, Ordering},
    };

    struct FakeHub {
        files: BTreeMap<String, Vec<u8>>,
        ranges: std::sync::Mutex<Vec<(String, u64, u64)>>,
        metadata_requests: std::sync::Mutex<Vec<String>>,
        file_requests: std::sync::Mutex<Vec<String>>,
        metadata_override: Option<Value>,
        search_models: Option<Vec<Value>>,
        config_status: AtomicU16,
        slow: AtomicBool,
        corrupt: AtomicBool,
        redirect_weights: AtomicBool,
        wrong_size: AtomicBool,
        ignore_ranges: AtomicBool,
    }

    impl FakeHub {
        fn fixture() -> Self {
            Self {
                files: BTreeMap::from([
                    (
                        "config.json".into(),
                        br#"{"model_type":"qwen2","quantization":{"bits":4,"group_size":64}}"#
                            .to_vec(),
                    ),
                    ("tokenizer.json".into(), b"{}".to_vec()),
                    ("model.safetensors".into(), (0u8..64).collect()),
                ]),
                ranges: std::sync::Mutex::new(Vec::new()),
                metadata_requests: std::sync::Mutex::new(Vec::new()),
                file_requests: std::sync::Mutex::new(Vec::new()),
                metadata_override: None,
                search_models: None,
                config_status: AtomicU16::new(0),
                slow: AtomicBool::new(false),
                corrupt: AtomicBool::new(false),
                redirect_weights: AtomicBool::new(false),
                wrong_size: AtomicBool::new(false),
                ignore_ranges: AtomicBool::new(false),
            }
        }

        fn metadata(&self) -> Value {
            if let Some(metadata) = &self.metadata_override {
                return metadata.clone();
            }
            json!({"library_name":"mlx","pipeline_tag":"text-generation","sha": "0123456789abcdef0123456789abcdef01234567", "siblings": self.files.iter().map(|(path, bytes)| {
                json!({"rfilename":path, "size":bytes.len(), "lfs":{"sha256":format!("{:x}", Sha256::digest(bytes)), "size":bytes.len()}})
            }).collect::<Vec<_>>()})
        }
    }

    async fn fake_response(
        State(state): State<Arc<FakeHub>>,
        request: Request<Body>,
    ) -> Response<Body> {
        if request.uri().path() == "/api/models" || request.uri().path().starts_with("/api/models/")
        {
            state
                .metadata_requests
                .lock()
                .unwrap()
                .push(request.uri().to_string());
            let metadata = if request.uri().path() == "/api/models" {
                // Like the real listing endpoint, return file names but no file sizes.
                state.search_models.as_ref().map(|models| json!(models)).unwrap_or_else(|| json!([{
                    "id":"org/model",
                    "library_name":"mlx",
                    "pipeline_tag":"text-generation",
                    "sha":"0123456789abcdef0123456789abcdef01234567",
                    "downloads":42,
                    "siblings":state.files.keys().map(|path| json!({"rfilename":path})).collect::<Vec<_>>()
                }]))
            } else {
                state.metadata()
            };
            return Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(metadata.to_string()))
                .unwrap();
        }
        let name = request.uri().path().rsplit('/').next().unwrap_or("");
        state
            .file_requests
            .lock()
            .unwrap()
            .push(request.uri().path().to_owned());
        if name == "config.json" && state.config_status.load(Ordering::Relaxed) != 0 {
            return Response::builder()
                .status(state.config_status.load(Ordering::Relaxed))
                .body(Body::empty())
                .unwrap();
        }
        let Some(bytes) = state.files.get(name) else {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        };
        let etag = format!("\"{:x}\"", Sha256::digest(bytes));
        let builder = Response::builder()
            .header("etag", etag)
            .header("x-repo-commit", "0123456789abcdef0123456789abcdef01234567");
        if name.ends_with(".safetensors")
            && state.redirect_weights.load(Ordering::Relaxed)
            && !request.uri().path().starts_with("/cdn/")
        {
            return builder
                .status(StatusCode::FOUND)
                .header("x-xet-hash", format!("{:x}", Sha256::digest(bytes)))
                .header("x-linked-size", bytes.len())
                .header("location", format!("/cdn/{name}"))
                .body(Body::empty())
                .unwrap();
        }
        if request.method() == "HEAD" {
            return builder
                .header("content-length", bytes.len())
                .body(Body::empty())
                .unwrap();
        }
        let (start, end) = request
            .headers()
            .get("range")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("bytes="))
            .and_then(|h| h.split_once('-'))
            .and_then(|(start, end)| {
                Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()? + 1))
            })
            .unwrap_or((0, bytes.len() as u64));
        if start >= end || end > bytes.len() as u64 {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .body(Body::empty())
                .unwrap();
        }
        state
            .ranges
            .lock()
            .unwrap()
            .push((name.to_owned(), start, end));
        let full = name.ends_with(".safetensors") && state.ignore_ranges.load(Ordering::Relaxed);
        let mut data = if full {
            bytes.clone()
        } else {
            bytes[start as usize..end as usize].to_vec()
        };
        if state.corrupt.load(Ordering::Relaxed) && name.ends_with(".safetensors") {
            data[0] ^= 1;
        }
        if full && state.wrong_size.load(Ordering::Relaxed) {
            data.pop();
        }
        let body_length = data.len();
        let body = if state.slow.load(Ordering::Relaxed) {
            Body::from_stream(futures_util::stream::unfold(
                (data, 0usize),
                |(data, offset)| async move {
                    if offset >= data.len() {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    let end = (offset + 2).min(data.len());
                    Some((
                        Ok::<_, std::io::Error>(data[offset..end].to_vec()),
                        (data, end),
                    ))
                },
            ))
        } else {
            Body::from(data)
        };
        let mut builder = builder
            .status(if full {
                StatusCode::OK
            } else {
                StatusCode::PARTIAL_CONTENT
            })
            .header("content-length", body_length);
        if !full {
            builder = builder.header(
                "content-range",
                format!("bytes {}-{}/{}", start, end - 1, bytes.len()),
            );
        }
        builder.body(body).unwrap()
    }

    async fn fixture_manager(
        root: &Path,
        fake: Arc<FakeHub>,
    ) -> (HubManager, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(fake_response).with_state(fake),
            )
            .await
            .unwrap();
        });
        let paths = AppPaths {
            data_dir: root.join("data"),
            database_path: root.join("data/studio.sqlite3"),
            socket_path: root.join("studio.sock"),
            models_dir: root.join("models"),
            locals_source: root.join("Locals"),
        };
        paths.prepare().unwrap();
        let store = Store::open(&paths.database_path).unwrap();
        let (events, _) = tokio::sync::broadcast::channel(128);
        let mut manager = HubManager::new(paths, store, events);
        let inner = Arc::get_mut(&mut manager.inner).unwrap();
        inner.endpoint = endpoint;
        inner.range_bytes = 8;
        (manager, server)
    }

    async fn wait_status(manager: &HubManager, id: &str, expected: &str) -> DownloadJob {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let job = manager.job(id).unwrap();
                if job.status == expected {
                    return job;
                }
                if job.status == "failed" && expected != "failed" {
                    panic!("{}", job.error.unwrap_or_default());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("download status timeout")
    }

    fn seed_partial(manager: &HubManager, fake: &FakeHub, id: &str, count: usize) -> DownloadJob {
        let mut files = build_manifest(&fake.metadata()).unwrap();
        files
            .iter_mut()
            .find(|file| file.path == "model.safetensors")
            .unwrap()
            .downloaded = count as u64;
        let mut job = DownloadJob {
            id: id.into(),
            repo_id: "org/model".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
            status: "running".into(),
            manifest_sha256: manifest_hash(&files).unwrap(),
            total_bytes: files.iter().map(|file| file.size).sum(),
            downloaded_bytes: count as u64,
            files,
            bytes_per_second: 0.0,
            eta_seconds: None,
            replayed_bytes: 0,
            transport: "http".into(),
            created_at: now(),
            updated_at: now(),
            destination: manager.inner.paths.models_dir.join(id),
            error: None,
        };
        let staging = manager.staging_dir(id);
        fs::create_dir_all(&staging).unwrap();
        fs::write(
            staging.join("model.safetensors"),
            &fake.files["model.safetensors"][..count],
        )
        .unwrap();
        manager.save(&mut job).unwrap();
        job
    }

    #[tokio::test]
    async fn full_http_200_replays_matching_prefix_once_and_publishes_verified_model() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        fake.ignore_ranges.store(true, Ordering::Relaxed);
        fake.redirect_weights.store(true, Ordering::Relaxed);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let job = seed_partial(&manager, &fake, "full-replay", 16);
        manager.recover().await.unwrap();
        let completed = wait_status(&manager, &job.id, "completed").await;
        assert_eq!(completed.replayed_bytes, 16);
        assert_eq!(completed.transport, "http");
        verify_directory(&completed.destination, &completed.files).unwrap();
        let ranges = fake.ranges.lock().unwrap();
        let weights: Vec<_> = ranges
            .iter()
            .filter(|(name, _, _)| name == "model.safetensors")
            .collect();
        assert_eq!(
            weights.len(),
            1,
            "a full response must not restart at every checkpoint"
        );
        assert_eq!(weights[0].1, 16);
        assert_eq!(weights[0].2, 64);
        server.abort();
    }

    #[tokio::test]
    async fn full_http_200_rejects_different_prefix_without_appending() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        fake.ignore_ranges.store(true, Ordering::Relaxed);
        fake.corrupt.store(true, Ordering::Relaxed);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let job = seed_partial(&manager, &fake, "prefix-mismatch", 16);
        manager.recover().await.unwrap();
        let failed = wait_status(&manager, &job.id, "failed").await;
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("HTTP prefix mismatch")
        );
        assert_eq!(
            fs::read(manager.staging_dir(&job.id).join("model.safetensors")).unwrap(),
            fake.files["model.safetensors"][..16]
        );
        assert_eq!(
            failed
                .files
                .iter()
                .find(|file| file.path == "model.safetensors")
                .unwrap()
                .downloaded,
            16
        );
        assert!(!failed.destination.exists());
        server.abort();
    }

    #[tokio::test]
    async fn full_http_200_rejects_wrong_length_and_changed_etag_before_append() {
        for wrong_size in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let fake = Arc::new(FakeHub::fixture());
            fake.ignore_ranges.store(true, Ordering::Relaxed);
            fake.wrong_size.store(wrong_size, Ordering::Relaxed);
            let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
            let job = seed_partial(&manager, &fake, "metadata-mismatch", 16);
            if !wrong_size {
                manager
                    .inner
                    .store
                    .set(
                        "download-etags",
                        &format!("{}:model.safetensors", job.id),
                        &"old-etag",
                    )
                    .unwrap();
            }
            manager.recover().await.unwrap();
            let failed = wait_status(&manager, &job.id, "failed").await;
            let error = failed.error.as_deref().unwrap();
            assert!(
                if wrong_size {
                    error.contains("incorrect size")
                } else {
                    error.contains("ETag changed")
                },
                "{error}"
            );
            assert_eq!(
                fs::metadata(manager.staging_dir(&job.id).join("model.safetensors"))
                    .unwrap()
                    .len(),
                16
            );
            assert!(!failed.destination.exists());
            server.abort();
        }
    }

    #[tokio::test]
    async fn full_http_200_pauses_at_durable_checkpoint_and_resumes_with_verified_replay() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        fake.ignore_ranges.store(true, Ordering::Relaxed);
        fake.slow.store(true, Ordering::Relaxed);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let added = manager.add("org/model", None).await.unwrap();
        let id = added["id"].as_str().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let job = manager.job(id).unwrap();
                if job
                    .files
                    .iter()
                    .any(|file| file.path == "model.safetensors" && file.downloaded >= 8)
                {
                    break;
                }
                assert_ne!(job.status, "failed", "{:?}", job.error);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        manager.pause(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while manager.inner.active.lock().await.is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let paused = manager.job(id).unwrap();
        let offset = paused
            .files
            .iter()
            .find(|file| file.path == "model.safetensors")
            .unwrap()
            .downloaded;
        assert!(offset >= 8 && offset < 64);
        assert_eq!(offset % 8, 0);
        assert!(!paused.destination.exists());
        fake.slow.store(false, Ordering::Relaxed);
        manager.resume(id).await.unwrap();
        let completed = wait_status(&manager, id, "completed").await;
        assert_eq!(completed.replayed_bytes, offset);
        verify_directory(&completed.destination, &completed.files).unwrap();
        server.abort();
    }

    #[test]
    fn partial_http_responses_must_match_offset_and_file_size_exactly() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_RANGE,
            "bytes 16-63/64".parse().unwrap(),
        );
        headers.insert(reqwest::header::CONTENT_LENGTH, "48".parse().unwrap());
        assert!(
            !validate_http_response(reqwest::StatusCode::PARTIAL_CONTENT, &headers, 16, 64)
                .unwrap()
        );
        assert!(
            validate_http_response(reqwest::StatusCode::PARTIAL_CONTENT, &headers, 8, 64).is_err()
        );
        assert!(validate_http_response(reqwest::StatusCode::OK, &headers, 16, 64).is_err());
    }

    #[tokio::test]
    async fn recovery_uses_saved_ranges_and_discards_uncheckpointed_bytes() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let mut files = build_manifest(&fake.metadata()).unwrap();
        let weight_index = files
            .iter()
            .position(|f| f.path == "model.safetensors")
            .unwrap();
        files[weight_index].downloaded = 16;
        let mut job = DownloadJob {
            id: "recover-job".into(),
            repo_id: "org/model".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
            status: "running".into(),
            manifest_sha256: manifest_hash(&files).unwrap(),
            total_bytes: files.iter().map(|f| f.size).sum(),
            downloaded_bytes: 16,
            files,
            bytes_per_second: 0.0,
            eta_seconds: None,
            replayed_bytes: 0,
            transport: "http".to_owned(),
            created_at: now(),
            updated_at: now(),
            destination: manager.inner.paths.models_dir.join("published"),
            error: None,
        };
        let staging = manager.staging_dir(&job.id);
        fs::create_dir_all(&staging).unwrap();
        let mut partial = fake.files["model.safetensors"][..16].to_vec();
        partial.extend_from_slice(b"uncheckpointed garbage");
        fs::write(staging.join("model.safetensors"), partial).unwrap();
        manager.save(&mut job).unwrap();
        manager.recover().await.unwrap();
        let completed = wait_status(&manager, &job.id, "completed").await;
        assert!(completed.destination.join("config.json").is_file());
        assert!(!staging.exists());
        let ranges = fake.ranges.lock().unwrap();
        assert_eq!(
            ranges
                .iter()
                .find(|(name, _, _)| name == "model.safetensors")
                .unwrap()
                .1,
            16
        );
        assert_eq!(
            ranges
                .iter()
                .filter(|(name, _, _)| name == "model.safetensors")
                .count(),
            1,
            "a file uses one continuous response, not one request per checkpoint"
        );
        assert_eq!(models::list(&manager.inner.store).unwrap().len(), 1);
        verify_directory(&completed.destination, &completed.files).unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn pause_resume_and_cancel_control_actual_network_downloads() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        fake.slow.store(true, Ordering::Relaxed);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let added = manager.add("org/model", None).await.unwrap();
        let id = added["id"].as_str().unwrap();
        wait_status(&manager, id, "running").await;
        manager.pause(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while manager.inner.active.lock().await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let paused = manager.job(id).unwrap();
        assert_eq!(paused.status, "paused");
        assert!(!paused.destination.exists());
        assert!(models::list(&manager.inner.store).unwrap().is_empty());
        fake.slow.store(false, Ordering::Relaxed);
        manager.resume(id).await.unwrap();
        wait_status(&manager, id, "completed").await;

        let mut cancelled = manager.job(id).unwrap();
        cancelled.id = "cancel-job".into();
        cancelled.status = "paused".into();
        cancelled.destination = manager.inner.paths.models_dir.join("never-published");
        let stage = manager.staging_dir(&cancelled.id);
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("partial"), b"partial").unwrap();
        manager.save(&mut cancelled).unwrap();
        manager.cancel(&cancelled.id).await.unwrap();
        assert_eq!(manager.job(&cancelled.id).unwrap().status, "cancelled");
        assert!(!stage.exists());
        assert!(!cancelled.destination.exists());
        server.abort();
    }

    #[tokio::test]
    async fn hash_failure_never_publishes_and_resume_redownloads_damaged_file() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        fake.corrupt.store(true, Ordering::Relaxed);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let added = manager.add("org/model", None).await.unwrap();
        let id = added["id"].as_str().unwrap();
        let failed = wait_status(&manager, id, "failed").await;
        assert!(!failed.destination.exists());
        assert!(failed.error.unwrap().contains("SHA-256"));
        assert!(models::list(&manager.inner.store).unwrap().is_empty());
        fake.corrupt.store(false, Ordering::Relaxed);
        manager.resume(id).await.unwrap();
        wait_status(&manager, id, "completed").await;
        server.abort();
    }

    #[test]
    fn discovery_sizes_separate_weights_download_and_revision_files() {
        let mut info = json!({
            "sha":"0123456789abcdef0123456789abcdef01234567",
            "usedStorage":999999,
            "safetensors":{"total":123456789},
            "siblings":[
                {"rfilename":"config.json","size":20},
                {"rfilename":"tokenizer.json","size":2},
                {"rfilename":"chat_template.jinja","size":3},
                {"rfilename":"model-00001.safetensors","size":100},
                {"rfilename":"model-00002.safetensors","lfs":{"size":200}},
                {"rfilename":"model-00002.safetensors","lfs":{"size":200}},
                {"rfilename":"README.md","size":80},
                {"rfilename":"model.gguf","size":10000},
                {"rfilename":"pytorch_model.bin","size":9000}
            ]
        });
        attach_size_metadata(&mut info);
        let size = &info["studio_size"];
        assert_eq!(size["weights_bytes"], 300);
        assert_eq!(size["download_bytes"], 325);
        assert_eq!(size["repository_bytes"], 19405);
        assert_eq!(size["hub_storage_bytes"], 999999);
        assert_eq!(size["revision"], info["sha"]);
        assert_eq!(size["weights_complete"], true);
        assert_eq!(size["download_complete"], true);
        assert_eq!(size["repository_complete"], true);
        assert_eq!(info["safetensors"]["total"], 123456789);
    }

    #[test]
    fn discovery_partial_sizes_and_unknown_sizes_are_not_complete_or_parameter_estimates() {
        let mut partial = json!({"siblings":[
            {"rfilename":"model-00001.safetensors","lfs":{"size":100}},
            {"rfilename":"model-00002.safetensors"},
            {"rfilename":"config.json","size":20},
            {"rfilename":"README.md"}
        ]});
        attach_size_metadata(&mut partial);
        assert_eq!(partial["studio_size"]["weights_bytes"], 100);
        assert_eq!(partial["studio_size"]["download_bytes"], 120);
        assert_eq!(partial["studio_size"]["repository_bytes"], 120);
        for field in [
            "weights_complete",
            "download_complete",
            "repository_complete",
        ] {
            assert_eq!(partial["studio_size"][field], false);
        }

        for mut unknown in [
            json!({"usedStorage":9000,"safetensors":{"total":1234}}),
            json!({"sha":"main","siblings":[{"rfilename":"model.safetensors"}]}),
            json!({"siblings":[]}),
        ] {
            attach_size_metadata(&mut unknown);
            for field in [
                "weights_bytes",
                "download_bytes",
                "repository_bytes",
                "revision",
            ] {
                assert!(unknown["studio_size"][field].is_null(), "{field}");
            }
            assert_eq!(unknown["studio_size"]["weights_complete"], false);
            assert_eq!(unknown["studio_size"]["download_complete"], false);
        }
    }

    #[test]
    fn discovery_size_overflow_or_unsafe_entries_never_become_complete() {
        let mut overflow = json!({"siblings":[
            {"rfilename":"first.safetensors","size":u64::MAX},
            {"rfilename":"second.safetensors","size":1}
        ]});
        attach_size_metadata(&mut overflow);
        assert!(overflow["studio_size"]["weights_bytes"].is_null());
        assert_eq!(overflow["studio_size"]["weights_complete"], false);
        let mut unsafe_entry = json!({"siblings":[
            {"rfilename":"model.safetensors","size":100},
            {"rfilename":"../unsafe.safetensors","size":999}
        ]});
        attach_size_metadata(&mut unsafe_entry);
        assert_eq!(unsafe_entry["studio_size"]["weights_bytes"], 100);
        assert_eq!(unsafe_entry["studio_size"]["weights_complete"], false);
    }

    #[tokio::test]
    async fn discovery_search_is_one_metadata_request_and_info_pins_the_displayed_revision() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let results = manager.search(" model ").await.unwrap();
        assert_eq!(results[0]["id"], "org/model");
        assert_eq!(results[0]["downloads"], 42);
        assert!(results[0]["studio_size"]["download_bytes"].is_null());
        assert_eq!(results[0]["studio_size"]["download_complete"], false);
        {
            let requests = fake.metadata_requests.lock().unwrap();
            assert_eq!(requests.len(), 1, "search must not fetch every repository");
            let query: BTreeMap<_, _> =
                url::form_urlencoded::parse(requests[0].split_once('?').unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            assert_eq!(query.get("full").map(String::as_str), Some("true"));
            assert_eq!(query.get("search").map(String::as_str), Some("model"));
            assert_eq!(query.get("limit").map(String::as_str), Some("50"));
            assert!(!query.contains_key("expand"));
        }
        let revision = results[0]["sha"].as_str().unwrap();
        let info = manager.info_at("org/model", Some(revision)).await.unwrap();
        assert_eq!(info["studio_size"]["revision"], revision);
        assert_eq!(info["studio_size"]["weights_bytes"], 64);
        assert_eq!(info["studio_size"]["download_complete"], true);
        assert_eq!(
            info["studio_size"]["download_bytes"].as_u64(),
            Some(
                build_manifest(&info)
                    .unwrap()
                    .iter()
                    .map(|file| file.size)
                    .sum::<u64>()
            )
        );
        let requests = fake.metadata_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1],
            format!("/api/models/org/model/revision/{revision}?blobs=true")
        );
        assert!(
            fake.ranges
                .lock()
                .unwrap()
                .iter()
                .all(|(name, _, _)| !name.ends_with(".safetensors")),
            "compatibility inspection never downloads weights"
        );
        assert_eq!(info["studio_compatibility"]["status"], "compatible");
        assert_eq!(info["studio_compatibility"]["revision"], revision);
        assert_eq!(
            *fake.file_requests.lock().unwrap(),
            vec![format!("/org/model/resolve/{revision}/config.json")]
        );
        assert!(manager.list().unwrap().is_empty());
        server.abort();
    }

    #[test]
    fn manifest_pins_hashes_and_rejects_incomplete_models() {
        let info = json!({"siblings": [
            {"rfilename":"config.json", "size":20}, {"rfilename":"tokenizer.json", "size":2},
            {"rfilename":"model.safetensors", "size":100, "lfs":{"sha256":"a".repeat(64)}},
            {"rfilename":"model.gguf", "size":1000}
        ]});
        let mut files = build_manifest(&info).unwrap();
        assert_eq!(files.len(), 3);
        let hash = manifest_hash(&files).unwrap();
        files[0].downloaded = 12;
        assert_eq!(hash, manifest_hash(&files).unwrap());
        files[0].size += 1;
        assert_ne!(hash, manifest_hash(&files).unwrap());
        assert!(build_manifest(&json!({"siblings":[]})).is_err());
    }

    #[tokio::test]
    async fn discovery_excludes_incomplete_adapters_and_non_text_tasks_without_extra_requests() {
        let root = tempfile::tempdir().unwrap();
        let mut fake = FakeHub::fixture();
        let viable = json!({"id":"org/viable","library_name":"mlx","sha":"a".repeat(40),"createdAt":"2026-10-01T10:00:00.000Z","siblings":[{"rfilename":"config.json"},{"rfilename":"tokenizer.json"},{"rfilename":"model.safetensors"}]});
        let mut missing_tokenizer = viable.clone();
        missing_tokenizer["id"] = json!("org/incomplete-MLX");
        missing_tokenizer["siblings"]
            .as_array_mut()
            .unwrap()
            .retain(|file| file["rfilename"] != "tokenizer.json");
        let mut adapter = viable.clone();
        adapter["id"] = json!("org/adapter-MLX");
        adapter["siblings"][2]["rfilename"] = json!("adapter_model.safetensors");
        let mut gguf = viable.clone();
        gguf["siblings"][2]["rfilename"] = json!("model.gguf");
        let mut nested = viable.clone();
        nested["siblings"][2]["rfilename"] = json!("4bit/model.safetensors");
        let mut embedding = viable.clone();
        embedding["pipeline_tag"] = json!("feature-extraction");
        let mut audio = viable.clone();
        audio["pipeline_tag"] = json!("automatic-speech-recognition");
        let mut image = viable.clone();
        image["pipeline_tag"] = json!("text-to-image");
        fake.search_models = Some(vec![
            missing_tokenizer,
            adapter,
            gguf,
            nested,
            embedding,
            audio,
            image,
            viable,
        ]);
        let fake = Arc::new(fake);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let results = manager.search("MLX").await.unwrap();
        assert_eq!(results.as_array().unwrap().len(), 1);
        assert_eq!(results[0]["id"], "org/viable");
        assert_eq!(results[0]["createdAt"], "2026-10-01T10:00:00.000Z");
        assert_eq!(results[0]["studio_compatibility"]["status"], "unknown");
        assert_eq!(
            results[0]["studio_compatibility"]["revision"],
            "a".repeat(40)
        );
        assert_eq!(fake.metadata_requests.lock().unwrap().len(), 1);
        assert!(fake.file_requests.lock().unwrap().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn complete_files_still_require_supported_text_configuration_and_actual_mlx_evidence() {
        let cases = [
            (
                json!({"model_type":"qwen2"}),
                "mlx",
                "text-generation",
                true,
            ),
            (
                json!({"model_type":"flint"}),
                "mlx",
                "text-generation",
                true,
            ),
            (
                json!({"model_type":"qwen3_5","text_config":{"model_type":"qwen3_5_text"},"vision_config":{}}),
                "mlx",
                "image-text-to-text",
                true,
            ),
            (
                json!({"model_type":"gemma4","text_config":{"model_type":"gemma4_text"},"vision_config":{},"audio_config":{},"quantization":{"bits":8,"group_size":64}}),
                "transformers",
                "any-to-any",
                true,
            ),
            (
                json!({"model_type":"qwen2","quantization":{"bits":4,"group_size":64}}),
                "transformers",
                "text-generation",
                true,
            ),
            (
                json!({"model_type":"qwen2","quantization_config":{"quant_method":"gptq","bits":4,"group_size":64}}),
                "transformers",
                "text-generation",
                false,
            ),
            (
                json!({"model_type":"qwen2"}),
                "transformers",
                "text-generation",
                false,
            ),
            (json!({"model_type":"whisper"}), "mlx", "any-to-any", false),
            (
                json!({"model_type":"bert"}),
                "mlx",
                "text-generation",
                false,
            ),
            (
                json!({"model_type":"qwen3","architectures":["Qwen3ForEmbedding"]}),
                "mlx",
                "text-generation",
                false,
            ),
            (
                json!({"model_type":"qwen3_5_mtp"}),
                "mlx",
                "text-generation",
                false,
            ),
            (
                json!({"model_type":"qwen2","model_file":"custom.py"}),
                "mlx",
                "text-generation",
                false,
            ),
            (
                json!({"model_type":"qwen2","vision_config":{}}),
                "mlx",
                "image-text-to-text",
                false,
            ),
            (json!({}), "mlx", "text-generation", false),
        ];
        for (config, library, task, compatible) in cases {
            let root = tempfile::tempdir().unwrap();
            let mut fake = FakeHub::fixture();
            fake.files
                .insert("config.json".into(), serde_json::to_vec(&config).unwrap());
            let mut metadata = fake.metadata();
            metadata["library_name"] = json!(library);
            metadata["pipeline_tag"] = json!(task);
            metadata["id"] = json!("org/definitely-MLX-in-the-name");
            fake.metadata_override = Some(metadata);
            let fake = Arc::new(fake);
            let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
            let info = manager.info_at("org/model", Some("release")).await.unwrap();
            assert_eq!(
                info["studio_compatibility"]["status"],
                if compatible {
                    "compatible"
                } else {
                    "incompatible"
                },
                "{config}"
            );
            assert_eq!(
                info["studio_compatibility"]["revision"],
                "0123456789abcdef0123456789abcdef01234567"
            );
            if !compatible {
                assert!(
                    info["studio_compatibility"]["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.is_empty())
                );
                assert!(manager.add("org/model", Some("release")).await.is_err());
            }
            assert!(manager.list().unwrap().is_empty());
            assert!(fake.file_requests.lock().unwrap().iter().all(|path| path
                == "/org/model/resolve/0123456789abcdef0123456789abcdef01234567/config.json"));
            server.abort();
        }
    }

    #[tokio::test]
    async fn incomplete_download_is_rejected_before_queue_or_weight_requests() {
        let root = tempfile::tempdir().unwrap();
        let mut fake = FakeHub::fixture();
        fake.files.remove("tokenizer.json");
        let fake = Arc::new(fake);
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        let info = manager.info("org/model").await.unwrap();
        assert_eq!(info["studio_compatibility"]["status"], "incompatible");
        assert!(
            info["studio_compatibility"]["reason"]
                .as_str()
                .unwrap()
                .contains("tokenizer")
        );
        let error = manager
            .add("org/model", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("tokenizer"));
        assert!(manager.list().unwrap().is_empty());
        assert!(fake.file_requests.lock().unwrap().is_empty());
        server.abort();
    }

    #[test]
    fn mlx_tags_cannot_override_incompatible_quantization_and_native_layer_overrides_are_preserved()
    {
        for method in ["awq", "gptq", "bitsandbytes", "aqlm", "hqq"] {
            let config = json!({"model_type":"llama","quantization_config":{"quant_method":method,"bits":4,"group_size":128}});
            assert!(mlx_evidence(&json!({"tags":["mlx"]}), Some(&config)));
            assert!(config_incompatibility(&config).unwrap().contains(method));
        }
        let mixed = json!({"model_type":"llama","quantization":{"bits":4,"group_size":64,"model.layers.0.self_attn.q_proj":{"bits":8,"group_size":32},"model.layers.0.self_attn.k_proj":false},"quantization_config":{"quant_method":"gptq"}});
        assert!(mlx_evidence(
            &json!({"library_name":"transformers"}),
            Some(&mixed)
        ));
        assert!(
            config_incompatibility(&mixed).is_none(),
            "the native MLX field takes precedence over legacy config and retains per-layer settings"
        );
        for config in [
            json!({"model_type":"llama","quantization":{"bits":4}}),
            json!({"model_type":"llama","quantization":{"bits":4,"group_size":128,"quant_method":"gptq"}}),
        ] {
            assert!(config_incompatibility(&config).is_some());
        }
    }

    #[tokio::test]
    async fn compatibility_checks_weight_indices_at_the_immutable_revision() {
        for (referenced, expected) in [
            ("model.safetensors", "compatible"),
            ("model-00002.safetensors", "incompatible"),
            ("../model.safetensors", "incompatible"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut fake = FakeHub::fixture();
            fake.files.insert(
                "model.safetensors.index.json".into(),
                serde_json::to_vec(&json!({"weight_map":{"layer.weight":referenced}})).unwrap(),
            );
            let fake = Arc::new(fake);
            let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
            let info = manager.info("org/model").await.unwrap();
            assert_eq!(info["studio_compatibility"]["status"], expected);
            let paths = fake.file_requests.lock().unwrap();
            assert_eq!(paths.len(), 2);
            assert!(paths.iter().all(|path| path.contains("/resolve/0123456789abcdef0123456789abcdef01234567/")));
            assert!(!paths.iter().any(|path| path.ends_with(".safetensors")));
            server.abort();
        }
    }

    #[test]
    fn missing_shards_and_empty_required_assets_are_never_complete_models() {
        let mut fake = FakeHub::fixture();
        let weights = fake.files.remove("model.safetensors").unwrap();
        fake.files
            .insert("model-00001-of-00002.safetensors".into(), weights.clone());
        assert!(
            build_manifest(&fake.metadata())
                .unwrap_err()
                .to_string()
                .contains("shards")
        );
        fake.files
            .insert("model-00002-of-00002.safetensors".into(), weights);
        assert!(build_manifest(&fake.metadata()).is_ok());
        fake.files.insert("tokenizer.json".into(), Vec::new());
        assert!(
            build_manifest(&fake.metadata())
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
    }

    #[tokio::test]
    async fn compatibility_does_not_mislabel_gated_rate_limited_or_network_failures() {
        let root = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeHub::fixture());
        let (manager, server) = fixture_manager(root.path(), fake.clone()).await;
        for (status, reason) in [
            (401, "denied access"),
            (403, "denied access"),
            (429, "rate limit"),
            (503, "status 503"),
        ] {
            fake.config_status.store(status, Ordering::Relaxed);
            let error = manager.info("org/model").await.unwrap_err().to_string();
            assert!(error.contains(reason), "{error}");
        }
        server.abort();
        let error = manager.info("org/model").await.unwrap_err().to_string();
        assert!(error.contains("unreachable"), "{error}");
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn verification_detects_corrupt_and_truncated_weights() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("model.safetensors");
        let data = b"complete model weights";
        let spec = DownloadFile {
            path: "model.safetensors".into(),
            size: data.len() as u64,
            sha256: Some(format!("{:x}", Sha256::digest(data))),
            git_blob_sha1: None,
            downloaded: 0,
            verified: false,
        };
        fs::write(&path, data).unwrap();
        verify_file(&path, &spec).unwrap();
        fs::write(&path, vec![0; data.len()]).unwrap();
        assert!(verify_file(&path, &spec).is_err());
        fs::write(&path, b"short").unwrap();
        assert!(verify_file(&path, &spec).is_err());
    }

    #[test]
    fn repo_ids_and_remote_paths_are_not_filesystem_paths() {
        assert!(validate_repo("mlx-community/Qwen3-4B-4bit").is_ok());
        for repo in ["../x", "a/b/c", "a\\b", "", "a/.."] {
            assert!(validate_repo(repo).is_err());
        }
        assert!(!model_file("weights.gguf"));
        assert!(!model_file("pytorch_model.bin"));
    }

    #[test]
    fn git_blob_hash_verifies_small_files_including_the_git_object_header() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let spec = DownloadFile {
            path: "config.json".into(),
            size: 5,
            sha256: None,
            git_blob_sha1: Some("b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0".into()),
            downloaded: 0,
            verified: false,
        };
        fs::write(&path, b"hello").unwrap();
        verify_file(&path, &spec).unwrap();
        fs::write(&path, b"jello").unwrap();
        assert!(
            verify_file(&path, &spec)
                .unwrap_err()
                .to_string()
                .contains("Git blob SHA-1")
        );
    }

    #[test]
    fn manifests_capture_git_blob_ids_and_legacy_jobs_keep_their_manifest_hash() {
        let legacy: DownloadFile = serde_json::from_value(json!({
            "path":"config.json","size":5,"sha256":null,"downloaded":0,"verified":false
        }))
        .unwrap();
        assert!(legacy.git_blob_sha1.is_none());
        assert!(
            serde_json::to_value(&legacy)
                .unwrap()
                .get("git_blob_sha1")
                .is_none()
        );
        let old_hash = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&vec![("config.json", 5u64, Option::<String>::None)]).unwrap()
            )
        );
        assert_eq!(manifest_hash(&[legacy.clone()]).unwrap(), old_hash);
        let info = json!({"siblings":[
            {"rfilename":"config.json","size":5,"blobId":"b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0"},
            {"rfilename":"tokenizer.json","size":2,"blobId":"9e26dfeeb6e641a33dae4961196235bdb965b21b"},
            {"rfilename":"model.safetensors","size":64,"lfs":{"sha256":"a".repeat(64)}}
        ]});
        let files = build_manifest(&info).unwrap();
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == "config.json")
                .unwrap()
                .git_blob_sha1,
            Some("b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0".into())
        );
        assert!(
            files
                .iter()
                .find(|file| file.path == "model.safetensors")
                .unwrap()
                .git_blob_sha1
                .is_none()
        );
        let mut upgraded = legacy;
        upgraded.git_blob_sha1 = Some("b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0".into());
        assert_ne!(manifest_hash(&[upgraded]).unwrap(), old_hash);
    }
}
