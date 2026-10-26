use crate::{
    paths::AppPaths,
    types::{RpcRequest, RpcResponse},
    upgrade,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, sync::OnceLock, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, mpsc},
};

pub const REQUIRED_CAPABILITIES: &[&str] = &["worker_activity_v1", "worker_log_entries_v1"];
static CONNECTION_UPDATE: OnceLock<Mutex<()>> = OnceLock::new();
pub fn service_info() -> Value {
    json!({"service":"locals-studio","version":env!("CARGO_PKG_VERSION"),"protocol":1,"capabilities":REQUIRED_CAPABILITIES})
}
fn compatible(info: &Value) -> bool {
    info["service"] == "locals-studio"
        && REQUIRED_CAPABILITIES.iter().all(|required| {
            info["capabilities"]
                .as_array()
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(required)))
        })
}
fn connection_info(mut info: Value, pending: Option<String>) -> Value {
    if !info.is_object() {
        info = json!({"service":"unrecognized"});
    }
    info["upgrade"] = match pending {
        Some(reason) => json!({"status":"pending","reason":reason}),
        None => json!({"status":"current"}),
    };
    info
}

async fn request_on(stream: &mut UnixStream, method: &str, params: Value) -> Result<Value> {
    let mut bytes = serde_json::to_vec(&RpcRequest {
        method: method.into(),
        params,
    })?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        bail!("Service closed the connection before replying");
    }
    let response: RpcResponse = serde_json::from_str(&line).context("Invalid service reply")?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    Ok(response.result.unwrap_or(Value::Null))
}
async fn raw_request_at(paths: &AppPaths, method: &str, params: Value) -> Result<Value> {
    let mut stream = UnixStream::connect(&paths.socket_path)
        .await
        .context("Locals Studio service is not running")?;
    request_on(&mut stream, method, params).await
}
struct Probe {
    info: Value,
    peer: std::result::Result<upgrade::Peer, String>,
}
async fn inspect(paths: &AppPaths) -> Option<Probe> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut stream = UnixStream::connect(&paths.socket_path).await?;
        let peer = upgrade::peer(&stream).map_err(|error| error.to_string());
        let info = request_on(&mut stream, "ping", json!({})).await?;
        Ok::<_, anyhow::Error>(Probe { info, peer })
    })
    .await
    .ok()?
    .ok()
}
pub async fn request(method: &str, params: Value) -> Result<Value> {
    let paths = AppPaths::discover()?;
    let info = ensure_connection(&paths).await?;
    let mut result = raw_request_at(&paths, method, params).await?;
    if matches!(method, "snapshot" | "status") && result.is_object() {
        result["service"] = info;
    }
    Ok(result)
}
pub async fn ensure_service() -> Result<()> {
    ensure_connection(&AppPaths::discover()?).await.map(|_| ())
}

async fn ensure_connection(paths: &AppPaths) -> Result<Value> {
    if let Some(probe) = inspect(paths).await {
        if compatible(&probe.info) {
            return Ok(connection_info(probe.info, None));
        }
    }
    let _local = CONNECTION_UPDATE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .await;
    paths.prepare()?;
    let _lock = match upgrade::lock(&paths.data_dir.join("service-upgrade.lock")).await {
        Ok(lock) => lock,
        Err(error) => {
            if let Some(probe) = inspect(paths).await {
                return Ok(connection_info(probe.info, Some(error.to_string())));
            }
            return Err(error);
        }
    };
    if let Some(probe) = inspect(paths).await {
        if compatible(&probe.info) {
            return Ok(connection_info(probe.info, None));
        }
        let update = upgrade_legacy(paths, &probe).await;
        match update {
            Ok(candidate) => {
                if let Some(candidate) = candidate {
                    launch_service(paths, Some(candidate)).await?;
                }
                let info = inspect(paths)
                    .await
                    .context("The updated service did not answer")?
                    .info;
                if !compatible(&info) {
                    bail!("The updated management service is missing required features");
                }
                return Ok(connection_info(info, None));
            }
            Err(error) => return Ok(connection_info(probe.info, Some(format!("{error:#}")))),
        }
    }
    launch_service(paths, None).await?;
    let info = inspect(paths)
        .await
        .context("Management service did not answer")?
        .info;
    Ok(connection_info(info, None))
}

