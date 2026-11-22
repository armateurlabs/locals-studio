//! The Locals engine: a Python environment per installed version, built with uv from a Locals
//! release (or a local checkout during development) and its own hash-locked dependencies.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::Mutex,
};

use super::{
    CHECK_TTL, LatestRelease, Launcher, Reporter, Version, download, extract, http_client,
    read_json, remove_unused, single_root, write_json,
};
use crate::{events::EventSender, paths::AppPaths, types::Engine};

pub const PYTHON_VERSION: &str = "3.12";
/// The oldest Locals whose options, `/health` and `/stats` this Studio understands.
pub const MINIMUM_VERSION: Version = Version(0, 2, 1);
const REPOSITORY: &str = "Generated-Computers/Locals";
/// Dependencies and source of the release this Studio was tested with: used when a release has no
/// `uv.lock`, and for a first installation when GitHub's release list cannot be read.
const REQUIREMENTS_LOCK: &str = include_str!("../../../../resources/runtime/requirements.lock.txt");
const SOURCE_LOCK: &str = include_str!("../../../../resources/runtime/source.lock.json");

static INSTALL_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
static LATEST: OnceLock<Arc<StdMutex<LatestRelease>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub python_path: PathBuf,
    pub python_version: String,
    pub locals_version: String,
    pub installed_at: String,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    #[serde(default)]
    pub source: Value,
    #[serde(default)]
    pub recommended_gpu_bytes: Option<u64>,
}

impl Installation {
    fn version(&self) -> Option<Version> {
        Version::parse(&self.locals_version)
    }

    /// The virtual environment holding this version: `<env>/bin/python`.
    fn environment(&self) -> Option<PathBuf> {
        Some(self.python_path.parent()?.parent()?.to_owned())
    }

    fn usable(&self) -> bool {
        self.python_version == PYTHON_VERSION
            && self
                .version()
                .is_some_and(|version| version >= MINIMUM_VERSION)
            && self.python_path.is_file()
    }
}

/// `runtime/current.json`. The current installation stays at the top level, where older Studio
/// versions read it, with the replaced one beside it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredManifest {
    #[serde(flatten)]
    current: Installation,
    #[serde(default)]
    previous: Option<Installation>,
}

/// What an installation or update installs.
#[derive(Debug, Clone)]
enum Target {
    /// A checkout named by `LOCALS_STUDIO_LOCALS_SOURCE`, or the neighboring one in a debug build.
    Local {
        path: PathBuf,
        version: Version,
        fingerprint: String,
    },
    Release {
        version: Version,
        revision: String,
    },
}

impl Target {
    fn version(&self) -> Version {
        match self {
            Target::Local { version, .. } | Target::Release { version, .. } => *version,
        }
    }

    /// Whether `installed` is already exactly this target.
    fn installed_as(&self, installed: &Installation) -> bool {
        if installed.version() != Some(self.version()) {
            return false;
        }
        match self {
            Target::Local { fingerprint, .. } => installed.source["fingerprint"] == *fingerprint,
            Target::Release { revision, .. } => {
                installed.source["kind"] != "local"
                    && installed.source["revision"]
                        .as_str()
                        .is_none_or(|installed| installed == revision)
            }
        }
    }
}

#[derive(Clone)]
pub struct LocalsRuntime {
    paths: AppPaths,
    reporter: Reporter,
    install_lock: Arc<Mutex<()>>,
    latest: Arc<StdMutex<LatestRelease>>,
}

impl LocalsRuntime {
    pub fn new(paths: AppPaths, events: EventSender) -> Self {
        Self {
            paths,
            reporter: Reporter::new(events, Engine::Locals),
            install_lock: INSTALL_LOCK.get_or_init(Default::default).clone(),
            latest: LATEST.get_or_init(Default::default).clone(),
        }
    }

    fn root(&self) -> PathBuf {
        self.paths.data_dir.join("runtime")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root().join("current.json")
    }

    async fn manifest(&self) -> Option<StoredManifest> {
        read_json(&self.manifest_path()).await
    }

    async fn installation(&self) -> Option<Installation> {
        self.manifest()
            .await
            .map(|manifest| manifest.current)
            .filter(Installation::usable)
    }

    fn local_source(&self) -> Option<&Path> {
        self.paths
            .locals_source
            .as_deref()
            .filter(|path| path.join("pyproject.toml").is_file())
    }

