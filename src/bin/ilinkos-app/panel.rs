//! The device tools panel: details, apps, running apps, the system log, crash
//! reports and location simulation, beside the phone's screen.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, ScrollArea, Ui};
use serde_json::Value;
use ilinkos::serve::{Action, Handle, Output};

use crate::{BAD, DIM, LINE, OK, PANEL, RAISED, TEXT, map::Map};

pub const WIDTH: f32 = 330.0;
/// The log keeps this many lines; older ones are dropped.
const LOG_LINES: usize = 3000;
/// How many of the newest matching log lines are drawn.
const LOG_SHOWN: usize = 400;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Device,
    Apps,
    Running,
    Logs,
    Crashes,
    Location,
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Device, "Device"),
    (Tab::Apps, "Apps"),
    (Tab::Running, "Running"),
    (Tab::Logs, "Logs"),
    (Tab::Crashes, "Crashes"),
    (Tab::Location, "Location"),
];

/// Something fetched from the phone, and where the fetch has got to.
#[derive(Default)]
enum Slot {
    #[default]
    Empty,
    Loading,
    Ready(Value),
    Failed(String),
}

#[derive(Default)]
struct Data {
    info: Slot,
    apps: Slot,
    running: Slot,
    crashes: Slot,
}

#[derive(Default)]
struct Log {
    lines: VecDeque<String>,
    paused: bool,
}

pub struct Panel {
    pub open: bool,
    tab: Tab,
    data: Arc<Mutex<Data>>,
    log: Arc<Mutex<Log>>,
    log_started: bool,
    app_search: String,
    log_filter: String,
    map: Option<Map>,
    /// The map view last written to disk.
    saved_view: (f64, f64, u8),
    /// The place chosen on the map or typed in, not yet sent to the phone.
    chosen: Option<(f64, f64)>,
    latitude: String,
    longitude: String,
    /// Where one-line messages for the user go; the app shows them as toasts.
    notice: Arc<Mutex<Option<String>>>,
}

/// Where the map's last view is remembered between launches.
fn view_file() -> Option<PathBuf> {
    Some(ilinkos::wifi::state_dir().ok()?.join("map-view"))
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).color(DIM).size(11.5).strong());
    ui.add_space(2.0);
}

/// A full-width row that highlights on hover and reports clicks.
fn row(ui: &mut Ui, title: &str, detail: &str) -> egui::Response {
    let height = if detail.is_empty() { 26.0 } else { 38.0 };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect, 6.0, RAISED);
    }
    let left = rect.left() + 8.0;
    let painter = ui.painter().with_clip_rect(rect);
    if detail.is_empty() {
        painter.text(egui::pos2(left, rect.center().y), egui::Align2::LEFT_CENTER, title, FontId::proportional(13.0), TEXT);
    } else {
        painter.text(egui::pos2(left, rect.top() + 12.0), egui::Align2::LEFT_CENTER, title, FontId::proportional(13.0), TEXT);
        painter.text(egui::pos2(left, rect.top() + 27.0), egui::Align2::LEFT_CENTER, detail, FontId::proportional(11.0), DIM);
    }
    response
}

