//! iLinkOS, the desktop app: a native window onto the phone. It runs the daemon
//! in-process, so opening the app connects and closing it disconnects.

#![windows_subsystem = "windows"]

mod audio;
mod bench;
mod clipboard;
mod hud;
mod map;
mod notify;
mod panel;

use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Event, FontId, Key, Layout, Pos2,
    Rect, ResizeDirection, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions,
    Ui, Vec2, ViewportCommand, WindowLevel,
};
use egui_phosphor::regular as icon;
use ilinkos::{
    control::Clip,
    daemon,
    serve::{self, Action, Handle, Output, Pixels},
    wifi,
};
use zune_jpeg::{JpegDecoder, zune_core::bytestream::ZCursor};

/// Smoothed microseconds spent turning one daemon picture into an egui image.
static CONVERT_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const BG: Color32 = Color32::from_rgb(11, 11, 13);
const PANEL: Color32 = Color32::from_rgb(17, 17, 21);
const RAISED: Color32 = Color32::from_rgb(30, 30, 36);
const LINE: Color32 = Color32::from_rgb(38, 38, 45);
const TEXT: Color32 = Color32::from_rgb(222, 222, 228);
const DIM: Color32 = Color32::from_rgb(141, 141, 150);
const OK: Color32 = Color32::from_rgb(47, 191, 91);
const WARN: Color32 = Color32::from_rgb(214, 170, 50);
const BAD: Color32 = Color32::from_rgb(217, 69, 59);

const TITLE_HEIGHT: f32 = 36.0;
const RAIL_WIDTH: f32 = 48.0;
/// Space around the phone's screen.
const PAD: f32 = 12.0;
/// How close to the window edge the pointer must be to resize it.
const RESIZE_GRIP: f32 = 5.0;
/// The wheel's virtual finger lifts after this long without scrolling.
const SCROLL_IDLE: Duration = Duration::from_millis(90);
const PINCH_INTERVAL: Duration = Duration::from_millis(260);
const TOAST_TIME: Duration = Duration::from_millis(2200);
/// A press this close to a screen edge counts as on it, so edge gestures
/// (Control Center, Notifications, Back) are easy to start with a mouse.
const EDGE_SNAP: f32 = 0.012;
/// An upward drag starting below this line (or in the margin under the picture)
/// is the Home gesture. The phone ignores remote touches on its Home bar, so the
/// app recognises the gesture itself.
const HOME_ZONE: f32 = 0.93;
/// Movement, in points, before a press in the Home zone is judged a swipe up
/// (the gesture) or something else (an ordinary touch, passed to the phone).
const HOME_DECIDE: f32 = 6.0;
/// Pausing this long mid-swipe opens the app switcher, as on the phone.
const SWITCHER_PAUSE: Duration = Duration::from_millis(260);

/// A decoded picture of the phone's screen, handed from the decode task to the UI.
struct Picture {
    image: Arc<egui::ColorImage>,
    seq: u64,
    /// When the daemon finished producing this picture.
    at: Instant,
}

/// A drag that started on the Home indicator at the bottom of the screen.
struct HomeSwipe {
    start: Pos2,
    at: Pos2,
    last_moved: Instant,
    /// True once the drag has clearly headed upwards.
    upward: bool,
    /// Whether the press was on the picture, so a plain click is a tap there.
    on_screen: bool,
    /// Set once the pause has opened the app switcher.
    fired: bool,
}

/// The wheel's virtual finger: down where the pointer was, following the wheel.
struct ScrollFinger {
    at: Pos2,
    last_moved: Instant,
}

struct App {
    handle: Handle,
    // Keeps the daemon and the decode task alive.
    _runtime: tokio::runtime::Runtime,
    latest: Arc<Mutex<Option<Picture>>>,
    texture: Option<TextureHandle>,
    shown: Option<Picture>,
    pinned: bool,
    /// The picture shape the window was last sized for.
    fitted: Option<f32>,
    /// The fractional position of the mouse finger while it is down.
    touch: Option<Pos2>,
    /// Alt + drag: two fingers, the second mirrored through the screen's centre.
    pinch: Option<Pos2>,
    home_swipe: Option<HomeSwipe>,
    scroll: Option<ScrollFinger>,
    last_pinch: Instant,
    toast: Option<(String, Instant)>,
    /// A toast raised from a background thread, picked up on the next frame.
    notice: Arc<Mutex<Option<String>>>,
    clipboard: clipboard::Sync,
    audio: audio::Audio,
    fps: f32,
    fps_mark: (Instant, u64),
    present_ms: f32,
    upload_us: u64,
    ui_us: u64,
    ui_passes: u32,
    shown_frames: u32,
    bench: bench::Bench,
    panel: panel::Panel,
    hud: hud::Hud,
    notify: notify::Notify,
}

/// Where the pinned state is remembered between launches.
fn pin_file() -> Option<PathBuf> {
    Some(wifi::state_dir().ok()?.join("pinned"))
}

/// Present when the user has turned clipboard sync off.
fn clipboard_off_file() -> Option<PathBuf> {
    Some(wifi::state_dir().ok()?.join("clipboard-sync-off"))
}

/// Present when the user has turned the phone's sound off on this PC.
fn audio_off_file() -> Option<PathBuf> {
    Some(wifi::state_dir().ok()?.join("audio-off"))
}

/// Present when the performance overlay is switched on.
fn hud_file() -> Option<PathBuf> {
    Some(wifi::state_dir().ok()?.join("performance-on"))
}

