//! A small draggable world map for choosing a location, drawn from OpenStreetMap tiles.

use std::{
    collections::HashMap,
    f64::consts::PI,
    sync::{Arc, Mutex, mpsc},
};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, TextureHandle, Ui, Vec2};

use crate::{BAD, LINE, OK, RAISED};

const TILE: f64 = 256.0;
const MAX_ZOOM: u8 = 18;
/// Tiles kept before the oldest views are forgotten.
const CACHE: usize = 400;
const FETCHERS: usize = 4;
/// OpenStreetMap asks every program that uses its tiles to say who it is.
const USER_AGENT: &str = "iLinkOS/0.1 (iPhone mirroring desktop app)";

type TileId = (u8, u32, u32);
/// A fetched tile's pixels, or `None` if it could not be fetched.
type Fetched = (TileId, Option<egui::ColorImage>);

pub struct Map {
    /// Latitude and longitude at the centre of the view.
    pub centre: (f64, f64),
    pub zoom: u8,
    /// `None` while a tile is being fetched or after it failed.
    tiles: HashMap<TileId, Option<TextureHandle>>,
    wanted: mpsc::Sender<TileId>,
    fetched: Arc<Mutex<Vec<Fetched>>>,
}

/// Where a position falls on the square world map, each axis from 0 to 1.
fn to_world((lat, lon): (f64, f64)) -> (f64, f64) {
    let lat = lat.clamp(-85.0511, 85.0511).to_radians();
    ((lon + 180.0) / 360.0, (1.0 - lat.tan().asinh() / PI) / 2.0)
}

fn from_world((x, y): (f64, f64)) -> (f64, f64) {
    let lat = (PI * (1.0 - 2.0 * y.clamp(0.0, 1.0))).sinh().atan().to_degrees();
    (lat, x.rem_euclid(1.0) * 360.0 - 180.0)
}

fn fetch((zoom, x, y): TileId) -> Option<egui::ColorImage> {
    let url = format!("https://tile.openstreetmap.org/{zoom}/{x}/{y}.png");
    let bytes = ureq::get(&url).header("User-Agent", USER_AGENT).call().ok()?.body_mut().read_to_vec().ok()?;
    let image = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

impl Map {
    pub fn new(ctx: &egui::Context, centre: (f64, f64), zoom: u8) -> Self {
        let (wanted, queue) = mpsc::channel::<TileId>();
        let queue = Arc::new(Mutex::new(queue));
        let fetched: Arc<Mutex<Vec<Fetched>>> = Arc::default();
        for _ in 0..FETCHERS {
            let (queue, fetched, ctx) = (queue.clone(), fetched.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    let Ok(id) = queue.lock().unwrap().recv() else {
                        return;
                    };
                    fetched.lock().unwrap().push((id, fetch(id)));
                    ctx.request_repaint();
                }
            });
        }
        Self { centre, zoom: zoom.min(MAX_ZOOM), tiles: HashMap::new(), wanted, fetched }
    }

    /// Draws the map with `pins` (position, whether it is the live one) and
    /// returns the position the user clicked, if they did.
    pub fn ui(&mut self, ui: &mut Ui, size: Vec2, pins: &[((f64, f64), bool)]) -> Option<(f64, f64)> {
        for (id, image) in self.fetched.lock().unwrap().drain(..) {
            let texture = image.map(|image| {
                ui.ctx().load_texture(format!("tile {id:?}"), image, egui::TextureOptions::LINEAR)
            });
            self.tiles.insert(id, texture);
        }
        if self.tiles.len() > CACHE {
            self.tiles.clear();
        }

        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 8.0, RAISED);

        // Pixels across the whole world at this zoom.
        let world = TILE * f64::from(1u32 << self.zoom);
        let (cx, cy) = to_world(self.centre);
        if response.dragged() {
            let moved = response.drag_delta();
            self.centre = from_world((cx - f64::from(moved.x) / world, cy - f64::from(moved.y) / world));
        }
        if response.hovered() {
            let wheel: f32 = ui.input(|i| {
                i.events
                    .iter()
                    .map(|event| match event {
                        egui::Event::MouseWheel { delta, .. } => delta.y,
                        _ => 0.0,
                    })
                    .sum()
            });
            if wheel > 0.0 && self.zoom < MAX_ZOOM {
                self.zoom += 1;
            } else if wheel < 0.0 && self.zoom > 1 {
                self.zoom -= 1;
            }
        }
        let world = TILE * f64::from(1u32 << self.zoom);
        let (cx, cy) = to_world(self.centre);
        // The world pixel at the view's top-left corner.
        let (left, top) = (cx * world - f64::from(rect.width()) / 2.0, cy * world - f64::from(rect.height()) / 2.0);
        let on_screen = |(x, y): (f64, f64)| {
            Pos2::new(rect.left() + (x * world - left) as f32, rect.top() + (y * world - top) as f32)
        };

        let count = 1i64 << self.zoom;
        let first = ((left / TILE).floor() as i64, (top / TILE).floor() as i64);
        let last = (((left + f64::from(rect.width())) / TILE).floor() as i64, ((top + f64::from(rect.height())) / TILE).floor() as i64);
        for ty in first.1.max(0)..=last.1.min(count - 1) {
            for tx in first.0..=last.0 {
                // The map repeats sideways.
                let id = (self.zoom, tx.rem_euclid(count) as u32, ty as u32);
                let corner = Pos2::new(
                    rect.left() + (tx as f64 * TILE - left) as f32,
                    rect.top() + (ty as f64 * TILE - top) as f32,
                );
                let tile = Rect::from_min_size(corner, Vec2::splat(TILE as f32));
                match self.tiles.get(&id) {
                    Some(Some(texture)) => {
                        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                        painter.image(texture.id(), tile, uv, Color32::WHITE);
                    }
                    Some(None) => {}
                    None => {
                        self.tiles.insert(id, None);
                        let _ = self.wanted.send(id);
                    }
                }
            }
        }

        for (at, live) in pins {
            let point = on_screen(to_world(*at));
            let color = if *live { OK } else { BAD };
            painter.circle_filled(point, 7.0, color);
            painter.circle_stroke(point, 7.0, Stroke::new(2.0, Color32::WHITE));
        }
        painter.rect_stroke(rect, 8.0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        let credit = "© OpenStreetMap";
        let at = rect.right_bottom() - Vec2::new(5.0, 4.0);
        let galley = painter.layout_no_wrap(credit.into(), FontId::proportional(9.5), Color32::from_gray(40));
        let back = Rect::from_min_max(at - galley.size() - Vec2::new(4.0, 2.0), at + Vec2::new(2.0, 1.0));
        painter.rect_filled(back, 3.0, Color32::from_white_alpha(190));
        painter.text(at, Align2::RIGHT_BOTTOM, credit, FontId::proportional(9.5), Color32::from_gray(40));

        let clicked = response.clicked().then(|| response.interact_pointer_pos()).flatten()?;
        let x = (left + f64::from(clicked.x - rect.left())) / world;
        let y = (top + f64::from(clicked.y - rect.top())) / world;
        Some(from_world((x, y)))
    }
}
