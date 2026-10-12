//! Input injection. The phone only honours synthetic HID events while a screen
//! media stream is running, so every action is wrapped in a [`Session`].

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    ReadWrite, RsdService,
    core_device::{
        ButtonState, CallInfoBlob, DataInclusionPolicy, DisplayServiceClient, GENERAL_PASTEBOARD,
        HevcDepacketizer, ImageFormat, IndigoHidClient, OrientationServiceClient,
        PasteboardServiceClient, RotationDirection, RtpPacket, ScreenCaptureServiceClient, UTI_PNG, TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE, TouchscreenContact,
        UniversalHidServiceClient, build_screen_audio_offer, build_screen_video_offer,
        build_start_audio_parameters, build_start_video_parameters, is_rtcp,
    },
    rsd::RsdHandshake,
    tcp::handle::{AdapterHandle, UdpSocketHandle},
};

const CLIENT_SUPPORTED_FEATURES: u64 = 140;
const MAIN_DISPLAY: i64 = 1;
pub const VIDEO_SENDER_PORT: u16 = 50001;
const KEY_LEFT_CONTROL: u64 = 0xE0;
const KEY_LEFT_SHIFT: u64 = 0xE1;
const KEY_LEFT_OPTION: u64 = 0xE2;
const KEY_LEFT_COMMAND: u64 = 0xE3;

#[derive(Debug, Default, Clone, Copy)]
pub struct Modifiers {
    pub command: bool,
    pub shift: bool,
    pub option: bool,
    pub control: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clip {
    Text(String),
    Image(Vec<u8>),
}

const BUTTONS: &[(&str, u64, u64, u64)] = &[
    ("home", 0x0C, 0x40, 80),
    ("lock", 0x0C, 0x30, 200),
    ("volume-up", 0x0C, 0xE9, 80),
    ("volume-down", 0x0C, 0xEA, 80),
    ("siri", 0x0C, 0xCF, 1200),
];

type Stream = Box<dyn ReadWrite>;

pub struct Session {
    handle: AdapterHandle,
    rsd: RsdHandshake,
    display: DisplayServiceClient<Stream>,
    audio_udp: Option<UdpSocketHandle>,
    pub audio_answer: plist::Value,
    video_udp: Option<UdpSocketHandle>,
    pub video_ssrc: u32,
    touch: Option<UniversalHidServiceClient<Stream>>,
    indigo: Option<IndigoHidClient<Stream>>,
    pasteboard: Option<PasteboardServiceClient<Stream>>,
}

impl Session {
    pub async fn start((mut handle, mut rsd): (AdapterHandle, RsdHandshake)) -> Result<Self> {
        let mut display = DisplayServiceClient::connect_rsd(&mut handle, &mut rsd)
            .await
            .context("no display service — run `ilinkos mount` first")?;
        let audio_udp = handle.bind_udp(0).await?;
        let video_udp = handle.bind_udp(0).await?;
        let receiver_ip = handle.host_ip().to_string();
        let sender_ip = handle.peer_ip().to_string();

        let call_info = CallInfoBlob {
            call_id: 0,
            client_version: 1,
            device_type: "Mac17,7".into(),
            framework_version: "2205.3.1".into(),
            os_version: "25F71".into(),
            device_name: None,
            audio_device_uid: None,
        };
        let session_id = uuid::Uuid::new_v4();
        let call_id = || uuid::Uuid::new_v4().to_string().to_uppercase();

        let offer = build_screen_audio_offer(&call_id(), &call_info)
            .map_err(|e| anyhow!("audio offer: {e:?}"))?;
        let audio_answer = display
            .start_media_stream(build_start_audio_parameters(
                &receiver_ip,
                audio_udp.local_port(),
                &sender_ip,
                50000,
                offer,
                CLIENT_SUPPORTED_FEATURES,
                session_id,
            ))
            .await
            .context("the phone refused the audio stream")?;

        let ssrc = uuid::Uuid::new_v4().as_u128() as u32;
        let offer = build_screen_video_offer(&call_id(), &call_info, ssrc)
            .map_err(|e| anyhow!("video offer: {e:?}"))?;
        display
            .start_media_stream(build_start_video_parameters(
                &receiver_ip,
                video_udp.local_port(),
                &sender_ip,
                VIDEO_SENDER_PORT,
                offer,
                CLIENT_SUPPORTED_FEATURES,
                MAIN_DISPLAY,
                session_id,
            ))
            .await
            .context("the phone refused the video stream")?;

        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(Self {
            handle,
            rsd,
            display,
            audio_udp: Some(audio_udp),
            audio_answer,
            video_udp: Some(video_udp),
            video_ssrc: ssrc,
            touch: None,
            indigo: None,
            pasteboard: None,
        })
    }