/// Present when the user has turned Windows notifications for phone events off.
fn notify_off_file() -> Option<PathBuf> {
    Some(wifi::state_dir().ok()?.join("notifications-off"))
}

/// Remembers an on/off choice as the presence of a file.
fn remember(file: Option<PathBuf>, present: bool) {
    if let Some(file) = file {
        let _ = if present { std::fs::write(file, "") } else { std::fs::remove_file(file) };
    }
}

fn decode(frame: &serve::Frame) -> Option<egui::ColorImage> {
    match &frame.pixels {
        // The bytes are already laid out as egui's pixels (opaque, so premultiplied
        // and straight alpha agree): reinterpret them in one copy, not pixel by pixel.
        Pixels::Rgba(rgba) => Some(egui::ColorImage::new(
            [frame.width as usize, frame.height as usize],
            bytemuck::cast_slice::<u8, Color32>(rgba).to_vec(),
        )),
        Pixels::Jpeg(jpeg) => {
            let mut decoder = JpegDecoder::new(ZCursor::new(&jpeg[..]));
            let rgb = decoder.decode().ok()?;
            let info = decoder.info()?;
            Some(egui::ColorImage::from_rgb([info.width as usize, info.height as usize], &rgb))
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_stack_size(16 * 1024 * 1024)
            .build()?;
        // One session per phone: take over from a headless daemon if one is running.
        // An agent using the MCP server then talks to this app instead.
        let (_, handle) = runtime.block_on(async {
            if let Some(running) = daemon::running().await {
                let _ = running.request("POST", "/quit", b"").await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            serve::start(serve::DEFAULT_PORT, false, None).await
        })?;

        // Decode off the UI thread and wake the window for each new picture.
        let latest = Arc::new(Mutex::new(None));
        let (slot, ctx, mut frames) = (latest.clone(), cc.egui_ctx.clone(), handle.frames());
        runtime.spawn(async move {
            while frames.changed().await.is_ok() {
                let Some(frame) = frames.borrow_and_update().clone() else {
                    continue;
                };
                let started = Instant::now();
                let decoded = decode(&frame);
                CONVERT_US.store(
                    (CONVERT_US.load(Ordering::Relaxed) * 9 + started.elapsed().as_micros() as u64) / 10,
                    Ordering::Relaxed,
                );
                if let Some(image) = decoded {
                    let picture =
                        Picture { image: Arc::new(image), seq: frame.seq, at: frame.at };
                    *slot.lock().unwrap() = Some(picture);
                    ctx.request_repaint();
                }
            }
        });

        let clipboard =
            clipboard::start(handle.clone(), !clipboard_off_file().is_some_and(|f| f.exists()));
        let audio = audio::start(handle.clone(), !audio_off_file().is_some_and(|f| f.exists()));
        let hud = hud::start(handle.clone(), cc.egui_ctx.clone(), hud_file().is_some_and(|f| f.exists()));
        let notify = notify::start(handle.clone(), !notify_off_file().is_some_and(|f| f.exists()));
        style(&cc.egui_ctx);
        let notice: Arc<Mutex<Option<String>>> = Arc::default();
        let pinned = pin_file().is_some_and(|f| f.exists());
        if pinned {
            cc.egui_ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        }
        Ok(Self {
            handle,
            _runtime: runtime,
            latest,
            texture: None,
            shown: None,
            pinned,
            fitted: None,
            touch: None,
            pinch: None,
            home_swipe: None,
            scroll: None,
            last_pinch: Instant::now(),
            toast: None,
            clipboard,
            audio,
            fps: 0.0,
            fps_mark: (Instant::now(), 0),
            present_ms: 0.0,
            upload_us: 0,
            ui_us: 0,
            ui_passes: 0,
            shown_frames: 0,
            bench: bench::Bench::new(),
            panel: panel::Panel::new(notice.clone()),
            hud,
            notify,
            notice,
        })
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    /// True when the phone is connected and a picture has arrived.
    fn live(&self) -> bool {
        self.handle.problem().is_none() && self.texture.is_some()
    }

    fn take_picture(&mut self, ctx: &egui::Context) {
        let Some(picture) = self.latest.lock().unwrap().take() else {
            return;
        };
        let started = Instant::now();
        match &mut self.texture {
            Some(texture) => texture.set(picture.image.clone(), TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("screen", picture.image.clone(), TextureOptions::LINEAR));
            }
        }
        self.upload_us = (self.upload_us * 9 + started.elapsed().as_micros() as u64) / 10;
        // How long the picture waited between the daemon and this window, smoothed.
        let waited = picture.at.elapsed().as_secs_f32() * 1000.0;
        self.present_ms = if self.present_ms == 0.0 { waited } else { self.present_ms * 0.9 + waited * 0.1 };
        self.shown_frames += 1;
        self.bench.on_picture(&picture.image, &self.handle);
        let (since, seq) = self.fps_mark;
        if since.elapsed() >= Duration::from_secs(1) {
            let seconds = since.elapsed().as_secs_f32();
            self.fps = (picture.seq - seq) as f32 / seconds;
            // Diagnostics for tools/bench.py.
            if let Ok(dir) = wifi::state_dir() {
                let stats = format!(
                    "{{\"present_ms\": {:.2}, \"shown_fps\": {:.1}, \"stream_fps\": {:.1}, \
                     \"convert_ms\": {:.2}, \"upload_ms\": {:.2}, \"ui_ms\": {:.2}, \"ui_fps\": {:.1}}}",
                    self.present_ms,
                    self.shown_frames as f32 / seconds,
                    self.fps,
                    CONVERT_US.load(Ordering::Relaxed) as f32 / 1000.0,
                    self.upload_us as f32 / 1000.0,
                    self.ui_us as f32 / 1000.0,
                    self.ui_passes as f32 / seconds,
                );
                self.ui_passes = 0;
                let _ = std::fs::write(dir.join("app-stats.json"), stats);
            }
            self.shown_frames = 0;
            self.fps_mark = (Instant::now(), picture.seq);
            self.bench.check_request();
        }
        self.shown = Some(picture);
    }

    fn set_pinned(&mut self, ctx: &egui::Context, pinned: bool) {
        self.pinned = pinned;
        let level = if pinned { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal };
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(level));
        remember(pin_file(), pinned);
        self.toast(if pinned { "Pinned above other windows" } else { "Unpinned" });
    }

    /// Takes a lossless full-resolution capture, copies it to the clipboard and
    /// saves it under Pictures.
    fn screenshot(&mut self, ctx: &egui::Context) {
        let (notice, copy, ctx) = (self.notice.clone(), self.clipboard.copy.clone(), ctx.clone());
        self.handle.request(Action::Screenshot, move |result| {
            let message = match result {
                Ok(Output::Png(png)) => {
                    let saved = std::env::var_os("USERPROFILE").and_then(|home| {
                        let dir = PathBuf::from(home).join("Pictures").join("iLinkOS");
                        std::fs::create_dir_all(&dir).ok()?;
                        let name = chrono::Local::now().format("iPhone %Y-%m-%d %H.%M.%S.png");
                        std::fs::write(dir.join(name.to_string()), &png).ok()
                    });
                    let _ = copy.send(Clip::Image(png));
                    match saved {
                        Some(()) => "Copied, and saved to Pictures\\iLinkOS".to_owned(),
                        None => "Copied to the clipboard".to_owned(),
                    }
                }
                Ok(_) => return,
                Err(e) => format!("Screenshot failed: {e}"),
            };
            *notice.lock().unwrap() = Some(message);
            ctx.request_repaint();
        });
    }

    /// Starts or stops recording the screen and sound.
    fn record(&mut self, ctx: &egui::Context, on: bool) {
        let (notice, ctx) = (self.notice.clone(), ctx.clone());
        self.handle.request(Action::Record { on, path: None }, move |result| {
            let message = match result {
                Ok(_) if on => "Recording — click again to stop".to_owned(),
                Ok(_) => "Saved to Videos\\iLinkOS".to_owned(),
                Err(e) => format!("Recording failed: {e}"),
            };
            *notice.lock().unwrap() = Some(message);
            ctx.request_repaint();
        });
    }

    fn set_clipboard_sync(&mut self, on: bool) {
        self.clipboard.enabled.store(on, std::sync::atomic::Ordering::Relaxed);
        remember(clipboard_off_file(), !on);
        self.toast(if on { "Clipboard sync on" } else { "Clipboard sync off" });
    }

    fn title_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let bar = ui.max_rect();
        let drag = ui.interact(bar, ui.id().with("drag"), Sense::click_and_drag());
        if drag.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }

        let problem = self.handle.problem();
        let pairing = self.handle.pairing();
        let (color, status) = match &problem {
            None if self.texture.is_some() && ilinkos::vm::target().is_some() => {
                (OK, format!("Virtual iPhone  ·  {:.0} fps", self.fps))
            }
            None if self.texture.is_some() => (OK, format!("Connected  ·  {:.0} fps", self.fps)),
            None => (WARN, "Waiting for video".to_owned()),
            Some(_) if pairing.active => (WARN, "Pairing".to_owned()),
            Some(p) if p == "connecting" || p.contains("mounting") => (WARN, "Connecting".to_owned()),
            Some(_) => (BAD, "Not connected".to_owned()),
        };

        ui.horizontal_centered(|ui| {
            ui.add_space(12.0);
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, color);
            ui.add_space(2.0);
            ui.label(RichText::new("iLinkOS").color(TEXT).strong());
            ui.label(RichText::new(status).color(DIM).size(12.0));
            if let Some(running) = self.handle.recording() {
                let seconds = running.as_secs();
                let text = format!("{}  {}:{:02}", icon::RECORD, seconds / 60, seconds % 60);
                ui.label(RichText::new(text).color(BAD).size(12.0));
                ctx.request_repaint_after(Duration::from_millis(500));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(RESIZE_GRIP);
                ui.spacing_mut().item_spacing.x = 2.0;
                if chrome_button(ui, icon::X, "Close", false).clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                if chrome_button(ui, icon::MINUS, "Minimise", false).clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                }
                let (glyph, tip) = if self.pinned {
                    (icon::PUSH_PIN, "Pinned above other windows — click to unpin")
                } else {
                    (icon::PUSH_PIN_SLASH, "Pin above other windows")
                };
                if chrome_button(ui, glyph, tip, self.pinned).clicked() {
                    self.set_pinned(&ctx, !self.pinned);
                }
            });
        });
    }

    fn rail(&mut self, ui: &mut Ui) {
        let enabled = self.live();
        let ctx = ui.ctx().clone();
        let divider = |ui: &mut Ui| {
            ui.add_space(3.0);
            let (line, _) = ui.allocate_exact_size(Vec2::new(22.0, 1.0), Sense::hover());
            ui.painter().rect_filled(line, 0.0, LINE);
            ui.add_space(3.0);
        };
        let scroll = egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden);
        scroll.show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.add_space(8.0);
                ui.add_enabled_ui(enabled, |ui| {
                    let button = |name: &str| Action::Button { name: name.into() };
                    let groups: [&[(&str, &str, Action)]; 3] = [
                        &[
                            (icon::HOUSE, "Home  (or right-click the screen)", Action::Home),
                            (icon::CARET_LEFT, "Back  (swipe in from the left edge)", Action::Back),
                            (icon::CARDS, "App switcher", Action::Apps),
                            (icon::MAGNIFYING_GLASS, "Spotlight search", Action::Spotlight),
                        ],
                        &[
                            (icon::BELL, "Notifications", Action::Notifications),
                            (icon::SLIDERS_HORIZONTAL, "Control Center", Action::ControlCenter),
                            (icon::MICROPHONE, "Siri", button("siri")),
                        ],
                        &[
                            (icon::SPEAKER_HIGH, "Volume up", button("volume-up")),
                            (icon::SPEAKER_LOW, "Volume down", button("volume-down")),
                            (icon::ARROW_CLOCKWISE, "Rotate", Action::Rotate { clockwise: true }),
                            (icon::LOCK, "Lock", button("lock")),
                        ],
                    ];
                    for group in groups {
                        for (glyph, tip, action) in group {
                            if rail_button(ui, glyph, tip, false).clicked() {
                                self.handle.send(action.clone());
                            }
                        }
                        divider(ui);
                    }
                    let shot = "Screenshot — full resolution, copied and saved to Pictures";
                    if rail_button(ui, icon::CAMERA, shot, false).clicked() {
                        self.screenshot(&ctx);
                    }
                    let recording = self.handle.recording().is_some();
                    let tip = if recording {
                        "Stop recording"
                    } else {
                        "Record the screen and sound to Videos\\iLinkOS"
                    };
                    let glyph = if recording { icon::STOP_CIRCLE } else { icon::RECORD };
                    if rail_button(ui, glyph, tip, recording).clicked() {
                        self.record(&ctx, !recording);
                    }
                });
                let playing = self.audio.enabled.load(std::sync::atomic::Ordering::Relaxed);
                let decoded = self.audio.stats.decoded.load(std::sync::atomic::Ordering::Relaxed);
                let errors = self.audio.stats.errors.load(std::sync::atomic::Ordering::Relaxed);
                let tip = format!(
                    "Phone sound on this PC is {}\n{decoded} frames played, {errors} errors",
                    if playing { "on" } else { "off" }
                );
                if rail_button(ui, icon::HEADPHONES, &tip, playing).clicked() {
                    self.audio.enabled.store(!playing, std::sync::atomic::Ordering::Relaxed);
                    remember(audio_off_file(), playing);
                    self.toast(if playing { "Phone sound off" } else { "Phone sound on" });
                }
                let syncing = self.clipboard.enabled.load(std::sync::atomic::Ordering::Relaxed);
                let tip = if syncing {
                    "Clipboard sync is on: what you copy on either side appears on the other"
                } else {
                    "Clipboard sync is off"
                };
                if rail_button(ui, icon::CLIPBOARD_TEXT, tip, syncing).clicked() {
                    self.set_clipboard_sync(!syncing);
                }
                let measuring = self.hud.enabled.load(Ordering::Relaxed);
                let tip = "Performance overlay: CPU, memory and frame rate of the app in front";
                if rail_button(ui, icon::GAUGE, tip, measuring).clicked() {
                    self.hud.enabled.store(!measuring, Ordering::Relaxed);
                    remember(hud_file(), !measuring);
                }
                let notifying = self.notify.enabled.load(Ordering::Relaxed);
                let tip = if notifying {
                    "Windows notifications for the phone are on"
                } else {
                    "Show a Windows notification when an app on the phone notifies, or its battery runs low"
                };
                if rail_button(ui, icon::BELL_SIMPLE_RINGING, tip, notifying).clicked() {
                    self.notify.enabled.store(!notifying, Ordering::Relaxed);
                    remember(notify_off_file(), notifying);
                    if notifying {
                        self.toast("Phone notifications off");
                    } else {
                        notify::show("iLinkOS", "You'll be told here when an app on your iPhone notifies.");
                    }
                }
                divider(ui);
                let tools = if self.panel.open { "Hide device tools" } else { "Device tools: apps, logs, crash reports, location" };
                if rail_button(ui, icon::WRENCH, tools, self.panel.open).clicked() {
                    self.panel.open = !self.panel.open;
                    // Grow or shrink the window so the phone's screen keeps its size.
                    let change = if self.panel.open { panel::WIDTH } else { -panel::WIDTH };
                    let size = ctx.content_rect().size() + Vec2::new(change, 0.0);
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                }
                rail_button(ui, icon::QUESTION, "", false).on_hover_ui(|ui| {
                    ui.label(RichText::new("Controls").color(TEXT).strong());
                    for (keys, what) in [
                        ("Click / drag", "tap and swipe"),
                        ("Hold", "long-press"),
                        ("Wheel", "scroll  (Shift for sideways)"),
                        ("Ctrl + wheel  /  Alt + drag", "pinch to zoom"),
                        ("Right-click", "Home"),
                        ("Swipe up from the bottom", "Home; pause part-way for the app switcher"),
                        ("Drag from the top / left edge", "Notifications, Control Center, Back"),
                        ("Keyboard", "types into the phone"),
                        ("Alt + key", "⌘ shortcut, e.g. Alt+H Home, Alt+Space Spotlight"),
                        ("Ctrl + C / X / V / A / Z", "copy, cut, paste, select all, undo"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(keys).color(TEXT));
                            ui.label(RichText::new(what).color(DIM));
                        });
                    }
                });
                ui.add_space(8.0);
            });
        });
    }

    /// The phone's screen, and everything the mouse and keyboard do to it.
    fn screen(&mut self, ui: &mut Ui) {
        let Some(texture) = self.texture.clone() else {
            return;
        };
        let area = ui.max_rect().shrink(PAD);
        let size = texture.size_vec2();
        let scale = (area.width() / size.x).min(area.height() / size.y);
        let rect = Rect::from_center_size(area.center(), size * scale);
        let radius = CornerRadius::same((rect.width() * 0.105).round() as u8);

        self.fit_window(ui, size.x / size.y);

        let response = ui.interact(rect, ui.id().with("screen"), Sense::click_and_drag());
        egui::Image::new(&texture).corner_radius(radius).paint_at(ui, rect);
        ui.painter().rect_stroke(rect, radius, Stroke::new(1.0, LINE), StrokeKind::Outside);
        self.hud.paint(ui, rect);

        let fraction = |pos: Pos2| {
            Pos2::new(
                ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
                ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0),
            )
        };
        let (x, y) = (|p: Pos2| p.x as f64, |p: Pos2| p.y as f64);
        let handle = self.handle.clone();

        // The mouse is a finger.
        let pointer = ui.input(|i| i.pointer.clone());
        let over = pointer.latest_pos().filter(|p| rect.contains(*p));
        // The Home zone is the bottom strip of the picture plus the margin under it.
        let home_zone = Rect::from_min_max(
            Pos2::new(rect.left(), rect.top() + rect.height() * HOME_ZONE),
            Pos2::new(rect.right(), rect.bottom() + PAD),
        );
        // Alt + drag pinches: a second finger mirrors the pointer through the centre,
        // so dragging towards the middle zooms out and away from it zooms in.
        let two_fingers = |at: Pos2, down: bool| Action::Touches {
            fingers: vec![(0, x(at), y(at), down), (1, 1.0 - x(at), 1.0 - y(at), down)],
        };
        let alt = ui.input(|i| i.modifiers.alt);
        if let Some(last) = self.pinch {
            let at = pointer.latest_pos().map_or(last, fraction);
            if pointer.primary_down() {
                if at != last {
                    self.pinch = Some(at);
                    handle.send(two_fingers(at, true));
                }
            } else {
                self.pinch = None;
                handle.send(two_fingers(at, false));
            }
            for finger in [at, Pos2::new(1.0 - at.x, 1.0 - at.y)] {
                let centre = rect.min + Vec2::new(finger.x * rect.width(), finger.y * rect.height());
                ui.painter().circle_filled(centre, 13.0, Color32::from_white_alpha(46));
                ui.painter().circle_stroke(centre, 13.0, Stroke::new(1.5, Color32::from_white_alpha(150)));
            }
        }
        if pointer.primary_pressed() && alt {
            if let Some(pos) = over {
                self.end_scroll();
                let at = fraction(pos);
                self.pinch = Some(at);
                handle.send(two_fingers(at, true));
            }
        } else if pointer.primary_pressed() {
            if let Some(pos) = pointer.latest_pos().filter(|p| home_zone.contains(*p)) {
                self.end_scroll();
                let at = fraction(pos);
                self.home_swipe = Some(HomeSwipe {
                    start: at,
                    at,
                    last_moved: Instant::now(),
                    upward: false,
                    on_screen: rect.contains(pos),
                    fired: false,
                });
            } else if let Some(pos) = over {
                self.end_scroll();
                let snap = |v: f32| match v {
                    v if v < EDGE_SNAP => 0.0,
                    v if v > 1.0 - EDGE_SNAP => 1.0,
                    v => v,
                };
                let at = fraction(pos);
                let at = Pos2::new(snap(at.x), snap(at.y));
                self.touch = Some(at);
                handle.send(Action::Down { x: x(at), y: y(at) });
            }
        }
        if let Some(swipe) = &mut self.home_swipe {
            let at = pointer.latest_pos().map_or(swipe.at, fraction);
            if (at - swipe.at).length() > 0.004 {
                swipe.at = at;
                swipe.last_moved = Instant::now();
            }
            let moved = Vec2::new(
                (swipe.at.x - swipe.start.x) * rect.width(),
                (swipe.start.y - swipe.at.y) * rect.height(),
            );
            if !swipe.upward && moved.length() >= HOME_DECIDE {
                if moved.y > moved.x.abs() {
                    swipe.upward = true;
                } else if swipe.on_screen {
                    // Not a swipe up: it is an ordinary touch, so hand it to the phone.
                    let (start, at) = (swipe.start, swipe.at);
                    self.home_swipe = None;
                    self.touch = Some(at);
                    handle.send(Action::Down { x: x(start), y: y(start) });
                    handle.send(Action::Move { x: x(at), y: y(at) });
                }
            }
        }
        if let Some(swipe) = &mut self.home_swipe {
            if pointer.primary_down() {
                // Pausing part-way up opens the app switcher straight away.
                if swipe.upward && !swipe.fired && swipe.last_moved.elapsed() >= SWITCHER_PAUSE {
                    swipe.fired = true;
                    handle.send(Action::Apps);
                }
                ui.ctx().request_repaint_after(Duration::from_millis(40));
                if swipe.upward {
                    let hint = if swipe.fired {
                        "App switcher"
                    } else {
                        "Release for Home  ·  pause for app switcher"
                    };
                    let bar = Rect::from_center_size(
                        Pos2::new(rect.center().x, rect.top() + swipe.at.y * rect.height()),
                        Vec2::new(rect.width() * 0.36, 5.0),
                    );
                    ui.painter().rect_filled(bar, 3.0, Color32::from_white_alpha(210));
                    ui.painter().text(
                        bar.center() - Vec2::new(0.0, 16.0),
                        Align2::CENTER_BOTTOM,
                        hint,
                        FontId::proportional(12.5),
                        Color32::from_white_alpha(230),
                    );
                }
            } else {
                let (start, upward, fired, on_screen) =
                    (swipe.start, swipe.upward, swipe.fired, swipe.on_screen);
                self.home_swipe = None;
                if fired {
                    // The switcher is already open.
                } else if upward {
                    handle.send(Action::Home);
                } else if on_screen {
                    handle.send(Action::Tap { x: x(start), y: y(start) });
                }
            }
        }
        if let Some(last) = self.touch {
            let at = pointer.latest_pos().map_or(last, fraction);
            if pointer.primary_down() {
                if at != last {
                    self.touch = Some(at);
                    handle.send(Action::Move { x: x(at), y: y(at) });
                }
            } else {
                self.touch = None;
                handle.send(Action::Up { x: x(at), y: y(at) });
            }
        }
        if response.secondary_clicked() {
            handle.send(Action::Home);
        }

        // The wheel drives a second, virtual finger so the phone's own momentum
        // takes over when scrolling stops. Ctrl + wheel pinches.
        let (wheel, zoom, shift) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers.shift));
        if let (Some(pos), None) = (over, self.touch) {
            if zoom != 1.0 {
                if self.last_pinch.elapsed() >= PINCH_INTERVAL {
                    self.last_pinch = Instant::now();
                    self.end_scroll();
                    let at = fraction(pos);
                    let scale = if zoom > 1.0 { 1.8 } else { 0.55 };
                    handle.send(Action::Pinch { x: x(at), y: y(at), scale, ms: Some(220) });
                }
            } else if wheel != Vec2::ZERO {
                let wheel = if shift { Vec2::new(wheel.y + wheel.x, 0.0) } else { wheel };
                let step = Vec2::new(wheel.x / rect.width(), wheel.y / rect.height());
                let start = fraction(pos);
                let finger = self.scroll.get_or_insert_with(|| {
                    handle.send(Action::Down { x: x(start), y: y(start) });
                    ScrollFinger { at: start, last_moved: Instant::now() }
                });
                let next = finger.at + step;
                if !(0.03..=0.97).contains(&next.x) || !(0.06..=0.94).contains(&next.y) {
                    // The finger ran out of screen: lift, and start again from the pointer.
                    self.end_scroll();
                } else {
                    finger.at = next;
                    finger.last_moved = Instant::now();
                    handle.send(Action::Move { x: x(next), y: y(next) });
                }
            }
        }
        if self.scroll.as_ref().is_some_and(|f| f.last_moved.elapsed() >= SCROLL_IDLE) {
            self.end_scroll();
        } else if self.scroll.is_some() {
            ui.ctx().request_repaint_after(SCROLL_IDLE);
        }

        // The keyboard types into the phone. Ctrl and Alt both act as Command.
        // Not while a field in the tools panel is being typed into, though.
        let typing_here = ui.ctx().egui_wants_keyboard_input();
        for event in ui.input(|i| if typing_here { Vec::new() } else { i.events.clone() }) {
            let command = |key: &str, shift: bool| Action::Combo {
                key: key.into(),
                command: true,
                shift,
                option: false,
                control: false,
            };
            match event {
                Event::Text(text) => handle.send(Action::Text { text }),
                // Plain text is typed, which needs no permission on the phone;
                // anything else goes by the clipboard, which the phone asks about.
                Event::Paste(text) if text.is_ascii() => handle.send(Action::Text { text }),
                Event::Paste(text) => handle.send(Action::Paste { text }),
                Event::Copy => handle.send(command("c", false)),
                Event::Cut => handle.send(command("x", false)),
                Event::Key { key, pressed: true, modifiers, .. } => {
                    let name = match key {
                        Key::Enter => "return",
                        Key::Backspace => "backspace",
                        Key::Delete => "delete",
                        Key::Tab => "tab",
                        Key::Escape => "esc",
                        Key::Space => "space",
                        Key::ArrowUp => "up",
                        Key::ArrowDown => "down",
                        Key::ArrowLeft => "left",
                        Key::ArrowRight => "right",
                        other => other.name(),
                    };
                    if modifiers.command || modifiers.alt {
                        // Copy, cut and paste arrive as their own events above.
                        let clipboard_key = modifiers.command && matches!(key, Key::C | Key::X | Key::V);
                        if (name.len() == 1 || name.chars().all(|c| c.is_ascii_lowercase()))
                            && !clipboard_key
                        {
                            handle.send(command(&name.to_ascii_lowercase(), modifiers.shift));
                        }
                    } else if name.chars().all(|c| c.is_ascii_lowercase()) && name != "space" {
                        handle.send(Action::Key { name: name.into() });
                    }
                }
                _ => {}
            }
        }

        // Show where the finger is.
        if self.touch.is_some() {
            if let Some(pos) = pointer.latest_pos() {
                let painter = ui.painter();
                painter.circle_filled(pos, 13.0, Color32::from_white_alpha(46));
                painter.circle_stroke(pos, 13.0, Stroke::new(1.5, Color32::from_white_alpha(150)));
            }
        }
    }

    fn end_scroll(&mut self) {
        if let Some(finger) = self.scroll.take() {
            self.handle.send(Action::Up { x: finger.at.x as f64, y: finger.at.y as f64 });
        }
    }

    /// When the first picture arrives, and whenever the picture changes shape
    /// (the phone rotated): make the window match it.
    fn fit_window(&mut self, ui: &Ui, aspect: f32) {
        if self.fitted.is_some_and(|fitted| (fitted - aspect).abs() < 0.01) {
            return;
        }
        let window = ui.ctx().content_rect().size();
        // Everything beside the picture: the rail, and the tools panel when open.
        let beside = RAIL_WIDTH + if self.panel.open { panel::WIDTH } else { 0.0 };
        // Keep the longer side of the picture the size it is now.
        let long = match self.fitted {
            Some(fitted) if fitted > 1.0 => window.x - beside - PAD * 2.0,
            _ => window.y - TITLE_HEIGHT - PAD * 2.0,
        };
        let (width, height) = if aspect > 1.0 { (long, long / aspect) } else { (long * aspect, long) };
        let size = Vec2::new(width + PAD * 2.0 + beside, height + PAD * 2.0 + TITLE_HEIGHT);
        self.fitted = Some(aspect);
        ui.ctx().send_viewport_cmd(ViewportCommand::InnerSize(size.round()));
    }

    /// Shown instead of the screen: what is wrong, and pairing when it is needed.
    fn offline(&mut self, ui: &mut Ui) {
        let problem = self.handle.problem().unwrap_or_default();
        let pairing = self.handle.pairing();
        let unpaired = ["not paired", "pair again", "rejected the stored pairing"]
            .iter()
            .any(|needle| problem.contains(needle));
        ui.ctx().request_repaint_after(Duration::from_millis(300));

        ui.centered_and_justified(|ui| {
            ui.allocate_ui(Vec2::new(300.0, 0.0), |ui| {
                card().show(ui, |ui| {
                    ui.set_width(260.0);
                    ui.vertical_centered(|ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        if let Some(pin) = pairing.pin.as_ref().filter(|_| pairing.active) {
                            heading(ui, "Enter this code on your iPhone");
                            ui.label(RichText::new(pin).font(FontId::monospace(34.0)).color(TEXT));
                            dim(ui, "The phone is asking for it now.");
                        } else if pairing.active {
                            ui.add(egui::Spinner::new().color(DIM));
                            heading(ui, "On your iPhone");
                            dim(ui, "1.  Join the same Wi-Fi as this PC.");
                            dim(ui, "2.  Open Settings › Privacy & Security › Developer Mode, and turn it on.");
                            dim(ui, format!("3.  On that screen, choose {}.", wifi::host_name()));
                            dim(ui, "A code will appear here to type into the phone.");
                        } else if unpaired {
                            ui.label(RichText::new(icon::DEVICE_MOBILE).size(30.0).color(DIM));
                            heading(ui, "Pair your iPhone");
                            dim(ui, "Needs iOS 27 or later on the same Wi-Fi. No cable required.");
                            if let Some(error) = &pairing.error {
                                ui.label(RichText::new(error).color(BAD));
                            }
                            ui.add_space(4.0);
                            let start = egui::Button::new(
                                RichText::new("Start pairing").color(Color32::from_rgb(6, 33, 15)).strong(),
                            )
                            .fill(OK)
                            .corner_radius(9)
                            .min_size(Vec2::new(140.0, 32.0));
                            if ui.add(start).clicked() {
                                self.handle.start_pairing();
                            }
                        } else if problem.is_empty() {
                            ui.add(egui::Spinner::new().color(DIM));
                            heading(ui, "Connected");
                            dim(ui, "Waiting for the screen. Unlock the iPhone if it is asleep.");
                        } else if problem == "connecting" || problem.contains("mounting") {
                            ui.add(egui::Spinner::new().color(DIM));
                            heading(ui, "Connecting to your iPhone");
                            dim(ui, "This takes a few seconds.");
                        } else {
                            ui.label(RichText::new(icon::WARNING).size(28.0).color(WARN));
                            heading(ui, "Can't reach your iPhone");
                            dim(ui, problem);
                            dim(ui, "Retrying automatically.");
                        }
                    });
                });
            });
        });
    }

    fn toasts(&mut self, ui: &Ui) {
        let Some((text, since)) = &self.toast else {
            return;
        };
        if since.elapsed() >= TOAST_TIME {
            self.toast = None;
            return;
        }
        ui.ctx().request_repaint_after(TOAST_TIME - since.elapsed());
        let anchor = Pos2::new(
            ui.max_rect().center().x - RAIL_WIDTH / 2.0,
            ui.max_rect().bottom() - 26.0,
        );
        let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, "toast".into()));
        let galley = painter.layout_no_wrap(text.clone(), FontId::proportional(12.5), TEXT);
        let pill = Rect::from_center_size(anchor, galley.size() + Vec2::new(24.0, 14.0));
        painter.rect_filled(pill, 14.0, Color32::from_rgba_unmultiplied(28, 28, 34, 240));
        painter.rect_stroke(pill, 14.0, Stroke::new(1.0, LINE), StrokeKind::Inside);
        painter.galley(pill.center() - galley.size() / 2.0, galley, TEXT);
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let pass_started = Instant::now();
        self.ui_passes += 1;
        self.take_picture(&ctx);
        let notice = self.notice.lock().unwrap().take();
        if let Some(notice) = notice {
            self.toast(notice);
        }
        resize_from_edges(&ctx);

        let bare = |fill| egui::Frame::new().fill(fill);
        egui::Panel::top("title")
            .exact_size(TITLE_HEIGHT)
            .frame(bare(PANEL))
            .show_separator_line(false)
            .show(ui, |ui| self.title_bar(ui));
        egui::Panel::right("rail")
            .exact_size(RAIL_WIDTH)
            .resizable(false)
            .frame(bare(PANEL))
            .show_separator_line(false)
            .show(ui, |ui| self.rail(ui));
        if self.panel.open {
            let connected = self.handle.problem().is_none();
            egui::Panel::left("tools")
                .exact_size(panel::WIDTH)
                .resizable(false)
                .frame(bare(PANEL))
                .show_separator_line(false)
                .show(ui, |ui| self.panel.ui(ui, &self.handle, connected));
        }
        egui::CentralPanel::default().frame(bare(BG)).show(ui, |ui| {
            if self.live() {
                self.screen(ui);
            } else {
                self.end_scroll();
                self.offline(ui);
            }
        });
        self.toasts(ui);
        self.ui_us = (self.ui_us * 9 + pass_started.elapsed().as_micros() as u64) / 10;
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