    pub async fn status(&self) -> Result<Value> {
        let manifest = self.manifest().await;
        let installation = manifest
            .as_ref()
            .map(|manifest| manifest.current.clone())
            .filter(Installation::usable);
        let previous = manifest
            .and_then(|manifest| manifest.previous)
            .filter(Installation::usable);
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let installed = installation.as_ref().and_then(Installation::version);
        let available = latest.version.as_deref().and_then(Version::parse);
        let local = self.local_source().map(Path::to_owned);
        Ok(json!({
            "engine": Engine::Locals,
            "name": Engine::Locals.label(),
            "installed": installation.is_some(),
            "version": installed.map(|version| version.to_string()),
            "installation": installation,
            "previous": previous.map(|previous| json!({
                "version": previous.locals_version,
                "installed_at": previous.installed_at,
            })),
            "minimum_version": MINIMUM_VERSION.to_string(),
            "latest": latest,
            "update_available": match (installed, available) {
                (Some(installed), Some(available)) => available > installed,
                _ => false,
            },
            "source_path": local,
            "source": if local.is_some() { "local" } else { "release" },
            "required_python": PYTHON_VERSION,
            "uv_available": find_uv().await.is_some(),
        }))
    }

    /// Resolve the newest Locals release, reusing a recent answer unless `force` is set.
    pub async fn check(&self, force: bool) -> LatestRelease {
        if let Some(path) = self.local_source() {
            let release = match pyproject_version(path).await {
                Ok(version) => LatestRelease::found(version, None),
                Err(error) => LatestRelease::failed(&LatestRelease::default(), &error),
            };
            *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = release.clone();
            return release;
        }
        let cached = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !force && cached.error.is_none() && cached.fresh(CHECK_TTL) {
            return cached;
        }
        let release = match latest_release().await {
            Ok((version, revision)) => LatestRelease::found(version, Some(revision)),
            Err(error) => LatestRelease::failed(&cached, &error),
        };
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = release.clone();
        release
    }

    /// What to install: the local checkout, else the newest release. A first installation may fall
    /// back to the tested release when GitHub cannot be reached; an update never silently does.
    async fn target(&self, fallback: bool) -> Result<Target> {
        if let Some(path) = self.local_source() {
            let path = path.canonicalize()?;
            let version = pyproject_version(&path).await?;
            if version < MINIMUM_VERSION {
                bail!(
                    "This Studio requires Locals {MINIMUM_VERSION} or newer; the checkout at {} is {version}",
                    path.display()
                );
            }
            let fingerprint = source_fingerprint(&path)?;
            return Ok(Target::Local {
                path,
                version,
                fingerprint,
            });
        }
        let latest = self.check(true).await;
        match (
            latest.version.as_deref().and_then(Version::parse),
            latest.revision,
        ) {
            (Some(version), Some(revision)) if latest.error.is_none() => {
                Ok(Target::Release { version, revision })
            }
            _ if fallback => {
                let (version, revision) = pinned_release()?;
                self.reporter.log(&format!(
                    "Could not read the Locals releases ({}); installing the tested Locals {version}.",
                    latest.error.as_deref().unwrap_or("no release found")
                ));
                Ok(Target::Release { version, revision })
            }
            _ => bail!(
                "Could not find the latest Locals release: {}",
                latest.error.as_deref().unwrap_or("no release found")
            ),
        }
    }

    pub async fn install(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        if self.installation().await.is_none() {
            let target = self.target(true).await?;
            self.provision(target).await?;
        }
        self.status().await
    }

    /// Install the newest release beside the current one; workers keep the version they run.
    pub async fn update(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        let target = match self.target(false).await {
            Ok(target) => target,
            Err(error) => {
                self.reporter.error(&error);
                return Err(error);
            }
        };
        if let Some(current) = self.installation().await
            && target.installed_as(&current)
        {
            self.reporter
                .progress("up_to_date", json!({"version":current.locals_version}));
            return self.status().await;
        }
        self.provision(target).await?;
        self.status().await
    }

    /// Return to the version the last update replaced.
    pub async fn rollback(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        let manifest = self.manifest().await.context("Locals is not installed")?;
        let previous = manifest
            .previous
            .filter(Installation::usable)
            .context("There is no previous Locals version to return to")?;
        let version = previous.locals_version.clone();
        write_json(
            &self.manifest_path(),
            &StoredManifest {
                current: previous,
                previous: Some(manifest.current),
            },
        )
        .await?;
        self.reporter
            .progress("rolled_back", json!({"version":version}));
        self.status().await
    }

