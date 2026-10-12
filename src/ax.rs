//! The phone's accessibility inspector: what is on screen, as labelled elements.
//!
//! This talks to `axAuditDaemon`, the service Xcode's Accessibility Inspector uses.
//! It has no "give me the tree" call: the inspector keeps a focus, and the host
//! walks it forward one element at a time, receiving a description of each.
//!
//! The service does not say where an element is, and refuses to press one for an
//! ordinary host. It can, however, draw a highlight box on the phone around the
//! focused element, and iLinkOS can see the phone's screen: so positions are read
//! off the picture, by finding the box that turned green.
//!
//! Moving the focus to an element that is off screen makes the phone scroll it
//! into view. A located walk therefore stops at the first scroll: it reports
//! what is visible, from the top down and from the bottom up, and leaves the
//! rest of a long list alone.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use idevice::{
    ReadWrite,
    dvt::{message::AuxValue, remote_server::RemoteServerClient},
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
};
use plist::{Dictionary, Value};
use tokio::sync::watch;

use crate::serve::{Frame, Pixels};

const SERVICE: &str = "com.apple.accessibility.axAuditDaemon.remoteserver.shim.remote";
const FOCUS_CHANGED: &str = "hostInspectorCurrentElementChanged:";
const STEP_TIMEOUT: Duration = Duration::from_millis(1200);
const HIGHLIGHT_TIMEOUT: Duration = Duration::from_millis(600);
const HIGHLIGHT_FADE: Duration = Duration::from_millis(350);
const MAX_ELEMENTS: usize = 400;
const GREENER: i32 = 10;
const DIFFERENT: i32 = 30;
const STRIDE: usize = 2;
const MIN_RUN: u32 = 6;
const SCROLLED: f32 = 0.04;

const PREVIOUS: i64 = 3;
const NEXT: i64 = 4;
const FIRST: i64 = 5;
const LAST: i64 = 6;

type Frames = watch::Receiver<Option<Arc<Frame>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    fn about(&self, other: &Rect) -> bool {
        const SLACK: u32 = 8;
        let near = |a: u32, b: u32| a.abs_diff(b) <= SLACK;
        near(self.x, other.x)
            && near(self.y, other.y)
            && near(self.x + self.width, other.x + other.width)
            && near(self.y + self.height, other.y + other.height)
    }

    fn contains(&self, x: usize, y: usize) -> bool {
        const MARGIN: u32 = 6;
        let (x, y) = (x as u32, y as u32);
        x + MARGIN >= self.x
            && x <= self.x + self.width + MARGIN
            && y + MARGIN >= self.y
            && y <= self.y + self.height + MARGIN
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Element {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
    #[serde(skip)]
    id: Vec<u8>,
}

fn passthrough(value: impl Into<Value>) -> Value {
    let mut dict = Dictionary::new();
    dict.insert("ObjectType".into(), "passthrough".into());
    dict.insert("Value".into(), value.into());
    Value::Dictionary(dict)
}

fn unwrap(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(unwrap).collect()),
        Value::Dictionary(dict) => match (dict.get("ObjectType"), dict.get("Value")) {
            (Some(_), Some(inner)) => unwrap(inner),
            _ => Value::Dictionary(dict.iter().map(|(k, v)| (k.clone(), unwrap(v))).collect()),
        },
        other => other.clone(),
    }
}

fn comparable<'a>(before: &'a Frame, now: &'a Frame) -> Option<(&'a [u8], &'a [u8], usize, usize)> {
    let (Pixels::Rgba(a), Pixels::Rgba(b)) = (&before.pixels, &now.pixels) else {
        return None;
    };
    ((before.width, before.height) == (now.width, now.height))
        .then_some((&a[..], &b[..], now.width as usize, now.height as usize))
}

