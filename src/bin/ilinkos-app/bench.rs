//! A built-in latency benchmark, driven by tools/bench.py.
//!
//! It times a touch from the moment it is sent until a changed picture reaches
//! the window: finger down on a home-screen page, wait for the picture to settle,
//! move the finger, and watch every incoming picture for the page to move.

use std::time::{Duration, Instant};

use eframe::egui::ColorImage;
use ilinkos::{
    serve::{Action, Handle},
    wifi,
};

const SETTLE: Duration = Duration::from_millis(350);
const COOLDOWN: Duration = Duration::from_millis(800);
const TIMEOUT: Duration = Duration::from_secs(2);
/// Mean absolute difference, out of 255, that counts as the picture having changed.
const CHANGED: f32 = 1.5;
const GRID: (usize, usize) = (24, 52);

enum State {
    Idle,
    Settle { since: Instant },
    Armed { before: Vec<u8>, sent: Instant },
    Cooldown { since: Instant },
}

pub struct Bench {
    state: State,
    remaining: u32,
    samples: Vec<f32>,
}

/// A coarse greyscale thumbnail, enough to tell that a page has moved.
fn thumbnail(image: &ColorImage) -> Vec<u8> {
    let [width, height] = image.size;
    let mut out = Vec::with_capacity(GRID.0 * GRID.1);
    for gy in 0..GRID.1 {
        for gx in 0..GRID.0 {
            let pixel = image.pixels[(gy * height / GRID.1) * width + gx * width / GRID.0];
            let luma = (pixel.r() as u32 * 2 + pixel.g() as u32 * 5 + pixel.b() as u32) / 8;
            out.push(luma as u8);
        }
    }
    out
}

fn difference(a: &[u8], b: &[u8]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as f32).sum::<f32>() / a.len().max(1) as f32
}

impl Bench {
    pub fn new() -> Self {
        Self { state: State::Idle, remaining: 0, samples: Vec::new() }
    }

    /// Starts a run if tools/bench.py has asked for one. Called about once a second.
    pub fn check_request(&mut self) {
        let Ok(dir) = wifi::state_dir() else {
            return;
        };
        let request = dir.join("bench-request");
        if !matches!(self.state, State::Idle) || self.remaining > 0 {
            return;
        }
        if let Ok(text) = std::fs::read_to_string(&request) {
            let _ = std::fs::remove_file(&request);
            self.remaining = text.trim().parse().unwrap_or(16);
            self.samples.clear();
        }
    }

    /// Advances the benchmark with a picture that has just been put in the window.
    pub fn on_picture(&mut self, image: &ColorImage, handle: &Handle) {
        match &self.state {
            State::Idle if self.remaining > 0 => {
                handle.send(Action::Down { x: 0.5, y: 0.5 });
                self.state = State::Settle { since: Instant::now() };
            }
            State::Settle { since } if since.elapsed() >= SETTLE => {
                let before = thumbnail(image);
                // Alternate direction so the page returns to where it started.
                let x = if self.remaining % 2 == 0 { 0.65 } else { 0.35 };
                handle.send(Action::Move { x, y: 0.5 });
                self.state = State::Armed { before, sent: Instant::now() };
            }
            State::Armed { before, sent } => {
                let changed = difference(before, &thumbnail(image)) > CHANGED;
                if changed || sent.elapsed() >= TIMEOUT {
                    if changed {
                        self.samples.push(sent.elapsed().as_secs_f32() * 1000.0);
                    }
                    handle.send(Action::Up { x: 0.5, y: 0.5 });
                    self.remaining -= 1;
                    self.state = State::Cooldown { since: Instant::now() };
                }
            }
            State::Cooldown { since } if since.elapsed() >= COOLDOWN => {
                self.state = State::Idle;
                if self.remaining == 0 {
                    self.write_result();
                }
            }
            _ => {}
        }
    }

    fn write_result(&self) {
        let Ok(dir) = wifi::state_dir() else {
            return;
        };
        let samples: Vec<String> = self.samples.iter().map(|s| format!("{s:.1}")).collect();
        let _ = std::fs::write(
            dir.join("bench-result.json"),
            format!("{{\"glass_ms\": [{}]}}", samples.join(", ")),
        );
    }
}
