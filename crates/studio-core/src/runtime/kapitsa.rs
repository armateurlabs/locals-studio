//! The Kapitsa engine: a release tarball per installed version, checked against its published
//! SHA-256 and run with automatic self-updates off, so only Studio changes the installed version.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{process::Command, sync::Mutex};

use super::{
    CHECK_TTL, LatestRelease, Launcher, Manifest, Reporter, Version, download, extract,
    http_client, read_json, remove_unused, single_root, write_json,
};
use crate::{events::EventSender, paths::AppPaths, types::Engine};

/// The oldest Kapitsa whose `serve` options, `/health` and `/engine/status` this Studio drives.
pub const MINIMUM_VERSION: Version = Version(0, 2, 39);
const REPOSITORY: &str = "nace-ai/kapitsa";

static INSTALL_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
static LATEST: OnceLock<Arc<StdMutex<LatestRelease>>> = OnceLock::new();

/// Environment for every Kapitsa process Studio starts: never update itself in place, never
/// prompt, and plain output for logs.
pub const ENVIRONMENT: &[(&str, &str)] = &[
    ("KAPITSA_NO_AUTO_UPDATE", "1"),
    ("KAPITSA_NO_PROMPT", "1"),
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub version: String,
    pub binary: PathBuf,
    pub installed_at: String,
    pub sha256: String,
    pub url: String,
}

impl Installation {
    fn usable(&self) -> bool {
        Version::parse(&self.version).is_some_and(|version| version >= MINIMUM_VERSION)
            && self.binary.is_file()
    }

    /// `versions/<version>-<id>`, holding the extracted release.
    fn root(&self) -> Option<PathBuf> {
        Some(self.binary.parent()?.parent()?.to_owned())
    }
}

#[derive(Clone)]
pub struct KapitsaRuntime {
    paths: AppPaths,
    reporter: Reporter,
    install_lock: Arc<Mutex<()>>,
    latest: Arc<StdMutex<LatestRelease>>,
}

impl KapitsaRuntime {
    pub fn new(paths: AppPaths, events: EventSender) -> Self {
        Self {
            paths,
            reporter: Reporter::new(events, Engine::Kapitsa),
            install_lock: INSTALL_LOCK.get_or_init(Default::default).clone(),
            latest: LATEST.get_or_init(Default::default).clone(),
        }
    }

    fn root(&self) -> PathBuf {
        self.paths.data_dir.join("engines/kapitsa")
    }

    fn versions(&self) -> PathBuf {
        self.root().join("versions")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root().join("manifest.json")
    }

    async fn manifest(&self) -> Option<Manifest<Installation>> {
        read_json(&self.manifest_path()).await
    }

    pub async fn installation(&self) -> Option<Installation> {
        self.manifest()
            .await
            .map(|manifest| manifest.current)
            .filter(Installation::usable)
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
        let installed = installation
            .as_ref()
            .and_then(|installation| Version::parse(&installation.version));
        let available = latest.version.as_deref().and_then(Version::parse);
        Ok(json!({
            "engine": Engine::Kapitsa,
            "name": Engine::Kapitsa.label(),
            "installed": installation.is_some(),
            "version": installed.map(|version| version.to_string()),
            "installation": installation,
            "previous": previous.map(|previous| json!({
                "version": previous.version,
                "installed_at": previous.installed_at,
            })),
            "minimum_version": MINIMUM_VERSION.to_string(),
            "latest": latest,
            "update_available": match (installed, available) {
                (Some(installed), Some(available)) => available > installed,
                _ => false,
            },
            "source": "release",
        }))
    }

    pub async fn check(&self, force: bool) -> LatestRelease {
        let cached = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !force && cached.error.is_none() && cached.fresh(CHECK_TTL) {
            return cached;
        }
        let release = match latest_release().await {
            Ok(version) if version >= MINIMUM_VERSION => LatestRelease::found(version, None),
            Ok(version) => LatestRelease::failed(
                &cached,
                &anyhow::anyhow!(
                    "The latest Kapitsa release, {version}, is older than the {MINIMUM_VERSION} this Studio requires"
                ),
            ),
            Err(error) => LatestRelease::failed(&cached, &error),
        };
        *self.latest.lock().unwrap_or_else(|e| e.into_inner()) = release.clone();
        release
    }