/// An undecorated window has no border to grab, so resize it from its edges here.
fn resize_from_edges(ctx: &egui::Context) {
    let Some(pos) = ctx.pointer_hover_pos() else {
        return;
    };
    let window = ctx.content_rect();
    let (left, right) = (pos.x < window.left() + RESIZE_GRIP, pos.x > window.right() - RESIZE_GRIP);
    let (top, bottom) = (pos.y < window.top() + RESIZE_GRIP, pos.y > window.bottom() - RESIZE_GRIP);
    let (direction, cursor) = match (left, right, top, bottom) {
        (true, _, true, _) => (ResizeDirection::NorthWest, CursorIcon::ResizeNwSe),
        (_, true, _, true) => (ResizeDirection::SouthEast, CursorIcon::ResizeNwSe),
        (true, _, _, true) => (ResizeDirection::SouthWest, CursorIcon::ResizeNeSw),
        (_, true, true, _) => (ResizeDirection::NorthEast, CursorIcon::ResizeNeSw),
        (true, ..) => (ResizeDirection::West, CursorIcon::ResizeHorizontal),
        (_, true, ..) => (ResizeDirection::East, CursorIcon::ResizeHorizontal),
        (_, _, true, _) => (ResizeDirection::North, CursorIcon::ResizeVertical),
        (_, _, _, true) => (ResizeDirection::South, CursorIcon::ResizeVertical),
        _ => return,
    };
    ctx.set_cursor_icon(cursor);
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(direction));
    }
}