impl Panel {
    pub fn new(notice: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            open: false,
            tab: Tab::Device,
            data: Arc::default(),
            log: Arc::default(),
            log_started: false,
            app_search: String::new(),
            log_filter: String::new(),
            map: None,
            saved_view: (0.0, 0.0, 0),
            chosen: None,
            latitude: String::new(),
            longitude: String::new(),
            notice,
        }
    }

    /// Asks the phone for something and files the answer under `pick(data)`.
    fn fetch(
        &self,
        ctx: &egui::Context,
        handle: &Handle,
        action: Action,
        pick: fn(&mut Data) -> &mut Slot,
    ) {
        *pick(&mut self.data.lock().unwrap()) = Slot::Loading;
        let (data, ctx) = (self.data.clone(), ctx.clone());
        handle.request(action, move |result| {
            let slot = match result {
                Ok(Output::Json(json)) => match serde_json::from_str(&json) {
                    Ok(value) => Slot::Ready(value),
                    Err(e) => Slot::Failed(e.to_string()),
                },
                Ok(_) => Slot::Failed("unexpected reply".into()),
                Err(e) => Slot::Failed(e),
            };
            *pick(&mut data.lock().unwrap()) = slot;
            ctx.request_repaint();
        });
    }

    /// Runs an action for its effect and tells the user how it went.
    fn run(&self, ctx: &egui::Context, handle: &Handle, action: Action, done: String) {
        let (notice, ctx) = (self.notice.clone(), ctx.clone());
        handle.request(action, move |result| {
            *notice.lock().unwrap() = Some(match result {
                Ok(_) => done,
                Err(e) => e,
            });
            ctx.request_repaint();
        });
    }

    /// Starts collecting the phone's log the first time the Logs tab is opened.
    fn start_log(&mut self, ctx: &egui::Context, handle: &Handle) {
        if std::mem::replace(&mut self.log_started, true) {
            return;
        }
        let (log, ctx, mut lines) = (self.log.clone(), ctx.clone(), handle.logs());
        std::thread::spawn(move || {
            loop {
                let line = match lines.blocking_recv() {
                    Ok(line) => line,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let mut log = log.lock().unwrap();
                if log.paused {
                    continue;
                }
                log.lines.push_back(line);
                if log.lines.len() > LOG_LINES {
                    log.lines.pop_front();
                }
                drop(log);
                // The log can run to hundreds of lines a second: don't redraw for each.
                ctx.request_repaint_after(Duration::from_millis(150));
            }
        });
    }

    pub fn ui(&mut self, ui: &mut Ui, handle: &Handle, connected: bool) {
        let ctx = ui.ctx().clone();
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.spacing_mut().button_padding.x = 5.0;
            for (tab, name) in TABS {
                let selected = self.tab == tab;
                let text = RichText::new(name).size(12.0).color(if selected { TEXT } else { DIM });
                if ui.add(egui::Button::new(text).fill(if selected { RAISED } else { PANEL }).corner_radius(7)).clicked() {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(4.0);
        let (line, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
        ui.painter().rect_filled(line, 0.0, LINE);

        if !connected {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.label(RichText::new("Connect an iPhone to use these tools.").color(DIM)));
            return;
        }
        egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| match self.tab {
            Tab::Device => self.device(ui, &ctx, handle),
            Tab::Apps => self.apps(ui, &ctx, handle),
            Tab::Running => self.running(ui, &ctx, handle),
            Tab::Logs => self.logs(ui, &ctx, handle),
            Tab::Crashes => self.crashes(ui, &ctx, handle),
            Tab::Location => self.location(ui, &ctx, handle),
        });
    }

    /// Draws the common states of a slot; returns the value once it is there.
    fn ready(&self, ui: &mut Ui, pick: fn(&Data) -> &Slot) -> Option<Value> {
        let data = self.data.lock().unwrap();
        match pick(&data) {
            Slot::Ready(value) => return Some(value.clone()),
            Slot::Empty | Slot::Loading => {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| ui.add(egui::Spinner::new().color(DIM)));
            }
            Slot::Failed(error) => {
                ui.add_space(10.0);
                ui.label(RichText::new(error).color(BAD));
            }
        }
        None
    }

    fn refresh_button(ui: &mut Ui) -> bool {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(egui::Button::new(RichText::new("Refresh").size(11.5).color(DIM)).corner_radius(6)).clicked()
        })
        .inner
    }

    fn device(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        if matches!(self.data.lock().unwrap().info, Slot::Empty) {
            self.fetch(ctx, handle, Action::Info, |d| &mut d.info);
        }
        ui.horizontal(|ui| {
            section(ui, "THIS IPHONE");
            if Self::refresh_button(ui) {
                self.fetch(ctx, handle, Action::Info, |d| &mut d.info);
            }
        });
        let Some(info) = self.ready(ui, |d| &d.info) else {
            return;
        };
        let text = |key: &str| info[key].as_str().unwrap_or("—").to_owned();
        let battery = match (info["battery_percent"].as_u64(), info["charging"].as_bool()) {
            (Some(level), Some(true)) => format!("{level}%, charging"),
            (Some(level), _) => format!("{level}%"),
            _ => "—".into(),
        };
        egui::Grid::new("info").num_columns(2).spacing([18.0, 7.0]).show(ui, |ui| {
            for (name, value) in [
                ("Name", text("name")),
                ("Model", text("model")),
                ("iOS", format!("{} ({})", text("ios"), text("build"))),
                ("Battery", battery),
            ] {
                ui.label(RichText::new(name).color(DIM));
                ui.label(RichText::new(value).color(TEXT));
                ui.end_row();
            }
        });

        section(ui, "APPEARANCE");
        let current = info["appearance"].as_str().unwrap_or_default().to_owned();
        ui.horizontal(|ui| {
            for style in ["light", "dark"] {
                let on = current == style;
                let label = RichText::new(if style == "light" { "Light" } else { "Dark" })
                    .color(if on { Color32::from_rgb(6, 33, 15) } else { TEXT });
                let button = egui::Button::new(label).fill(if on { OK } else { RAISED }).corner_radius(7);
                if ui.add(button).clicked() && !on {
                    // The reply is the phone's new details, so the panel updates with it.
                    self.fetch(ctx, handle, Action::Appearance { style: Some(style.into()) }, |d| &mut d.info);
                }
            }
        });
    }

    fn apps(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        if matches!(self.data.lock().unwrap().apps, Slot::Empty) {
            self.fetch(ctx, handle, Action::InstalledApps, |d| &mut d.apps);
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.app_search).hint_text("Search apps").desired_width(210.0));
            if Self::refresh_button(ui) {
                self.fetch(ctx, handle, Action::InstalledApps, |d| &mut d.apps);
            }
        });
        let Some(apps) = self.ready(ui, |d| &d.apps) else {
            return;
        };
        let wanted = self.app_search.to_lowercase();
        ui.add_space(4.0);
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for app in apps.as_array().into_iter().flatten() {
                let (name, bundle) = (app["name"].as_str().unwrap_or_default(), app["bundle_id"].as_str().unwrap_or_default());
                if !wanted.is_empty() && !name.to_lowercase().contains(&wanted) && !bundle.to_lowercase().contains(&wanted) {
                    continue;
                }
                if row(ui, name, bundle).on_hover_text("Click to open").clicked() {
                    self.run(ctx, handle, Action::Launch { app: bundle.into() }, format!("Opened {name}"));
                }
            }
        });
    }

    fn running(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        if matches!(self.data.lock().unwrap().running, Slot::Empty) {
            self.fetch(ctx, handle, Action::Processes, |d| &mut d.running);
        }
        ui.horizontal(|ui| {
            section(ui, "RUNNING APPS");
            if Self::refresh_button(ui) {
                self.fetch(ctx, handle, Action::Processes, |d| &mut d.running);
            }
        });
        let Some(processes) = self.ready(ui, |d| &d.running) else {
            return;
        };
        let all: Vec<&Value> = processes.as_array().into_iter().flatten().collect();
        let apps: Vec<&&Value> = all.iter().filter(|p| p["app"].as_bool() == Some(true)).collect();
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for process in &apps {
                let pid = format!("pid {}", process["pid"]);
                row(ui, process["name"].as_str().unwrap_or_default(), &pid);
            }
            ui.add_space(6.0);
            ui.label(RichText::new(format!("and {} background processes", all.len() - apps.len())).color(DIM).size(11.5));
        });
    }

    fn logs(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        self.start_log(ctx, handle);
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.log_filter).hint_text("Filter").desired_width(150.0));
            let mut log = self.log.lock().unwrap();
            let pause = if log.paused { "Resume" } else { "Pause" };
            if ui.add(egui::Button::new(RichText::new(pause).size(11.5)).corner_radius(6)).clicked() {
                log.paused = !log.paused;
            }
            if ui.add(egui::Button::new(RichText::new("Clear").size(11.5)).corner_radius(6)).clicked() {
                log.lines.clear();
            }
        });
        ui.add_space(4.0);
        let wanted = self.log_filter.to_lowercase();
        let log = self.log.lock().unwrap();
        let mut shown: Vec<&String> = log
            .lines
            .iter()
            .rev()
            .filter(|l| wanted.is_empty() || l.to_lowercase().contains(&wanted))
            .take(LOG_SHOWN)
            .collect();
        shown.reverse();
        if shown.is_empty() {
            ui.label(RichText::new(if log.lines.is_empty() { "Waiting for the log…" } else { "No lines match." }).color(DIM));
            return;
        }
        ScrollArea::both().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for line in shown {
                ui.add(egui::Label::new(RichText::new(line).font(FontId::monospace(10.5)).color(TEXT)).extend());
            }
        });
    }

    fn crashes(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        if matches!(self.data.lock().unwrap().crashes, Slot::Empty) {
            self.fetch(ctx, handle, Action::CrashReports, |d| &mut d.crashes);
        }
        ui.horizontal(|ui| {
            section(ui, "CRASH REPORTS");
            if Self::refresh_button(ui) {
                self.fetch(ctx, handle, Action::CrashReports, |d| &mut d.crashes);
            }
        });
        let Some(reports) = self.ready(ui, |d| &d.crashes) else {
            return;
        };
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            for report in reports.as_array().into_iter().flatten().filter_map(|r| r.as_str()) {
                // Entries without an extension are folders, which cannot be saved whole.
                let file = report.rsplit('/').next().is_some_and(|name| name.contains('.'));
                let detail = if file { "Click to save to Documents\\iLinkOS" } else { "folder" };
                if row(ui, report, detail).clicked() && file {
                    self.save_crash(ctx, handle, report);
                }
            }
            if reports.as_array().is_some_and(|r| r.is_empty()) {
                ui.label(RichText::new("No crash reports.").color(DIM));
            }
        });
    }

    fn location(&mut self, ui: &mut Ui, ctx: &egui::Context, handle: &Handle) {
        let live = handle.location();
        section(ui, "LOCATION");
        match live {
            Some((lat, lon)) => {
                ui.label(RichText::new(format!("The iPhone reports {lat:.5}, {lon:.5}")).color(OK));
            }
            None => {
                ui.label(RichText::new("The iPhone is reporting where it really is.").color(DIM));
            }
        }
        ui.add_space(4.0);

        if self.map.is_none() {
            // Open where the map was last left.
            let view = view_file().and_then(|file| std::fs::read_to_string(file).ok());
            let mut parts = view.as_deref().unwrap_or_default().split_whitespace();
            let mut number = || parts.next()?.parse::<f64>().ok();
            let (lat, lon, zoom) = (number().unwrap_or(25.0), number().unwrap_or(0.0), number().unwrap_or(1.0));
            self.saved_view = (lat, lon, zoom as u8);
            self.map = Some(Map::new(ctx, (lat, lon), zoom as u8));
        }
        let map = self.map.as_mut().unwrap();
        let mut pins = Vec::new();
        pins.extend(self.chosen.map(|at| (at, false)));
        pins.extend(live.map(|at| (at, true)));
        let size = egui::vec2(ui.available_width(), (ui.available_height() - 150.0).clamp(180.0, 420.0));
        if let Some((lat, lon)) = map.ui(ui, size, &pins) {
            self.chosen = Some((lat, lon));
            (self.latitude, self.longitude) = (format!("{lat:.5}"), format!("{lon:.5}"));
        }
        let view = (map.centre.0, map.centre.1, map.zoom);
        if view != self.saved_view && !ui.input(|i| i.pointer.any_down()) {
            self.saved_view = view;
            if let Some(file) = view_file() {
                let _ = std::fs::write(file, format!("{} {} {}", view.0, view.1, view.2));
            }
        }
        ui.label(RichText::new("Drag to move, scroll to zoom, click to choose a place.").color(DIM).size(11.5));

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let field = |text| egui::TextEdit::singleline(text).desired_width(96.0);
            let typed = ui.add(field(&mut self.latitude).hint_text("Latitude")).changed()
                | ui.add(field(&mut self.longitude).hint_text("Longitude")).changed();
            if typed {
                self.chosen = match (self.latitude.trim().parse::<f64>(), self.longitude.trim().parse::<f64>()) {
                    (Ok(lat), Ok(lon)) if lat.abs() <= 90.0 && lon.abs() <= 180.0 => Some((lat, lon)),
                    _ => None,
                };
                if let (Some(at), Some(map)) = (self.chosen, self.map.as_mut()) {
                    map.centre = at;
                }
            }
        });
        ui.horizontal(|ui| {
            let go = egui::Button::new(RichText::new("Simulate this place").color(Color32::from_rgb(6, 33, 15)).strong())
                .fill(OK)
                .corner_radius(7);
            if ui.add_enabled(self.chosen.is_some(), go).clicked() {
                if let Some((lat, lon)) = self.chosen.take() {
                    let done = format!("The iPhone now reports {lat:.5}, {lon:.5}");
                    self.run(ctx, handle, Action::Location { lat, lon }, done);
                }
            }
            let stop = egui::Button::new(RichText::new("Stop").color(TEXT)).fill(RAISED).corner_radius(7);
            if ui.add_enabled(live.is_some(), stop).clicked() {
                self.run(ctx, handle, Action::LocationOff, "Back to the real location".into());
            }
        });
        ui.label(
            RichText::new("Apps on the phone see the simulated place until you stop it or close iLinkOS.")
                .color(DIM)
                .size(11.5),
        );
    }


    fn save_crash(&self, ctx: &egui::Context, handle: &Handle, report: &str) {
        let (notice, ctx, name) = (self.notice.clone(), ctx.clone(), report.to_owned());
        handle.request(Action::CrashReport { name: name.clone() }, move |result| {
            let saved = result.and_then(|output| {
                let Output::Bytes(bytes) = output else {
                    return Err("unexpected reply".into());
                };
                let home = std::env::var_os("USERPROFILE").ok_or("no user folder")?;
                let dir = PathBuf::from(home).join("Documents").join("iLinkOS");
                std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let file = dir.join(name.rsplit('/').next().unwrap_or(&name));
                std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
                Ok(file)
            });
            *notice.lock().unwrap() = Some(match saved {
                Ok(file) => format!("Saved {}", file.display()),
                Err(e) => format!("Could not save: {e}"),
            });
            ctx.request_repaint();
        });
    }
}
