//! Records the phone's screen and sound to an MP4. The video is the phone's own
//! HEVC stream, copied as it arrives; only the sound is re-encoded.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::body::Bytes;
use crate::control::ACCESS_UNIT_DELIMITER;
use fdk_aac::dec::{Decoder, Transport};
use tokio::{
    io::AsyncWriteExt,
    net::windows::named_pipe::ServerOptions,
    process::Command,
    sync::{Notify, mpsc, oneshot},
};

/// The phone's sound: 48 kHz stereo, 480 samples (10 ms) a frame.
pub const AUDIO_RATE: usize = 48_000;
pub const AUDIO_FRAME: usize = 480;
/// AudioSpecificConfig for AAC-ELD, 48 kHz, stereo, 480-sample frames. The phone
/// advertises 512-sample frames but sends 480, so the flag is set by hand.
pub const AUDIO_CONFIG: [u8; 4] = [0xF8, 0xE6, 0x50, 0x00];
/// Packets this small are the encoder's "nothing is playing" marker, not audio.
pub const SILENCE_MARKER: usize = 4;
/// A recording can only start on a keyframe. If asking the phone for one has not
/// produced it after this long, the stream is restarted, which always begins with one.
const KEYFRAME_WAIT: Duration = Duration::from_millis(1500);
/// Sound that has fallen this far behind the clock is padded with silence.
const AUDIO_SLACK: Duration = Duration::from_millis(120);

/// A piece of the phone's streams, as the daemon receives it.
pub enum Piece {
    /// Annex-B HEVC; `frame_end` marks the last bytes of a picture, and `timestamp`
    /// is the phone's clock reading for it.
    Video { bytes: Vec<u8>, frame_end: bool, timestamp: u32 },
    /// One AAC-ELD frame.
    Audio(Bytes),
}

/// A recording in progress. Dropping `pieces` ends it.
pub struct Recording {
    pub pieces: mpsc::UnboundedSender<Piece>,
    pub path: PathBuf,
    pub since: Instant,
    done: oneshot::Receiver<Result<(), String>>,
}

impl Recording {
    /// Starts recording to `path`. `keyframe` is signalled to ask the phone for a
    /// keyframe; `restart` restarts the stream if that does not work.
    pub fn start(
        path: PathBuf,
        runtime: tokio::runtime::Handle,
        keyframe: Arc<Notify>,
        restart: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (pieces, incoming) = mpsc::unbounded_channel();
        let (finished, done) = oneshot::channel();
        let file = path.clone();
        // Its own thread: the audio decoder is not `Send`.
        std::thread::Builder::new().name("record".into()).spawn(move || {
            let result = runtime.block_on(run(&file, incoming, keyframe, restart));
            let _ = finished.send(result.map_err(|e| format!("{e:#}")));
        })?;
        Ok(Self { pieces, path, since: Instant::now(), done })
    }

    /// Ends the recording and waits for the file to be complete.
    pub async fn finish(self) -> Result<PathBuf, String> {
        drop(self.pieces);
        self.done.await.map_err(|_| "the recorder stopped unexpectedly".to_owned())??;
        Ok(self.path)
    }
}

/// True if an Annex-B picture can be decoded on its own.
fn is_keyframe(frame: &[u8]) -> bool {
    frame.windows(4).any(|w| {
        // NAL unit types 16 to 21 are the random-access pictures.
        w[..3] == [0, 0, 1] && (16..=21).contains(&((w[3] >> 1) & 0x3F))
    })
}

/// Ticks of the 90 kHz clock that video timestamps count in.
const TICKS: u64 = 90_000;
/// The phone stamps its pictures in 24,000ths of a second (measured: 400 a frame
/// at 60 frames a second), whatever rate was negotiated.
const PHONE_TICKS: i64 = 24_000;
/// If the phone's clock and ours disagree by more than this, its stream restarted.
const CLOCK_JUMP: i64 = TICKS as i64;

/// Turns the phone's picture timestamps into times since the recording began.
#[derive(Default)]
struct Clock {
    /// A phone timestamp and the recording time it corresponds to.
    anchor: Option<(u32, u64)>,
    last: u64,
}

