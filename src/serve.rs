//! The long-running daemon: holds one session with the phone, decodes its screen,
//! and serves the control API and frames to the MCP server and the desktop app.

use std::{
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::{Notify, broadcast, mpsc, oneshot, watch},
};

use crate::control::{Clip, HevcReader, Modifiers, Session, VIDEO_SENDER_PORT};
use crate::record::{Piece, Recording};
use crate::tools::{Perf, PhoneEvent};
use idevice::core_device::{RtpPacket, build_keyframe_request, is_rtcp};

pub const DEFAULT_PORT: u16 = 7878;
/// Frames are scaled to this height: plenty for a window, cheap for a model to read.
const FRAME_HEIGHT: u32 = 1280;
const RETRY_DELAY: Duration = Duration::from_secs(3);
const CLIPBOARD_POLL: Duration = Duration::from_millis(800);
/// How often the session is checked on while no action is waiting.
const TICK: Duration = Duration::from_millis(400);
/// A settled picture is one that has not changed for this long...
const SETTLED_FOR: Duration = Duration::from_millis(350);
/// ...and nothing counts as settled until this long after the last action, since
/// the phone takes a moment to start reacting.
const REACTION_TIME: Duration = Duration::from_millis(300);
/// A picture that never settles (a video, a spinner) is handed over after this long.
const SETTLE_LIMIT: Duration = Duration::from_millis(3000);
/// Pixels are compared on a grid this many pixels apart, ...
const SETTLE_GRID: usize = 16;
/// ...and a picture has changed if more than this share of them differ noticeably.
/// Small enough to see a toggle flip; large enough to ignore a blinking cursor.
const SETTLE_CHANGED: f64 = 0.002;
/// How long a new simulated location may take to be accepted.
const LOCATION_WAIT: Duration = Duration::from_secs(15);
/// With no audio packets for this long, the phone's audio stream is taken as dead.
const AUDIO_STALL: Duration = Duration::from_secs(5);
/// Cleared if ffmpeg cannot decode on an NVIDIA GPU, so the next start skips it.
static CUDA_WORKS: AtomicBool = AtomicBool::new(true);
/// Smoothed milliseconds between a frame arriving and its picture being ready.
static DECODE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Action {
    Tap { x: f64, y: f64 },
    Down { x: f64, y: f64 },
    Move { x: f64, y: f64 },
    Up { x: f64, y: f64 },
    Swipe { x1: f64, y1: f64, x2: f64, y2: f64, ms: Option<u64> },
    Home,
    /// The app switcher.
    Apps,
    Pinch { x: f64, y: f64, scale: f64, ms: Option<u64> },
    /// One raw multi-finger frame: `[finger, x, y, down]` per finger.
    Touches { fingers: Vec<(u8, f64, f64, bool)> },
    ControlCenter,
    Notifications,
    Spotlight,
    Back,
    Rotate { clockwise: bool },
    /// A key with modifiers held; `key` is a named key or one character.
    Combo {
        key: String,
        #[serde(default)]
        command: bool,
        #[serde(default)]
        shift: bool,
        #[serde(default)]
        option: bool,
        #[serde(default)]
        control: bool,
    },
    /// Puts text on the phone's clipboard and pastes it, which handles any language.
    Paste { text: String },
    /// A lossless full-resolution capture; replies with PNG bytes.
    Screenshot,
    /// The phone's name, model, iOS version, battery and appearance, as JSON.
    Info,
    /// Sets light or dark appearance when `style` is given; replies like `Info`.
    Appearance { style: Option<String> },
    /// Lists running apps and processes; replies with JSON.
    Processes,
    /// Lists crash reports on the phone; replies with JSON.
    CrashReports,
    /// Fetches one crash report; replies with its bytes.
    CrashReport { name: String },
    /// Lists installed apps; replies with JSON.
    InstalledApps,
    /// Opens an app by bundle id or name; replies with JSON describing it.
    Launch { app: String },
    /// Lists what is on screen as labelled elements with positions; replies with
    /// JSON. Briefly flashes a highlight across the phone's screen.
    Elements,
    /// Replaces the phone's clipboard.
    #[serde(skip)]
    SetClipboard(Clip),
    Button { name: String },
    Key { name: String },
    Text { text: String },
    /// Starts or stops recording the screen and sound to an MP4; replies with
    /// JSON naming the file. `path` defaults to a dated file under Videos\\iLinkOS.
    Record { on: bool, path: Option<String> },
    /// Makes the phone report this position as its location until `LocationOff`.
    Location { lat: f64, lon: f64 },
    /// Returns the phone to its real location.
    LocationOff,
    /// What the app in front is using right now; replies with JSON.
    Perf,
}

/// A picture as it came out of the decoder.
pub enum Pixels {
    Jpeg(Bytes),
    /// Uncompressed RGBA rows, top to bottom.
    Rgba(Bytes),
}

pub struct Frame {
    pub pixels: Pixels,
    pub width: u32,
    pub height: u32,
    pub seq: u64,
    pub at: Instant,
}

/// What an action produced.
pub enum Output {
    Done,
    Png(Vec<u8>),
    Json(String),
    Bytes(Vec<u8>),
}

type Reply = oneshot::Sender<Result<Output, String>>;

