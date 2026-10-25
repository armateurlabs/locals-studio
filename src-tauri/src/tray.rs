use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    cmp::Ordering as Compare,
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

const TRAY_ID: &str = "locals-studio-menu-bar";
const SELECT_PREFIX: &str = "locals.tray.select.";

#[derive(Default)]
pub struct TrayState {
    snapshot: Mutex<Value>,
    rendered: Mutex<Option<MenuModel>>,
    enabled: AtomicBool,
    quitting: AtomicBool,
    starting: AtomicBool,
    action_epoch: AtomicU64,
    snapshot_epoch: AtomicU64,
    rendering: AtomicBool,
    refreshing: AtomicBool,
    refresh_again: AtomicBool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProfileItem {
    id: String,
    label: String,
    selected: bool,
    status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuModel {
    enabled: bool,
    connected: bool,
    running: bool,
    unavailable: bool,
    starting: bool,
    profiles: Vec<ProfileItem>,
}

impl MenuModel {
    fn from_snapshot(snapshot: &Value, starting: bool) -> Self {
        let available: HashSet<&str> = snapshot["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|model| model["compatible"].as_bool() == Some(true))
            .filter_map(|model| model["id"].as_str())
            .collect();
        let mut profiles: Vec<ProfileItem> = snapshot["profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|profile| {
                profile["model_id"]
                    .as_str()
                    .is_some_and(|id| available.contains(id))
                    || profile["selected"].as_bool() == Some(true)
            })
            .filter_map(|profile| {
                let id = profile["id"].as_str()?.to_owned();
                let status = snapshot["servers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|server| server["profile_id"].as_str() == Some(id.as_str()))
                    .and_then(|server| server["state"].as_str())
                    .unwrap_or("stopped")
                    .to_owned();
                let status = if profile["model_id"]
                    .as_str()
                    .is_some_and(|id| available.contains(id))
                {
                    status
                } else {
                    "unavailable".to_owned()
                };
                Some(ProfileItem {
                    label: profile["alias"].as_str().unwrap_or(&id).to_owned(),
                    id,
                    selected: profile["selected"].as_bool().unwrap_or(false),
                    status,
                })
            })
            .collect();
        profiles.sort_by(|left, right| {
            natural_compare(&left.label, &right.label).then_with(|| left.id.cmp(&right.id))
        });
        Self {
            enabled: snapshot
                .pointer("/config/desktop/menu_bar_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            connected: snapshot["_tray_connected"].as_bool().unwrap_or(false),
            running: snapshot["api"]["running"].as_bool().unwrap_or(false)
                && snapshot
                    .pointer("/config/api/enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            unavailable: snapshot["api"]["error"].as_str().is_some(),
            starting,
            profiles,
        }
    }
}

fn natural_compare(left: &str, right: &str) -> Compare {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        if left[a].is_ascii_digit() && right[b].is_ascii_digit() {
            let (start_a, start_b) = (a, b);
            while a < left.len() && left[a].is_ascii_digit() {
                a += 1;
            }
            while b < right.len() && right[b].is_ascii_digit() {
                b += 1;
            }
            let x = &left[start_a..a];
            let y = &right[start_b..b];
            let x = x
                .iter()
                .position(|byte| *byte != b'0')
                .map(|start| &x[start..])
                .unwrap_or(&[]);
            let y = y
                .iter()
                .position(|byte| *byte != b'0')
                .map(|start| &y[start..])
                .unwrap_or(&[]);
            let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if order != Compare::Equal {
                return order;
            }
        } else {
            let order = left[a].cmp(&right[b]);
            if order != Compare::Equal {
                return order;
            }
            a += 1;
            b += 1;
        }
    }
    (left.len() - a).cmp(&(right.len() - b))
}

pub fn setup(app: &AppHandle) -> Result<()> {
    // Keep the standard Edit/Window menus and their keyboard shortcuts; replace only About/Quit.
    let menu = Menu::default(app)?;
    #[cfg(target_os = "macos")]
    if let Some(application) = menu
        .items()?
        .first()
        .and_then(|item| item.as_submenu())
        .cloned()
    {
        application.remove_at(0)?;
        application.insert(
            &MenuItem::with_id(
                app,
                "desktop.about",
                "About Locals Studio",
                true,
                None::<&str>,
            )?,
            0,
        )?;
        let last = application.items()?.len().saturating_sub(1);
        application.remove_at(last)?;
        application.insert(
            &MenuItem::with_id(
                app,
                "desktop.quit",
                "Quit Locals Studio",
                true,
                Some("CmdOrCtrl+Q"),
            )?,
            last,
        )?;
    }
    app.set_menu(menu)?;
    Ok(())
}

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn keep_window(app: &AppHandle) -> bool {
    let state = app.state::<TrayState>();
    state.enabled.load(Ordering::Acquire) && !state.quitting.load(Ordering::Acquire)
}

fn commit_snapshot(app: &AppHandle, epoch: u64, mut incoming: Value) -> bool {
    let state = app.state::<TrayState>();
    let mut cached = state.snapshot.lock().unwrap();
    if epoch != state.snapshot_epoch.load(Ordering::Acquire) {
        return false;
    }
    incoming["_tray_connected"] = json!(true);
    *cached = incoming;
    drop(cached);
    queue_render(app);
    true
}

pub fn disconnected(app: &AppHandle) {
    let state = app.state::<TrayState>();
    let mut snapshot = state.snapshot.lock().unwrap();
    state.snapshot_epoch.fetch_add(1, Ordering::AcqRel);
    state.action_epoch.fetch_add(1, Ordering::AcqRel);
    snapshot["_tray_connected"] = json!(false);
    drop(snapshot);
    queue_render(app);
}

pub fn service_event(app: &AppHandle, event: &Value) {
    let kind = event["type"].as_str().unwrap_or_default();
    let state = app.state::<TrayState>();
    match kind {
        "config.changed" | "api.changed" | "servers.changed" => {
            let mut snapshot = state.snapshot.lock().unwrap();
            snapshot["_tray_connected"] = json!(true);
            let config_revokes = kind == "config.changed"
                && (snapshot.pointer("/config/memory_budget_bytes")
                    != event["data"].get("memory_budget_bytes")
                    || event["data"]["api"]["enabled"].as_bool() == Some(false)
                    || (snapshot
                        .pointer("/config/desktop/menu_bar_enabled")
                        .and_then(Value::as_bool)
                        == Some(true)
                        && event["data"]["desktop"]["menu_bar_enabled"].as_bool() == Some(false)));
            if config_revokes
                || (kind == "api.changed" && event["data"]["enabled"].as_bool() == Some(false))
            {
                // CLI/GUI stops also revoke a tray confirmation that has not launched a request yet.
                state.action_epoch.fetch_add(1, Ordering::AcqRel);
            }
            state.snapshot_epoch.fetch_add(1, Ordering::AcqRel);
            let key = match kind {
                "config.changed" => "config",
                "api.changed" => "api",
                _ => "servers",
            };
            snapshot[key] = event["data"].clone();
            drop(snapshot);
            queue_render(app);
        }
        "server.state" => {
            let mut snapshot = state.snapshot.lock().unwrap();
            snapshot["_tray_connected"] = json!(true);
            state.snapshot_epoch.fetch_add(1, Ordering::AcqRel);
            if !snapshot["servers"].is_array() {
                snapshot["servers"] = json!([]);
            }
            let servers = snapshot["servers"].as_array_mut().unwrap();
            if let Some(old) = servers
                .iter_mut()
                .find(|server| server["profile_id"] == event["data"]["profile_id"])
            {
                *old = event["data"].clone();
            } else {
                servers.push(event["data"].clone());
            }
            drop(snapshot);
            queue_render(app);
        }
        "profiles.changed" | "models.changed" | "resync" => {
            let snapshot = state.snapshot.lock().unwrap();
            state.snapshot_epoch.fetch_add(1, Ordering::AcqRel);
            state.action_epoch.fetch_add(1, Ordering::AcqRel);
            drop(snapshot);
            request_refresh(app, false);
        }
        _ => {} // Telemetry and conversation events never rebuild the native menu.
    }
}

pub fn request_refresh(app: &AppHandle, force: bool) {
    let state = app.state::<TrayState>();
    if force {
        *state.rendered.lock().unwrap() = None;
    }
    if state.refreshing.swap(true, Ordering::AcqRel) {
        state.refresh_again.store(true, Ordering::Release);
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let state = app.state::<TrayState>();
            state.refresh_again.store(false, Ordering::Release);
            let epoch = state.snapshot_epoch.load(Ordering::Acquire);
            match studio_client::client::request("snapshot", json!({})).await {
                Ok(snapshot) => {
                    if !commit_snapshot(&app, epoch, snapshot) {
                        state.refresh_again.store(true, Ordering::Release);
                    }
                }
                Err(_) => disconnected(&app),
            }
            if !state.refresh_again.swap(false, Ordering::AcqRel) {
                break;
            }
        }
        let state = app.state::<TrayState>();
        state.refreshing.store(false, Ordering::Release);
        if state.refresh_again.swap(false, Ordering::AcqRel) {
            request_refresh(&app, false);
        }
    });
}

fn queue_render(app: &AppHandle) {
    if app
        .state::<TrayState>()
        .rendering
        .swap(true, Ordering::AcqRel)
    {
        return;
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        handle
            .state::<TrayState>()
            .rendering
            .store(false, Ordering::Release);
        if let Err(error) = render(&handle) {
            visible_error(
                &handle,
                &format!("Could not update the menu bar: {error:#}"),
            );
        }
    });
}

fn render(app: &AppHandle) -> Result<()> {
    let state = app.state::<TrayState>();
    let model = MenuModel::from_snapshot(
        &state.snapshot.lock().unwrap(),
        state.starting.load(Ordering::Acquire),
    );
    if state.rendered.lock().unwrap().as_ref() == Some(&model) {
        return Ok(());
    }
    if !model.enabled {
        state.enabled.store(false, Ordering::Release);
        if let Some(icon) = app.remove_tray_by_id(TRAY_ID) {
            let _ = icon.set_visible(false);
            drop(icon);
            show_main_if_hidden(app);
        }
        *state.rendered.lock().unwrap() = Some(model);
        return Ok(());
    }
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(
        app,
        "tray.show",
        "Show Locals Studio",
        true,
        None::<&str>,
    )?)?;
    let status = if !model.connected {
        "Management service disconnected"
    } else if model.unavailable {
        "Server unavailable"
    } else if model.running {
        "Server running"
    } else {
        "Server stopped"
    };
    menu.append(&MenuItem::new(app, status, false, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let selected = model
        .profiles
        .iter()
        .filter(|profile| profile.selected)
        .count();
    let start = if model.starting {
        "Starting selected…".to_owned()
    } else if selected == 0 {
        "Start selected (API only)".to_owned()
    } else {
        format!("Start selected ({selected})")
    };
    menu.append(&MenuItem::with_id(
        app,
        "tray.start",
        start,
        model.connected && !model.starting,
        None::<&str>,
    )?)?;
    // Always reachable, including during preflight, a memory dialog or a model load.
    menu.append(&MenuItem::with_id(
        app,
        "tray.stop",
        "Stop server",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::new(
        app,
        "Selected for the next Start",
        false,
        None::<&str>,
    )?)?;
    if model.profiles.is_empty() {
        menu.append(&MenuItem::new(
            app,
            "No compatible models available",
            false,
            None::<&str>,
        )?)?;
    }
    for profile in &model.profiles {
        let status = match profile.status.as_str() {
            "ready" | "running" => "Ready",
            "starting" | "loading" => "Starting",
            "draining" | "stopping" => "Stopping",
            "error" | "failed" | "crashed" => "Error",
            "unavailable" => "Unavailable",
            _ => "Stopped",
        };
        let label = format!("{} — {status}", profile.label.replace('&', "&&"));
        menu.append(&CheckMenuItem::with_id(
            app,
            selection_id(&profile.id),
            label,
            model.connected,
            profile.selected,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "tray.about",
        "About Locals Studio…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "tray.quit",
        "Quit Locals Studio",
        true,
        None::<&str>,
    )?)?;
    if let Some(icon) = app.tray_by_id(TRAY_ID) {
        icon.set_menu(Some(menu))?;
        icon.set_tooltip(Some(format!("Locals Studio — {status}")))?;
    } else {
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(template_icon())
            .icon_as_template(true)
            .tooltip(format!("Locals Studio — {status}"))
            .menu(&menu)
            .show_menu_on_left_click(true)
            .build(app)?;
    }
    state.enabled.store(true, Ordering::Release);
    *state.rendered.lock().unwrap() = Some(model);
    Ok(())
}

fn show_main_if_hidden(app: &AppHandle) {
    if app
        .get_webview_window("main")
        .is_some_and(|window| !window.is_visible().unwrap_or(true))
    {
        show_main(app);
    }
}

pub fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "tray.show" => show_main(app),
        "tray.about" | "desktop.about" => {
            show_main(app);
            let _ = app.emit("studio:event", json!({"type":"desktop.about","data":{}}));
        }
        "tray.quit" | "desktop.quit" => {
            let state = app.state::<TrayState>();
            state.quitting.store(true, Ordering::Release);
            state.action_epoch.fetch_add(1, Ordering::AcqRel);
            app.exit(0);
        }
        "tray.start" => start_selected(app),
        "tray.stop" => {
            app.state::<TrayState>()
                .action_epoch
                .fetch_add(1, Ordering::AcqRel);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) =
                    studio_client::client::request("server.stop", json!({"all":true})).await
                {
                    visible_error(&app, &format!("Could not stop the server: {error:#}"));
                }
                request_refresh(&app, false);
            });
        }
        _ => {
            if let Some(profile_id) = decode_selection_id(id) {
                app.state::<TrayState>()
                    .action_epoch
                    .fetch_add(1, Ordering::AcqRel);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let result = async {
                        // A native menu can outlive GUI edits. Save only a freshly fetched profile.
                        let profiles =
                            studio_client::client::request("profiles.list", json!({})).await?;
                        let mut profile = profiles
                            .as_array()
                            .context("Invalid profile list")?
                            .iter()
                            .find(|profile| profile["id"].as_str() == Some(profile_id.as_str()))
                            .cloned()
                            .context("This profile is no longer available")?;
                        profile["selected"] =
                            json!(!profile["selected"].as_bool().unwrap_or(false));
                        studio_client::client::request("profiles.save", json!({"profile":profile}))
                            .await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(error) = result {
                        visible_error(&app, &format!("Could not update the selection: {error:#}"));
                    }
                    request_refresh(&app, true);
                });
            }
        }
    }
}