impl Clock {
    fn time(&mut self, timestamp: u32, elapsed: Duration) -> u64 {
        let now = (elapsed.as_secs_f64() * TICKS as f64) as u64;
        let (from, at) = *self.anchor.get_or_insert((timestamp, now));
        let passed = timestamp.wrapping_sub(from) as i32 as i64;
        let mut time = at as i64 + passed * TICKS as i64 / PHONE_TICKS;
        if (time - now as i64).abs() > CLOCK_JUMP {
            self.anchor = Some((timestamp, now));
            time = now as i64;
        }
        self.last = (time.max(0) as u64).max(self.last + 1);
        self.last
    }
}

const PACKET: usize = 188;
const TABLE_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x100;
/// Pictures are presented this long after the stream's clock reference.
const PRESENT_AFTER: u64 = TICKS / 2;

/// Wraps pictures in an MPEG transport stream, which carries a time for each.
#[derive(Default)]
struct Muxer {
    /// Per-stream packet counters: tables, video.
    counters: [u8; 3],
}

fn crc32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0xFFFF_FFFF, |crc, byte| {
        (0..8).fold(crc ^ (*byte as u32) << 24, |crc, _| {
            if crc & 0x8000_0000 != 0 { crc << 1 ^ 0x04C1_1DB7 } else { crc << 1 }
        })
    })
}

impl Muxer {
    fn header(&mut self, out: &mut Vec<u8>, pid: u16, counter: usize, start: bool, adaptation: bool) {
        let flags = if adaptation { 0x30 } else { 0x10 };
        out.extend_from_slice(&[
            0x47,
            (start as u8) << 6 | (pid >> 8) as u8,
            pid as u8,
            flags | self.counters[counter],
        ]);
        self.counters[counter] = (self.counters[counter] + 1) & 0x0F;
    }

    /// One packet holding a table that says what the stream contains.
    fn table(&mut self, out: &mut Vec<u8>, pid: u16, counter: usize, mut section: Vec<u8>) {
        section.extend_from_slice(&crc32(&section).to_be_bytes());
        let end = out.len() + PACKET;
        self.header(out, pid, counter, true, false);
        out.push(0);
        out.extend_from_slice(&section);
        out.resize(end, 0xFF);
    }

    /// The packets for one picture shown at `time`.
    fn picture(&mut self, picture: &[u8], time: u64, key: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(picture.len() + picture.len() / 40 + 3 * PACKET);
        if key {
            let (table, video) = (TABLE_PID.to_be_bytes(), VIDEO_PID.to_be_bytes());
            // Programme 1 is described by the table on TABLE_PID...
            let programmes = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE0 | table[0], table[1]];
            self.table(&mut out, 0, 0, programmes);
            // ...which says it is one HEVC stream on VIDEO_PID, carrying the clock.
            let streams = vec![
                0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE0 | video[0], video[1], 0xF0, 0x00,
                0x24, 0xE0 | video[0], video[1], 0xF0, 0x00,
            ];
            self.table(&mut out, TABLE_PID, 1, streams);
        }

        let shown = time + PRESENT_AFTER;
        let mut payload = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x80, 0x05];
        payload.extend_from_slice(&[
            0x21 | ((shown >> 29) & 0x0E) as u8,
            (shown >> 22) as u8,
            0x01 | ((shown >> 14) & 0xFE) as u8,
            (shown >> 7) as u8,
            0x01 | ((shown << 1) & 0xFE) as u8,
        ]);
        payload.extend_from_slice(&ACCESS_UNIT_DELIMITER);
        payload.extend_from_slice(picture);

        let mut rest = &payload[..];
        let mut first = true;
        while !rest.is_empty() {
            // The first packet carries the clock; the last is padded to full length.
            let room = PACKET - 4 - if first { 8 } else { 0 };
            let padding = room.saturating_sub(rest.len());
            self.header(&mut out, VIDEO_PID, 2, first, first || padding > 0);
            if first {
                out.extend_from_slice(&[(7 + padding) as u8, 0x10]);
                out.extend_from_slice(&[(time >> 25) as u8, (time >> 17) as u8, (time >> 9) as u8, (time >> 1) as u8]);
                out.extend_from_slice(&[((time & 1) as u8) << 7 | 0x7E, 0x00]);
                out.resize(out.len() + padding, 0xFF);
            } else if padding == 1 {
                out.push(0);
            } else if padding > 1 {
                out.extend_from_slice(&[(padding - 1) as u8, 0x00]);
                out.resize(out.len() + padding - 2, 0xFF);
            }
            let take = rest.len().min(room);
            out.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            first = false;
        }
        out
    }
}

