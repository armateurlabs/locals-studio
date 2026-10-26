#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{Emitter, Manager};

mod tray;

#[derive(Default)]
struct EventBridge {
    active: AtomicBool,
    connected: AtomicBool,
    notifications: Mutex<()>,
}

impl EventBridge {
    fn publish(&self, connected: bool, event: Value, emit: impl FnOnce(Value)) {
        // Replay and live events share this short lock: an EOF must not be followed
        // by an older handshake claiming that the stream is still connected.
        let _guard = self
            .notifications
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.connected.store(connected, Ordering::SeqCst);
        emit(event);
    }

    fn replay_connection(&self, emit: impl FnOnce(Value)) {
        let _guard = self
            .notifications
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.connected.load(Ordering::SeqCst) {
            emit(serde_json::json!({"type":"service.connected","data":{}}));
        }
    }
}

#[tauri::command]
async fn studio_request(
    app: tauri::AppHandle,
    method: String,
    params: Value,
) -> Result<Value, String> {
    studio_client::client::ensure_service()
        .await
        .map_err(|error| format!("{error:#}"))?;
    // Older running services ignore newly introduced configuration fields. Detect that before
    // applying a Settings form so unrelated fields are not partially saved and reported failed.
    let requested = menu_bar_preference(&params);
    if method == "config.set" && requested == Some(true) {
        let config = studio_client::client::request("config.get", serde_json::json!({}))
            .await
            .map_err(|error| format!("{error:#}"))?;
        if config
            .pointer("/desktop/menu_bar_enabled")
            .and_then(Value::as_bool)
            .is_none()
        {
            return Err("The running management service must be updated before enabling the menu bar. No settings were saved. Restart the updated management service when your model servers can be stopped.".into());
        }
    }
    let result = studio_client::client::request(&method, params)
        .await
        .map_err(|error| format!("{error:#}"))?;
    if method == "config.set" {
        if requested == Some(true)
            && result
                .pointer("/config/desktop/menu_bar_enabled")
                .and_then(Value::as_bool)
                != requested
        {
            return Err("The management service did not persist the menu bar preference. Update the service before enabling it.".into());
        }
        if let Some(config) = result.get("config") {
            tray::service_event(
                &app,
                &serde_json::json!({"type":"config.changed","data":config}),
            );
        }
    }
    Ok(result)
}

fn menu_bar_preference(params: &Value) -> Option<bool> {
    if params["key"] == "desktop.menu_bar_enabled" {
        params["value"].as_bool()
    } else {
        params
            .pointer("/config/desktop/menu_bar_enabled")
            .and_then(Value::as_bool)
    }
}

fn allowed_external_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("huggingface.co" | "armateurlabs.fr" | "generatedcomputers.com")
        )
        && url.username().is_empty()
        && url.password().is_none()
}

#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|_| "Invalid URL".to_owned())?;
    if !allowed_external_url(&parsed) {
        return Err("Only HTTPS links to huggingface.co, armateurlabs.fr, or generatedcomputers.com are allowed. URL credentials are not allowed.".into());
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
async fn studio_events(app: tauri::AppHandle) -> Result<(), String> {
    start_event_bridge(app.clone());
    // The bridge starts in setup(), before the renderer installs its listener.
    // Replay only a confirmed live IPC connection, not the task's active flag.
    app.state::<EventBridge>().replay_connection(|event| {
        let _ = app.emit("studio:event", event);
    });
    Ok(())
}

fn start_event_bridge(app: tauri::AppHandle) {
    if app
        .state::<EventBridge>()
        .active
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    tauri::async_runtime::spawn(async move {
        loop {
            let result = async {
                studio_client::client::ensure_service().await?;
                let mut receiver = studio_client::client::subscribe().await?;
                tray::request_refresh(&app, false);
                while let Some(event) = receiver.recv().await {
                    tray::service_event(&app, &event);
                    app.state::<EventBridge>().publish(true, event, |event| {
                        let _ = app.emit("studio:event", event);
                    });
                }
                Ok::<(), anyhow::Error>(())
            }
            .await;
            let error = match result {
                Err(error) => format!("{error:#}"),
                Ok(()) => "The Studio event connection closed; reconnecting.".to_owned(),
            };
            app.state::<EventBridge>().publish(
                false,
                serde_json::json!({"type":"service.disconnected", "data":{"error":error}}),
                |event| {
                    let _ = app.emit("studio:event", event);
                },
            );
            tray::disconnected(&app);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

fn main() {
    tauri::Builder::default()
        .manage(EventBridge::default())
        .manage(tray::TrayState::default())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            studio_request,
            studio_events,
            open_external
        ])
        .on_menu_event(|app, event| tray::handle_menu(app, event.id.as_ref()))
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if tray::keep_window(window.app_handle()) {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
            }
        })
        .setup(|app| {
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
            tray::setup(app.handle())?;
            start_event_bridge(app.handle().clone());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Could not launch Locals Studio")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                tray::show_main(app);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn late_event_listeners_receive_only_a_confirmed_live_connection() {
        let bridge = EventBridge::default();
        let mut received = Vec::new();
        // Starting the reconnect loop does not establish an IPC connection.
        bridge.active.store(true, Ordering::SeqCst);
        bridge.replay_connection(|event| received.push(event));
        assert!(received.is_empty());

        // setup() can receive this initial event before the frontend listens.
        bridge.publish(
            true,
            serde_json::json!({"type":"connected","data":{}}),
            |_| {},
        );
        bridge.replay_connection(|event| received.push(event));
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["type"], "service.connected");

        // EOF and errors both revoke the remembered connection before notifying.
        bridge.publish(
            false,
            serde_json::json!({"type":"service.disconnected","data":{"error":"EOF"}}),
            |event| received.push(event),
        );
        bridge.replay_connection(|event| received.push(event));
        assert_eq!(
            received.len(),
            2,
            "an active reconnect loop must not replay a stale connection"
        );
        assert_eq!(received[1]["type"], "service.disconnected");

        // Only another message from the actual IPC stream restores the handshake.
        bridge.publish(
            true,
            serde_json::json!({"type":"connected","data":{}}),
            |_| {},
        );
        bridge.replay_connection(|event| received.push(event));
        assert_eq!(received.len(), 3);
        assert_eq!(received[2]["type"], "service.connected");
    }

    #[test]
    fn external_links_allow_only_the_three_exact_https_hosts_without_credentials() {
        for value in [
            "https://huggingface.co/models",
            "https://armateurlabs.fr",
            "https://generatedcomputers.com/",
        ] {
            assert!(allowed_external_url(&url::Url::parse(value).unwrap()));
        }
        for value in [
            "http://armateurlabs.fr",
            "https://armateurlabs.fr.evil.example",
            "https://other.armateurlabs.fr",
            "https://user@huggingface.co",
            "https://user:secret@armateurlabs.fr",
            "http://generatedcomputers.com/",
            "https://generatedcomputers.com.evil.example/",
            "https://www.generatedcomputers.com/",
            "https://other.generatedcomputers.com/",
            "https://user@generatedcomputers.com/",
            "https://user:secret@generatedcomputers.com/",
            "file:///tmp/example",
        ] {
            assert!(
                !allowed_external_url(&url::Url::parse(value).unwrap()),
                "{value}"
            );
        }
    }
}
