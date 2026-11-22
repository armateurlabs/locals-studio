//! Managed inference engines. Each engine installs side by side: an update stages a new version,
//! verifies it, then switches the manifest while keeping the previous one for rollback. Workers keep
//! the version they started with; versions no worker uses are removed afterwards.
pub mod kapitsa;
pub mod locals;

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex};

use crate::{
    events::{self, EventSender},
    paths::AppPaths,
    types::Engine,
};

pub use kapitsa::KapitsaRuntime;
pub use locals::LocalsRuntime;

/// A release number, `MAJOR.MINOR.PATCH`, read from tags (`v0.2.2`) or version output
/// (`kapitsa 0.2.39 (d387c2d)`). Pre-release suffixes are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches('v');
        let core = text
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()?;
        let mut parts = core.split('.');
        let version = Version(
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(version)
    }

    /// The first `X.Y.Z` among whitespace-separated words.
    pub fn find(text: &str) -> Option<Self> {
        text.split_whitespace().find_map(Version::parse)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The newest release an engine publishes, as last checked.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LatestRelease {
    pub version: Option<String>,
    /// The commit or tag the release is built from, when the engine needs it to install.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
    #[serde(skip)]
    checked: Option<Instant>,
}

impl LatestRelease {
    pub fn fresh(&self, ttl: Duration) -> bool {
        self.checked.is_some_and(|at| at.elapsed() < ttl)
    }

    fn found(version: Version, revision: Option<String>) -> Self {
        Self {
            version: Some(version.to_string()),
            revision,
            checked_at: Some(chrono::Utc::now().to_rfc3339()),
            error: None,
            checked: Some(Instant::now()),
        }
    }

    fn failed(previous: &LatestRelease, error: &anyhow::Error) -> Self {
        Self {
            error: Some(format!("{error:#}")),
            checked_at: Some(chrono::Utc::now().to_rfc3339()),
            checked: Some(Instant::now()),
            ..previous.clone()
        }
    }
}

/// How long a successful release check is reused before asking the network again.
pub const CHECK_TTL: Duration = Duration::from_secs(10 * 60);

/// The current installation and the one it replaced, which a rollback restores.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Manifest<T> {
    pub current: T,
    #[serde(default)]
    pub previous: Option<T>,
}

pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}

/// Replace a manifest atomically, so a reader sees the old or the new installation, never half of one.
pub(crate) async fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Manifest path has no parent")?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".manifest-{}.json", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(value)?).await?;
    tokio::fs::rename(&temporary, path).await?;
    Ok(())
}

pub(crate) fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Locals-Studio/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?)
}

/// Stream a download to `destination` and return its SHA-256, reporting progress as it goes.
pub(crate) async fn download(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    progress: impl Fn(u64, Option<u64>),
) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Could not reach {url}"))?
        .error_for_status()
        .with_context(|| format!("Could not download {url}"))?;
    let total = response.content_length();
    let mut file = tokio::fs::File::create(destination).await?;
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut last_report = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("The download of {url} was interrupted"))?;
        digest.update(&chunk);
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
        if received - last_report >= 1 << 20 {
            last_report = received;
            progress(received, total);
        }
    }
    file.flush().await?;
    progress(received, total);
    Ok(format!("{:x}", digest.finalize()))
}

/// Extract a `.tar.gz` with the system tar into `destination`, which must not exist yet.
pub(crate) async fn extract(archive: &Path, destination: &Path) -> Result<()> {
    tokio::fs::create_dir_all(destination).await?;
    let output = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .output()
        .await
        .context("Could not run /usr/bin/tar")?;
    if !output.status.success() {
        bail!(
            "Could not extract {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The single top-level folder of an extracted archive.
pub(crate) async fn single_root(directory: &Path) -> Result<PathBuf> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut roots = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            roots.push(entry.path());
        }
    }
    match roots.as_slice() {
        [root] => Ok(root.clone()),
        _ => bail!("The archive does not contain exactly one top-level folder"),
    }
}

/// Remove the entries of `root` whose names start with `prefix` and that no installation or
/// running worker uses. Returns what was removed.
pub(crate) async fn remove_unused(
    root: &Path,
    prefix: &str,
    keep: &HashSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return Ok(removed);
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        if keep.iter().any(|kept| kept.starts_with(&path)) {
            continue;
        }
        let result = if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        match result {
            Ok(()) => removed.push(path),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "Could not remove an unused runtime")
            }
        }
    }
    Ok(removed)
}

/// Events from the installation of one engine carry that engine, so each Runtime card shows its own.
#[derive(Clone)]
pub(crate) struct Reporter {
    events: EventSender,
    engine: Engine,
}

impl Reporter {
    pub(crate) fn new(events: EventSender, engine: Engine) -> Self {
        Self { events, engine }
    }