struct App {
    token: String,
    actions: mpsc::Sender<(Action, Option<Reply>)>,
    frames: watch::Receiver<Option<Arc<Frame>>>,
    /// Why the phone is not connected, if it is not.
    problem: Mutex<Option<String>>,
    pairing: Mutex<Pairing>,
    runtime: tokio::runtime::Handle,
    /// Whether to watch the phone's clipboard; only the desktop app turns this on.
    clipboard_sync: AtomicBool,
    /// The phone's clipboard, each time it changes.
    clips: watch::Sender<Option<Arc<Clip>>>,
    /// The phone's sound, one encoded frame per message, for whoever is listening.
    audio: broadcast::Sender<Bytes>,
    /// The live session's tunnel, for work that runs beside the session.
    tunnel: Mutex<Option<crate::device::Tunnel>>,
    /// When the last action was sent to the phone.
    acted: Mutex<Instant>,
    /// Counts sessions, so work beside the session can tell when its tunnel is stale.
    sessions: AtomicU64,
    /// The recording in progress, fed by the stream pumps.
    recording: Mutex<Option<Recording>>,
    /// Signalled to ask the phone for a keyframe.
    keyframe: Arc<Notify>,
    /// Set to have the session rebuilt, which restarts the streams.
    restart: Arc<AtomicBool>,
    /// The simulated location, and the task holding it in place.
    location: Mutex<Option<(f64, f64)>>,
    location_changes: Mutex<Option<mpsc::UnboundedSender<Option<(f64, f64)>>>>,
    /// Told how the latest location change went.
    location_set: Mutex<Option<oneshot::Sender<Result<(), String>>>>,
    /// Performance samples, while anyone is listening.
    perf: broadcast::Sender<Perf>,
    sampling: AtomicBool,
    /// Things happening on the phone, while anyone is listening.
    events: broadcast::Sender<PhoneEvent>,
    watching: AtomicBool,
    /// The phone's system log, a line per message, while anyone is listening.
    logs: broadcast::Sender<String>,
    logging: AtomicBool,
}

/// Progress of a pairing started from the app.
#[derive(Default, Clone, serde::Serialize)]
pub struct Pairing {
    pub active: bool,
    /// The code the user must type on the phone, once the phone has connected.
    pub pin: Option<String>,
    pub error: Option<String>,
}

/// A live session plus the processes decoding its video.
struct Live {
    session: Session,
    dead: Arc<AtomicBool>,
    _ffmpeg: Child,
    /// The phone's clipboard change counter when last seen.
    clip_change: Option<i64>,
    /// The pictures, for locating accessibility elements.
    frames: watch::Receiver<Option<Arc<Frame>>>,
}

