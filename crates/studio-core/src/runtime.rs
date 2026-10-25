use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
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

use crate::{
    events::{self, EventSender},
    paths::AppPaths,
};

pub const PYTHON_VERSION: &str = "3.12";
pub const LOCALS_VERSION: &str = "0.2.1";
static INSTALL_LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
const REQUIREMENTS_LOCK: &str = include_str!("../../../resources/runtime/requirements.lock.txt");
const SOURCE_LOCK: &str = include_str!("../../../resources/runtime/source.lock.json");

#[derive(Clone)]
pub struct RuntimeManager {
    paths: AppPaths,
    events: EventSender,
    install_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Installation {
    python_path: PathBuf,
    python_version: String,
    locals_version: String,
    installed_at: String,
    source_path: PathBuf,
    #[serde(default)]
    source: Value,
    #[serde(default)]
    recommended_gpu_bytes: Option<u64>,
}

impl RuntimeManager {
    pub fn new(paths: AppPaths, events: EventSender) -> Self {
        Self {
            paths,
            events,
            install_lock: INSTALL_LOCK
                .get_or_init(|| Arc::new(Mutex::new(())))
                .clone(),
        }
    }

    fn manifest_path(&self) -> PathBuf {
        self.paths.data_dir.join("runtime/current.json")
    }

    async fn installation(&self) -> Option<Installation> {
        let bytes = tokio::fs::read(self.manifest_path()).await.ok()?;
        let installed: Installation = serde_json::from_slice(&bytes).ok()?;
        if installed.locals_version != LOCALS_VERSION
            || installed.python_version != PYTHON_VERSION
            || !installed.python_path.is_file()
        {
            return None;
        }
        Some(installed)
    }

    pub async fn status(&self) -> Result<Value> {
        let installation = self.installation().await;
        Ok(json!({
            "installed": installation.is_some(), "installation": installation,
            "required_python": PYTHON_VERSION, "required_locals": LOCALS_VERSION,
            "source_path": self.paths.locals_source,
            "uv_available": find_uv().await.is_some(),
        }))
    }

    pub async fn python(&self) -> Result<PathBuf> {
        if let Some(installed) = self.installation().await {
            return Ok(installed.python_path);
        }
        self.install().await?;
        Ok(self
            .installation()
            .await
            .context("The managed Python installation could not be verified")?
            .python_path)
    }

    pub async fn recommended_memory(&self) -> Option<u64> {
        self.installation().await?.recommended_gpu_bytes
    }

    pub async fn install(&self) -> Result<Value> {
        self.provision(false).await
    }

    /// An update stages a fresh environment; workers already running keep their own interpreter.
    pub async fn update(&self) -> Result<Value> {
        self.provision(true).await
    }