    pub async fn launcher(&self) -> Result<Launcher> {
        let installation = match self.installation().await {
            Some(installation) => installation,
            None => {
                self.install().await?;
                self.installation()
                    .await
                    .context("The managed Locals installation could not be verified")?
            }
        };
        Ok(Launcher {
            root: installation
                .environment()
                .context("The Locals interpreter is not inside an environment")?,
            executable: installation.python_path,
            version: installation.locals_version,
        })
    }

    pub async fn recommended_memory(&self) -> Option<u64> {
        self.installation().await?.recommended_gpu_bytes
    }

    pub(crate) async fn collect_garbage(&self, in_use: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
        // An installation in progress owns its staging folders.
        let Ok(_guard) = self.install_lock.try_lock() else {
            return Ok(Vec::new());
        };
        let mut keep = in_use.clone();
        if let Some(manifest) = self.manifest().await {
            keep.extend(manifest.current.environment());
            keep.extend(
                manifest
                    .previous
                    .and_then(|previous| previous.environment()),
            );
        } else {
            // Without a readable manifest, nothing proves an environment unused.
            return Ok(Vec::new());
        }
        let mut removed = remove_unused(&self.root(), "env-", &keep).await?;
        removed.extend(remove_unused(&self.root(), "src-", &keep).await?);
        Ok(removed)
    }