    pub async fn warm_up(&mut self) -> Result<()> {
        self.touch().await?;
        self.indigo().await?;
        Ok(())
    }

    pub fn take_audio(&mut self) -> Option<UdpSocketHandle> {
        self.audio_udp.take()
    }

    pub fn tunnel(&mut self) -> (&mut AdapterHandle, &mut RsdHandshake) {
        (&mut self.handle, &mut self.rsd)
    }

    pub async fn inspector(&mut self) -> Result<crate::ax::Inspector> {
        crate::ax::Inspector::connect(&mut self.handle, &self.rsd).await
    }

    pub fn take_video(&mut self) -> Option<UdpSocketHandle> {
        self.video_udp.take()
    }

    async fn touch(&mut self) -> Result<&mut UniversalHidServiceClient<Stream>> {
        if self.touch.is_none() {
            self.touch =
                Some(UniversalHidServiceClient::connect_rsd(&mut self.handle, &mut self.rsd).await?);
        }
        Ok(self.touch.as_mut().unwrap())
    }

    async fn indigo(&mut self) -> Result<&mut IndigoHidClient<Stream>> {
        if self.indigo.is_none() {
            self.indigo =
                Some(IndigoHidClient::connect_rsd(&mut self.handle, &mut self.rsd).await?);
        }
        Ok(self.indigo.as_mut().unwrap())
    }

    pub async fn touch_sample(&mut self, x: f64, y: f64, down: bool) -> Result<()> {
        let state = if down {
            TOUCHSCREEN_STATE_CONTACT
        } else {
            TOUCHSCREEN_STATE_RELEASE
        };
        let (x, y) = (norm(x)?, norm(y)?);
        self.touch().await?.send_touchscreen(state, x, y, None).await?;
        Ok(())
    }

    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        let (x, y) = (norm(x)?, norm(y)?);
        self.touch().await?.tap(x, y).await?;
        Ok(())
    }

    pub async fn drag(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        let steps = (ms / 16).clamp(4, 120) as u32;
        let (x1, y1, x2, y2) = (norm(from.0)?, norm(from.1)?, norm(to.0)?, norm(to.1)?);
        self.touch()
            .await?
            .drag(x1, y1, x2, y2, steps, ms / steps as u64)
            .await?;
        Ok(())
    }

    pub async fn touches(&mut self, fingers: &[(u8, f64, f64, bool)]) -> Result<()> {
        let contacts = fingers
            .iter()
            .map(|&(identity, x, y, touching)| {
                Ok(TouchscreenContact { identity, touching, x: norm(x)?, y: norm(y)? })
            })
            .collect::<Result<Vec<_>>>()?;
        self.touch().await?.send_multitouch(&contacts, None).await?;
        Ok(())
    }

    pub async fn pinch(&mut self, x: f64, y: f64, scale: f64, ms: u64) -> Result<()> {
        let (from, to) = if scale >= 1.0 { (0.08, 0.08 * scale) } else { (0.30, 0.30 * scale) };
        let steps = (ms / 16).clamp(6, 90);
        let touch = self.touch().await?;
        let frame = |half: f64, touching: bool| -> Result<[TouchscreenContact; 2]> {
            let half = half.clamp(0.02, 0.45);
            let contact = |identity, fx: f64| -> Result<TouchscreenContact> {
                Ok(TouchscreenContact { identity, touching, x: norm(fx.clamp(0.02, 0.98))?, y: norm(y)? })
            };
            Ok([contact(0, x - half)?, contact(1, x + half)?])
        };
        for i in 0..=steps {
            let half = from + (to - from) * i as f64 / steps as f64;
            touch.send_multitouch(&frame(half, true)?, None).await?;
            tokio::time::sleep(Duration::from_millis(ms / steps)).await;
        }
        touch.send_multitouch(&frame(to, false)?, None).await?;
        Ok(())
    }