fn highlight_box(before: &Frame, now: &Frame) -> Option<Rect> {
    let (a, b, width, height) = comparable(before, now)?;
    let greenness = |p: &[u8]| p[1] as i32 - (p[0] as i32).max(p[2] as i32);
    let mut rows = vec![0u32; height];
    let mut columns = vec![0u32; width];
    for y in (0..height).step_by(STRIDE) {
        for x in (0..width).step_by(STRIDE) {
            let i = (y * width + x) * 4;
            if greenness(&b[i..i + 4]) - greenness(&a[i..i + 4]) >= GREENER {
                rows[y] += 1;
                columns[x] += 1;
            }
        }
    }
    let span = |counts: &[u32]| {
        let first = counts.iter().position(|&c| c >= MIN_RUN)?;
        let last = counts.iter().rposition(|&c| c >= MIN_RUN)?;
        Some((first as u32, (last - first + 1) as u32))
    };
    let ((x, width), (y, height)) = (span(&columns)?, span(&rows)?);
    Some(Rect { x, y, width, height })
}

fn content_moved(before: &Frame, now: &Frame, highlight: Option<Rect>) -> bool {
    let Some((a, b, width, height)) = comparable(before, now) else {
        return true;
    };
    let (mut samples, mut different) = (0u32, 0u32);
    for y in (0..height).step_by(STRIDE * 2) {
        for x in (0..width).step_by(STRIDE * 2) {
            if highlight.is_some_and(|r| r.contains(x, y)) {
                continue;
            }
            let i = (y * width + x) * 4;
            samples += 1;
            let changed = (0..3).any(|c| (a[i + c] as i32 - b[i + c] as i32).abs() >= DIFFERENT);
            different += changed as u32;
        }
    }
    different as f32 > samples as f32 * SCROLLED
}

enum Located {
    At(Rect),
    Hidden,
    Scrolled,
}

/// Watches incoming pictures until the highlight has moved off `stale` and
async fn locate(frames: &mut Frames, before: &Frame, stale: Option<Rect>) -> Located {
    let mut seen: Option<Rect> = None;
    let mut latest: Option<Arc<Frame>> = None;
    let search = async {
        loop {
            if frames.changed().await.is_err() {
                return None;
            }
            let frame = frames.borrow_and_update().clone()?;
            let found = highlight_box(before, &frame)
                .filter(|found| !stale.is_some_and(|stale| stale.about(found)));
            let settled = matches!((found, seen), (Some(f), Some(s)) if f.about(&s));
            latest = Some(frame);
            if settled {
                return found;
            }
            seen = found;
        }
    };
    let found = tokio::time::timeout(HIGHLIGHT_TIMEOUT, search).await.ok().flatten();
    let huge = |r: Rect| r.width * r.height > before.width * before.height / 2;
    match (found, &latest) {
        (Some(rect), _) if huge(rect) => Located::Scrolled,
        (_, Some(now)) if content_moved(before, now, found) => Located::Scrolled,
        (Some(rect), _) => Located::At(rect),
        _ => Located::Hidden,
    }
}

pub struct Inspector {
    client: RemoteServerClient<Box<dyn ReadWrite>>,
}

impl Inspector {
    pub async fn connect(handle: &mut AdapterHandle, rsd: &RsdHandshake) -> Result<Self> {
        let port = rsd
            .services
            .get(SERVICE)
            .context("this phone does not offer the accessibility inspector")?
            .port;
        let mut socket = idevice::Idevice::new(Box::new(handle.connect(port).await?), "ilinkos");
        socket.rsd_checkin().await?;
        let stream = socket.get_socket().context("the accessibility connection closed")?;
        let mut client = RemoteServerClient::new(stream);
        let mut capabilities = Dictionary::new();
        capabilities.insert("com.apple.private.DTXBlockCompression".into(), 2.into());
        capabilities.insert("com.apple.private.DTXConnection".into(), 1.into());
        client
            .call_method(
                0,
                Some("_notifyOfPublishedCapabilities:"),
                Some(vec![AuxValue::archived_value(Value::Dictionary(capabilities))]),
                false,
            )
            .await?;
        let mut inspector = Self { client };
        inspector.send("deviceSetAppMonitoringEnabled:", vec![Value::Boolean(true)]).await?;
        inspector.send("deviceInspectorSetMonitoredEventType:", vec![0.into()]).await?;
        inspector.show_highlight(false).await?;
        Ok(inspector)
    }

    async fn send(&mut self, selector: &str, args: Vec<Value>) -> Result<()> {
        let args = args.into_iter().map(AuxValue::archived_value).collect();
        self.client.call_method(0, Some(selector), Some(args), false).await?;
        Ok(())
    }