    pub(crate) fn progress(&self, phase: &str, mut detail: Value) {
        if !detail.is_object() {
            detail = json!({});
        }
        detail["engine"] = json!(self.engine);
        detail["phase"] = json!(phase);
        events::emit(&self.events, "runtime.progress", &detail);
    }

    pub(crate) fn log(&self, line: &str) {
        events::emit(
            &self.events,
            "runtime.log",
            &json!({"engine":self.engine,"line":line}),
        );
    }

    pub(crate) fn error(&self, error: &anyhow::Error) {
        events::emit(
            &self.events,
            "runtime.error",
            &json!({"engine":self.engine,"error":format!("{error:#}")}),
        );
    }
}

/// What a worker needs to start on the installed version of its engine.
#[derive(Debug, Clone)]
pub struct Launcher {
    /// The interpreter (Locals) or binary (Kapitsa) to run.
    pub executable: PathBuf,
    pub version: String,
    /// The folder this version is installed in; a worker using it keeps it from removal.
    pub root: PathBuf,
}

/// Every managed engine. Operations name the engine they act on.
#[derive(Clone)]
pub struct RuntimeManager {
    pub locals: LocalsRuntime,
    pub kapitsa: KapitsaRuntime,
    gc_lock: Arc<Mutex<()>>,
}

impl RuntimeManager {
    pub fn new(paths: AppPaths, events: EventSender) -> Self {
        Self {
            locals: LocalsRuntime::new(paths.clone(), events.clone()),
            kapitsa: KapitsaRuntime::new(paths, events),
            gc_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn status(&self, engine: Engine) -> Result<Value> {
        match engine {
            Engine::Locals => self.locals.status().await,
            Engine::Kapitsa => self.kapitsa.status().await,
        }
    }

    pub async fn statuses(&self) -> Result<Value> {
        Ok(json!({
            "locals": self.locals.status().await?,
            "kapitsa": self.kapitsa.status().await?,
        }))
    }

    pub async fn check(&self, engine: Engine, force: bool) -> Result<Value> {
        match engine {
            Engine::Locals => self.locals.check(force).await,
            Engine::Kapitsa => self.kapitsa.check(force).await,
        };
        self.status(engine).await
    }

    pub async fn install(&self, engine: Engine) -> Result<Value> {
        match engine {
            Engine::Locals => self.locals.install().await,
            Engine::Kapitsa => self.kapitsa.install().await,
        }
    }

    pub async fn update(&self, engine: Engine) -> Result<Value> {
        match engine {
            Engine::Locals => self.locals.update().await,
            Engine::Kapitsa => self.kapitsa.update().await,
        }
    }

    pub async fn rollback(&self, engine: Engine) -> Result<Value> {
        match engine {
            Engine::Locals => self.locals.rollback().await,
            Engine::Kapitsa => self.kapitsa.rollback().await,
        }
    }

    /// Ensure the engine is installed and return how to start a worker on its current version.
    pub async fn launcher(&self, engine: Engine) -> Result<Launcher> {
        match engine {
            Engine::Locals => self.locals.launcher().await,
            Engine::Kapitsa => self.kapitsa.launcher().await,
        }
    }

    /// Remove versions that are neither current, previous, nor used by a running worker.
    pub async fn collect_garbage(&self, in_use: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
        let _guard = self.gc_lock.lock().await;
        let mut removed = self.locals.collect_garbage(in_use).await?;
        removed.extend(self.kapitsa.collect_garbage(in_use).await?);
        Ok(removed)
    }

    pub async fn recommended_memory(&self) -> Option<u64> {
        self.locals.recommended_memory().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_tags_and_version_output_and_compare_numerically() {
        assert_eq!(Version::parse("v0.2.10"), Some(Version(0, 2, 10)));
        assert_eq!(Version::parse("0.2.2"), Some(Version(0, 2, 2)));
        assert_eq!(Version::parse("0.2.2rc1"), Some(Version(0, 2, 2)));
        assert_eq!(Version::parse("0.2"), None);
        assert_eq!(Version::parse("0.2.1.4"), None);
        assert_eq!(
            Version::find("kapitsa 0.2.39 (d387c2d)"),
            Some(Version(0, 2, 39))
        );
        assert!(Version(0, 2, 10) > Version(0, 2, 9));
    }

    #[tokio::test]
    async fn unused_versions_are_removed_but_kept_and_in_use_ones_stay() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "env-current",
            "env-previous",
            "env-running",
            "env-old",
            "other",
        ] {
            std::fs::create_dir_all(temp.path().join(name).join("bin")).unwrap();
        }
        let keep: HashSet<PathBuf> = [
            temp.path().join("env-current"),
            temp.path().join("env-previous/bin/python"),
            temp.path().join("env-running"),
        ]
        .into();
        let removed = remove_unused(temp.path(), "env-", &keep).await.unwrap();
        assert_eq!(removed, vec![temp.path().join("env-old")]);
        for name in ["env-current", "env-previous", "env-running", "other"] {
            assert!(temp.path().join(name).is_dir(), "{name}");
        }
    }
}
