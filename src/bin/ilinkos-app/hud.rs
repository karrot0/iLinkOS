//! The performance overlay: what the app in front is using, drawn over the screen.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use ilinkos::{serve::Handle, tools::Perf};
use tokio::sync::broadcast::error::TryRecvError;

use crate::{DIM, OK, TEXT};

const POLL: Duration = Duration::from_millis(200);
/// How many samples, one a second, the graph shows.
const HISTORY: usize = 40;
const SIZE: Vec2 = Vec2::new(176.0, 62.0);

#[derive(Default)]
struct State {
    latest: Option<Perf>,
    cpu: VecDeque<f32>,
}

pub struct Hud {
    pub enabled: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

pub fn start(handle: Handle, ctx: egui::Context, enabled: bool) -> Hud {
    let enabled = Arc::new(AtomicBool::new(enabled));
    let state = Arc::new(Mutex::new(State::default()));
    let (on, shared) = (enabled.clone(), state.clone());
    std::thread::spawn(move || {
        // The phone is only sampled while the overlay is switched on.
        let mut samples = None;
        loop {
            if !on.load(Ordering::Relaxed) {
                if samples.take().is_some() {
                    *shared.lock().unwrap() = State::default();
                }
                std::thread::sleep(POLL);
                continue;
            }
            match samples.get_or_insert_with(|| handle.perf()).try_recv() {
                Ok(sample) => {
                    let mut state = shared.lock().unwrap();
                    // A different app starts a fresh graph.
                    if state.latest.as_ref().is_some_and(|last| last.app != sample.app) {
                        state.cpu.clear();
                    }
                    state.cpu.push_back(sample.cpu as f32);
                    if state.cpu.len() > HISTORY {
                        state.cpu.pop_front();
                    }
                    state.latest = Some(sample);
                    drop(state);
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty | TryRecvError::Lagged(_)) => std::thread::sleep(POLL),
                Err(TryRecvError::Closed) => return,
            }
        }
    });
    Hud { enabled, state }
}

impl Hud {
    /// Draws the overlay in the top-left of the phone's screen at `screen`.
    pub fn paint(&self, ui: &egui::Ui, screen: Rect) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let state = self.state.lock().unwrap();
        let corner = screen.left_top() + Vec2::new(12.0, (screen.height() * 0.065).max(40.0));
        let card = Rect::from_min_size(corner, SIZE);
        let painter = ui.painter().with_clip_rect(screen);
        painter.rect_filled(card, 9.0, Color32::from_rgba_unmultiplied(12, 12, 16, 215));
        painter.rect_stroke(card, 9.0, Stroke::new(1.0, Color32::from_white_alpha(28)), egui::StrokeKind::Inside);
        let text = |at: Pos2, text: String, size: f32, color: Color32| {
            painter.text(at, Align2::LEFT_TOP, text, FontId::proportional(size), color);
        };
        let Some(sample) = &state.latest else {
            text(card.left_top() + Vec2::new(10.0, 22.0), "Measuring…".into(), 12.0, DIM);
            return;
        };
        let inner = card.shrink2(Vec2::new(10.0, 7.0));
        painter.with_clip_rect(inner).text(
            inner.left_top(),
            Align2::LEFT_TOP,
            &sample.app,
            FontId::proportional(12.5),
            TEXT,
        );
        let figures = format!("CPU {:.0}%   {:.0} MB   {:.0} fps", sample.cpu, sample.memory_mb, sample.fps);
        text(inner.left_top() + Vec2::new(0.0, 17.0), figures, 11.0, DIM);

        // CPU over the last while, scaled to its own peak (at least one full core).
        let graph = Rect::from_min_max(Pos2::new(inner.left(), inner.bottom() - 14.0), inner.right_bottom());
        let peak = state.cpu.iter().copied().fold(100.0f32, f32::max);
        let step = graph.width() / (HISTORY - 1) as f32;
        let points: Vec<Pos2> = state
            .cpu
            .iter()
            .enumerate()
            .map(|(i, cpu)| Pos2::new(graph.left() + i as f32 * step, graph.bottom() - cpu / peak * graph.height()))
            .collect();
        if points.len() > 1 {
            painter.line(points, Stroke::new(1.3, OK));
        }
    }
}