    async fn provision(&self, target: Target) -> Result<()> {
        let uv = find_uv().await.context(
            "uv is required to install Locals. Install uv (https://docs.astral.sh/uv/), then retry.",
        )?;
        let root = self.root();
        tokio::fs::create_dir_all(&root).await?;
        let id = uuid::Uuid::new_v4();
        let environment = root.join(format!("env-{id}"));
        let staging = root.join(format!("src-{id}"));
        let archive = root.join(format!("src-{id}.tar.gz"));
        let result = self
            .build(&uv, &target, &environment, &staging, &archive)
            .await;
        let _ = tokio::fs::remove_dir_all(&staging).await;
        let _ = tokio::fs::remove_file(&archive).await;
        match result {
            Ok(installed) => {
                let previous = self
                    .manifest()
                    .await
                    .map(|manifest| manifest.current)
                    .filter(|previous| {
                        previous.usable() && previous.python_path != installed.python_path
                    });
                let version = installed.locals_version.clone();
                write_json(
                    &self.manifest_path(),
                    &StoredManifest {
                        current: installed,
                        previous,
                    },
                )
                .await?;
                self.reporter.progress("ready", json!({"version":version}));
                Ok(())
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&environment).await;
                self.reporter.error(&error);
                Err(error)
            }
        }
    }

    async fn build(
        &self,
        uv: &Path,
        target: &Target,
        environment: &Path,
        staging: &Path,
        archive: &Path,
    ) -> Result<Installation> {
        let version = target.version();
        let (source, mut record) = match target {
            Target::Local {
                path, fingerprint, ..
            } => (
                path.clone(),
                json!({"kind":"local","path":path,"version":version.to_string(),"fingerprint":fingerprint}),
            ),
            Target::Release { revision, .. } => {
                let url = format!("https://github.com/{REPOSITORY}/archive/{revision}.tar.gz");
                self.reporter
                    .progress("downloading", json!({"version":version.to_string()}));
                let reporter = self.reporter.clone();
                download(&http_client()?, &url, archive, |received, total| {
                    reporter.progress(
                        "downloading",
                        json!({"version":version.to_string(),"received_bytes":received,"total_bytes":total}),
                    );
                })
                .await?;
                extract(archive, staging).await?;
                (
                    single_root(staging).await?,
                    json!({"kind":"upstream","repository":format!("https://github.com/{REPOSITORY}"),"revision":revision,"url":url,"version":version.to_string()}),
                )
            }
        };
        let reported = pyproject_version(&source).await?;
        if reported != version {
            bail!("The Locals source reports version {reported}, expected {version}");
        }
        self.reporter
            .progress("installing_python", json!({"python":PYTHON_VERSION}));
        self.run(
            uv,
            &["python".into(), "install".into(), PYTHON_VERSION.into()],
        )
        .await?;
        self.run(
            uv,
            &[
                "venv".into(),
                "--python".into(),
                PYTHON_VERSION.into(),
                environment.as_os_str().to_owned(),
            ],
        )
        .await?;
        let python = environment.join("bin/python");
        let requirements = environment.join("requirements.lock.txt");
        if source.join("uv.lock").is_file() {
            // The release's own lock, exported with hashes: each Locals version runs on what it was tested with.
            self.run(
                uv,
                &[
                    "export".into(),
                    "--project".into(),
                    source.as_os_str().to_owned(),
                    "--frozen".into(),
                    "--no-dev".into(),
                    "--no-emit-project".into(),
                    "--format".into(),
                    "requirements-txt".into(),
                    "--output-file".into(),
                    requirements.as_os_str().to_owned(),
                ],
            )
            .await?;
        } else if Some(version) == pinned_release().ok().map(|(pinned, _)| pinned) {
            tokio::fs::write(&requirements, REQUIREMENTS_LOCK).await?;
        } else {
            bail!("Locals {version} has no uv.lock to install its dependencies from");
        }
        record["requirements_fingerprint"] = json!(format!(
            "{:x}",
            Sha256::digest(tokio::fs::read(&requirements).await?)
        ));
        self.reporter
            .progress("installing_locals", json!({"version":version.to_string()}));
        self.run(
            uv,
            &[
                "pip".into(),
                "install".into(),
                "--python".into(),
                python.as_os_str().to_owned(),
                "--no-deps".into(),
                "--require-hashes".into(),
                "--requirement".into(),
                requirements.as_os_str().to_owned(),
            ],
        )
        .await?;
        self.run(
            uv,
            &[
                "pip".into(),
                "install".into(),
                "--python".into(),
                python.as_os_str().to_owned(),
                "--no-deps".into(),
                source.as_os_str().to_owned(),
            ],
        )
        .await?;
        self.reporter
            .progress("verifying", json!({"version":version.to_string()}));
        let verification = Command::new(&python)
            .args([
                "-c",
                "import sys, json, importlib.metadata; import locals.server; import mlx.core as mx; \
                 assert sys.version_info[:2] == (3, 12); \
                 found = importlib.metadata.version('locals'); \
                 assert found == sys.argv[1], f'installed Locals {found}, expected {sys.argv[1]}'; \
                 print(json.dumps(mx.device_info()))",
                &version.to_string(),
            ])
            .output()
            .await
            .context("Could not verify the Locals installation")?;
        if !verification.status.success() {
            bail!(
                "Locals verification failed: {}",
                String::from_utf8_lossy(&verification.stderr).trim()
            );
        }
        Ok(Installation {
            python_path: python,
            python_version: PYTHON_VERSION.into(),
            locals_version: version.to_string(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            source_path: matches!(target, Target::Local { .. }).then(|| source.clone()),
            source: record,
            recommended_gpu_bytes: String::from_utf8_lossy(&verification.stdout)
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Value>(line).ok())
                .and_then(|info| info["max_recommended_working_set_size"].as_u64()),
        })
    }

    async fn run(&self, executable: &Path, args: &[std::ffi::OsString]) -> Result<()> {
        let mut child = Command::new(executable)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start uv")?;
        let stdout = child.stdout.take().context("uv stdout is unavailable")?;
        let stderr = child.stderr.take().context("uv stderr is unavailable")?;
        let mut out = BufReader::new(stdout).lines();
        let mut err = BufReader::new(stderr).lines();
        let (mut out_done, mut err_done) = (false, false);
        let mut tail = std::collections::VecDeque::new();
        while !out_done || !err_done {
            let line = tokio::select! {
                line = out.next_line(), if !out_done => { match line? { Some(line) => Some(line), None => {out_done=true; None} } },
                line = err.next_line(), if !err_done => { match line? { Some(line) => Some(line), None => {err_done=true; None} } },
            };
            if let Some(line) = line {
                if tail.len() >= 20 {
                    tail.pop_front();
                }
                tail.push_back(line.clone());
                self.reporter.log(&line);
            }
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!(
                "uv failed ({status}): {}",
                tail.into_iter().collect::<Vec<_>>().join("\n")
            );
        }
        Ok(())
    }
}

