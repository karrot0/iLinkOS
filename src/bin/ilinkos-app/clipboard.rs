//! Keeps the PC's clipboard and the phone's in step, in both directions.

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use arboard::{Clipboard, ImageData};
use ilinkos::{
    control::Clip,
    serve::{Action, Handle},
};

const POLL: Duration = Duration::from_millis(500);
/// Larger images are left alone rather than pushed over Wi-Fi on every copy.
const MAX_IMAGE_PIXELS: usize = 12_000_000;

/// A cheap identity for clipboard contents, to tell a change from an echo.
fn signature(clip: &Clip) -> u64 {
    let mut hasher = DefaultHasher::new();
    match clip {
        Clip::Text(text) => (0u8, text).hash(&mut hasher),
        Clip::Image(bytes) => (1u8, bytes).hash(&mut hasher),
    }
    hasher.finish()
}

/// Reads the PC clipboard. Images come back as raw RGBA plus their size.
fn read_pc(clipboard: &mut Clipboard) -> Option<(Clip, Option<(usize, usize)>)> {
    if let Ok(text) = clipboard.get_text() {
        return (!text.is_empty()).then_some((Clip::Text(text), None));
    }
    let image = clipboard.get_image().ok()?;
    if image.width * image.height > MAX_IMAGE_PIXELS {
        return None;
    }
    Some((Clip::Image(image.bytes.into_owned()), Some((image.width, image.height))))
}

fn encode_png(rgba: &[u8], (width, height): (usize, usize)) -> Option<Vec<u8>> {
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())?;
    let mut png = Vec::new();
    image.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some(png)
}

/// Puts `clip` on the PC clipboard and returns the signature it will read back as.
fn write_pc(clipboard: &mut Clipboard, clip: &Clip) -> Option<u64> {
    match clip {
        Clip::Text(text) => {
            clipboard.set_text(text.clone()).ok()?;
            Some(signature(clip))
        }
        Clip::Image(encoded) => {
            let image = image::load_from_memory(encoded).ok()?.into_rgba8();
            let (width, height) = (image.width() as usize, image.height() as usize);
            let bytes = image.into_raw();
            let sig = signature(&Clip::Image(bytes.clone()));
            clipboard.set_image(ImageData { width, height, bytes: bytes.into() }).ok()?;
            Some(sig)
        }
    }
}

/// The app's side of clipboard sync.
pub struct Sync {
    pub enabled: Arc<AtomicBool>,
    /// Things the app itself wants on the PC clipboard, such as a screenshot.
    /// Going through here stops them being mistaken for a copy made by the user.
    pub copy: mpsc::Sender<Clip>,
}

pub fn start(handle: Handle, enabled: bool) -> Sync {
    let enabled = Arc::new(AtomicBool::new(enabled));
    let (copy, copies) = mpsc::channel::<Clip>();
    let on = enabled.clone();
    handle.set_clipboard_sync(enabled.load(Ordering::Relaxed));
    std::thread::spawn(move || {
        let Ok(mut clipboard) = Clipboard::new() else {
            return;
        };
        let mut from_phone = handle.clips();
        // What is on the PC clipboard now is not news: only later copies are sent.
        let mut known = read_pc(&mut clipboard).map(|(clip, _)| signature(&clip));
        loop {
            std::thread::sleep(POLL);
            for clip in copies.try_iter() {
                known = write_pc(&mut clipboard, &clip).or(known);
            }
            let syncing = on.load(Ordering::Relaxed);
            handle.set_clipboard_sync(syncing);
            if !syncing {
                // Stay current, so turning sync on later does not send old contents.
                known = read_pc(&mut clipboard).map(|(clip, _)| signature(&clip));
                continue;
            }
            // Phone -> PC.
            if from_phone.has_changed().unwrap_or(false) {
                if let Some(clip) = from_phone.borrow_and_update().clone() {
                    known = write_pc(&mut clipboard, &clip).or(known);
                }
            }
            // PC -> phone.
            let Some((clip, size)) = read_pc(&mut clipboard) else {
                continue;
            };
            let sig = signature(&clip);
            if Some(sig) == known {
                continue;
            }
            known = Some(sig);
            let clip = match (clip, size) {
                (Clip::Image(rgba), Some(size)) => match encode_png(&rgba, size) {
                    Some(png) => Clip::Image(png),
                    None => continue,
                },
                (clip, _) => clip,
            };
            handle.send(Action::SetClipboard(clip));
        }
    });
    Sync { enabled, copy }
}