    async fn provision(&self, replace: bool) -> Result<Value> {
        let _guard = self.install_lock.lock().await;
        if !replace && self.installation().await.is_some() {
            return self.status().await;
        }
        let uv = find_uv().await.context("uv is required to install the managed runtime. Install uv, then retry Runtime installation.")?;
        let (source_argument, source_record) = self.source().await?;
        let root = self.paths.data_dir.join("runtime");
        tokio::fs::create_dir_all(&root).await?;
        let environment = root.join(format!("env-{}", uuid::Uuid::new_v4()));
        let python = environment.join("bin/python");
        events::emit(
            &self.events,
            "runtime.progress",
            &json!({"phase":"installing_python", "python":PYTHON_VERSION}),
        );
        let result: Result<()> = async {
            self.run(&uv, &["python".into(), "install".into(), PYTHON_VERSION.into()]).await?;
            self.run(&uv, &["venv".into(), "--python".into(), PYTHON_VERSION.into(), environment.as_os_str().to_owned()]).await?;
            let requirements=environment.join("requirements.lock.txt");
            tokio::fs::write(&requirements,REQUIREMENTS_LOCK).await?;
            events::emit(&self.events, "runtime.progress", &json!({"phase":"installing_locals", "version":LOCALS_VERSION}));
            self.run(&uv, &["pip".into(), "install".into(), "--python".into(), python.as_os_str().to_owned(), "--no-deps".into(), "--require-hashes".into(), "--requirement".into(),requirements.as_os_str().to_owned()]).await?;
            self.run(&uv, &["pip".into(), "install".into(), "--python".into(), python.as_os_str().to_owned(),"--no-deps".into(),source_argument]).await?;
            let verification = Command::new(&python).args(["-c", "import sys, json, importlib.metadata; import locals.server; import mlx.core as mx; assert sys.version_info[:2] == (3,12); assert importlib.metadata.version('locals') == '0.2.1'; print(json.dumps(mx.device_info()))"])
                .output().await.context("Could not verify the managed runtime")?;
            if !verification.status.success() {
                bail!("Managed runtime verification failed: {}", String::from_utf8_lossy(&verification.stderr));
            }
            let installed = Installation {
                python_path: python.clone(), python_version: PYTHON_VERSION.into(), locals_version: LOCALS_VERSION.into(),
                installed_at: chrono::Utc::now().to_rfc3339(), source_path:self.paths.locals_source.clone(),source:source_record,
                recommended_gpu_bytes:String::from_utf8_lossy(&verification.stdout).lines().rev()
                    .find_map(|line|serde_json::from_str::<Value>(line).ok())
                    .and_then(|info|info["max_recommended_working_set_size"].as_u64()),
            };
            let temporary = root.join(format!("current-{}.json", uuid::Uuid::new_v4()));
            tokio::fs::write(&temporary, serde_json::to_vec_pretty(&installed)?).await?;
            tokio::fs::rename(temporary, self.manifest_path()).await?;
            Ok(())
        }.await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_dir_all(&environment).await;
            events::emit(
                &self.events,
                "runtime.error",
                &json!({"error":format!("{error:#}")}),
            );
            return Err(error);
        }
        events::emit(
            &self.events,
            "runtime.progress",
            &json!({"phase":"ready", "version":LOCALS_VERSION}),
        );
        self.status().await
    }

    async fn source(&self) -> Result<(std::ffi::OsString, Value)> {
        if self.paths.locals_source.join("pyproject.toml").is_file() {
            let path = self.paths.locals_source.canonicalize()?;
            let pyproject = tokio::fs::read_to_string(path.join("pyproject.toml")).await?;
            let version = pyproject.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "version").then(|| value.trim().trim_matches('"').to_owned())
            });
            if version.as_deref() != Some(LOCALS_VERSION) {
                bail!(
                    "This Studio build requires Locals {LOCALS_VERSION}; the supplied source reports {:?}",
                    version
                );
            }
            return Ok((
                path.as_os_str().to_owned(),
                json!({"kind":"local","path":path,"version":version,"fingerprint":source_fingerprint(&path)?,"requirements_fingerprint":format!("{:x}",Sha256::digest(REQUIREMENTS_LOCK.as_bytes()))}),
            ));
        }
        if std::env::var_os("LOCALS_STUDIO_LOCALS_SOURCE").is_some() {
            bail!("The explicit LOCALS_STUDIO_LOCALS_SOURCE override has no pyproject.toml");
        }
        let pin: Value =
            serde_json::from_str(SOURCE_LOCK).context("Invalid bundled runtime source pin")?;
        let repository = pin["repository"]
            .as_str()
            .context("Runtime source repository is missing")?;
        let revision = pin["revision"]
            .as_str()
            .context("Runtime source revision is missing")?;
        if repository != "https://github.com/Generated-Computers/Locals"
            || revision.len() != 40
            || !revision.bytes().all(|b| b.is_ascii_hexdigit())
            || pin["version"] != LOCALS_VERSION
        {
            bail!("The bundled Locals source pin is invalid");
        }
        let url = format!("{repository}/archive/{revision}.tar.gz");
        Ok((
            url.clone().into(),
            json!({"kind":"upstream","url":url,"revision":revision,"version":LOCALS_VERSION,"requirements_fingerprint":format!("{:x}",Sha256::digest(REQUIREMENTS_LOCK.as_bytes()))}),
        ))
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
                events::emit(&self.events, "runtime.log", &json!({"line":line}));
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