    pub async fn combo(&mut self, key: &str, mods: Modifiers) -> Result<()> {
        let mut chars = key.chars();
        let usage = match (named_key(key), chars.next(), chars.next()) {
            (Some(usage), ..) => usage,
            (None, Some(c), None) => {
                ascii_to_usage(c.to_ascii_lowercase()).with_context(|| format!("unknown key '{key}'"))?.0
            }
            _ => bail!("unknown key '{key}'"),
        };
        let held: Vec<u64> = [
            (mods.control, KEY_LEFT_CONTROL),
            (mods.option, KEY_LEFT_OPTION),
            (mods.shift, KEY_LEFT_SHIFT),
            (mods.command, KEY_LEFT_COMMAND),
        ]
        .into_iter()
        .filter_map(|(on, usage)| on.then_some(usage))
        .collect();
        let indigo = self.indigo().await?;
        for &modifier in &held {
            indigo.send_keyboard(modifier, ButtonState::Down).await?;
        }
        indigo.send_keyboard(usage, ButtonState::Down).await?;
        indigo.send_keyboard(usage, ButtonState::Up).await?;
        for &modifier in held.iter().rev() {
            indigo.send_keyboard(modifier, ButtonState::Up).await?;
        }
        Ok(())
    }

    pub async fn control_center(&mut self) -> Result<()> {
        self.drag((0.92, 0.0), (0.92, 0.42), 240).await
    }

    pub async fn notifications(&mut self) -> Result<()> {
        self.drag((0.25, 0.0), (0.25, 0.6), 280).await
    }

    pub async fn back(&mut self) -> Result<()> {
        self.drag((0.0, 0.5), (0.65, 0.5), 220).await
    }

    pub async fn rotate(&mut self, clockwise: bool) -> Result<()> {
        let direction = if clockwise { RotationDirection::Right } else { RotationDirection::Left };
        let mut client = OrientationServiceClient::connect_rsd(&mut self.handle, &mut self.rsd).await?;
        client.rotate(direction).await?;
        Ok(())
    }

    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        let mut client =
            ScreenCaptureServiceClient::connect_rsd(&mut self.handle, &mut self.rsd).await?;
        Ok(client.take_screenshot(None, ImageFormat::Png).await?)
    }

    async fn pasteboard(&mut self) -> Result<&mut PasteboardServiceClient<Stream>> {
        if self.pasteboard.is_none() {
            self.pasteboard =
                Some(PasteboardServiceClient::connect_rsd(&mut self.handle, &mut self.rsd).await?);
        }
        Ok(self.pasteboard.as_mut().unwrap())
    }

    pub async fn clipboard_change(&mut self) -> Result<Option<i64>> {
        let snapshot = self
            .pasteboard()
            .await?
            .get_with_policy(GENERAL_PASTEBOARD, DataInclusionPolicy::AllPromised)
            .await?;
        Ok(snapshot.change_count)
    }

    pub async fn clipboard_get(&mut self) -> Result<Option<Clip>> {
        let snapshot = self.pasteboard().await?.get(GENERAL_PASTEBOARD).await?;
        Ok(match (snapshot.text(), snapshot.image()) {
            (Some(text), _) => Some(Clip::Text(text)),
            (None, Some((_, bytes))) => Some(Clip::Image(bytes)),
            _ => None,
        })
    }

    pub async fn clipboard_set(&mut self, clip: &Clip) -> Result<()> {
        let pasteboard = self.pasteboard().await?;
        match clip {
            Clip::Text(text) => pasteboard.set_text(text, GENERAL_PASTEBOARD).await?,
            Clip::Image(png) => pasteboard.set_image(png, UTI_PNG, GENERAL_PASTEBOARD).await?,
        }
        Ok(())
    }

    pub async fn app_switcher(&mut self) -> Result<()> {
        self.button("home").await?;
        tokio::time::sleep(Duration::from_millis(90)).await;
        self.button("home").await
    }

    pub async fn button(&mut self, name: &str) -> Result<()> {
        let Some(&(_, page, usage, hold_ms)) = BUTTONS.iter().find(|b| b.0 == name) else {
            let known: Vec<_> = BUTTONS.iter().map(|b| b.0).collect();
            bail!("unknown button '{name}' — known: {}", known.join(", "));
        };
        let indigo = self.indigo().await?;
        indigo.send_button(page, usage, ButtonState::Down).await?;
        tokio::time::sleep(Duration::from_millis(hold_ms)).await;
        indigo.send_button(page, usage, ButtonState::Up).await?;
        Ok(())
    }

    async fn key_usage(&mut self, usage: u64, shift: bool) -> Result<()> {
        let indigo = self.indigo().await?;
        if shift {
            indigo.send_keyboard(KEY_LEFT_SHIFT, ButtonState::Down).await?;
        }
        indigo.send_keyboard(usage, ButtonState::Down).await?;
        indigo.send_keyboard(usage, ButtonState::Up).await?;
        if shift {
            indigo.send_keyboard(KEY_LEFT_SHIFT, ButtonState::Up).await?;
        }
        tokio::time::sleep(Duration::from_millis(12)).await;
        Ok(())
    }

    pub async fn key(&mut self, name: &str) -> Result<()> {
        let usage = named_key(name).with_context(|| format!("unknown key '{name}'"))?;
        self.key_usage(usage, false).await
    }

    pub async fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            if let Some((usage, shift)) = ascii_to_usage(c) {
                self.key_usage(usage, shift).await?;
            }
        }
        Ok(())
    }

    pub async fn count_video_packets(&mut self, window: Duration) -> (usize, usize) {
        let (mut packets, mut bytes) = (0, 0);
        let Some(udp) = &self.video_udp else {
            return (0, 0);
        };
        let _ = tokio::time::timeout(window, async {
            while let Ok(datagram) = udp.recv().await {
                packets += 1;
                bytes += datagram.data.len();
            }
        })
        .await;
        (packets, bytes)
    }

    pub async fn record(&mut self, window: Duration) -> Result<Vec<u8>> {
        let udp = self.video_udp.as_ref().context("video stream already taken")?;
        let mut hevc = HevcReader::default();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(window, async {
            while let Ok(datagram) = udp.recv().await {
                out.extend(hevc.push(&datagram.data).0);
            }
        })
        .await;
        if !hevc.0.has_parameter_sets() {
            bail!("no decodable video arrived — is the phone unlocked and awake?");
        }
        Ok(out)
    }

    pub async fn stop(mut self) -> Result<()> {
        self.display.stop_media_stream().await?;
        Ok(())
    }
}

