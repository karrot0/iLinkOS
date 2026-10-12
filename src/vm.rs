//! A virtual iPhone: an emulator (Inferno, a QEMU fork) reached through its VNC
//! display. The emulator turns pointer events into touches and function keys
//! into the phone's buttons, so screen and input both travel over one connection.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::body::Bytes;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, tcp::OwnedReadHalf},
    sync::{mpsc, watch},
};

use crate::serve::{Action, Frame, Output, Pixels};

/// Set to the emulator's VNC address (`host:port`) to drive it instead of a phone.
pub const TARGET_VAR: &str = "ILINKOS_VM";
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Pictures are asked for no faster than this.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// How long a tap holds the finger down.
const TAP_HOLD: Duration = Duration::from_millis(70);
const SWIPE_STEP: Duration = Duration::from_millis(12);

// X11 key symbols for the function keys Inferno maps to the phone's buttons.
const KEY_VOLUME_DOWN: u32 = 0xFFC0; // F3
const KEY_VOLUME_UP: u32 = 0xFFC1; // F4
const KEY_SIDE: u32 = 0xFFC2; // F5
const KEY_HOME: u32 = 0xFFC3; // F6

/// The emulator's VNC address, if iLinkOS was asked to drive one.
pub fn target() -> Option<String> {
    std::env::var(TARGET_VAR).ok().filter(|t| !t.is_empty())
}

/// A connection to the emulator's display.
struct Display {
    /// Messages to the emulator, written in order by one task.
    out: mpsc::UnboundedSender<Vec<u8>>,
    width: u16,
    height: u16,
    frames: watch::Receiver<Option<Arc<Frame>>>,
}

async fn expect(stream: &mut TcpStream, count: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; count];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}