impl Live {
    async fn start(
        usb: bool,
        udid: Option<&str>,
        frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
        app: &Arc<App>,
    ) -> Result<Self> {
        let sound = app.audio.clone();
        let mut session = Session::start(crate::device::open_tunnel(usb, udid).await?).await?;
        let frames_rx = frames.subscribe();
        session.warm_up().await?;
        let video = session.take_video().context("session has no video socket")?;

        // Phone -> anyone listening. Nothing is decoded here: with no listener
        // (the headless daemon) the frames are simply dropped.
        let dead = Arc::new(AtomicBool::new(false));
        if let Some(audio) = session.take_audio() {
            let (flag, app) = (dead.clone(), app.clone());
            tokio::spawn(async move {
                let mut heard = false;
                loop {
                    let datagram = match tokio::time::timeout(AUDIO_STALL, audio.recv()).await {
                        Ok(Ok(datagram)) => datagram,
                        Ok(Err(_)) => return,
                        Err(_) => {
                            // The phone sends packets even for silence, so a gap means the
                            // audio stream has died. If someone is listening, reconnect.
                            let recording = app.recording.lock().unwrap().is_some();
                            if heard && (recording || sound.receiver_count() > 0) {
                                flag.store(true, Ordering::Relaxed);
                                return;
                            }
                            continue;
                        }
                    };
                    heard = true;
                    if is_rtcp(&datagram.data) {
                        continue;
                    }
                    let recording = app.recording.lock().unwrap().as_ref().map(|r| r.pieces.clone());
                    if recording.is_none() && sound.receiver_count() == 0 {
                        continue;
                    }
                    if let Some(packet) = RtpPacket::parse(&datagram.data) {
                        let payload = Bytes::copy_from_slice(packet.payload);
                        if let Some(recording) = recording {
                            let _ = recording.send(Piece::Audio(payload.clone()));
                        }
                        let _ = sound.send(payload);
                    }
                }
            });
        }

        // How ffmpeg decodes, chosen by ILINKOS_HWACCEL:
        //   cuda     decode and scale on an NVIDIA GPU: by far the cheapest (default,
        //            dropped automatically on machines where it does not start)
        //   d3d11va  decode on any GPU, scale on the CPU
        //   none     everything on the CPU
        let wanted = std::env::var("ILINKOS_HWACCEL").unwrap_or_else(|_| "cuda".into());
        let hwaccel = match wanted.as_str() {
            "none" => None,
            "cuda" if !CUDA_WORKS.load(Ordering::Relaxed) => Some("d3d11va"),
            "cuda" => Some("cuda"),
            _ => Some("d3d11va"),
        };
        let cuda = hwaccel == Some("cuda");
        // ILINKOS_FRAMES=jpeg has ffmpeg hand over JPEGs; the default is uncompressed
        // pictures, which skips an encode here and a decode in the window.
        let raw = std::env::var("ILINKOS_FRAMES").as_deref() != Ok("jpeg");
        // The longer side becomes FRAME_HEIGHT, whichever way up the phone is, and
        // the phone's limited-range colour becomes full range.
        let size = format!("'if(gt(iw,ih),{FRAME_HEIGHT},-2)':'if(gt(iw,ih),-2,{FRAME_HEIGHT})'");
        let format = if raw { "rgba" } else { "yuvj420p" };
        let scale = if cuda {
            // Scaled on the GPU, so only the small picture is copied back.
            format!("scale_cuda={size}:format=nv12,hwdownload,format=nv12,scale=out_range=pc,format={format}")
        } else {
            format!("scale={size}:out_range=pc,format={format}")
        };
        let mut ffmpeg = Command::new(crate::ffmpeg())
            // One decoder thread: frame threading holds back the newest frames.
            .args(["-loglevel", "error", "-threads", "1"])
            .args(hwaccel.iter().flat_map(|h| ["-hwaccel", h]))
            .args(if cuda { &["-hwaccel_output_format", "cuda"][..] } else { &[] })
            .args(["-probesize", "32", "-analyzeduration", "0", "-f", "hevc", "-i", "pipe:0"])
            .args(["-vf", &scale, "-fps_mode", "passthrough", "-q:v", "4"])
            // One encoder thread and a flush per image: both otherwise hold frames back.
            .args(["-threads", "1", "-flush_packets", "1"])
            .args(["-f", "image2pipe", "-c:v", if raw { "pam" } else { "mjpeg" }, "pipe:1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // No console window when launched from the GUI app.
            .creation_flags(0x0800_0000)
            .kill_on_drop(true)
            .spawn()
            .context("could not start ffmpeg — it must be beside iLinkOS or on PATH")?;
        let mut stdin = ffmpeg.stdin.take().unwrap();
        // When each frame finished arriving, to time how long decoding holds it.
        let arrivals = Arc::new(Mutex::new(std::collections::VecDeque::<Instant>::new()));
        let decoded = arrivals.clone();
        let mut stdout = ffmpeg.stdout.take().unwrap();

        // Phone -> ffmpeg.
        let (flag, app, our_ssrc) = (dead.clone(), app.clone(), session.video_ssrc);
        tokio::spawn(async move {
            let mut hevc = HevcReader::default();
            // Where the phone sends from, which is where requests to it go.
            let (mut requests, mut sender) = (0u8, VIDEO_SENDER_PORT);
            loop {
                let datagram = tokio::select! {
                    datagram = video.recv() => datagram,
                    _ = app.keyframe.notified() => {
                        if let Some(theirs) = hevc.ssrc() {
                            requests = requests.wrapping_add(1);
                            let request = build_keyframe_request(our_ssrc, "ilinkos", theirs, &[], requests);
                            let _ = video.send_to(sender, request).await;
                        }
                        continue;
                    }
                };
                let Ok(datagram) = datagram else {
                    break;
                };
                sender = datagram.source_port;
                let (out, frame_end) = hevc.push(&datagram.data);
                if out.is_empty() {
                    continue;
                }
                if let Some(recording) = app.recording.lock().unwrap().as_ref() {
                    let piece = Piece::Video { bytes: out.clone(), frame_end, timestamp: hevc.timestamp() };
                    let _ = recording.pieces.send(piece);
                }
                if stdin.write_all(&out).await.is_err() {
                    break;
                }
                if frame_end {
                    arrivals.lock().unwrap().push_back(Instant::now());
                }
            }
            flag.store(true, Ordering::Relaxed);
        });

        // ffmpeg -> latest frame.
        let flag = dead.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 1024 * 1024];
            let (mut scanned, mut seq) = (0usize, 0u64);
            while let Ok(n) = stdout.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                loop {
                    let (pixels, size) = if raw {
                        let Some((width, height, start, end)) = find_pam(&buf) else {
                            break;
                        };
                        let rgba = Bytes::copy_from_slice(&buf[start..end]);
                        buf.drain(..end);
                        (Pixels::Rgba(rgba), Some((width, height)))
                    } else {
                        // Entropy-coded data is byte-stuffed, so FF D9 only ever ends an image.
                        let Some(end) = find_eoi(&buf, scanned) else {
                            break;
                        };
                        let jpeg: Vec<u8> = buf.drain(..end).collect();
                        scanned = 0;
                        let size = jpeg_size(&jpeg);
                        (Pixels::Jpeg(jpeg.into()), size)
                    };
                    let arrived = decoded.lock().unwrap().pop_front();
                    if let Some((width, height)) = size {
                        seq += 1;
                        if let Some(arrived) = arrived {
                            let ms = arrived.elapsed().as_secs_f64() * 1000.0;
                            let avg = f64::from_bits(DECODE_MS.load(Ordering::Relaxed));
                            DECODE_MS.store((avg * 0.9 + ms * 0.1).to_bits(), Ordering::Relaxed);
                        }
                        let frame = Frame { pixels, width, height, seq, at: Instant::now() };
                        frames.send_replace(Some(Arc::new(frame)));
                    }
                }
                scanned = buf.len().saturating_sub(1);
            }
            // ffmpeg stopped, e.g. because the picture changed shape: start over.
            if cuda && seq == 0 {
                // It never produced a picture: no usable NVIDIA GPU. Don't try again.
                CUDA_WORKS.store(false, Ordering::Relaxed);
            }
            flag.store(true, Ordering::Relaxed);
        });

        Ok(Self { session, dead, _ffmpeg: ffmpeg, clip_change: None, frames: frames_rx })
    }

    async fn run(&mut self, action: &Action) -> Result<Output> {
        let s = &mut self.session;
        match action {
            Action::Screenshot => return Ok(Output::Png(s.screenshot_png().await?)),
            Action::Info | Action::Appearance { .. } => {
                let (handle, rsd) = s.tunnel();
                if let Action::Appearance { style: Some(style) } = action {
                    crate::tools::appearance(handle, rsd, Some(style)).await?;
                }
                let info = crate::tools::info(handle, rsd).await?;
                return Ok(Output::Json(info.to_string()));
            }
            Action::Processes => {
                let (handle, rsd) = s.tunnel();
                let processes = crate::tools::processes(handle, rsd).await?;
                return Ok(Output::Json(serde_json::to_string(&processes)?));
            }
            Action::CrashReports => {
                let (handle, rsd) = s.tunnel();
                let reports = crate::tools::crash_reports(handle, rsd).await?;
                return Ok(Output::Json(serde_json::to_string(&reports)?));
            }
            Action::CrashReport { name } => {
                let (handle, rsd) = s.tunnel();
                return Ok(Output::Bytes(crate::tools::crash_report(handle, rsd, name).await?));
            }
            Action::InstalledApps => {
                let (handle, rsd) = s.tunnel();
                let apps = crate::tools::apps(handle, rsd).await?;
                return Ok(Output::Json(serde_json::to_string(&apps)?));
            }
            Action::Launch { app } => {
                let (handle, rsd) = s.tunnel();
                let opened = crate::tools::launch(handle, rsd, app).await?;
                return Ok(Output::Json(serde_json::to_string(&opened)?));
            }
            Action::Elements => {
                let elements = s.inspector().await?.elements(Some(&mut self.frames)).await?;
                return Ok(Output::Json(serde_json::to_string(&elements)?));
            }
            Action::ControlCenter => s.control_center().await,
            Action::Notifications => s.notifications().await,
            Action::Back => s.back().await,
            Action::Spotlight => {
                s.combo("space", Modifiers { command: true, ..Default::default() }).await
            }
            Action::Rotate { clockwise } => s.rotate(*clockwise).await,
            Action::Combo { key, command, shift, option, control } => {
                let mods = Modifiers {
                    command: *command,
                    shift: *shift,
                    option: *option,
                    control: *control,
                };
                s.combo(key, mods).await
            }
            Action::Paste { text } => {
                s.clipboard_set(&Clip::Text(text.clone())).await?;
                self.clip_change = s.clipboard_change().await?;
                self.session.combo("v", Modifiers { command: true, ..Default::default() }).await
            }
            Action::SetClipboard(clip) => {
                s.clipboard_set(clip).await?;
                // Remember our own write so it is not synced straight back.
                self.clip_change = s.clipboard_change().await?;
                Ok(())
            }
            Action::Tap { x, y } => s.tap(*x, *y).await,
            Action::Down { x, y } | Action::Move { x, y } => s.touch_sample(*x, *y, true).await,
            Action::Up { x, y } => {
                // A final contact where the finger lifts, so a dropped move can't leave it behind.
                s.touch_sample(*x, *y, true).await?;
                s.touch_sample(*x, *y, false).await
            }
            Action::Swipe { x1, y1, x2, y2, ms } => {
                s.drag((*x1, *y1), (*x2, *y2), ms.unwrap_or(400)).await
            }
            Action::Home => s.button("home").await,
            Action::Apps => s.app_switcher().await,
            Action::Touches { fingers } => s.touches(fingers).await,
            Action::Pinch { x, y, scale, ms } => s.pinch(*x, *y, *scale, ms.unwrap_or(350)).await,
            Action::Button { name } => s.button(name).await,
            Action::Key { name } => s.key(name).await,
            Action::Text { text } => s.text(text).await,
            // Handled beside the session, in `App::beside`.
            Action::Record { .. } | Action::Location { .. } | Action::LocationOff | Action::Perf => Ok(()),
        }?;
        Ok(Output::Done)
    }

    /// Returns the phone's clipboard if it changed since the last look.
    async fn changed_clipboard(&mut self) -> Result<Option<Clip>> {
        let change = self.session.clipboard_change().await?;
        if change == self.clip_change {
            return Ok(None);
        }
        // The first look only records where the counter stands.
        let first = self.clip_change.is_none();
        self.clip_change = change;
        if first { Ok(None) } else { self.session.clipboard_get().await }
    }
}