pub const ACCESS_UNIT_DELIMITER: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];

#[derive(Default)]
pub struct HevcReader(HevcDepacketizer, Option<u32>, u32);

impl HevcReader {
    pub fn ssrc(&self) -> Option<u32> {
        self.1
    }

    pub fn timestamp(&self) -> u32 {
        self.2
    }

    pub fn push(&mut self, datagram: &[u8]) -> (Vec<u8>, bool) {
        if is_rtcp(datagram) {
            return (Vec::new(), false);
        }
        let Some(packet) = RtpPacket::parse(datagram) else {
            return (Vec::new(), false);
        };
        self.1 = Some(packet.ssrc);
        self.2 = packet.timestamp;
        self.0.push(packet.sequence_number, packet.timestamp, packet.payload);
        let mut out = self.0.take_output();
        let frame_end = packet.marker && !out.is_empty();
        if frame_end {
            out.extend_from_slice(&ACCESS_UNIT_DELIMITER);
        }
        (out, frame_end)
    }
}

fn norm(v: f64) -> Result<u16> {
    if !(0.0..=1.0).contains(&v) {
        bail!("coordinates are fractions of the screen, 0.0 to 1.0 (got {v})");
    }
    Ok((v * 65535.0).round() as u16)
}

fn named_key(name: &str) -> Option<u64> {
    Some(match name.to_ascii_lowercase().as_str() {
        "return" | "enter" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2A,
        "delete" => 0x4C,
        "tab" => 0x2B,
        "space" => 0x2C,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        _ => return None,
    })
}

fn ascii_to_usage(c: char) -> Option<(u64, bool)> {
    const SHIFTED_DIGITS: &str = "!@#$%^&*()";
    const PAIRS: &[(char, char, u64)] = &[
        ('-', '_', 0x2D),
        ('=', '+', 0x2E),
        ('[', '{', 0x2F),
        (']', '}', 0x30),
        ('\\', '|', 0x31),
        (';', ':', 0x33),
        ('\'', '"', 0x34),
        ('`', '~', 0x35),
        (',', '<', 0x36),
        ('.', '>', 0x37),
        ('/', '?', 0x38),
    ];
    Some(match c {
        'a'..='z' => (0x04 + (c as u64 - 'a' as u64), false),
        'A'..='Z' => (0x04 + (c as u64 - 'A' as u64), true),
        '1'..='9' => (0x1E + (c as u64 - '1' as u64), false),
        '0' => (0x27, false),
        '\n' => (0x28, false),
        '\t' => (0x2B, false),
        ' ' => (0x2C, false),
        _ => {
            if let Some(i) = SHIFTED_DIGITS.find(c) {
                return Some((0x1E + i as u64, true));
            }
            let &(plain, _, usage) = PAIRS.iter().find(|p| p.0 == c || p.1 == c)?;
            (usage, c != plain)
        }
    })
}
