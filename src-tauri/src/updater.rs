//! Automatic updates with Sparkle 2. The framework is loaded at run time from the app bundle, so
//! development builds without it run normally with updates unavailable. Sparkle keeps its own
//! preferences (automatic checks, last check) in the app's user defaults.
use serde_json::{json, Value};
use tauri::AppHandle;

#[cfg(target_os = "macos")]
mod sparkle {
    use objc2::{
        msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_foundation::{NSBundle, NSString};
    use std::cell::RefCell;

    thread_local! {
        // AppKit objects stay on the main thread; every access goes through run_on_main_thread.
        static CONTROLLER: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
    }

    /// Load Sparkle from `Contents/Frameworks` and start its standard updater.
    pub fn start() -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let framework = executable
            .parent()
            .and_then(|macos| macos.parent())
            .map(|contents| contents.join("Frameworks/Sparkle.framework"))
            .filter(|path| path.is_dir())
            .ok_or("Updates are available in the installed app")?;
        let path = NSString::from_str(&framework.to_string_lossy());
        let bundle =
            NSBundle::bundleWithPath(&path).ok_or("The bundled Sparkle framework is unreadable")?;
        // SAFETY: loading Sparkle's own framework bundle, on the main thread at launch.
        if !unsafe { bundle.load() } {
            return Err("The bundled Sparkle framework could not be loaded".into());
        }
        let class = AnyClass::get(c"SPUStandardUpdaterController")
            .ok_or("Sparkle has no standard updater controller")?;
        let null = std::ptr::null_mut::<AnyObject>();
        // SAFETY: Sparkle 2's designated initializer, called on the main thread; init returns +1.
        let controller = unsafe {
            let allocated: *mut AnyObject = msg_send![class, alloc];
            let controller: *mut AnyObject = msg_send![
                allocated,
                initWithStartingUpdater: true,
                updaterDelegate: null,
                userDriverDelegate: null
            ];
            Retained::from_raw(controller).ok_or("Sparkle did not start")?
        };
        CONTROLLER.with(|cell| *cell.borrow_mut() = Some(controller));
        Ok(())
    }

    fn with_updater<T>(read: impl FnOnce(&AnyObject, &AnyObject) -> T) -> Option<T> {
        CONTROLLER.with(|cell| {
            let controller = cell.borrow();
            let controller = controller.as_ref()?;
            // SAFETY: `updater` is a non-null property of a started controller.
            let updater: *mut AnyObject = unsafe { msg_send![&**controller, updater] };
            let updater = unsafe { updater.as_ref()? };
            Some(read(controller, updater))
        })
    }

    pub struct Status {
        pub automatic: bool,
        pub can_check: bool,
        pub last_check: Option<f64>,
    }

    pub fn status() -> Option<Status> {
        with_updater(|_, updater| unsafe {
            let automatic: bool = msg_send![updater, automaticallyChecksForUpdates];
            let can_check: bool = msg_send![updater, canCheckForUpdates];
            let date: *mut AnyObject = msg_send![updater, lastUpdateCheckDate];
            let last_check = date.as_ref().map(|date| {
                let seconds: f64 = msg_send![date, timeIntervalSince1970];
                seconds
            });
            Status {
                automatic,
                can_check,
                last_check,
            }
        })
    }

    pub fn set_automatic(enabled: bool) -> bool {
        with_updater(|_, updater| unsafe {
            let _: () = msg_send![updater, setAutomaticallyChecksForUpdates: enabled];
        })
        .is_some()
    }

    pub fn check() -> bool {
        with_updater(|controller, _| unsafe {
            let _: () = msg_send![controller, checkForUpdates: std::ptr::null_mut::<AnyObject>()];
        })
        .is_some()
    }
}

/// Start Sparkle when the app launches. Called from setup, on the main thread.
pub fn start() {
    #[cfg(target_os = "macos")]
    if let Err(reason) = sparkle::start() {
        eprintln!("Automatic updates are off: {reason}");
    }
}

/// Run `work` on the main thread and wait for its answer.
async fn on_main<T: Send + 'static>(
    app: &AppHandle,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = sender.send(work());
    })
    .map_err(|error| error.to_string())?;
    receiver.await.map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn status_json() -> Value {
    match sparkle::status() {
        Some(status) => json!({
            "available": true,
            "automatic": status.automatic,
            "can_check": status.can_check,
            "last_check": status.last_check.and_then(|seconds| {
                utc_timestamp(seconds)
            }),
            "interval_hours": 24,
        }),
        None => json!({"available": false}),
    }
}

#[cfg(not(target_os = "macos"))]
fn status_json() -> Value {
    json!({"available": false})
}

/// Seconds since 1970 as an RFC 3339 UTC timestamp, without a date library.
#[cfg(target_os = "macos")]
fn utc_timestamp(seconds: f64) -> Option<String> {
    let time =
        std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs_f64(seconds.max(0.)))?;
    let total = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let (days, rest) = (total / 86_400, total % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    ))
}

#[tauri::command]
pub async fn updates_status(app: AppHandle) -> Result<Value, String> {
    on_main(&app, status_json).await
}

#[tauri::command]
pub async fn updates_set_automatic(app: AppHandle, enabled: bool) -> Result<Value, String> {
    on_main(&app, move || {
        #[cfg(target_os = "macos")]
        sparkle::set_automatic(enabled);
        #[cfg(not(target_os = "macos"))]
        let _ = enabled;
        status_json()
    })
    .await
}

/// Check now and show Sparkle's window with the result.
#[tauri::command]
pub async fn updates_check(app: AppHandle) -> Result<Value, String> {
    on_main(&app, || {
        #[cfg(target_os = "macos")]
        if !sparkle::check() {
            return Err("Updates are available in the installed app".to_owned());
        }
        Ok(status_json())
    })
    .await?
}

/// The app menu's Check for Updates… item.
pub fn check_from_menu(app: &AppHandle) {
    let _ = app.run_on_main_thread(|| {
        #[cfg(target_os = "macos")]
        sparkle::check();
    });
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn last_check_dates_are_written_as_utc_timestamps() {
        assert_eq!(
            super::utc_timestamp(0.).as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(
            super::utc_timestamp(1_795_366_985.).as_deref(),
            Some("2026-11-22T17:03:05Z")
        );
    }
}