fn icon_button(ui: &mut Ui, glyph: &str, size: Vec2, glyph_size: f32, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        RAISED.gamma_multiply(1.5)
    } else if response.hovered() {
        RAISED
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, 8.0, fill);
    let color = match (ui.is_enabled(), active, response.hovered()) {
        (false, ..) => DIM.gamma_multiply(0.45),
        (_, true, _) => OK,
        (_, _, true) => TEXT,
        _ => DIM,
    };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, FontId::proportional(glyph_size), color);
    response
}

fn rail_button(ui: &mut Ui, glyph: &str, tip: &str, active: bool) -> egui::Response {
    let response = icon_button(ui, glyph, Vec2::splat(36.0), 19.0, active);
    if tip.is_empty() { response } else { response.on_hover_text(tip) }
}

fn chrome_button(ui: &mut Ui, glyph: &str, tip: &str, active: bool) -> egui::Response {
    icon_button(ui, glyph, Vec2::new(32.0, 26.0), 15.0, active).on_hover_text(tip)
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(16)
        .inner_margin(22)
}

fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(16.0).color(TEXT).strong());
}

fn dim(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text).color(DIM));
}

fn style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0, LINE);
    visuals.override_text_color = Some(TEXT);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.interaction.selectable_labels = false;
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    });
}

fn main() -> eframe::Result {
    // `--vm host:port` opens a virtual iPhone (an Inferno emulator) instead of a phone.
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let (true, Some(address)) = (arg == "--vm", args.next()) {
            // SAFETY: no other thread is running yet.
            unsafe { std::env::set_var(ilinkos::vm::TARGET_VAR, address) };
        }
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/ilinkos.png")).ok();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("iLinkOS")
        .with_inner_size([452.0, 920.0])
        .with_min_inner_size([300.0, 420.0])
        .with_decorations(false);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("iLinkOS", options, Box::new(|cc| Ok(Box::new(App::new(cc)?))))
}