    async fn target(&self) -> Result<Version> {
        let latest = self.check(true).await;
        match latest.version.as_deref().and_then(Version::parse) {
            Some(version) if latest.error.is_none() => Ok(version),
            _ => bail!(
                "Could not find the latest Kapitsa release: {}",
                latest.error.as_deref().unwrap_or("no release found")
            ),
        }
    }

    pub async fn install(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        if self.installation().await.is_none() {
            let result = async {
                let version = self.target().await?;
                self.provision(version).await
            }
            .await;
            if let Err(error) = result {
                self.reporter.error(&error);
                return Err(error);
            }
        }
        self.status().await
    }

    pub async fn update(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        let result = async {
            let version = self.target().await?;
            if let Some(current) = self.installation().await
                && Version::parse(&current.version) == Some(version)
            {
                self.reporter
                    .progress("up_to_date", json!({"version":current.version}));
                return Ok(());
            }
            self.provision(version).await
        }
        .await;
        if let Err(error) = result {
            self.reporter.error(&error);
            return Err(error);
        }
        self.status().await
    }

    pub async fn rollback(&self) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        let manifest = self.manifest().await.context("Kapitsa is not installed")?;
        let previous = manifest
            .previous
            .filter(Installation::usable)
            .context("There is no previous Kapitsa version to return to")?;
        let version = previous.version.clone();
        write_json(
            &self.manifest_path(),
            &Manifest {
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
                    .context("The managed Kapitsa installation could not be verified")?
            }
        };
        Ok(Launcher {
            root: installation
                .root()
                .context("The Kapitsa binary is not inside a version folder")?,
            executable: installation.binary,
            version: installation.version,
        })
    }

    pub(crate) async fn collect_garbage(&self, in_use: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
        let Ok(_guard) = self.install_lock.try_lock() else {
            return Ok(Vec::new());
        };
        let Some(manifest) = self.manifest().await else {
            return Ok(Vec::new());
        };
        let mut keep = in_use.clone();
        keep.extend(manifest.current.root());
        keep.extend(manifest.previous.and_then(|previous| previous.root()));
        remove_unused(&self.versions(), "", &keep).await
    }

    async fn provision(&self, version: Version) -> Result<()> {
        let name = format!("kapitsa-{version}-darwin-arm64.tar.gz");
        let url = format!("https://github.com/{REPOSITORY}/releases/download/v{version}/{name}");
        let client = http_client()?;
        self.reporter
            .progress("downloading", json!({"version":version.to_string()}));
        let expected = client
            .get(format!("{url}.sha256"))
            .send()
            .await
            .context("Could not reach GitHub")?
            .error_for_status()
            .with_context(|| format!("Kapitsa {version} publishes no checksum for {name}"))?
            .text()
            .await?;
        let expected = parse_checksum(&expected).context("The Kapitsa checksum file is invalid")?;
        let versions = self.versions();
        tokio::fs::create_dir_all(&versions).await?;
        let id = uuid::Uuid::new_v4();
        let archive = versions.join(format!(".download-{id}.tar.gz"));
        let reporter = self.reporter.clone();
        let downloaded = download(&client, &url, &archive, |received, total| {
            reporter.progress(
                "downloading",
                json!({"version":version.to_string(),"received_bytes":received,"total_bytes":total}),
            );
        })
        .await;
        let result = match downloaded {
            Ok(actual) => {
                self.install_archive(
                    &archive,
                    version,
                    &expected,
                    &actual,
                    &url,
                    &versions.join(format!("{version}-{id}")),
                )
                .await
            }
            Err(error) => Err(error),
        };
        let _ = tokio::fs::remove_file(&archive).await;
        result
    }

    /// Unpack a downloaded release, check that it is what was published and that it runs, then
    /// make it current. A failure leaves the current version untouched.
    pub(crate) async fn install_archive(
        &self,
        archive: &Path,
        version: Version,
        expected_sha256: &str,
        actual_sha256: &str,
        url: &str,
        destination: &Path,
    ) -> Result<()> {
        let result = async {
            if !expected_sha256.eq_ignore_ascii_case(actual_sha256) {
                bail!("The Kapitsa {version} download does not match its published checksum");
            }
            self.reporter
                .progress("installing", json!({"version":version.to_string()}));
            extract(archive, destination).await?;
            let binary = single_root(destination).await?.join("kapitsa");
            if !binary.is_file() {
                bail!("The Kapitsa {version} release has no kapitsa binary");
            }
            self.reporter
                .progress("verifying", json!({"version":version.to_string()}));
            let reported = binary_version(&binary).await?;
            if reported != version {
                bail!("The Kapitsa binary reports {reported}, expected {version}");
            }
            let installed = Installation {
                version: version.to_string(),
                binary,
                installed_at: chrono::Utc::now().to_rfc3339(),
                sha256: actual_sha256.to_owned(),
                url: url.to_owned(),
            };
            let previous = self
                .manifest()
                .await
                .map(|manifest| manifest.current)
                .filter(|previous| previous.usable() && previous.binary != installed.binary);
            write_json(
                &self.manifest_path(),
                &Manifest {
                    current: installed,
                    previous,
                },
            )
            .await
        }
        .await;
        match result {
            Ok(()) => {
                self.reporter
                    .progress("ready", json!({"version":version.to_string()}));
                Ok(())
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(destination).await;
                Err(error)
            }
        }
    }
}

