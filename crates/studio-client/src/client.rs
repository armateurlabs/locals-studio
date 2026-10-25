use crate::{
    paths::AppPaths,
    types::{RpcRequest, RpcResponse},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
};

async fn connect() -> Result<UnixStream> {
    let paths = AppPaths::discover()?;
    UnixStream::connect(&paths.socket_path)
        .await
        .context("Locals Studio service is not running")
}

async fn raw_request(method: &str, params: Value) -> Result<Value> {
    let mut connection = connect().await?;
    let mut bytes = serde_json::to_vec(&RpcRequest {
        method: method.into(),
        params,
    })?;
    bytes.push(b'\n');
    connection.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(connection).read_line(&mut line).await?;
    if line.is_empty() {
        bail!("Service closed the connection before replying");
    }
    let response: RpcResponse = serde_json::from_str(&line).context("Invalid service reply")?;
    if let Some(error) = response.error {
        bail!("{error}");
    }
    Ok(response.result.unwrap_or(Value::Null))
}

pub async fn request(method: &str, params: Value) -> Result<Value> {
    ensure_service().await?;
    raw_request(method, params).await
}

pub async fn ensure_service() -> Result<()> {
    if probe().await {
        return Ok(());
    }
    let paths = AppPaths::discover()?;
    paths.prepare()?;
    let current = std::env::current_exe()?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LOCALS_STUDIO_CLI_BIN") {
        candidates.push(PathBuf::from(path));
    }
    if current.file_stem().and_then(|s| s.to_str()) == Some("locals-studio") {
        candidates.push(current.clone());
    }
    if let Some(parent) = current.parent() {
        candidates.push(parent.join("locals-studio"));
    }
    candidates.push(root.join("target/debug/locals-studio"));
    candidates.push(root.join("target/release/locals-studio"));
    let mut command = if let Some(path) = candidates.into_iter().find(|p| p.is_file()) {
        let mut c = tokio::process::Command::new(path);
        c.arg("daemon");
        c
    } else if root.join("Cargo.toml").is_file() {
        let mut c = tokio::process::Command::new("cargo");
        c.current_dir(&root)
            .args(["run", "--quiet", "-p", "studio-cli", "--", "daemon"]);
        c
    } else {
        let mut c = tokio::process::Command::new("locals-studio");
        c.arg("daemon");
        c
    };
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.data_dir.join("daemon.log"))?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    // A separate session keeps the service alive when the CLI or GUI exits.
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
    for _ in 0..600 {
        if probe().await {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            // Another starter can win the service lock; give its socket time to appear.
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if probe().await {
                return Ok(());
            }
            if !status.success() {
                bail!(
                    "Service exited ({status}). See {}",
                    paths.data_dir.join("daemon.log").display()
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    bail!(
        "Service did not become available. See {}",
        paths.data_dir.join("daemon.log").display()
    )
}

async fn probe() -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        raw_request("ping", json!({})),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

pub async fn subscribe() -> Result<mpsc::UnboundedReceiver<Value>> {
    ensure_service().await?;
    let mut stream = connect().await?;
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