impl Frame {
    /// The picture as a JPEG, encoding it now if it is held uncompressed.
    pub fn jpeg(&self) -> Bytes {
        match &self.pixels {
            Pixels::Jpeg(jpeg) => jpeg.clone(),
            Pixels::Rgba(rgba) => {
                let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
                let mut jpeg = Vec::new();
                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85);
                let _ = encoder.encode(&rgb, self.width, self.height, image::ExtendedColorType::Rgb8);
                jpeg.into()
            }
        }
    }
}

/// Splits one PAM image off the front of `buf`: returns its size and where its
/// RGBA pixels start and end, once the whole image has arrived.
fn find_pam(buf: &[u8]) -> Option<(u32, u32, usize, usize)> {
    const END: &[u8] = b"ENDHDR\n";
    let header_end = buf.windows(END.len()).position(|w| w == END)? + END.len();
    let header = std::str::from_utf8(&buf[..header_end]).ok()?;
    let field = |name: &str| -> Option<u32> {
        header.lines().find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
    };
    let (width, height) = (field("WIDTH")?, field("HEIGHT")?);
    let end = header_end + width as usize * height as usize * 4;
    (buf.len() >= end).then_some((width, height, header_end, end))
}

/// Returns the index just past the first end-of-image marker at or after `from`.
fn find_eoi(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..].windows(2).position(|w| w == [0xFF, 0xD9]).map(|i| from + i + 2)
}

/// Reads the pixel size from a JPEG's start-of-frame segment.
fn jpeg_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 9 < jpeg.len() && jpeg[i] == 0xFF {
        let (marker, len) = (jpeg[i + 1], u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize);
        if (0xC0..=0xC2).contains(&marker) {
            let height = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]) as u32;
            let width = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]) as u32;
            return Some((width, height));
        }
        i += 2 + len;
    }
    None
}