/// Kapitsa's data folder, shared with the user's own Kapitsa: models, caches and run state.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("KAPITSA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache/kapitsa")))
}

/// The key of the engine view of the Kapitsa server holding the control socket.
pub fn engine_key() -> Option<String> {
    let key = std::fs::read_to_string(home()?.join("run/default.token")).ok()?;
    let key = key.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

/// One model of Kapitsa's catalog as `kapitsa --help` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogModel {
    pub name: String,
    pub alias: String,
    pub description: String,
    pub default_variant: String,
    /// The variant Kapitsa picks on Macs where the default does not fit.
    pub small_mac_variant: Option<String>,
    pub variants: Vec<CatalogVariant>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogVariant {
    pub name: String,
    pub size_bytes: u64,
    pub quantization: Option<String>,
}

/// Read the catalog from `kapitsa --help`: the binary is its only offline source (its `doctor
/// --json` plans each model over the network). Lines it does not recognise are skipped.
pub fn parse_catalog(help: &str) -> Vec<CatalogModel> {
    let mut models: Vec<CatalogModel> = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        if line.starts_with("Models") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if line.trim_start().starts_with("or a ") || (!line.is_empty() && !line.starts_with(' ')) {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        let text = line.trim();
        if indent == 2 {
            // `name  (alias)  4.6 GB  description`
            let Some((name, rest)) = text.split_once(char::is_whitespace) else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('(') else {
                continue;
            };
            let Some((alias, rest)) = rest.split_once(')') else {
                continue;
            };
            let mut words = rest.trim_start().splitn(3, ' ');
            let (Some(size), Some("GB"), Some(description)) =
                (words.next(), words.next(), words.next())
            else {
                continue;
            };
            let Some(size_bytes) = gigabytes(size) else {
                continue;
            };
            models.push(CatalogModel {
                name: name.to_owned(),
                alias: alias.to_owned(),
                description: description.trim().to_owned(),
                default_variant: String::new(),
                small_mac_variant: None,
                variants: vec![CatalogVariant {
                    name: String::new(),
                    size_bytes,
                    quantization: None,
                }],
            });
        } else if let Some(model) = models.last_mut() {
            if let Some(also) = text.strip_prefix("also ") {
                // `:6bit 5.6 GB, :8bit 6.7 GB`
                for entry in also.split(", ") {
                    let mut words = entry.trim().trim_start_matches(':').split(' ');
                    if let (Some(name), Some(size), Some("GB")) =
                        (words.next(), words.next(), words.next())
                        && let Some(size_bytes) = gigabytes(size)
                    {
                        model.variants.push(CatalogVariant {
                            name: name.to_owned(),
                            size_bytes,
                            quantization: None,
                        });
                    }
                }
            } else if model.default_variant.is_empty() {
                // `4bit (MLX affine 4-bit, group 64 (Nace)); smaller Macs: 16gb`
                let (head, small) = match text.rsplit_once("; smaller Macs: ") {
                    Some((head, small)) => (head, Some(small.trim().to_owned())),
                    None => (text, None),
                };
                let (variant, quantization) = match head.split_once(' ') {
                    Some((variant, quantization)) => (
                        variant,
                        // Strip only the outer parentheses: the description can hold its own.
                        Some(
                            quantization
                                .trim()
                                .strip_prefix('(')
                                .and_then(|inner| inner.strip_suffix(')'))
                                .unwrap_or(quantization.trim())
                                .to_owned(),
                        ),
                    ),
                    None => (head, None),
                };
                model.default_variant = variant.to_owned();
                model.small_mac_variant = small;
                if let Some(first) = model.variants.first_mut() {
                    first.name = variant.to_owned();
                    first.quantization = quantization;
                }
            }
        }
    }
    models.retain(|model| !model.default_variant.is_empty());
    models
}

fn gigabytes(text: &str) -> Option<u64> {
    let value: f64 = text.parse().ok()?;
    (value.is_finite() && value > 0.).then_some((value * 1e9) as u64)
}

/// How a download of `size_bytes` fits a Mac with `ram_bytes`, as Nearloop judges it before a plan.
pub fn fit(size_bytes: u64, ram_bytes: u64) -> &'static str {
    let share = size_bytes as f64 / ram_bytes.max(1) as f64;
    if share <= 0.66 {
        "fits"
    } else if share < 0.85 {
        "tight"
    } else {
        "too_big"
    }
}