    async fn show_highlight(&mut self, on: bool) -> Result<()> {
        self.send("deviceInspectorShowVisuals:", vec![(on as i64).into()]).await
    }

    async fn move_focus(&mut self, direction: i64) -> Result<()> {
        let mut options = Dictionary::new();
        options.insert("allowNonAX".into(), passthrough(0));
        options.insert("direction".into(), passthrough(direction));
        options.insert("includeContainers".into(), passthrough(1));
        self.send("deviceInspectorMoveWithOptions:", vec![passthrough(Value::Dictionary(options))])
            .await
    }

    async fn next_focus(&mut self) -> Result<Option<Value>> {
        let wait = tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                let message = self.client.read_message(0).await?;
                if message.data.as_ref().and_then(|d| d.as_string()) != Some(FOCUS_CHANGED) {
                    continue;
                }
                let Some(aux) = message.aux else {
                    continue;
                };
                for value in aux.values {
                    if let AuxValue::Array(bytes) = value {
                        if let Ok(decoded) = ns_keyed_archive::decode::from_bytes(&bytes) {
                            return anyhow::Ok(decoded);
                        }
                    }
                }
            }
        });
        match wait.await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }

    pub async fn elements(&mut self, mut frames: Option<&mut Frames>) -> Result<Vec<Element>> {
        let mut elements = self.pass(&mut frames, FIRST, NEXT, &[]).await?;
        let mut tail = self.pass(&mut frames, LAST, PREVIOUS, &elements).await?;
        tail.reverse();
        elements.append(&mut tail);
        self.show_highlight(false).await?;
        Ok(elements)
    }

    async fn pass(
        &mut self,
        frames: &mut Option<&mut Frames>,
        start: i64,
        direction: i64,
        known: &[Element],
    ) -> Result<Vec<Element>> {
        let before = match frames.as_mut() {
            Some(frames) => {
                self.show_highlight(false).await?;
                tokio::time::sleep(HIGHLIGHT_FADE).await;
                let before = frames.borrow_and_update().clone();
                self.show_highlight(true).await?;
                before
            }
            None => None,
        };
        let mut elements: Vec<Element> = Vec::new();
        let mut stale: Option<Rect> = None;
        if let (Some(frames), Some(before)) = (frames.as_mut(), &before) {
            tokio::time::sleep(HIGHLIGHT_FADE).await;
            stale = frames.borrow().as_ref().and_then(|now| highlight_box(before, now));
        }
        self.move_focus(start).await?;
        let mut silent = 0;
        while elements.len() + known.len() < MAX_ELEMENTS {
            let Some(event) = self.next_focus().await? else {
                silent += 1;
                if silent >= 2 {
                    break;
                }
                self.move_focus(direction).await?;
                continue;
            };
            silent = 0;
            let event = unwrap(&event);
            let focus = match &event {
                Value::Array(items) => items.first().cloned().unwrap_or(Value::Boolean(false)),
                other => other.clone(),
            };
            let Some(fields) = focus.as_dictionary() else {
                bail!("unexpected focus event: {event:?}");
            };
            let text = |key: &str| fields.get(key).and_then(|v| v.as_string()).map(str::to_owned);
            let id = fields
                .get("ElementValue_v1")
                .and_then(|e| e.as_dictionary())
                .and_then(|e| e.get("PlatformElementValue_v1"))
                .and_then(|v| v.as_data())
                .map(<[u8]>::to_vec)
                .unwrap_or_default();
            if !id.is_empty() && elements.iter().chain(known).any(|e| e.id == id) {
                break;
            }
            let rect = match (frames.as_mut(), &before) {
                (Some(frames), Some(before)) => match locate(frames, before, stale).await {
                    Located::At(rect) => Some(rect),
                    Located::Hidden => None,
                    Located::Scrolled => break,
                },
                _ => None,
            };
            let label = text("CaptionTextValue_v1").unwrap_or_default();
            let description = text("SpokenDescriptionValue_v1").filter(|d| *d != label);
            elements.push(Element { label, description, rect, id });
            if let (Some(frames), Some(before)) = (frames.as_mut(), &before) {
                let now = frames.borrow().clone();
                stale = now.and_then(|now| highlight_box(before, &now)).or(rect);
            }
            self.move_focus(direction).await?;
        }
        Ok(elements)
    }
}