/// Owns the session: connects, reconnects on failure, and runs actions in order.
async fn actor(
    app: Arc<App>,
    mut actions: mpsc::Receiver<(Action, Option<Reply>)>,
    frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
    usb: bool,
    udid: Option<String>,
) {
    let mut live: Option<Live> = None;
    let mut tried_mount = false;
    let mut clip_checked = Instant::now();
    // An action taken off the queue while skipping stale moves.
    let mut held: Option<(Action, Option<Reply>)> = None;
    loop {
        if live.as_ref().is_some_and(|l| l.dead.load(Ordering::Relaxed))
            || app.restart.swap(false, Ordering::Relaxed)
        {
            live = None;
        }
        if live.is_none() {
            match Live::start(usb, udid.as_deref(), frames.clone(), &app).await {
                Ok(l) => {
                    eprintln!("connected to the phone");
                    *app.problem.lock().unwrap() = None;
                    tried_mount = false;
                    live = Some(l);
                    app.share_tunnel(&mut live);
                }
                Err(e) => {
                    let mut problem = format!("{e:#}");
                    // The image unmounts whenever the phone restarts; put it back once.
                    if problem.contains("no display service") && !tried_mount {
                        tried_mount = true;
                        *app.problem.lock().unwrap() = Some("mounting the developer image".into());
                        let mounted = match crate::device::open_tunnel(usb, udid.as_deref()).await {
                            Ok(tunnel) => crate::device::mount(tunnel).await,
                            Err(e) => Err(e),
                        };
                        match mounted {
                            Ok(_) => continue,
                            Err(e) => problem = format!("could not mount the developer image: {e:#}"),
                        }
                    }
                    eprintln!("not connected: {problem}");
                    *app.problem.lock().unwrap() = Some(problem.clone());
                    // Answer callers while waiting to retry rather than leaving them hanging.
                    let retry = tokio::time::sleep(RETRY_DELAY);
                    tokio::pin!(retry);
                    loop {
                        tokio::select! {
                            _ = &mut retry => break,
                            next = actions.recv() => match next {
                                Some((_, Some(reply))) => { let _ = reply.send(Err(problem.clone())); }
                                Some(_) => {}
                                None => return,
                            },
                        }
                    }
                    continue;
                }
            }
        }
        let next = match held.take() {
            Some(next) => Some(next),
            None => {
                tokio::select! {
                    next = actions.recv() => next,
                    _ = tokio::time::sleep(TICK) => {
                        if app.clipboard_sync.load(Ordering::Relaxed) && clip_checked.elapsed() >= CLIPBOARD_POLL {
                            clip_checked = Instant::now();
                            if let Ok(Some(clip)) = live.as_mut().unwrap().changed_clipboard().await {
                                app.clips.send_replace(Some(Arc::new(clip)));
                            }
                        }
                        continue;
                    }
                }
            }
        };
        let Some((mut action, reply)) = next else {
            return;
        };
        if let Some(result) = app.beside(&action).await {
            if let Some(reply) = reply {
                let _ = reply.send(result.map_err(|e| format!("{e:#}")));
            }
            continue;
        }
        // Only the newest finger position matters: skip moves that are already stale.
        while matches!(action, Action::Move { .. }) {
            match actions.try_recv() {
                Ok((newer @ Action::Move { .. }, _)) => action = newer,
                Ok(other) => {
                    held = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }
        let session = live.as_mut().unwrap();
        let mut result = session.run(&action).await;
        if result.is_err() || session.dead.load(Ordering::Relaxed) {
            // The tunnel drops when the phone sleeps or changes network: rebuild and retry once.
            live = None;
            if let Ok(mut fresh) = Live::start(usb, udid.as_deref(), frames.clone(), &app).await {
                result = fresh.run(&action).await;
                live = Some(fresh);
                app.share_tunnel(&mut live);
            }
        }
        if let Some(reply) = reply {
            let _ = reply.send(result.map_err(|e| format!("{e:#}")));
        }
    }
}

impl App {
    /// Publishes the live session's tunnel for tasks that run beside it.
    fn share_tunnel(&self, live: &mut Option<Live>) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        *self.tunnel.lock().unwrap() = live.as_mut().map(|live| {
            let (handle, rsd) = live.session.tunnel();
            (handle.clone(), rsd.clone())
        });
    }
}

impl App {
    /// Runs the actions that work beside the session rather than through it.
    async fn beside(self: &Arc<Self>, action: &Action) -> Option<Result<Output>> {
        Some(match action {
            Action::Record { on: true, path } => self.start_recording(path.as_deref()),
            Action::Record { on: false, .. } => self.stop_recording().await,
            Action::Location { lat, lon } => self.set_location(*lat, *lon).await,
            Action::LocationOff => {
                if let Some(changes) = self.location_changes.lock().unwrap().take() {
                    let _ = changes.send(None);
                }
                *self.location.lock().unwrap() = None;
                Ok(Output::Done)
            }
            Action::Perf => {
                let mut samples = self.perf.subscribe();
                start_sampling(self.clone());
                match tokio::time::timeout(Duration::from_secs(8), samples.recv()).await {
                    Ok(Ok(sample)) => serde_json::to_string(&sample).map(Output::Json).map_err(Into::into),
                    _ => Err(anyhow::anyhow!("the phone sent no performance figures")),
                }
            }
            _ => return None,
        })
    }

    fn start_recording(self: &Arc<Self>, path: Option<&str>) -> Result<Output> {
        let mut recording = self.recording.lock().unwrap();
        if recording.is_none() {
            let path = match path {
                Some(path) => std::path::PathBuf::from(path),
                None => {
                    let home = std::env::var_os("USERPROFILE").context("no user folder")?;
                    let name = chrono::Local::now().format("iPhone %Y-%m-%d %H.%M.%S.mp4");
                    std::path::PathBuf::from(home).join("Videos").join("iLinkOS").join(name.to_string())
                }
            };
            let restart = self.restart.clone();
            *recording = Some(Recording::start(path, self.runtime.clone(), self.keyframe.clone(), move || {
                restart.store(true, Ordering::Relaxed)
            })?);
        }
        let path = recording.as_ref().unwrap().path.display().to_string();
        Ok(Output::Json(serde_json::json!({ "recording": true, "path": path }).to_string()))
    }

    async fn stop_recording(&self) -> Result<Output> {
        let recording = self.recording.lock().unwrap().take().context("nothing is being recorded")?;
        let seconds = recording.since.elapsed().as_secs();
        let path = recording.finish().await.map_err(anyhow::Error::msg)?;
        let path = path.display().to_string();
        Ok(Output::Json(serde_json::json!({ "recording": false, "path": path, "seconds": seconds }).to_string()))
    }

    async fn set_location(self: &Arc<Self>, lat: f64, lon: f64) -> Result<Output> {
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            anyhow::bail!("latitude is -90 to 90 and longitude -180 to 180");
        }
        let (set, done) = oneshot::channel();
        *self.location_set.lock().unwrap() = Some(set);
        let sent = self
            .location_changes
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|changes| changes.send(Some((lat, lon))).is_ok());
        if !sent {
            hold_location(self.clone(), (lat, lon))?;
        }
        match tokio::time::timeout(LOCATION_WAIT, done).await {
            Ok(Ok(Ok(()))) => Ok(Output::Done),
            Ok(Ok(Err(e))) => Err(anyhow::Error::msg(e)),
            _ => Err(anyhow::anyhow!("the phone did not accept the location")),
        }
    }
}

