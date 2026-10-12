//! Shows things that happen on the phone as Windows notifications.

use std::{
    os::windows::process::CommandExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tauri_winrt_notification::Toast;
use ilinkos::serve::Handle;
use tokio::sync::broadcast::error::TryRecvError;

/// The identity Windows files the notifications under.
const APP_ID: &str = "iLinkOS.iPhone";
const POLL: Duration = Duration::from_millis(300);

pub struct Notify {
    pub enabled: Arc<AtomicBool>,
}

/// Tells Windows who `APP_ID` is, so notifications say "iLinkOS" and carry its icon.
fn register() {
    let key = format!(r"HKCU\Software\Classes\AppUserModelId\{APP_ID}");
    let set = |name: &str, value: &str| {
        let _ = std::process::Command::new("reg")
            .args(["add", &key, "/v", name, "/t", "REG_SZ", "/d", value, "/f"])
            .creation_flags(0x0800_0000)
            .status();
    };
    set("DisplayName", "iLinkOS");
    if let Ok(dir) = ilinkos::wifi::state_dir() {
        let icon = dir.join("icon.png");
        if std::fs::write(&icon, include_bytes!("../../../assets/ilinkos.png")).is_ok() {
            set("IconUri", &icon.display().to_string());
        }
    }
}

pub fn show(title: &str, body: &str) {
    let toast = |id: &str| Toast::new(id).title(title).text1(body).show();
    // Without the registration Windows refuses our identity; borrow PowerShell's.
    if toast(APP_ID).is_err() {
        let _ = toast(Toast::POWERSHELL_APP_ID);
    }
}

pub fn start(handle: Handle, enabled: bool) -> Notify {
    let enabled = Arc::new(AtomicBool::new(enabled));
    let on = enabled.clone();
    std::thread::spawn(move || {
        register();
        // The phone is only watched while this is switched on.
        let mut events = None;
        loop {
            if !on.load(Ordering::Relaxed) {
                events = None;
                std::thread::sleep(POLL);
                continue;
            }
            match events.get_or_insert_with(|| handle.events()).try_recv() {
                Ok(event) => show(&event.title, &event.body),
                Err(TryRecvError::Empty | TryRecvError::Lagged(_)) => std::thread::sleep(POLL),
                Err(TryRecvError::Closed) => return,
            }
        }
    });
    Notify { enabled }
}