impl KapitsaRuntime {
    /// The catalog with each variant's fit on this Mac and the downloaded variants, from the
    /// installed binary.
    pub async fn catalog(&self, ram_bytes: u64) -> Result<Value> {
        let installation = self
            .installation()
            .await
            .context("Install Kapitsa to browse its catalog")?;
        let help = command(&installation.binary).arg("--help").output().await?;
        let models = parse_catalog(&String::from_utf8_lossy(&help.stdout));
        if models.is_empty() {
            bail!("Kapitsa {} printed no catalog", installation.version);
        }
        let downloaded = self.downloaded().await.unwrap_or_default();
        let entries: Vec<Value> = models
            .iter()
            .map(|model| {
                let variants: Vec<Value> = model
                    .variants
                    .iter()
                    .map(|variant| {
                        let folder = format!("{}-{}", model.name, variant.name);
                        let local = downloaded.iter().find(|item| item["name"] == folder.as_str());
                        json!({
                            "name": variant.name,
                            "id": format!("{}:{}", model.name, variant.name),
                            "size_bytes": variant.size_bytes,
                            "quantization": variant.quantization,
                            "default": variant.name == model.default_variant,
                            "small_mac": model.small_mac_variant.as_deref() == Some(variant.name.as_str()),
                            "fit": fit(variant.size_bytes, ram_bytes),
                            "downloaded": local.is_some_and(|item| item["complete"] == true),
                            "path": local.and_then(|item| item["dir"].as_str()),
                        })
                    })
                    .collect();
                json!({
                    "name": model.name,
                    "alias": model.alias,
                    "description": model.description,
                    "default_variant": model.default_variant,
                    "variants": variants,
                })
            })
            .collect();
        Ok(
            json!({"version":installation.version,"ram_bytes":ram_bytes,"models":entries,"store":home().map(|home| home.join("models"))}),
        )
    }