impl Display {
    async fn connect(
        address: &str,
        frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
        closed: mpsc::Sender<()>,
    ) -> Result<Self> {
        let mut stream = TcpStream::connect(address)
            .await
            .with_context(|| format!("no virtual iPhone is running at {address}"))?;
        stream.set_nodelay(true)?;

        // The RFB handshake: agree a version, take the no-password security type.
        let version = expect(&mut stream, 12).await?;
        if !version.starts_with(b"RFB 003.") {
            bail!("{address} is not a VNC display");
        }
        stream.write_all(b"RFB 003.008\n").await?;
        let count = expect(&mut stream, 1).await?[0] as usize;
        let types = expect(&mut stream, count).await?;
        if !types.contains(&1) {
            bail!("the emulator's display wants a password; start it without one");
        }
        stream.write_all(&[1]).await?;
        if expect(&mut stream, 4).await? != [0, 0, 0, 0] {
            bail!("the emulator refused the connection");
        }
        stream.write_all(&[1]).await?; // share the display with other viewers
        let init = expect(&mut stream, 24).await?;
        let (width, height) = (u16::from_be_bytes([init[0], init[1]]), u16::from_be_bytes([init[2], init[3]]));
        let name = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
        expect(&mut stream, name).await?;

        // Ask for pixels as R, G, B, unused bytes, which is the layout frames use.
        let mut format = vec![0, 0, 0, 0, 32, 24, 0, 1];
        format.extend_from_slice(&[0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);
        stream.write_all(&format).await?;
        // Raw pixels only: the emulator is on this machine, so size does not matter.
        stream.write_all(&[2, 0, 0, 1, 0, 0, 0, 0]).await?;

        let (reader, mut writer) = stream.into_split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if writer.write_all(&message).await.is_err() {
                    break;
                }
            }
        });
        let receiver = frames.subscribe();
        let requests = out.clone();
        tokio::spawn(async move {
            let _ = pump(reader, requests, frames, width, height).await;
            let _ = closed.send(()).await;
        });
        Ok(Self { out, width, height, frames: receiver })
    }

    /// Moves the pointer to a fractional position with the button up or down,
    /// which the emulator reads as a finger.
    fn finger(&self, x: f64, y: f64, down: bool) {
        let at = |v: f64, size: u16| ((v.clamp(0.0, 1.0) * f64::from(size - 1)) as u16).to_be_bytes();
        let (x, y) = (at(x, self.width), at(y, self.height));
        let _ = self.out.send(vec![5, down as u8, x[0], x[1], y[0], y[1]]);
    }

    fn key(&self, key: u32, down: bool) {
        let mut message = vec![4, down as u8, 0, 0];
        message.extend_from_slice(&key.to_be_bytes());
        let _ = self.out.send(message);
    }

    async fn press(&self, key: u32, hold: Duration) {
        self.key(key, true);
        tokio::time::sleep(hold).await;
        self.key(key, false);
    }

    async fn drag(&self, from: (f64, f64), to: (f64, f64), ms: u64) {
        let steps = (ms / SWIPE_STEP.as_millis() as u64).max(2);
        self.finger(from.0, from.1, true);
        for step in 1..=steps {
            tokio::time::sleep(SWIPE_STEP).await;
            let t = step as f64 / steps as f64;
            self.finger(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t, true);
        }
        self.finger(to.0, to.1, false);
    }

    async fn run(&mut self, action: &Action) -> Result<Output> {
        match action {
            Action::Tap { x, y } => {
                self.finger(*x, *y, true);
                tokio::time::sleep(TAP_HOLD).await;
                self.finger(*x, *y, false);
            }
            Action::Down { x, y } | Action::Move { x, y } => self.finger(*x, *y, true),
            Action::Up { x, y } => self.finger(*x, *y, false),
            Action::Swipe { x1, y1, x2, y2, ms } => {
                self.drag((*x1, *y1), (*x2, *y2), ms.unwrap_or(400)).await;
            }
            Action::Home => self.press(KEY_HOME, TAP_HOLD).await,
            Action::Apps => {
                self.press(KEY_HOME, TAP_HOLD).await;
                tokio::time::sleep(Duration::from_millis(120)).await;
                self.press(KEY_HOME, TAP_HOLD).await;
            }
            Action::Back => self.drag((0.0, 0.5), (0.6, 0.5), 250).await,
            Action::Notifications => self.drag((0.2, 0.0), (0.2, 0.6), 300).await,
            Action::ControlCenter => self.drag((0.92, 0.0), (0.92, 0.5), 300).await,
            Action::Button { name } => {
                let (key, hold) = match name.as_str() {
                    "home" => (KEY_HOME, TAP_HOLD),
                    "lock" => (KEY_SIDE, Duration::from_millis(200)),
                    "volume-up" => (KEY_VOLUME_UP, TAP_HOLD),
                    "volume-down" => (KEY_VOLUME_DOWN, TAP_HOLD),
                    "siri" => (KEY_SIDE, Duration::from_millis(1200)),
                    other => bail!("the virtual iPhone has no '{other}' button"),
                };
                self.press(key, hold).await;
            }
            Action::Screenshot => {
                let frame = self.frames.borrow().clone().context("no picture yet")?;
                let Pixels::Rgba(rgba) = &frame.pixels else { bail!("no picture yet") };
                let mut png = Vec::new();
                image::ImageEncoder::write_image(
                    image::codecs::png::PngEncoder::new(&mut png),
                    rgba,
                    frame.width,
                    frame.height,
                    image::ExtendedColorType::Rgba8,
                )?;
                return Ok(Output::Png(png));
            }
            _ => bail!("the virtual iPhone can't do that yet"),
        }
        Ok(Output::Done)
    }
}