async fn upgrade_legacy(paths: &AppPaths, probe: &Probe) -> Result<Option<PathBuf>> {
    // Only known legacy 0.1.0 uses default SIGTERM semantics that preserve workers. Other
    // incompatible protocols need a negotiated handoff; never guess their signal behaviour.
    if probe.info["service"] != "locals-studio" || probe.info["version"] != "0.1.0" {
        bail!("This management service requires an explicit compatible update");
    }
    let peer = probe.peer.clone().map_err(anyhow::Error::msg)?;
    let process = upgrade::identity(&peer)?;
    upgrade::verify_daemon(&process)?;
    let candidate =
        launcher_path().context("Cannot find the bundled replacement management service")?;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(&candidate)
            .arg("service-info")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("Replacement management service inspection timed out")??;
    let replacement: Value = serde_json::from_slice(&output.stdout)
        .context("The bundled service does not support this application yet")?;
    if !output.status.success() || !compatible(&replacement) {
        bail!("The bundled management service does not provide the required features");
    }
    for _ in 0..2 {
        let snapshot = tokio::time::timeout(
            Duration::from_secs(8),
            raw_request_at(paths, "snapshot", json!({})),
        )
        .await
        .context("Management activity verification timed out")??;
        upgrade::quiescent(&snapshot)?;
        upgrade::children_are_workers(&process, &snapshot)?;
    }
    // Reconnect immediately before the signal: another client may have completed the update.
    // Legacy has no atomic quiesce RPC; even when idle, handoff briefly closes the HTTP router.
    let current = inspect(paths)
        .await
        .context("The management service changed during verification")?;
    if compatible(&current.info) {
        return Ok(None);
    }
    if current.peer.map_err(anyhow::Error::msg)? != peer || upgrade::identity(&peer)? != process {
        bail!("The management process changed during verification");
    }
    if unsafe { libc::kill(peer.pid, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let started = tokio::time::Instant::now();
    while upgrade::identity(&peer).is_ok_and(|current| current == process) {
        if started.elapsed() > Duration::from_secs(5) {
            bail!("The previous management service has not exited yet");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(Some(candidate))
}

fn launcher_path() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LOCALS_STUDIO_CLI_BIN") {
        candidates.push(PathBuf::from(path));
    }
    if current.file_stem().and_then(|name| name.to_str()) == Some("locals-studio") {
        candidates.push(current.clone());
    }
    if let Some(parent) = current.parent() {
        candidates.push(parent.join("locals-studio"));
    }
    candidates.push(root.join("target/debug/locals-studio"));
    candidates.push(root.join("target/release/locals-studio"));
    candidates.into_iter().find(|path| path.is_file())
}
async fn launch_service(paths: &AppPaths, replacement: Option<PathBuf>) -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = if let Some(path) = replacement.or_else(launcher_path) {
        let mut command = tokio::process::Command::new(path);
        command.arg("daemon");
        command
    } else if root.join("Cargo.toml").is_file() {
        let mut command = tokio::process::Command::new("cargo");
        command
            .current_dir(&root)
            .args(["run", "--quiet", "-p", "studio-cli", "--", "daemon"]);
        command
    } else {
        let mut command = tokio::process::Command::new("locals-studio");
        command.arg("daemon");
        command
    };
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data_dir.join("daemon.log"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("Cannot launch the bundled locals-studio service")?;
    let started = tokio::time::Instant::now();
    for _ in 0..600 {
        if started.elapsed() > Duration::from_secs(60) {
            break;
        }
        if inspect(paths)
            .await
            .is_some_and(|probe| compatible(&probe.info))
        {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if inspect(paths)
                .await
                .is_some_and(|probe| compatible(&probe.info))
            {
                return Ok(());
            }
            if !status.success() {
                bail!(
                    "Service exited ({status}). See {}",
                    paths.data_dir.join("daemon.log").display()
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "Service did not become available with the required features. See {}",
        paths.data_dir.join("daemon.log").display()
    )
}
pub async fn subscribe() -> Result<mpsc::UnboundedReceiver<Value>> {
    ensure_service().await?;
    let paths = AppPaths::discover()?;
    let mut stream = UnixStream::connect(&paths.socket_path).await?;
    stream
        .write_all(b"{\"method\":\"subscribe\",\"params\":{}}\n")
        .await?;
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(event) = serde_json::from_str::<Value>(&line) {
                if sender.send(event).is_err() {
                    break;
                }
            }
        }
    });
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn required_features_are_checked_even_when_versions_match() {
        let current = service_info();
        assert!(compatible(&current));
        assert!(!compatible(
            &json!({"service":"locals-studio","version":"0.1.0"})
        ));
        let mut partial = current.clone();
        partial["capabilities"] = json!(["worker_activity_v1"]);
        assert!(!compatible(&partial));
        assert_eq!(
            connection_info(partial, Some("active request".into()))["upgrade"]["status"],
            "pending"
        );
        assert_eq!(
            connection_info(Value::Null, Some("Invalid service metadata".into()))["upgrade"]["status"],
            "pending"
        );
    }
    #[tokio::test]
    async fn capable_service_is_reused_without_locks_or_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            data_dir: temp.path().join("data"),
            database_path: temp.path().join("db"),
            socket_path: temp.path().join("socket"),
            models_dir: temp.path().join("models"),
            locals_source: temp.path().join("source"),
        };
        let listener = tokio::net::UnixListener::bind(&paths.socket_path).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let mut reply = serde_json::to_vec(&RpcResponse {
                result: Some(service_info()),
                error: None,
            })
            .unwrap();
            reply.push(b'\n');
            stream.write_all(&reply).await.unwrap();
        });
        let info = ensure_connection(&paths).await.unwrap();
        assert_eq!(info["upgrade"]["status"], "current");
        assert!(!paths.data_dir.exists());
        task.await.unwrap();
    }
}