async fn run(
    path: &Path,
    mut incoming: mpsc::UnboundedReceiver<Piece>,
    keyframe: Arc<Notify>,
    restart: impl Fn(),
) -> Result<()> {
    // ffmpeg takes the video on its standard input and the sound on a named pipe.
    let pipe_name = format!(r"\\.\pipe\ilinkos-record-{}", uuid::Uuid::new_v4().simple());
    let mut pipe = ServerOptions::new().first_pipe_instance(true).create(&pipe_name)?;
    let mut ffmpeg = Command::new(crate::ffmpeg())
        .args(["-y", "-loglevel", "error"])
        .args(["-f", "mpegts", "-i", "pipe:0"])
        .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", &pipe_name])
        .args(["-map", "0:v", "-map", "1:a", "-c:v", "copy", "-tag:v", "hvc1"])
        .args(["-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .kill_on_drop(true)
        .spawn()
        .context("could not start ffmpeg — it must be beside iLinkOS or on PATH")?;
    let mut stdin = ffmpeg.stdin.take().unwrap();

    let (video, mut frames) = mpsc::unbounded_channel::<Vec<u8>>();
    let video_task = tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if stdin.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let (audio, mut samples) = mpsc::unbounded_channel::<Vec<u8>>();
    let audio_task = tokio::spawn(async move {
        // ffmpeg opens the pipe once it has seen the start of the video.
        if pipe.connect().await.is_err() {
            return;
        }
        while let Some(pcm) = samples.recv().await {
            if pipe.write_all(&pcm).await.is_err() {
                break;
            }
        }
        let _ = pipe.flush().await;
    });

    let mut decoder = Decoder::new(Transport::Raw);
    decoder.config_raw(&AUDIO_CONFIG).map_err(|e| anyhow::anyhow!("audio decoder: {e:?}"))?;
    let mut pcm = vec![0i16; AUDIO_FRAME * 2];
    let mut frame = Vec::new();
    let (mut muxer, mut clock) = (Muxer::default(), Clock::default());
    // When the first picture was written, and how much sound has been since.
    let mut started: Option<Instant> = None;
    let mut written = 0usize;
    let (asked, mut restarted) = (Instant::now(), false);
    keyframe.notify_one();

    while let Some(piece) = incoming.recv().await {
        match piece {
            Piece::Video { bytes, frame_end, timestamp } => {
                frame.extend_from_slice(&bytes);
                if !frame_end {
                    continue;
                }
                // The delimiter that ends each picture on the way to the live decoder
                // belongs at the start of one in a file.
                if frame.ends_with(&ACCESS_UNIT_DELIMITER) {
                    frame.truncate(frame.len() - ACCESS_UNIT_DELIMITER.len());
                }
                let key = is_keyframe(&frame);
                if started.is_none() && key {
                    started = Some(Instant::now());
                }
                if let Some(started) = started {
                    let time = clock.time(timestamp, started.elapsed());
                    let _ = video.send(muxer.picture(&frame, time, key));
                }
                frame.clear();
            }
            Piece::Audio(packet) => {
                let Some(started) = started else {
                    if !restarted && asked.elapsed() >= KEYFRAME_WAIT {
                        restarted = true;
                        restart();
                    }
                    continue;
                };
                if packet.len() <= SILENCE_MARKER
                    || decoder.fill(&packet).is_err()
                    || decoder.decode_frame(&mut pcm).is_err()
                {
                    pcm.fill(0);
                }
                // Keep the sound in step with the clock across lost packets and reconnects.
                let due = (started.elapsed().as_secs_f64() * AUDIO_RATE as f64) as usize;
                let slack = AUDIO_SLACK.as_millis() as usize * AUDIO_RATE / 1000;
                if due > written + slack {
                    let gap = due - written - AUDIO_FRAME;
                    let _ = audio.send(vec![0u8; gap * 4]);
                    written += gap;
                }
                let _ = audio.send(bytemuck::cast_slice::<i16, u8>(&pcm).to_vec());
                written += AUDIO_FRAME;
            }
        }
    }

    // Closing both inputs lets ffmpeg finish the file.
    drop((video, audio));
    let _ = video_task.await;
    let output = ffmpeg.wait_with_output().await?;
    audio_task.abort();
    if started.is_none() {
        let _ = std::fs::remove_file(path);
        anyhow::bail!("no video arrived while recording");
    }
    if !output.status.success() {
        anyhow::bail!("ffmpeg: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}