/// Reads the emulator's screen updates and publishes each finished picture.
async fn pump(
    mut reader: OwnedReadHalf,
    requests: mpsc::UnboundedSender<Vec<u8>>,
    frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
    width: u16,
    height: u16,
) -> Result<()> {
    let (w, h) = (width as usize, height as usize);
    let mut screen = vec![0u8; w * h * 4];
    let request = |incremental: u8| {
        let (w, h) = (width.to_be_bytes(), height.to_be_bytes());
        requests.send(vec![3, incremental, 0, 0, 0, 0, w[0], w[1], h[0], h[1]])
    };
    request(0)?;
    let mut seq = 0u64;
    loop {
        let asked = Instant::now();
        match reader.read_u8().await? {
            // A screen update: a list of rectangles of raw pixels.
            0 => {
                reader.read_u8().await?;
                for _ in 0..reader.read_u16().await? {
                    let (x, y) = (reader.read_u16().await? as usize, reader.read_u16().await? as usize);
                    let (rw, rh) = (reader.read_u16().await? as usize, reader.read_u16().await? as usize);
                    if reader.read_i32().await? != 0 {
                        bail!("the emulator sent pixels in a form that was not asked for");
                    }
                    let mut row = vec![0u8; rw * 4];
                    for line in y..y + rh {
                        reader.read_exact(&mut row).await?;
                        if line < h && x + rw <= w {
                            let start = (line * w + x) * 4;
                            screen[start..start + rw * 4].copy_from_slice(&row);
                        }
                    }
                }
                // The unused byte of each pixel becomes full opacity.
                let mut rgba = screen.clone();
                rgba.chunks_exact_mut(4).for_each(|pixel| pixel[3] = 255);
                seq += 1;
                let frame = Frame {
                    pixels: Pixels::Rgba(Bytes::from(rgba)),
                    width: width as u32,
                    height: height as u32,
                    seq,
                    at: Instant::now(),
                };
                frames.send_replace(Some(Arc::new(frame)));
                tokio::time::sleep(FRAME_INTERVAL.saturating_sub(asked.elapsed())).await;
                request(1)?;
            }
            // Colour map entries: not used with true colour, but skip them properly.
            1 => {
                reader.read_u8().await?;
                reader.read_u16().await?;
                let colours = reader.read_u16().await? as usize;
                reader.read_exact(&mut vec![0u8; colours * 6]).await?;
            }
            2 => {} // bell
            // Clipboard text from the emulator.
            3 => {
                reader.read_exact(&mut [0u8; 3]).await?;
                let length = reader.read_u32().await? as usize;
                reader.read_exact(&mut vec![0u8; length]).await?;
            }
            other => bail!("unexpected message {other} from the emulator's display"),
        }
    }
}

type Reply = tokio::sync::oneshot::Sender<Result<Output, String>>;

/// Owns the connection to the emulator: connects, reconnects, and runs actions.
/// `problem` is told why the emulator is unreachable, or `None` once it is.
pub async fn actor(
    address: String,
    mut actions: mpsc::Receiver<(Action, Option<Reply>)>,
    frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
    problem: impl Fn(Option<String>),
) {
    loop {
        let (closed, mut on_close) = mpsc::channel(1);
        let mut display = match Display::connect(&address, frames.clone(), closed).await {
            Ok(display) => display,
            Err(e) => {
                let why = format!("{e:#}");
                problem(Some(why.clone()));
                let retry = tokio::time::sleep(RETRY_DELAY);
                tokio::pin!(retry);
                loop {
                    tokio::select! {
                        _ = &mut retry => break,
                        next = actions.recv() => match next {
                            Some((_, Some(reply))) => { let _ = reply.send(Err(why.clone())); }
                            Some(_) => {}
                            None => return,
                        },
                    }
                }
                continue;
            }
        };
        problem(None);
        loop {
            tokio::select! {
                _ = on_close.recv() => break,
                next = actions.recv() => {
                    let Some((action, reply)) = next else { return };
                    let result = display.run(&action).await;
                    if let Some(reply) = reply {
                        let _ = reply.send(result.map_err(|e| format!("{e:#}")));
                    }
                }
            }
        }
        frames.send_replace(None);
    }
}
