//! Downloads from Kapitsa's catalog. `kapitsa pull` fetches a model with the companion files that
//! speed it up into Kapitsa's own store; Studio follows the folder's size for progress and adds the
//! finished model to the library.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    events::{EventSender, emit},
    runtime::KapitsaRuntime,
};

struct Job {
    state: Value,
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Clone)]
pub struct KapitsaPulls {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    events: EventSender,
}

impl KapitsaPulls {
    pub fn new(events: EventSender) -> Self {
        Self {
            jobs: Default::default(),
            events,
        }
    }

    pub fn list(&self) -> Value {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        json!(
            jobs.values()
                .map(|job| job.state.clone())
                .collect::<Vec<_>>()
        )
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Value)) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get_mut(id) {
            change(&mut job.state);
            emit(&self.events, "kapitsa.pull", &job.state);
        }
    }

    pub fn cancel(&self, id: &str) -> Result<Value> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let job = jobs.get_mut(id).context("Unknown Kapitsa download")?;
        if let Some(cancel) = job.cancel.take() {
            let _ = cancel.send(());
        }
        Ok(job.state.clone())
    }

    /// Start `kapitsa pull <model>`. `finished` runs with the model folder once it is complete.
    pub async fn start(
        &self,
        runtime: &KapitsaRuntime,
        model: &str,
        size_bytes: Option<u64>,
        finished: impl FnOnce(PathBuf) -> Result<Value> + Send + 'static,
    ) -> Result<Value> {
        let model = model.trim();
        if model.is_empty() || model.starts_with('-') || model.contains(char::is_whitespace) {
            bail!("Name a catalog model, such as 35b or qwen3.5-4b:6bit");
        }
        let plan = runtime.plan(model).await?;
        if plan["kind"] == "local" {
            bail!("{model} is a folder on this Mac: add it to the library instead");
        }
        let dir = PathBuf::from(
            plan["dir"]
                .as_str()
                .context("Kapitsa did not say where it downloads")?,
        );
        let id = plan["model"].as_str().unwrap_or(model).to_owned();
        {
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if jobs
                .get(&id)
                .is_some_and(|job| job.state["status"] == "downloading")
            {
                bail!("{id} is already downloading");
            }
        }
        let mut child = runtime
            .pull_command(&id)
            .await?
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("Could not start kapitsa pull")?;
        let (cancel, mut cancelled) = oneshot::channel();
        let state = json!({
            "id": id, "model": id, "repo": plan["repo"], "dir": dir,
            "status": "downloading", "received_bytes": folder_size(&dir), "total_bytes": size_bytes,
            "error": null, "model_record": null,
        });
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            Job {
                state: state.clone(),
                cancel: Some(cancel),
            },
        );
        emit(&self.events, "kapitsa.pull", &state);
        let pulls = self.clone();
        tokio::spawn(async move {
            let stderr = child.stderr.take();
            let tail = tokio::spawn(async move {
                let mut text = String::new();
                if let Some(mut stderr) = stderr {
                    use tokio::io::AsyncReadExt;
                    let _ = stderr.read_to_string(&mut text).await;
                }
                text
            });
            let outcome = loop {
                tokio::select! {
                    status = child.wait() => break Ok(status),
                    _ = &mut cancelled => {
                        // An interrupt keeps the parts downloaded so far, as Ctrl+C does.
                        if let Some(pid) = child.id() {
                            unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
                        }
                        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
                        let _ = child.kill().await;
                        break Err("cancelled");
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        let received = folder_size(&dir);
                        pulls.update(&id, |state| state["received_bytes"] = json!(received));
                    }
                }
            };
            let log = tail.await.unwrap_or_default();
            match outcome {
                Ok(Ok(status)) if status.success() => {
                    let record = finished(dir.clone());
                    pulls.update(&id, |state| match record {
                        Ok(record) => {
                            state["status"] = json!("completed");
                            state["received_bytes"] = json!(folder_size(&dir));
                            state["model_record"] = record;
                        }
                        Err(error) => {
                            state["status"] = json!("error");
                            state["error"] = json!(format!(
                                "Downloaded, but could not add it to the library: {error:#}"
                            ));
                        }
                    });
                }
                Ok(result) => {
                    let message = log
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("kapitsa pull failed ({result:?})"));
                    pulls.update(&id, |state| {
                        state["status"] = json!("error");
                        state["error"] = json!(message);
                    });
                }
                Err(_) => pulls.update(&id, |state| state["status"] = json!("cancelled")),
            }
        });
        Ok(state)
    }
}

/// Bytes under a folder, following no links; what a download has written so far.
fn folder_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => folder_size(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}
