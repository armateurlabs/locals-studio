//! Hugging Face discovery and durable, checkpointed model downloads.
//!
//! Studio uses the Hub HTTP download endpoint, owns a durable staging manifest, and never edits
//! the shared Hugging Face cache. Checkpoints survive proxies that ignore Range: a full response
//! is identified by length and ETag, and its replayed prefix is compared before appending.
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Notify};

use crate::{
    events::{EventSender, emit},
    models,
    paths::AppPaths,
    store::Store,
};

const RANGE_BYTES: u64 = 64 * 1024 * 1024;
const JOBS: &str = "downloads";

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
            bail!("Recherche trop longue (256 caractères maximum)");
        }
        let mut url = self.api_url(&["api", "models"])?;
        url.query_pairs_mut()
            .append_pair("search", query)
            .append_pair("filter", "mlx")
            .append_pair("sort", "downloads")
            .append_pair("direction", "-1")
            .append_pair("limit", "50");
        self.get_json(url).await
    }

    pub async fn info(&self, repo_id: &str) -> Result<Value> {
        validate_repo(repo_id)?;
        self.metadata(repo_id, "main").await
    }

    pub async fn add(&self, repo_id: &str, revision: Option<&str>) -> Result<Value> {
        validate_repo(repo_id)?;
        let info = self.metadata(repo_id, revision.unwrap_or("main")).await?;
        let sha = info
            .get("sha")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit()))
            .context("Hugging Face n'a pas fourni de révision immuable valide")?
            .to_owned();
        let _state = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("État du téléchargement indisponible"))?;
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
            .context("Taille totale du modèle trop grande")?;
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
                bail!("Ce téléchargement ne peut pas être repris dans son état actuel");
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
                bail!("Un téléchargement terminé ne peut pas être annulé");
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
            .ok_or_else(|| anyhow!("Téléchargement inconnu : {id}"))
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
            .map_err(|_| anyhow!("État du téléchargement indisponible"))?;
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
            bail!("Le manifeste du téléchargement a été modifié");
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
            bail!("Espace disque insuffisant : {remaining} octets restent à télécharger");
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
                            "ETag modifié pour {} ; reprise refusée sans modifier les octets conservés",
                            spec.path
                        );
                    }
                } else if let Some(etag) = &etag {
                    self.inner.store.set("download-etags", &etag_key, etag)?;
                } else if replay {
                    bail!(
                        "Réponse HTTP complète sans ETag pour {} ; reprise non sûre refusée",
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
                                "Préfixe HTTP différent pour {} ; reprise refusée, aucun nouvel octet enregistré",
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
                            "Réponse HTTP trop longue pour {} ; publication refusée",
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
                        "Réponse HTTP incomplète pour {} ; reprise disponible depuis le dernier checkpoint",
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
            .map_err(|_| anyhow!("État du téléchargement indisponible"))?;
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
            .map_err(|_| anyhow!("État du téléchargement indisponible"))?;
        if self.interrupted(&job.id)? {
            return Ok(());
        }
        if let Some(parent) = job.destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(staging, &job.destination)
            .context("Publication atomique du modèle impossible")?;
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
                .map_err(|_| anyhow!("Endpoint invalide"))?;
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
            .map_err(|_| anyhow!("Endpoint Hugging Face invalide"))?;
        segments.pop_if_empty();
        for component in components {
            segments.push(component);
        }
        drop(segments);
        Ok(url)
    }

    async fn metadata(&self, repo_id: &str, revision: &str) -> Result<Value> {
        if revision.is_empty() || revision.len() > 256 {
            bail!("Révision invalide");
        }
        let mut url = self.api_url(&["api", "models"])?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| anyhow!("Endpoint invalide"))?;
            for part in repo_id.split('/') {
                path.push(part);
            }
            path.push("revision").push(revision);
        }
        url.query_pairs_mut().append_pair("blobs", "true");
        self.get_json(url).await
    }

    async fn get_json(&self, url: reqwest::Url) -> Result<Value> {
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
                    .context("Hugging Face est inaccessible (connexion TLS, proxy ou réseau)")
            })?;
        match response.status().as_u16() {
            401 | 403 => bail!(
                "Accès refusé par Hugging Face ; vérifiez votre connexion et les conditions du modèle"
            ),
            404 => bail!("Modèle ou révision introuvable sur Hugging Face"),
            429 => bail!("Limite Hugging Face atteinte ; réessayez plus tard"),
            status if !(200..300).contains(&status) => {
                bail!("Hugging Face a répondu avec le statut {status}")
            }
            _ => {}
        }
        response
            .json()
            .await
            .context("Réponse JSON Hugging Face invalide")
    }
}