fn start_selected(app: &AppHandle) {
    let state = app.state::<TrayState>();
    if state.starting.swap(true, Ordering::AcqRel) {
        return;
    }
    let epoch = state.action_epoch.load(Ordering::Acquire);
    queue_render(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = start_with_memory_check(&app, epoch).await;
        app.state::<TrayState>()
            .starting
            .store(false, Ordering::Release);
        queue_render(&app);
        request_refresh(&app, false);
        if let Err(error) = result {
            visible_error(
                &app,
                &format!("Could not start the selected models: {error:#}"),
            );
        }
    });
}

fn current_action(app: &AppHandle, epoch: u64) -> bool {
    epoch
        == app
            .state::<TrayState>()
            .action_epoch
            .load(Ordering::Acquire)
}

async fn start_with_memory_check(app: &AppHandle, epoch: u64) -> Result<()> {
    let profiles = studio_client::client::request("profiles.list", json!({})).await?;
    let selected = profiles
        .as_array()
        .context("Invalid profile list")?
        .iter()
        .any(|profile| profile["selected"].as_bool() == Some(true));
    if !current_action(app, epoch) {
        return Ok(());
    }
    let estimate = if selected {
        studio_client::client::request("memory.estimate", json!({})).await?
    } else {
        json!({"requires_override":false})
    };
    if !current_action(app, epoch) {
        return Ok(());
    }
    let mut allow = false;
    if estimate["requires_override"].as_bool() == Some(true) {
        allow = confirm_memory(app, &estimate).await;
        if !allow || !current_action(app, epoch) {
            return Ok(());
        }
    }
    let result = studio_client::client::request(
        "server.start",
        json!({"all":true,"allow_memory_overcommit":allow}),
    )
    .await;
    if let Err(error) = result {
        // Memory pressure may change between estimation and launch. Ask rather than silently retrying.
        if !allow && current_action(app, epoch) {
            if let Some(estimate) = memory_estimate_from_error(&format!("{error:#}")) {
                if confirm_memory(app, &estimate).await && current_action(app, epoch) {
                    studio_client::client::request(
                        "server.start",
                        json!({"all":true,"allow_memory_overcommit":true}),
                    )
                    .await?;
                    return Ok(());
                }
                return Ok(());
            }
        }
        if !current_action(app, epoch) {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

fn memory_estimate_from_error(message: &str) -> Option<Value> {
    if !message.to_lowercase().contains("memory") {
        return None;
    }
    let start = message.find('{')?;
    let mut deserializer =
        serde_json::Deserializer::from_str(&message[start..]).into_iter::<Value>();
    let value = deserializer.next()?.ok()?;
    (value["requires_override"].as_bool() == Some(true)).then_some(value)
}

async fn confirm_memory(app: &AppHandle, estimate: &Value) -> bool {
    show_main(app);
    let warnings: Vec<_> = estimate["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|warning| warning["message"].as_str())
        .collect();
    let message = format!(
        "{}\n\nStart the selected models anyway?",
        if warnings.is_empty() {
            "The selected models may exceed the available memory.".to_owned()
        } else {
            warnings.join("\n\n")
        }
    );
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(message)
        .title("Locals Studio — Memory estimate")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Start anyway".into(),
            "Cancel".into(),
        ))
        .show(move |confirmed| {
            let _ = sender.send(confirmed);
        });
    receiver.await.unwrap_or(false)
}

fn visible_error(app: &AppHandle, message: &str) {
    show_main(app);
    app.dialog()
        .message(message)
        .title("Locals Studio")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

fn selection_id(id: &str) -> String {
    format!(
        "{SELECT_PREFIX}{}",
        id.as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}
fn decode_selection_id(id: &str) -> Option<String> {
    let encoded = id.strip_prefix(SELECT_PREFIX)?;
    if encoded.len() % 2 != 0 || !encoded.is_ascii() {
        return None;
    }
    String::from_utf8(
        (0..encoded.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&encoded[index..index + 2], 16))
            .collect::<std::result::Result<Vec<_>, _>>()
            .ok()?,
    )
    .ok()
}

fn template_icon() -> tauri::image::Image<'static> {
    let size = 36usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let x = x as f32 / 2.;
            let y = y as f32 / 2.;
            // A small outlined window with an L, drawn as a monochrome template at 2× resolution.
            let frame = (2.5..=15.5).contains(&x)
                && (2.5..=15.5).contains(&y)
                && (!(4.0..=14.0).contains(&x) || !(4.0..=14.0).contains(&y));
            let letter = ((6.0..=7.5).contains(&x) && (6.0..=12.0).contains(&y))
                || ((6.0..=12.0).contains(&x) && (10.5..=12.0).contains(&y));
            if frame || letter {
                let index = ((y * 2.) as usize * size + (x * 2.) as usize) * 4;
                rgba[index + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, size as u32, size as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Value {
        json!({"config":{"desktop":{"menu_bar_enabled":true},"api":{"enabled":true}},"api":{"running":true},"_tray_connected":true,"models":[{"id":"m","compatible":true}],"profiles":[{"id":"ten","model_id":"m","alias":"Qwen 10","selected":false},{"id":"two","model_id":"m","alias":"qwen 2","selected":true},{"id":"missing","model_id":"gone","alias":"Gone"}],"servers":[{"profile_id":"two","state":"ready","active_requests":1}]})
    }
    #[test]
    fn menu_is_natural_sorted_and_only_changes_for_visible_state() {
        let mut snapshot = snapshot();
        let initial = MenuModel::from_snapshot(&snapshot, false);
        assert_eq!(
            initial
                .profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>(),
            vec!["two", "ten"]
        );
        snapshot["telemetry"] = json!({"cpu_percent":90});
        snapshot["servers"][0]["active_requests"] = json!(50);
        assert_eq!(initial, MenuModel::from_snapshot(&snapshot, false));
        snapshot["profiles"][0]["selected"] = json!(true);
        assert_ne!(initial, MenuModel::from_snapshot(&snapshot, false));
        snapshot["profiles"][2]["selected"] = json!(true);
        let unavailable = MenuModel::from_snapshot(&snapshot, false);
        let missing = unavailable
            .profiles
            .iter()
            .find(|profile| profile.id == "missing")
            .unwrap();
        assert!(
            missing.selected,
            "an unavailable selected profile must remain visible so it can be unchecked"
        );
        assert_eq!(missing.status, "unavailable");
    }
    #[test]
    fn profile_ids_round_trip_without_menu_id_collisions_and_memory_errors_require_override() {
        for id in ["profile:one", "a.b", "modèle/2"] {
            assert_eq!(decode_selection_id(&selection_id(id)), Some(id.into()));
        }
        assert_eq!(decode_selection_id("locals.tray.select.zz"), None);
        assert!(memory_estimate_from_error(
            "MEMORY_CONFIRMATION_REQUIRED: {\"requires_override\":true}"
        )
        .is_some());
        assert!(memory_estimate_from_error("memory: {\"requires_override\":false}").is_none());
    }
}
