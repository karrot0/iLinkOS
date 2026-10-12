//! Plays the phone's sound on the PC: AAC-ELD frames in, the default speakers out.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fdk_aac::dec::{Decoder, Transport};
use ilinkos::serve::Handle;

/// The phone's stream: 48 kHz stereo, 480 samples (10 ms) a frame.
const RATE: u32 = 48_000;
const FRAME: usize = 480;
const CONFIG: [u8; 4] = [0xF8, 0xE6, 0x50, 0x00];
const SILENCE_MARKER: usize = 4;
const PREBUFFER: usize = RATE as usize * 60 / 1000;
const MAX_BUFFER: usize = RATE as usize * 180 / 1000;

type Sample = [f32; 2];

#[derive(Default)]
struct Queue {
    frames: VecDeque<Sample>,
    playing: bool,
    phase: f64,
}

#[derive(Default)]
pub struct Stats {
    pub decoded: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Audio {
    pub enabled: Arc<AtomicBool>,
    pub stats: Arc<Stats>,
    _stream: Option<cpal::Stream>,
}

fn open_output(queue: Arc<Mutex<Queue>>, enabled: Arc<AtomicBool>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let supported = device.default_output_config().ok()?;
    let channels = supported.channels() as usize;
    let step = RATE as f64 / supported.sample_rate() as f64;
    let config: cpal::StreamConfig = supported.into();
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [f32], _| {
                let mut queue = queue.lock().unwrap();
                if !enabled.load(Ordering::Relaxed) {
                    queue.frames.clear();
                    queue.playing = false;
                }
                if !queue.playing && queue.frames.len() >= PREBUFFER {
                    queue.playing = true;
                }
                for frame in out.chunks_mut(channels) {
                    let sample = if queue.playing && queue.frames.len() >= 2 {
                        let (a, b, t) = (queue.frames[0], queue.frames[1], queue.phase as f32);
                        queue.phase += step;
                        while queue.phase >= 1.0 && queue.frames.len() > 1 {
                            queue.phase -= 1.0;
                            queue.frames.pop_front();
                        }
                        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
                    } else {
                        queue.playing = false;
                        [0.0, 0.0]
                    };
                    match frame {
                        [mono] => *mono = (sample[0] + sample[1]) * 0.5,
                        [left, right, rest @ ..] => {
                            (*left, *right) = (sample[0], sample[1]);
                            rest.fill(0.0);
                        }
                        [] => {}
                    }
                }
            },
            |_| {},
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

pub fn start(handle: Handle, enabled: bool) -> Audio {
    let enabled = Arc::new(AtomicBool::new(enabled));
    let stats = Arc::new(Stats::default());
    let queue = Arc::new(Mutex::new(Queue::default()));
    let stream = open_output(queue.clone(), enabled.clone());

    let (on, counters, mut packets) = (enabled.clone(), stats.clone(), handle.audio());
    std::thread::spawn(move || {
        let mut decoder = Decoder::new(Transport::Raw);
        if decoder.config_raw(&CONFIG).is_err() {
            return;
        }
        let mut pcm = vec![0i16; FRAME * 2];
        let mut received = 0u64;
        loop {
            let packet = match packets.blocking_recv() {
                Ok(packet) => packet,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            };
            if !on.load(Ordering::Relaxed) {
                continue;
            }
            if packet.len() <= SILENCE_MARKER {
                pcm.fill(0);
            } else if decoder.fill(&packet).is_err() || decoder.decode_frame(&mut pcm).is_err() {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            } else {
                counters.decoded.fetch_add(1, Ordering::Relaxed);
            }
            received += 1;
            if received % 500 == 0 {
                if let Ok(dir) = ilinkos::wifi::state_dir() {
                    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                    let line = format!(
                        "packets={received} decoded={} errors={} last_frame_peak={peak}
",
                        counters.decoded.load(Ordering::Relaxed),
                        counters.errors.load(Ordering::Relaxed),
                    );
                    let _ = std::fs::write(dir.join("audio-stats.txt"), line);
                }
            }
            let mut queue = queue.lock().unwrap();
            queue.frames.extend(
                pcm.chunks_exact(2).map(|s| [s[0] as f32 / 32768.0, s[1] as f32 / 32768.0]),
            );
            if queue.frames.len() > MAX_BUFFER {
                let excess = queue.frames.len() - PREBUFFER;
                queue.frames.drain(..excess);
            }
        }
    });
    Audio { enabled, stats, _stream: stream }
}