fn build_manifest(info: &Value) -> Result<Vec<DownloadFile>> {
    let siblings = info
        .get("siblings")
        .and_then(Value::as_array)
        .context("Liste de fichiers absente")?;
    let mut files = Vec::new();
    for sibling in siblings {
        let Some(path) = sibling.get("rfilename").and_then(Value::as_str) else {
            continue;
        };
        if !model_file(path) {
            continue;
        }
        if !models::safe_relative(path) {
            bail!("Chemin de fichier non sûr dans le dépôt");
        }
        let size = sibling
            .get("size")
            .and_then(Value::as_u64)
            .or_else(|| sibling.pointer("/lfs/size").and_then(Value::as_u64))
            .with_context(|| format!("Taille absente pour {path}"))?;
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
            bail!("Empreinte SHA-256 absente pour les poids {path}");
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
        || !files.iter().any(|f| f.path.ends_with(".safetensors"))
        || !files
            .iter()
            .any(|f| f.path == "tokenizer.json" || f.path == "tokenizer.model")
    {
        bail!(
            "Le dépôt ne contient pas un modèle MLX complet (config, poids safetensors et tokenizer requis)"
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
    let mut file =
        fs::File::open(path).with_context(|| format!("Fichier absent : {}", spec.path))?;
    if file.metadata()?.len() != spec.size {
        bail!("Taille incorrecte pour {}", spec.path);
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
                "Vérification SHA-256 échouée pour {} ; retirez le téléchargement et recommencez",
                spec.path
            );
        }
        if spec.git_blob_sha1.as_ref().is_some_and(|expected| {
            format!("{:x}", git_hash.finalize()) != expected.to_ascii_lowercase()
        }) {
            bail!(
                "Vérification Git blob SHA-1 échouée pour {} ; publication refusée",
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
        bail!("Identifiant Hugging Face invalide (auteur/modèle attendu)");
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
        bail!("Encodage HTTP incompatible avec une reprise exacte");
    }
    let length = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    match status {
        reqwest::StatusCode::OK => {
            if length != Some(file_size) {
                bail!(
                    "Réponse HTTP complète de taille incorrecte ; reprise refusée avant toute écriture"
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
                bail!(
                    "Content-Range ne correspond pas à la reprise demandée ; aucun nouvel octet enregistré"
                );
            }
            Ok(false)
        }
        _ => bail!(
            "Téléchargement HTTP impossible (statut {}) ; reprise disponible",
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
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{Request, Response, StatusCode},
    };
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicBool, Ordering},
    };

    struct FakeHub {
        files: BTreeMap<String, Vec<u8>>,
        ranges: std::sync::Mutex<Vec<(String, u64, u64)>>,
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
                        br#"{"model_type":"qwen2","quantization":{"bits":4}}"#.to_vec(),
                    ),
                    ("tokenizer.json".into(), b"{}".to_vec()),
                    ("model.safetensors".into(), (0u8..64).collect()),
                ]),
                ranges: std::sync::Mutex::new(Vec::new()),
                slow: AtomicBool::new(false),
                corrupt: AtomicBool::new(false),
                redirect_weights: AtomicBool::new(false),
                wrong_size: AtomicBool::new(false),
                ignore_ranges: AtomicBool::new(false),
            }
        }

        fn metadata(&self) -> Value {
            json!({"sha": "0123456789abcdef0123456789abcdef01234567", "siblings": self.files.iter().map(|(path, bytes)| {
                json!({"rfilename":path, "size":bytes.len(), "lfs":{"sha256":format!("{:x}", Sha256::digest(bytes)), "size":bytes.len()}})
            }).collect::<Vec<_>>()})
        }
    }

    async fn fake_response(
        State(state): State<Arc<FakeHub>>,
        request: Request<Body>,
    ) -> Response<Body> {
        if request.uri().path().starts_with("/api/models/") {
            return Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(state.metadata().to_string()))
                .unwrap();
        }
        let name = request.uri().path().rsplit('/').next().unwrap_or("");
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
                .contains("Préfixe HTTP différent")
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
                    error.contains("taille incorrecte")
                } else {
                    error.contains("ETag modifié")
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