    /// `kapitsa list --json`: the models in Kapitsa's store.
    pub async fn downloaded(&self) -> Result<Vec<Value>> {
        let installation = self
            .installation()
            .await
            .context("Kapitsa is not installed")?;
        let output = command(&installation.binary)
            .args(["list", "--json"])
            .output()
            .await?;
        if !output.status.success() {
            bail!(
                "kapitsa list failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let list: Value =
            serde_json::from_slice(&output.stdout).context("kapitsa list printed invalid JSON")?;
        Ok(list.as_array().cloned().unwrap_or_default())
    }

    /// What `kapitsa pull <model>` would download, and where.
    pub async fn plan(&self, model: &str) -> Result<Value> {
        let installation = self.installation().await.context("Install Kapitsa first")?;
        let output = command(&installation.binary)
            .args(["pull", model, "--dry-run", "--json"])
            .output()
            .await?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        serde_json::from_slice(&output.stdout)
            .context("kapitsa pull --dry-run printed invalid JSON")
    }

    /// A `kapitsa pull` process for the current version.
    pub async fn pull_command(&self, model: &str) -> Result<Command> {
        let installation = self.installation().await.context("Install Kapitsa first")?;
        let mut pull = command(&installation.binary);
        pull.args(["pull", model]);
        Ok(pull)
    }
}

/// A `Command` for the managed binary with Studio's environment.
pub fn command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command.envs(ENVIRONMENT.iter().copied());
    command
}

async fn binary_version(binary: &Path) -> Result<Version> {
    let output = command(binary)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("Could not run {}", binary.display()))?;
    if !output.status.success() {
        bail!(
            "{} --version failed: {}",
            binary.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Version::find(&String::from_utf8_lossy(&output.stdout))
        .context("kapitsa --version printed no version")
}

/// GitHub redirects `/releases/latest` to the newest release's tag page; this reads it without
/// the API's rate limit, as Kapitsa's own installer does.
async fn latest_release() -> Result<Version> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("Locals-Studio/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()?;
    let response = client
        .get(format!("https://github.com/{REPOSITORY}/releases/latest"))
        .send()
        .await
        .context("Could not reach GitHub")?;
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .context("GitHub did not name the latest Kapitsa release")?;
    release_from_location(location).context("GitHub named no Kapitsa release version")
}

fn release_from_location(location: &str) -> Option<Version> {
    let tag = location.rsplit_once("/releases/tag/")?.1;
    tag.starts_with('v').then(|| Version::parse(tag))?
}

/// `<sha256>  <file>` as `shasum -a 256` writes it.
fn parse_checksum(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?;
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn runtime(root: &Path) -> KapitsaRuntime {
        let (events, _) = tokio::sync::broadcast::channel(64);
        KapitsaRuntime::new(
            AppPaths {
                data_dir: root.join("data"),
                database_path: root.join("db"),
                socket_path: root.join("socket"),
                models_dir: root.join("models"),
                locals_source: None,
            },
            events,
        )
    }

    /// A release tarball whose `kapitsa --version` reports `reported`.
    fn release(root: &Path, version: &str, reported: &str) -> (PathBuf, String) {
        let folder = root.join(format!("build-{version}"));
        let inner = folder.join(format!("kapitsa-{version}-darwin-arm64"));
        std::fs::create_dir_all(&inner).unwrap();
        let binary = inner.join("kapitsa");
        std::fs::write(
            &binary,
            format!("#!/bin/sh\necho 'kapitsa {reported} (test)'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let archive = root.join(format!("kapitsa-{version}.tar.gz"));
        let status = std::process::Command::new("/usr/bin/tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&folder)
            .arg(format!("kapitsa-{version}-darwin-arm64"))
            .status()
            .unwrap();
        assert!(status.success());
        let digest = sha2::Sha256::digest(std::fs::read(&archive).unwrap());
        (archive, format!("{digest:x}"))
    }

    use sha2::Digest;

    async fn install(
        runtime: &KapitsaRuntime,
        archive: &Path,
        version: Version,
        expected: &str,
        actual: &str,
    ) -> Result<()> {
        let destination = runtime
            .versions()
            .join(format!("{version}-{}", uuid::Uuid::new_v4()));
        runtime
            .install_archive(archive, version, expected, actual, "test://", &destination)
            .await
    }

    #[tokio::test]
    async fn an_update_keeps_the_previous_version_for_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path());
        let (old, old_sha) = release(temp.path(), "0.2.39", "0.2.39");
        let (new, new_sha) = release(temp.path(), "0.2.40", "0.2.40");
        install(&runtime, &old, Version(0, 2, 39), &old_sha, &old_sha)
            .await
            .unwrap();
        install(&runtime, &new, Version(0, 2, 40), &new_sha, &new_sha)
            .await
            .unwrap();
        let status = runtime.status().await.unwrap();
        assert_eq!(status["version"], "0.2.40");
        assert_eq!(status["previous"]["version"], "0.2.39");

        runtime.rollback().await.unwrap();
        let status = runtime.status().await.unwrap();
        assert_eq!(status["version"], "0.2.39");
        assert_eq!(status["previous"]["version"], "0.2.40");
    }

    #[tokio::test]
    async fn a_mismatched_checksum_or_version_leaves_the_current_version_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path());
        let (good, good_sha) = release(temp.path(), "0.2.39", "0.2.39");
        install(&runtime, &good, Version(0, 2, 39), &good_sha, &good_sha)
            .await
            .unwrap();

        let (tampered, tampered_sha) = release(temp.path(), "0.2.40", "0.2.40");
        let error = install(
            &runtime,
            &tampered,
            Version(0, 2, 40),
            &"0".repeat(64),
            &tampered_sha,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("checksum"));

        let (lying, lying_sha) = release(temp.path(), "0.2.41", "0.2.38");
        let error = install(&runtime, &lying, Version(0, 2, 41), &lying_sha, &lying_sha)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("reports 0.2.38"));

        assert_eq!(runtime.status().await.unwrap()["version"], "0.2.39");
        let leftovers = std::fs::read_dir(runtime.versions()).unwrap().count();
        assert_eq!(leftovers, 1, "failed installations are removed");
    }