/// The newest `vX.Y.Z` tag of the Locals repository at or above the minimum, with its commit.
async fn latest_release() -> Result<(Version, String)> {
    let tags: Value = http_client()?
        .get(format!(
            "https://api.github.com/repos/{REPOSITORY}/tags?per_page=100"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Could not reach GitHub")?
        .error_for_status()
        .context("GitHub did not list the Locals releases")?
        .json()
        .await?;
    newest_tag(&tags).context("No Locals release tag was found")
}

fn newest_tag(tags: &Value) -> Option<(Version, String)> {
    tags.as_array()?
        .iter()
        .filter_map(|tag| {
            let name = tag["name"].as_str()?;
            let version = name
                .starts_with('v')
                .then(|| Version::parse(name))
                .flatten()?;
            let revision = tag["commit"]["sha"].as_str()?;
            (version >= MINIMUM_VERSION
                && revision.len() == 40
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| (version, revision.to_owned()))
        })
        .max_by_key(|(version, _)| *version)
}

fn pinned_release() -> Result<(Version, String)> {
    let pin: Value = serde_json::from_str(SOURCE_LOCK).context("Invalid bundled Locals pin")?;
    let revision = pin["revision"]
        .as_str()
        .context("The Locals pin has no revision")?;
    let version = pin["version"]
        .as_str()
        .and_then(Version::parse)
        .context("The Locals pin has no version")?;
    if pin["repository"] != format!("https://github.com/{REPOSITORY}")
        || revision.len() != 40
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("The bundled Locals pin is invalid");
    }
    Ok((version, revision.to_owned()))
}

async fn pyproject_version(root: &Path) -> Result<Version> {
    let pyproject = tokio::fs::read_to_string(root.join("pyproject.toml"))
        .await
        .with_context(|| format!("{} has no pyproject.toml", root.display()))?;
    pyproject
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "version").then(|| Version::parse(value.trim().trim_matches('"')))?
        })
        .with_context(|| {
            format!(
                "{} does not declare a version",
                root.join("pyproject.toml").display()
            )
        })
}

