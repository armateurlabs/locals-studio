#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Emitter, State};

#[derive(Default)]
struct EventBridge {
    active: AtomicBool,
}

#[tauri::command]
async fn studio_request(method: String, params: Value) -> Result<Value, String> {
    studio_client::client::ensure_service()
        .await
        .map_err(|error| format!("{error:#}"))?;
    studio_client::client::request(&method, params)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_owned())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("huggingface.co")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("Only HTTPS links to huggingface.co can be opened".into());
    }
    let status = tokio::process::Command::new("/usr/bin/open")
        .arg(parsed.as_str())
        .status()
        .await
        .map_err(|error| format!("Could not open the browser: {error}"))?;
    if !status.success() {
        return Err("Could not open the browser".into());
    }
    Ok(())
}

#[tauri::command]
async fn studio_events(
    app: tauri::AppHandle,
    bridge: State<'_, EventBridge>,
) -> Result<(), String> {
    if bridge.active.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    tauri::async_runtime::spawn(async move {
        loop {
            let result = async {
                studio_client::client::ensure_service().await?;
                let mut receiver = studio_client::client::subscribe().await?;
                while let Some(event) = receiver.recv().await {
                    let _ = app.emit("studio:event", event);
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = app.emit("studio:event", serde_json::json!({"type":"service.disconnected", "data":{"error":format!("{error:#}")}}));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(EventBridge::default())
        .invoke_handler(tauri::generate_handler![
            studio_request,
            studio_events,
            open_external
        ])
        .setup(|_app| {
            // An installed sidecar sits next to the app executable. Core also supports
            // LOCALS_STUDIO_CLI_BIN, PATH and the source checkout for local development.
            if std::env::var_os("LOCALS_STUDIO_CLI_BIN").is_none() {
                if let Ok(path) = std::env::current_exe() {
                    let sibling = path.with_file_name("locals-studio");
                    if sibling.is_file() {
                        std::env::set_var("LOCALS_STUDIO_CLI_BIN", sibling);
                    }
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Could not launch Locals Studio");
}