    #[tokio::test]
    async fn garbage_collection_spares_current_previous_and_running_versions() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path());
        for (version, minor) in [("0.2.39", 39), ("0.2.40", 40), ("0.2.41", 41)] {
            let (archive, sha) = release(temp.path(), version, version);
            install(&runtime, &archive, Version(0, 2, minor), &sha, &sha)
                .await
                .unwrap();
        }
        let running = std::fs::read_dir(runtime.versions())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("0.2.39")
            })
            .unwrap();
        let removed = runtime
            .collect_garbage(&HashSet::from([
                running.join("kapitsa-0.2.39-darwin-arm64/kapitsa")
            ]))
            .await
            .unwrap();
        assert!(
            removed.is_empty(),
            "0.2.39 runs, 0.2.40 is previous, 0.2.41 current"
        );
        let removed = runtime.collect_garbage(&HashSet::new()).await.unwrap();
        assert_eq!(removed, vec![running]);
    }

    #[test]
    fn the_catalog_is_read_from_the_help_of_a_real_release() {
        let catalog = parse_catalog(include_str!("kapitsa_help.txt"));
        assert_eq!(catalog.len(), 15);
        let small = &catalog[0];
        assert_eq!(
            (small.name.as_str(), small.alias.as_str()),
            ("qwen3.5-4b", "4b")
        );
        assert_eq!(small.default_variant, "4bit");
        assert_eq!(
            small.variants[0].quantization.as_deref(),
            Some("MLX affine 4-bit, group 64 (Nace)")
        );
        assert_eq!(
            small
                .variants
                .iter()
                .map(|variant| (variant.name.as_str(), variant.size_bytes))
                .collect::<Vec<_>>(),
            vec![
                ("4bit", 4_600_000_000),
                ("6bit", 5_600_000_000),
                ("8bit", 6_700_000_000),
                ("bf16", 10_600_000_000)
            ]
        );
        let moe = catalog.iter().find(|model| model.alias == "35b").unwrap();
        assert_eq!(moe.small_mac_variant.as_deref(), Some("16gb"));
        assert!(
            moe.variants
                .iter()
                .any(|variant| variant.name == "16gb" && variant.size_bytes == 10_800_000_000)
        );
        let flash = catalog.iter().find(|model| model.alias == "flash").unwrap();
        assert_eq!(flash.name, "qwen3.8-flash-next");
        assert_eq!(catalog.last().unwrap().name, "hy4-flash");
        assert_eq!(fit(4_600_000_000, 24_000_000_000), "fits");
        assert_eq!(fit(18_000_000_000, 24_000_000_000), "tight");
        assert_eq!(fit(22_100_000_000, 24_000_000_000), "too_big");
    }

    #[test]
    fn release_redirects_and_checksum_files_are_read_strictly() {
        assert_eq!(
            release_from_location("https://github.com/nace-ai/kapitsa/releases/tag/v0.2.39"),
            Some(Version(0, 2, 39))
        );
        assert_eq!(
            release_from_location("https://github.com/nace-ai/kapitsa/releases"),
            None
        );
        let hash = "600b7958e96e48465558d3efb2ee98449170d91b212a6c2a9e09ade5c655b99c";
        assert_eq!(
            parse_checksum(&format!("{hash}  kapitsa.tar.gz\n")).as_deref(),
            Some(hash)
        );
        assert_eq!(parse_checksum("not-a-hash file"), None);
    }
}