/// Runs work that lives beside the session on a thread of its own: the device
/// futures are not `Send`, and they need a deep stack.
fn spawn_beside<F: std::future::Future<Output = ()>>(
    app: &Arc<App>,
    work: impl FnOnce(Arc<App>) -> F + Send + 'static,
) -> std::io::Result<()> {
    let (app, runtime) = (app.clone(), app.runtime.clone());
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || runtime.block_on(work(app)))
        .map(|_| ())
}

/// Resolves once the session this tunnel came from has been replaced.
async fn session_replaced(app: &App, session: u64) {
    while app.sessions.load(Ordering::Relaxed) == session {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Keeps the phone at a simulated location, re-applying it when the connection
/// is rebuilt, until told to stop.
fn hold_location(app: Arc<App>, at: (f64, f64)) -> Result<()> {
    let (changes, mut incoming) = mpsc::unbounded_channel();
    *app.location_changes.lock().unwrap() = Some(changes);
    spawn_beside(&app, move |app| async move {
        let mut at = at;
        loop {
            let session = app.sessions.load(Ordering::Relaxed);
            let tunnel = app.tunnel.lock().unwrap().clone();
            let result = match tunnel {
                Some((mut handle, mut rsd)) => {
                    let set = |at: (f64, f64), result: Result<()>| {
                        if result.is_ok() {
                            *app.location.lock().unwrap() = Some(at);
                        }
                        if let Some(set) = app.location_set.lock().unwrap().take() {
                            let _ = set.send(result.map_err(|e| format!("{e:#}")));
                        }
                    };
                    tokio::select! {
                        result = crate::tools::simulate_location(&mut handle, &mut rsd, at, &mut incoming, set) => result,
                        _ = session_replaced(&app, session) => Err(anyhow::anyhow!("reconnected")),
                    }
                }
                None => Err(anyhow::anyhow!("the phone is not connected")),
            };
            // Stopped on request, or failed while someone was waiting to hear: give up.
            let waiting = app.location_set.lock().unwrap().take();
            match (result, waiting) {
                (Ok(()), _) => break,
                (Err(e), Some(set)) => {
                    let _ = set.send(Err(format!("{e:#}")));
                    break;
                }
                (Err(_), None) => {}
            }
            // The connection dropped: put the location back once it returns.
            match *app.location.lock().unwrap() {
                Some(last) => at = last,
                None => break,
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        *app.location.lock().unwrap() = None;
        app.location_changes.lock().unwrap().take();
    })?;
    Ok(())
}

/// Samples the app in front for as long as anyone listens.
fn start_sampling(app: Arc<App>) {
    if app.sampling.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = spawn_beside(&app, |app| async move {
        while app.perf.receiver_count() > 0 {
            let tunnel = app.tunnel.lock().unwrap().clone();
            if let Some((mut handle, mut rsd)) = tunnel {
                let _ = crate::tools::perf(&mut handle, &mut rsd, |sample| app.perf.send(sample).is_ok()).await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        app.sampling.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        app.sampling.store(false, Ordering::Relaxed);
    }
}

/// Watches for things happening on the phone for as long as anyone listens.
fn start_watching(app: Arc<App>) {
    if app.watching.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = spawn_beside(&app, |app| async move {
        while app.events.receiver_count() > 0 {
            let tunnel = app.tunnel.lock().unwrap().clone();
            if let Some((mut handle, mut rsd)) = tunnel {
                let _ = crate::tools::events(&mut handle, &mut rsd, |event| app.events.send(event).is_ok()).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        app.watching.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        app.watching.store(false, Ordering::Relaxed);
    }
}

/// Streams the phone's system log to `app.logs` for as long as anyone listens.
fn start_logging(app: Arc<App>) {
    if app.logging.swap(true, Ordering::Relaxed) {
        return;
    }
    let runtime = app.runtime.clone();
    let spawned = std::thread::Builder::new().stack_size(16 * 1024 * 1024).spawn({
        let app = app.clone();
        move || {
            runtime.block_on(async {
                while app.logs.receiver_count() > 0 {
                    let tunnel = app.tunnel.lock().unwrap().clone();
                    if let Some((mut handle, mut rsd)) = tunnel {
                        let _ = crate::tools::logs(&mut handle, &mut rsd, |line| {
                            app.logs.send(line.to_owned()).is_ok()
                        })
                        .await;
                    }
                    // Not connected, or the stream ended: try again shortly.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            });
            app.logging.store(false, Ordering::Relaxed);
        }
    });
    if spawned.is_err() {
        app.logging.store(false, Ordering::Relaxed);
    }
}

/// In-process access to the daemon, for the desktop app.
#[derive(Clone)]
pub struct Handle {
    app: Arc<App>,
}

impl Handle {
    /// Queues an action without waiting for it. Finger moves are dropped if the
    /// phone is behind, since a stale position is worthless.
    pub fn send(&self, action: Action) {
        if let Err(mpsc::error::TrySendError::Full((action, _))) =
            self.app.actions.try_send((action, None))
        {
            if !matches!(action, Action::Move { .. }) {
                let _ = self.app.actions.blocking_send((action, None));
            }
        }
    }

    /// The newest picture of the phone's screen.
    pub fn frames(&self) -> watch::Receiver<Option<Arc<Frame>>> {
        self.app.frames.clone()
    }

    /// Why the phone is not connected, or `None` if it is.
    pub fn problem(&self) -> Option<String> {
        self.app.problem.lock().unwrap().clone()
    }

    pub fn pairing(&self) -> Pairing {
        self.app.pairing.lock().unwrap().clone()
    }

    /// Runs an action and hands its result to `done` on a background thread.
    pub fn request(&self, action: Action, done: impl FnOnce(Result<Output, String>) + Send + 'static) {
        let app = self.app.clone();
        self.app.runtime.spawn(async move {
            let (tx, rx) = oneshot::channel();
            let result = match app.actions.send((action, Some(tx))).await {
                Ok(()) => rx.await.unwrap_or_else(|_| Err("the action was dropped".into())),
                Err(_) => Err("iLinkOS is shutting down".into()),
            };
            done(result);
        });
    }

    /// Turns watching the phone's clipboard on or off.
    pub fn set_clipboard_sync(&self, on: bool) {
        self.app.clipboard_sync.store(on, Ordering::Relaxed);
    }

    /// The phone's sound: one AAC-ELD frame per message (48 kHz stereo, 480 samples).
    pub fn audio(&self) -> broadcast::Receiver<Bytes> {
        self.app.audio.subscribe()
    }

    /// The phone's system log, a line per message, from now on.
    pub fn logs(&self) -> broadcast::Receiver<String> {
        let lines = self.app.logs.subscribe();
        start_logging(self.app.clone());
        lines
    }

    /// How long the current recording has been running, if there is one.
    pub fn recording(&self) -> Option<Duration> {
        self.app.recording.lock().unwrap().as_ref().map(|r| r.since.elapsed())
    }

    /// The simulated location, if one is in force.
    pub fn location(&self) -> Option<(f64, f64)> {
        *self.app.location.lock().unwrap()
    }

    /// What the app in front is using, about once a second, from now on.
    pub fn perf(&self) -> broadcast::Receiver<Perf> {
        let samples = self.app.perf.subscribe();
        start_sampling(self.app.clone());
        samples
    }

    /// Things happening on the phone, such as a notification arriving, from now on.
    pub fn events(&self) -> broadcast::Receiver<PhoneEvent> {
        let events = self.app.events.subscribe();
        start_watching(self.app.clone());
        events
    }

    /// The phone's clipboard, each time it changes while sync is on.
    pub fn clips(&self) -> watch::Receiver<Option<Arc<Clip>>> {
        self.app.clips.subscribe()
    }

    /// Starts phone-initiated pairing; progress shows up in [`Self::pairing`].
    pub fn start_pairing(&self) {
        start_pairing(self.app.clone());
    }
}

/// Starts the daemon on this runtime and returns once its port is bound.
pub async fn start(
    port: u16,
    usb: bool,
    udid: Option<String>,
) -> Result<(crate::daemon::Daemon, Handle)> {
    let vm = crate::vm::target();
    // A virtual iPhone's daemon sits one port up, so both can run at once.
    let port = if vm.is_some() && port == DEFAULT_PORT { port + 1 } else { port };
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    // Any local program can reach 127.0.0.1, so every request must carry this token.
    let token = uuid::Uuid::new_v4().simple().to_string();
    crate::daemon::write_info(port, &token)?;

    let (frame_tx, frame_rx) = watch::channel(None);
    let (action_tx, action_rx) = mpsc::channel(64);
    let runtime = tokio::runtime::Handle::current();
    let app = Arc::new(App {
        token: token.clone(),
        actions: action_tx,
        frames: frame_rx,
        problem: Mutex::new(Some("connecting".into())),
        pairing: Mutex::default(),
        runtime: runtime.clone(),
        clipboard_sync: AtomicBool::new(false),
        clips: watch::channel(None).0,
        audio: broadcast::channel(64).0,
        tunnel: Mutex::new(None),
        acted: Mutex::new(Instant::now()),
        sessions: AtomicU64::new(0),
        recording: Mutex::new(None),
        keyframe: Arc::default(),
        restart: Arc::default(),
        location: Mutex::new(None),
        location_changes: Mutex::new(None),
        location_set: Mutex::new(None),
        perf: broadcast::channel(8).0,
        sampling: AtomicBool::new(false),
        events: broadcast::channel(32).0,
        watching: AtomicBool::new(false),
        logs: broadcast::channel(1024).0,
        logging: AtomicBool::new(false),
    });
    // Its own thread: the pairing futures are not `Send`, and they need a deep stack.
    let actor_app = app.clone();
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(move || match vm {
        Some(address) => {
            let app = actor_app.clone();
            let problem = move |problem| *app.problem.lock().unwrap() = problem;
            runtime.block_on(crate::vm::actor(address, action_rx, Arc::new(frame_tx), problem))
        }
        None => runtime.block_on(actor(actor_app, action_rx, Arc::new(frame_tx), usb, udid)),
    })?;

    let router = Router::new()
        .route("/frame.jpg", get(frame))
        .route("/status", get(status))
        .route("/api", post(api))
        .route("/pair", post(pair))
        .route("/quit", post(quit))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((crate::daemon::Daemon { port, token }, Handle { app }))
}

/// Serves until the process exits.
pub async fn run(port: u16, usb: bool, udid: Option<String>) -> Result<()> {
    let (daemon, _handle) = start(port, usb, udid).await?;
    println!("iLinkOS is serving on 127.0.0.1:{}", daemon.port);
    std::future::pending().await
}

async fn auth(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let authorized = request.headers().get("x-ilinkos-token").and_then(|v| v.to_str().ok())
        == Some(app.token.as_str());
    if authorized {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

async fn status(State(app): State<Arc<App>>) -> Json<serde_json::Value> {
    let frame = app.frames.borrow().clone();
    Json(serde_json::json!({
        "connected": app.problem.lock().unwrap().is_none(),
        "problem": *app.problem.lock().unwrap(),
        "pairing": *app.pairing.lock().unwrap(),
        "host": crate::wifi::host_name(),
        "recording": app.recording.lock().unwrap().as_ref().map(|r| r.path.display().to_string()),
        "location": *app.location.lock().unwrap(),
        "width": frame.as_ref().map(|f| f.width),
        "height": frame.as_ref().map(|f| f.height),
        "frames": frame.as_ref().map_or(0, |f| f.seq),
        "decode_ms": decode_ms(),
        "frame_age_ms": frame.as_ref().map(|f| f.at.elapsed().as_millis() as u64),
    }))
}

/// Smoothed milliseconds between a frame arriving and its picture being ready.
pub fn decode_ms() -> f64 {
    f64::from_bits(DECODE_MS.load(Ordering::Relaxed))
}

async fn quit() -> &'static str {
    // Exit after the reply has gone out.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::process::exit(0);
    });
    "stopping"
}

fn start_pairing(app: Arc<App>) -> bool {
    {
        let mut pairing = app.pairing.lock().unwrap();
        if pairing.active {
            return true;
        }
        *pairing = Pairing { active: true, ..Default::default() };
    }
    // Its own thread for the same reasons as the actor.
    let runtime = app.runtime.clone();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let on_pin = |pin| app.pairing.lock().unwrap().pin = Some(pin);
            let result = runtime.block_on(crate::wifi::pair_with(on_pin));
            *app.pairing.lock().unwrap() =
                Pairing { error: result.err().map(|e| format!("{e:#}")), ..Default::default() };
        })
        .is_ok()
}

async fn pair(State(app): State<Arc<App>>) -> &'static str {
    if start_pairing(app) { "pairing started" } else { "could not start pairing" }
}

/// A coarse sample of a picture, for telling whether two pictures differ.
fn sample(frame: &Frame) -> Vec<[u8; 3]> {
    let Pixels::Rgba(rgba) = &frame.pixels else {
        return Vec::new();
    };
    let (width, height) = (frame.width as usize, frame.height as usize);
    (0..height)
        .step_by(SETTLE_GRID)
        .flat_map(|y| (0..width).step_by(SETTLE_GRID).map(move |x| (y * width + x) * 4))
        .filter_map(|i| Some([*rgba.get(i)?, *rgba.get(i + 1)?, *rgba.get(i + 2)?]))
        .collect()
}

fn differs(a: &[[u8; 3]], b: &[[u8; 3]]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let changed = a.iter().zip(b).filter(|(a, b)| (0..3).any(|c| a[c].abs_diff(b[c]) > 24)).count();
    changed as f64 > a.len() as f64 * SETTLE_CHANGED
}

/// Waits until the phone has finished reacting to the last action and its screen
/// has stopped moving, then returns that picture.
async fn settled(app: &App) -> Option<Arc<Frame>> {
    let started = Instant::now();
    let since_action = app.acted.lock().unwrap().elapsed();
    tokio::time::sleep(REACTION_TIME.saturating_sub(since_action)).await;
    let mut frames = app.frames.clone();
    let mut last = frames.borrow_and_update().clone()?;
    let (mut seen, mut still_since) = (sample(&last), Instant::now());
    while still_since.elapsed() < SETTLED_FOR && started.elapsed() < SETTLE_LIMIT {
        // The phone sends pictures even when nothing moves; a gap means it is asleep.
        if tokio::time::timeout(SETTLED_FOR, frames.changed()).await.is_err() {
            break;
        }
        let Some(frame) = frames.borrow_and_update().clone() else { break };
        let now = sample(&frame);
        if differs(&seen, &now) {
            (seen, still_since) = (now, Instant::now());
        }
        last = frame;
    }
    Some(last)
}

#[derive(Deserialize)]
struct FrameQuery {
    /// Wait for the screen to stop changing first.
    #[serde(default)]
    settled: bool,
}

async fn frame(State(app): State<Arc<App>>, query: axum::extract::Query<FrameQuery>) -> Response {
    let frame = if query.settled { settled(&app).await } else { app.frames.borrow().clone() };
    match frame {
        Some(frame) => ([(header::CONTENT_TYPE, "image/jpeg")], frame.jpeg()).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no frame yet").into_response(),
    }
}

async fn api(State(app): State<Arc<App>>, Json(action): Json<Action>) -> Response {
    let (tx, rx) = oneshot::channel();
    if app.actions.send((action, Some(tx))).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "daemon is shutting down").into_response();
    }
    let result = rx.await;
    *app.acted.lock().unwrap() = Instant::now();
    match result {
        Ok(Ok(Output::Done)) => "ok".into_response(),
        Ok(Ok(Output::Png(png))) => ([(header::CONTENT_TYPE, "image/png")], png).into_response(),
        Ok(Ok(Output::Bytes(bytes))) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
        Ok(Ok(Output::Json(json))) => {
            ([(header::CONTENT_TYPE, "application/json")], json).into_response()
        }
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, e).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "action was dropped").into_response(),
    }
}