fn source_fingerprint(root: &Path) -> Result<String> {
    fn collect(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if entry.file_name() != "__pycache__" {
                    collect(&entry.path(), files)?;
                }
            } else if matches!(
                entry.path().extension().and_then(|e| e.to_str()),
                Some("py" | "json")
            ) {
                files.push(entry.path());
            }
        }
        Ok(())
    }
    let mut files = vec![root.join("pyproject.toml")];
    collect(&root.join("locals"), &mut files)?;
    files.sort();
    let mut digest = Sha256::new();
    for path in files {
        digest.update(path.strip_prefix(root)?.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(std::fs::read(path)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

async fn find_uv() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("uv")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/bin/uv"));
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/uv"),
        PathBuf::from("/usr/local/bin/uv"),
    ]);
    for candidate in candidates {
        if Command::new(&candidate)
            .arg("--version")
            .output()
            .await
            .is_ok_and(|result| result.status.success())
        {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(root: &Path, source: Option<PathBuf>) -> LocalsRuntime {
        let (events, _) = tokio::sync::broadcast::channel(64);
        LocalsRuntime::new(
            AppPaths {
                data_dir: root.join("data"),
                database_path: root.join("db"),
                socket_path: root.join("socket"),
                models_dir: root.join("models"),
                locals_source: source,
            },
            events,
        )
    }

    /// An environment whose interpreter exists, as `uv venv` leaves it.
    fn environment(root: &Path, name: &str, version: &str, revision: &str) -> Installation {
        let python = root.join("data/runtime").join(name).join("bin/python");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        Installation {
            python_path: python,
            python_version: PYTHON_VERSION.into(),
            locals_version: version.into(),
            installed_at: "test".into(),
            source_path: None,
            source: json!({"kind":"upstream","revision":revision}),
            recommended_gpu_bytes: None,
        }
    }

    #[test]
    fn the_newest_release_tag_at_or_above_the_minimum_is_chosen() {
        let sha = |c: char| c.to_string().repeat(40);
        let tags = json!([
            {"name":"v0.2.1","commit":{"sha":sha('a')}},
            {"name":"v0.2.10","commit":{"sha":sha('b')}},
            {"name":"v0.2.9","commit":{"sha":sha('c')}},
            {"name":"nightly","commit":{"sha":sha('d')}},
            {"name":"v0.1.9","commit":{"sha":sha('e')}},
            {"name":"v0.3.0","commit":{"sha":"short"}},
        ]);
        assert_eq!(newest_tag(&tags), Some((Version(0, 2, 10), sha('b'))));
        assert_eq!(
            newest_tag(&json!([{"name":"v0.1.0","commit":{"sha":sha('a')}}])),
            None
        );
    }

    #[test]
    fn an_update_reinstalls_only_a_different_release_or_changed_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let installed = environment(temp.path(), "env-a", "0.2.1", &"a".repeat(40));
        let same = Target::Release {
            version: Version(0, 2, 1),
            revision: "a".repeat(40),
        };
        let newer = Target::Release {
            version: Version(0, 2, 2),
            revision: "b".repeat(40),
        };
        assert!(same.installed_as(&installed));
        assert!(!newer.installed_as(&installed));

        let mut local = installed.clone();
        local.source = json!({"kind":"local","fingerprint":"one"});
        let checkout = |fingerprint: &str| Target::Local {
            path: temp.path().into(),
            version: Version(0, 2, 1),
            fingerprint: fingerprint.into(),
        };
        assert!(checkout("one").installed_as(&local));
        assert!(!checkout("two").installed_as(&local));
        assert!(
            !same.installed_as(&local),
            "a release replaces a checkout install"
        );
    }

    #[tokio::test]
    async fn installations_from_older_studios_and_newer_locals_versions_are_usable() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path(), None);
        let legacy = environment(temp.path(), "env-legacy", "0.2.1", &"a".repeat(40));
        // The flat manifest older Studio versions wrote, with a plain source_path.
        let mut document = serde_json::to_value(&legacy).unwrap();
        document["source_path"] = json!("/somewhere/Locals");
        std::fs::write(
            runtime.manifest_path(),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let status = runtime.status().await.unwrap();
        assert_eq!(status["installed"], true);
        assert_eq!(status["version"], "0.2.1");

        let newer = environment(temp.path(), "env-newer", "0.2.2", &"b".repeat(40));
        write_json(
            &runtime.manifest_path(),
            &StoredManifest {
                current: newer,
                previous: Some(legacy),
            },
        )
        .await
        .unwrap();
        let status = runtime.status().await.unwrap();
        assert_eq!(
            status["version"], "0.2.2",
            "versions above the minimum are not rejected"
        );
        assert_eq!(status["previous"]["version"], "0.2.1");
        // An older Studio still reads the current installation at the top level.
        let flat: Value = read_json(&runtime.manifest_path()).await.unwrap();
        assert_eq!(flat["locals_version"], "0.2.2");

        runtime.rollback().await.unwrap();
        let status = runtime.status().await.unwrap();
        assert_eq!(status["version"], "0.2.1");
        assert_eq!(status["previous"]["version"], "0.2.2");
        let launcher = runtime.launcher().await.unwrap();
        assert_eq!(launcher.version, "0.2.1");
        assert_eq!(launcher.root, temp.path().join("data/runtime/env-legacy"));
    }

    #[tokio::test]
    async fn garbage_collection_keeps_current_previous_and_running_environments() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path(), None);
        let current = environment(temp.path(), "env-current", "0.2.2", &"b".repeat(40));
        let previous = environment(temp.path(), "env-previous", "0.2.1", &"a".repeat(40));
        environment(temp.path(), "env-running", "0.2.0", &"c".repeat(40));
        environment(temp.path(), "env-unused", "0.2.0", &"d".repeat(40));
        std::fs::create_dir_all(temp.path().join("data/runtime/src-leftover")).unwrap();
        write_json(
            &runtime.manifest_path(),
            &StoredManifest {
                current,
                previous: Some(previous),
            },
        )
        .await
        .unwrap();
        let mut removed = runtime
            .collect_garbage(&HashSet::from([temp
                .path()
                .join("data/runtime/env-running")]))
            .await
            .unwrap();
        removed.sort();
        assert_eq!(
            removed,
            vec![
                temp.path().join("data/runtime/env-unused"),
                temp.path().join("data/runtime/src-leftover"),
            ]
        );
        assert!(runtime.manifest_path().is_file());
    }

    #[tokio::test]
    async fn a_local_checkout_is_installed_in_its_own_version() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("Locals");
        std::fs::create_dir_all(checkout.join("locals")).unwrap();
        std::fs::write(
            checkout.join("pyproject.toml"),
            "[project]\nname = \"locals\"\nversion = \"0.2.2\"\n",
        )
        .unwrap();
        let runtime = runtime(temp.path(), Some(checkout.clone()));
        let latest = runtime.check(true).await;
        assert_eq!(latest.version.as_deref(), Some("0.2.2"));
        let Target::Local { version, .. } = runtime.target(false).await.unwrap() else {
            panic!("a checkout is installed from its folder");
        };
        assert_eq!(version, Version(0, 2, 2));

        std::fs::write(
            checkout.join("pyproject.toml"),
            "[project]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let error = runtime.target(false).await.unwrap_err();
        assert!(format!("{error:#}").contains("requires Locals 0.2.1 or newer"));
    }
}
