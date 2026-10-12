//! Device management beyond the screen: apps, processes, logs, crash reports,
//! details and appearance. Each works over an open tunnel.

use anyhow::{Context, Result, bail};
use idevice::{
    RsdService,
    core_device::{ConfigurationServiceClient, UserInterfaceStyle},
    dvt::{
        device_info::DeviceInfoClient, process_control::ProcessControlClient,
        remote_server::RemoteServerClient,
    },
    installation_proxy::InstallationProxyClient,
    crashreportcopymobile::CrashReportCopyMobileClient,
    diagnostics_relay::DiagnosticsRelayClient,
    lockdown::LockdownClient,
    rsd::RsdHandshake,
    syslog_relay::SyslogRelayClient,
    tcp::handle::AdapterHandle,
};
use serde::Serialize;

/// An app as the user knows it.
#[derive(Debug, Serialize)]
pub struct App {
    pub name: String,
    pub bundle_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// False for apps that ship with iOS.
    pub removable: bool,
}

#[derive(Debug, Serialize)]
pub struct Process {
    pub pid: u32,
    pub name: String,
    /// True for an app, false for a background process.
    pub app: bool,
}

/// Installed apps the user can see, sorted by name.
pub async fn apps(handle: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<Vec<App>> {
    let mut service = InstallationProxyClient::connect_rsd(handle, rsd).await?;
    let mut apps: Vec<App> = service
        .get_apps(Some("Any"), None)
        .await?
        .into_iter()
        .filter_map(|(bundle_id, info)| {
            let info = info.as_dictionary()?;
            let text = |key: &str| info.get(key).and_then(|v| v.as_string()).map(str::to_owned);
            // Background services and system components carry a "hidden" tag.
            let hidden = info
                .get("SBAppTags")
                .and_then(|t| t.as_array())
                .is_some_and(|tags| tags.iter().any(|t| t.as_string() == Some("hidden")));
            if hidden {
                return None;
            }
            Some(App {
                name: text("CFBundleDisplayName").or_else(|| text("CFBundleName"))?,
                bundle_id,
                version: text("CFBundleShortVersionString"),
                removable: text("ApplicationType").as_deref() == Some("User"),
            })
        })
        .collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

/// Finds an installed app by bundle id or by name, exact match first.
pub async fn find_app(handle: &mut AdapterHandle, rsd: &mut RsdHandshake, app: &str) -> Result<App> {
    let wanted = app.to_lowercase();
    let mut all = apps(handle, rsd).await?;
    let index = all
        .iter()
        .position(|a| a.bundle_id.to_lowercase() == wanted || a.name.to_lowercase() == wanted)
        .or_else(|| all.iter().position(|a| a.name.to_lowercase().contains(&wanted)))
        .with_context(|| format!("no installed app matches '{app}'"))?;
    Ok(all.swap_remove(index))
}

/// Opens an app, given its bundle id or name, and returns what was opened.
pub async fn launch(handle: &mut AdapterHandle, rsd: &mut RsdHandshake, app: &str) -> Result<App> {
    let found = find_app(handle, rsd, app).await?;
    let mut instruments = RemoteServerClient::connect_rsd(handle, rsd).await?;
    let mut control = ProcessControlClient::new(&mut instruments).await?;
    control.launch_app(found.bundle_id.clone(), None, None, false, false).await?;
    Ok(found)
}

/// Running apps and processes, apps first.
pub async fn processes(handle: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<Vec<Process>> {
    let mut instruments = RemoteServerClient::connect_rsd(handle, rsd).await?;
    let mut info = DeviceInfoClient::new(&mut instruments).await?;
    let mut running = info.running_processes().await?;
    running.sort_by_key(|p| (!p.is_application, p.name.to_lowercase()));
    Ok(running
        .into_iter()
        .map(|p| Process { pid: p.pid, name: p.name, app: p.is_application })
        .collect())
}

/// The phone's name, model, iOS version, battery and appearance.
pub async fn info(handle: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<serde_json::Value> {
    let mut lockdown = LockdownClient::connect_rsd(handle, rsd).await?;
    let mut out = serde_json::Map::new();
    for (key, name) in [
        ("DeviceName", "name"),
        ("ProductType", "model"),
        ("ProductVersion", "ios"),
        ("BuildVersion", "build"),
        ("DeviceClass", "class"),
    ] {
        if let Ok(value) = lockdown.get_value(Some(key), None).await {
            if let Some(text) = value.as_string() {
                out.insert(name.into(), text.into());
            }
        }
    }
    let battery = lockdown.get_value(None, Some("com.apple.mobile.battery")).await.ok();
    if let Some(battery) = battery.as_ref().and_then(|b| b.as_dictionary()) {
        if let Some(level) = battery.get("BatteryCurrentCapacity").and_then(|v| v.as_unsigned_integer()) {
            out.insert("battery_percent".into(), level.into());
        }
        if let Some(charging) = battery.get("BatteryIsCharging").and_then(|v| v.as_boolean()) {
            out.insert("charging".into(), charging.into());
        }
    }
    if let Ok(style) = appearance(handle, rsd, None).await {
        out.insert("appearance".into(), style.into());
    }
    Ok(serde_json::Value::Object(out))
}

/// Reads the light/dark appearance, or sets it when `set` is given.
pub async fn appearance(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    set: Option<&str>,
) -> Result<&'static str> {
    let mut service = ConfigurationServiceClient::connect_rsd(handle, rsd).await?;
    match set {
        Some("light") => service.set_user_interface_style(UserInterfaceStyle::Light).await?,
        Some("dark") => service.set_user_interface_style(UserInterfaceStyle::Dark).await?,
        Some(other) => bail!("appearance is 'light' or 'dark', not '{other}'"),
        None => {}
    }
    Ok(match service.get_user_interface_style().await? {
        UserInterfaceStyle::Light => "light",
        UserInterfaceStyle::Dark => "dark",
    })
}

/// Battery health and charge details from the phone's diagnostics.
pub async fn battery(handle: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<plist::Dictionary> {
    let mut service = DiagnosticsRelayClient::connect_rsd(handle, rsd).await?;
    service.gasguage().await?.context("the phone reported no battery details")
}

/// Names of the crash reports stored on the phone.
pub async fn crash_reports(handle: &mut AdapterHandle, rsd: &mut RsdHandshake) -> Result<Vec<String>> {
    let mut service = CrashReportCopyMobileClient::connect_rsd(handle, rsd).await?;
    let mut names = service.ls(None).await?;
    names.retain(|n| n != "." && n != "..");
    names.sort();
    Ok(names)
}

/// The contents of one crash report.
pub async fn crash_report(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    name: &str,
) -> Result<Vec<u8>> {
    let mut service = CrashReportCopyMobileClient::connect_rsd(handle, rsd).await?;
    Ok(service.pull(name).await?)
}

/// Streams the phone's system log, one line at a time, until `line` returns false.
pub async fn logs(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    mut line: impl FnMut(&str) -> bool,
) -> Result<()> {
    let mut service = SyslogRelayClient::connect_rsd(handle, rsd).await?;
    loop {
        if !line(&service.next().await?) {
            return Ok(());
        }
    }
}

/// What the app in front is costing the phone, about once a second.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Perf {
    /// The app in front, or "Home screen".
    pub app: String,
    /// Percent of one processor core.
    pub cpu: f64,
    pub memory_mb: f64,
    /// Frames the phone drew in the last second; 0 while the picture is still.
    pub fps: f64,
}

/// The phone lists every running app within this long of being asked.
const FIRST_LISTING: std::time::Duration = std::time::Duration::from_millis(1500);
/// The process flag the phone sets on apps it is keeping responsive: the one in
/// front, and a few always-on services. Found by watching apps come and go.
const IN_USE: u64 = 0x40_0000;
/// Always-on services are smaller than this; apps people open are bigger.
const REAL_APP_BYTES: u64 = 20_000_000;

/// Streams [`Perf`] samples until `each` returns false.
pub async fn perf(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    mut each: impl FnMut(Perf) -> bool,
) -> Result<()> {
    use idevice::dvt::{graphics::GraphicsClient, message::AuxValue, notifications::NotificationsClient};
    use plist::Value;
    use std::collections::HashMap;

    // A channel borrows its connection, so each stream gets a connection of its own.
    let mut for_processes = RemoteServerClient::connect_rsd(handle, rsd).await?;
    let mut for_graphics = RemoteServerClient::connect_rsd(handle, rsd).await?;
    let mut for_apps = RemoteServerClient::connect_rsd(handle, rsd).await?;

    let mut sampler =
        for_processes.make_channel("com.apple.instruments.server.services.sysmontap").await?;
    let text = |s: &str| Value::String(s.into());
    let mut config = plist::Dictionary::new();
    config.insert("ur".into(), 1000i64.into());
    config.insert("bm".into(), 0i64.into());
    config.insert("procAttrs".into(), Value::Array(vec![text("pid"), text("name"), text("cpuUsage"), text("physFootprint"), text("procFlags")]));
    // The phone drops the connection if no system attribute is asked for.
    config.insert("sysAttrs".into(), Value::Array(vec![text("physMemSize")]));
    config.insert("cpuUsage".into(), true.into());
    config.insert("physFootprint".into(), true.into());
    config.insert("sampleInterval".into(), 1_000_000_000i64.into());
    sampler
        .call_method(Some(text("setConfig:")), Some(vec![AuxValue::archived_value(Value::Dictionary(config))]), false)
        .await?;
    sampler.call_method(Some(text("start")), None, false).await?;

    let mut graphics = GraphicsClient::new(&mut for_graphics).await?;
    graphics.start_sampling(1.0).await?;
    let mut apps = NotificationsClient::new(&mut for_apps).await?;
    apps.start_notifications().await?;

    // pid -> (name, cpu, bytes, in use), kept up to date from samples that list only changes.
    let mut processes: HashMap<u32, (String, f64, u64, bool)> = HashMap::new();
    // Apps that are running rather than suspended. The phone lists them all when the
    // stream opens, which says nothing about which is in front; after that, an app
    // that starts running has just come to the front.
    let mut running: HashMap<u32, String> = HashMap::new();
    let mut front: Option<u32> = None;
    let opened = std::time::Instant::now();
    let mut fps = 0.0;
    loop {
        tokio::select! {
            sample = graphics.sample() => fps = sample?.fps,
            event = apps.get_notification() => {
                let Ok(event) = event else { continue };
                if event.state_description == "Running" {
                    running.insert(event.pid, event.app_name);
                    if opened.elapsed() > FIRST_LISTING {
                        front = Some(event.pid);
                    }
                } else {
                    running.remove(&event.pid);
                    front = front.filter(|pid| *pid != event.pid);
                }
            }
            message = sampler.read_message() => {
                let Some(Value::Array(rows)) = message?.data else { continue };
                let mut sampled = false;
                for row in rows.iter().filter_map(|r| r.as_dictionary()) {
                    for pid in row.get("DeadProcesses").and_then(|d| d.as_array()).into_iter().flatten() {
                        let pid = pid.as_unsigned_integer().unwrap_or(0) as u32;
                        processes.remove(&pid);
                        running.remove(&pid);
                        front = front.filter(|f| *f != pid);
                    }
                    let Some(listed) = row.get("Processes").and_then(|p| p.as_dictionary()) else { continue };
                    sampled = true;
                    for fields in listed.values().filter_map(|v| v.as_array()) {
                        let [pid, name, cpu, bytes, flags] = &fields[..] else { continue };
                        let Some(pid) = pid.as_unsigned_integer() else { continue };
                        processes.insert(pid as u32, (
                            name.as_string().unwrap_or_default().to_owned(),
                            // The first sample has no usage figure yet.
                            cpu.as_real().unwrap_or(0.0),
                            bytes.as_unsigned_integer().unwrap_or(0),
                            flags.as_unsigned_integer().unwrap_or(0) & IN_USE != 0,
                        ));
                    }
                }
                if !sampled {
                    continue;
                }
                // Until an app is seen coming forward, guess: the biggest app the phone
                // is treating as in use. Small ones are services that are always on.
                let in_use = |pid: &u32| processes.get(pid).is_some_and(|p| p.3);
                let biggest = || {
                    running
                        .keys()
                        .filter(|pid| in_use(pid))
                        .filter_map(|pid| Some((*pid, processes.get(pid)?.2)))
                        .filter(|(_, bytes)| *bytes >= REAL_APP_BYTES)
                        .max_by_key(|(_, bytes)| *bytes)
                        .map(|(pid, _)| pid)
                };
                let (name, pid) = match front.filter(|pid| running.contains_key(pid) && in_use(pid)).or_else(biggest) {
                    Some(pid) => (running[&pid].clone(), Some(pid)),
                    None => ("Home screen".to_owned(), processes.iter().find(|(_, p)| p.0 == "SpringBoard").map(|(pid, _)| *pid)),
                };
                let (cpu, bytes) = pid.and_then(|pid| processes.get(&pid)).map_or((0.0, 0), |p| (p.1, p.2));
                if !each(Perf { app: name, cpu, memory_mb: bytes as f64 / 1e6, fps }) {
                    return Ok(());
                }
            }
        }
    }
}

/// Makes the phone report `at` (latitude, longitude) as its location until
/// `changes` yields `None` or closes, following each new position it yields.
/// `set` is told how each change went.
pub async fn simulate_location(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    mut at: (f64, f64),
    changes: &mut tokio::sync::mpsc::UnboundedReceiver<Option<(f64, f64)>>,
    mut set: impl FnMut((f64, f64), Result<()>),
) -> Result<()> {
    use idevice::dvt::location_simulation::LocationSimulationClient;
    let mut instruments = RemoteServerClient::connect_rsd(handle, rsd).await?;
    let mut location = LocationSimulationClient::new(&mut instruments).await?;
    loop {
        let result = location.set(at.0, at.1).await;
        let failed = result.is_err();
        set(at, result.map_err(Into::into));
        if failed {
            bail!("the phone refused the location");
        }
        match changes.recv().await {
            Some(Some(next)) => at = next,
            _ => {
                location.clear().await?;
                return Ok(());
            }
        }
    }
}

/// Something that happened on the phone, worded for a desktop notification.
#[derive(Debug, Clone, Serialize)]
pub struct PhoneEvent {
    pub title: String,
    pub body: String,
}

/// How often the battery is looked at.
const BATTERY_POLL: std::time::Duration = std::time::Duration::from_secs(60);
/// The same app notifying again within this long is taken as the same event.
const SAME_NOTIFICATION: std::time::Duration = std::time::Duration::from_secs(3);

/// Picks the app out of a log line saying a notification arrived for it. The
/// phone words it two ways, depending on which part of SpringBoard is speaking:
///   `[com.example.app] Received notification request ...`
///   `Post notification id: ...; section: com.example.app; ...`
fn notifying_app(message: &str) -> Option<&str> {
    let lower = message.to_ascii_lowercase();
    let id = if lower.contains("notification request") && lower.contains("receiv") {
        // Silent pushes wake an app without showing anything.
        if lower.contains("hasalertcontent: 0") {
            return None;
        }
        let start = message.find('[')? + 1;
        &message[start..start + message[start..].find(']')?]
    } else if lower.contains("notification id:") && !lower.starts_with("withdraw") && !lower.starts_with("remov") {
        let start = message.find("section: ")? + "section: ".len();
        &message[start..start + message[start..].find(';')?]
    } else {
        return None;
    };
    (id.contains('.') && !id.contains(' ')).then_some(id)
}

/// A name for a notifier that is not an installed app, from its identifier:
/// `com.apple.ScreenTimeNotifications` becomes "Screen Time".
fn readable(id: &str) -> String {
    let last = id.rsplit('.').next().unwrap_or(id);
    let last = last.strip_suffix("Notifications").filter(|rest| !rest.is_empty()).unwrap_or(last);
    let mut name = String::new();
    for (i, c) in last.chars().enumerate() {
        if i > 0 && c.is_uppercase() && !name.ends_with(|p: char| p.is_uppercase()) {
            name.push(' ');
        }
        name.push(c);
    }
    name
}

/// Reports notifications arriving and battery changes until `each` returns false.
/// The phone only says which app notified, not what the notification says.
pub async fn events(
    handle: &mut AdapterHandle,
    rsd: &mut RsdHandshake,
    mut each: impl FnMut(PhoneEvent) -> bool,
) -> Result<()> {
    use idevice::os_trace_relay::OsTraceRelayClient;
    use std::time::Instant;

    let names: std::collections::HashMap<String, String> = apps(handle, rsd)
        .await
        .map(|apps| apps.into_iter().map(|a| (a.bundle_id, a.name)).collect())
        .unwrap_or_default();
    // Notifications are delivered by SpringBoard, so only its log is followed.
    let springboard = processes(handle, rsd)
        .await
        .ok()
        .and_then(|all| all.into_iter().find(|p| p.name == "SpringBoard"))
        .map(|p| p.pid);
    let (found, mut found_events) = tokio::sync::mpsc::unbounded_channel();

    let (mut battery_handle, mut battery_rsd, battery_found) = (handle.clone(), rsd.clone(), found.clone());
    let battery = async move {
        let mut last: Option<(u64, bool)> = None;
        loop {
            let mut lockdown = LockdownClient::connect_rsd(&mut battery_handle, &mut battery_rsd).await?;
            let state = lockdown.get_value(None, Some("com.apple.mobile.battery")).await?;
            let read = |key: &str| state.as_dictionary().and_then(|d| d.get(key));
            let level = read("BatteryCurrentCapacity").and_then(|v| v.as_unsigned_integer());
            let charging = read("BatteryIsCharging").and_then(|v| v.as_boolean()).unwrap_or(false);
            if let (Some(level), Some((was, _))) = (level, last) {
                let event = |title: &str, body: String| {
                    let _ = battery_found.send(PhoneEvent { title: title.into(), body });
                };
                for mark in [20, 10, 5] {
                    if !charging && was > mark && level <= mark {
                        event("iPhone battery low", format!("{level}% remaining"));
                    }
                }
                if charging && was < 100 && level >= 100 {
                    event("iPhone fully charged", "The battery is at 100%".into());
                }
            }
            last = level.map(|level| (level, charging)).or(last);
            tokio::time::sleep(BATTERY_POLL).await;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let log = async {
        let service = OsTraceRelayClient::connect_rsd(handle, rsd).await?;
        let mut trace = service.start_trace(springboard).await?;
        let mut last: Option<(String, Instant)> = None;
        loop {
            let entry = trace.next().await?;
            let Some(id) = notifying_app(&entry.message) else { continue };
            if last.as_ref().is_some_and(|(app, at)| app == id && at.elapsed() < SAME_NOTIFICATION) {
                continue;
            }
            last = Some((id.to_owned(), Instant::now()));
            let name = names.get(id).cloned().unwrap_or_else(|| readable(id));
            let _ = found.send(PhoneEvent { title: name, body: "New notification on your iPhone".into() });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let deliver = async {
        while let Some(event) = found_events.recv().await {
            if !each(event) {
                break;
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        result = battery => result,
        result = log => result,
        result = deliver => result,
    }
}
