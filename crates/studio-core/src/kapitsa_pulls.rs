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
        let repos = plan_repos(&plan);
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
                    let blocked = if served_web_page(&log) {
                        blocked_repos(&repos).await
                    } else {
                        Vec::new()
                    };
                    let message = failure_message(&log, &blocked)
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

/// The Hugging Face repositories a pull reads, with their pinned revisions: the model and its companions.
fn plan_repos(plan: &Value) -> Vec<(String, String)> {
    let entry = |item: &Value| {
        Some((
            item["repo"].as_str()?.to_owned(),
            item["revision"].as_str().unwrap_or("main").to_owned(),
        ))
    };
    entry(plan)
        .into_iter()
        .chain(
            plan["companions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(entry),
        )
        .collect()
}

/// Kapitsa 0.2.39 aborts when an HTML page arrives where a JSON file was expected.
fn served_web_page(log: &str) -> bool {
    log.contains("json.exception.parse_error") && log.contains("last read: '<'")
}

fn hugging_face_host(host: &str) -> bool {
    ["huggingface.co", "hf.co"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// The repositories this network does not let through, each with the host that answered instead.
async fn blocked_repos(repos: &[(String, String)]) -> Vec<(String, String)> {
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
    else {
        return Vec::new();
    };
    let mut blocked = Vec::new();
    for (repo, revision) in repos {
        let url = format!("https://huggingface.co/{repo}/resolve/{revision}/config.json");
        let Ok(response) = client.head(&url).send().await else {
            continue;
        };
        let elsewhere = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|location| location.to_str().ok())
            .and_then(|location| reqwest::Url::parse(location).ok())
            .and_then(|location| location.host_str().map(str::to_owned))
            .filter(|host| !hugging_face_host(host));
        let html = response.status().is_success()
            && response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|kind| kind.to_str().ok())
                .is_some_and(|kind| kind.starts_with("text/html"));
        if let Some(host) = elsewhere {
            blocked.push((repo.clone(), host));
        } else if html {
            blocked.push((repo.clone(), "a web page".into()));
        }
    }
    blocked
}

/// The reason a pull stopped, from Kapitsa's error output and the repositories found blocked.
fn failure_message(log: &str, blocked: &[(String, String)]) -> Option<String> {
    let last = log
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    if served_web_page(log) {
        if !blocked.is_empty() {
            let names = blocked
                .iter()
                .map(|(repo, host)| format!("huggingface.co/{repo} (answered by {host})"))
                .collect::<Vec<_>>()
                .join(", ");
            return Some(format!(
                "This network blocks {names}, so Kapitsa could not fetch the files that speed this model up and stopped. A web filter such as a company security gateway sends its own page instead. Ask for huggingface.co/{} to be allowed, or download on another network, then try again: the parts already saved are kept.",
                blocked[0].0
            ));
        }
        return Some("Hugging Face answered with a web page instead of a model file, and Kapitsa stopped. A proxy or web filter on this network (such as a company security gateway) is probably blocking huggingface.co, or the repository asks you to sign in. Allow huggingface.co or use another network, then download again: the parts already saved are kept.".into());
    }
    if last.contains("ignored a byte-range request") {
        let what = last
            .trim_start_matches("kapitsa: ")
            .trim_end_matches(" Run the same command again.");
        return Some(format!(
            "{what} Kapitsa downloads every file in byte ranges, and a web gateway that inspects HTTPS on this network (such as a company security gateway) answers them with whole files. Ask for huggingface.co to be exempted from HTTPS inspection, or download on another network: the parts already saved are kept."
        ));
    }
    if let Some(reason) =
        last.strip_prefix("libc++abi: terminating due to uncaught exception of type ")
    {
        return Some(format!("Kapitsa stopped unexpectedly: {reason}"));
    }
    Some(last.to_owned())
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

#[cfg(test)]
mod tests {
    use super::{failure_message, plan_repos};

    #[test]
    fn explains_a_web_page_served_instead_of_hugging_face_json() {
        let log = "Pulling qwen3.5-4b\nlibc++abi: terminating due to uncaught exception of type nlohmann::json_abi_v3_11_3::detail::parse_error: [json.exception.parse_error.101] parse error at line 1, column 1: syntax error while parsing value - invalid literal; last read: '<'\n";
        let message = failure_message(log, &[]).unwrap();
        assert!(
            message.contains("web page instead of a model file"),
            "{message}"
        );
        assert_eq!(
            failure_message(
                "libc++abi: terminating due to uncaught exception of type std::runtime_error: boom",
                &[]
            )
            .as_deref(),
            Some("Kapitsa stopped unexpectedly: std::runtime_error: boom")
        );
        assert_eq!(
            failure_message("Not enough disk space.\n\n", &[]).as_deref(),
            Some("Not enough disk space.")
        );
        assert_eq!(failure_message("  \n", &[]), None);
        let range = failure_message("kapitsa: The download server ignored a byte-range request for chat_template.jinja. Run the same command again.", &[]).unwrap();
        assert!(
            range.starts_with("The download server ignored a byte-range request for chat_template.jinja. Kapitsa downloads"),
            "{range}"
        );
    }

    #[test]
    fn lists_the_model_and_companion_repositories() {
        let plan = serde_json::json!({"repo": "nace-ai/Qwen3.5-4B-MLX", "revision": "9f7d", "companions": [
            {"kind": "mtp-sidecar", "repo": "Qwen/Qwen3.5-4B", "revision": "851b"},
            {"kind": "dflash", "repo": "z-lab/Qwen3.5-4B-DFlash"}]});
        assert_eq!(
            plan_repos(&plan),
            [
                ("nace-ai/Qwen3.5-4B-MLX", "9f7d"),
                ("Qwen/Qwen3.5-4B", "851b"),
                ("z-lab/Qwen3.5-4B-DFlash", "main")
            ]
            .map(|(repo, revision)| (repo.to_owned(), revision.to_owned()))
        );
    }
}
