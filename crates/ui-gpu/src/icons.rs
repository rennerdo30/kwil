//! Trusted repository SVGs, compiled at build time and rasterized once per DPI.
use super::*;
use std::{cell::RefCell, collections::VecDeque};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Icon {
    Close,
    Back,
    Forward,
    Up,
    Down,
    Check,
    Open,
    Save,
    Undo,
    Redo,
    GoTo,
    Settings,
    Markdown,
    Split,
    Text,
    File,
    Sun,
    Moon,
    Search,
}
const ALL: &[Icon] = &[
    Icon::Close,
    Icon::Back,
    Icon::Forward,
    Icon::Up,
    Icon::Down,
    Icon::Check,
    Icon::Open,
    Icon::Save,
    Icon::Undo,
    Icon::Redo,
    Icon::GoTo,
    Icon::Settings,
    Icon::Markdown,
    Icon::Split,
    Icon::Text,
    Icon::File,
    Icon::Sun,
    Icon::Moon,
    Icon::Search,
];
#[derive(Clone, Copy)]
enum Primitive {
    Line([f32; 4]),
    Circle([f32; 3]),
}
include!(concat!(env!("OUT_DIR"), "/svg_icons.rs"));
struct Mask {
    icon: Icon,
    size: usize,
    alpha: Vec<u8>,
}
#[derive(Default)]
struct Cache {
    masks: VecDeque<Mask>,
    bytes: usize,
}
thread_local! {static CACHE:RefCell<Cache>=RefCell::new(Cache::default());}
const LIMIT: usize = 1 << 20;
fn distance(p: (f32, f32), shape: Primitive) -> f32 {
    match shape {
        Primitive::Line([ax, ay, bx, by]) => {
            let (dx, dy) = (bx - ax, by - ay);
            let length = dx * dx + dy * dy;
            let t = if length > 0. { ((p.0 - ax) * dx + (p.1 - ay) * dy) / length } else { 0. }.clamp(0., 1.);
            ((p.0 - ax - t * dx).powi(2) + (p.1 - ay - t * dy).powi(2)).sqrt()
        }
        Primitive::Circle([x, y, r]) => (((p.0 - x).powi(2) + (p.1 - y).powi(2)).sqrt() - r).abs(),
    }
}
fn raster(icon: Icon, size: usize) -> Vec<u8> {
    let mut mask = vec![0; size * size];
    let shapes = geometry(icon);
    for y in 0..size {
        for x in 0..size {
            let mut samples = 0;
            for oy in [0.25, 0.75] {
                for ox in [0.25, 0.75] {
                    let p = ((x as f32 + ox) * 24. / size as f32, (y as f32 + oy) * 24. / size as f32);
                    samples += u16::from(shapes.iter().any(|shape| distance(p, *shape) <= 0.875));
                }
            }
            mask[y * size + x] = (samples * 255 / 4) as u8;
        }
    }
    mask
}
impl Cache {
    fn get(&mut self, icon: Icon, size: usize) -> &Mask {
        if let Some(index) = self.masks.iter().position(|m| m.icon == icon && m.size == size) {
            let value = self.masks.remove(index).unwrap();
            self.masks.push_back(value);
        } else {
            while self.bytes + size * size > LIMIT || self.masks.len() >= ALL.len() * 2 {
                if let Some(old) = self.masks.pop_front() {
                    self.bytes -= old.alpha.len();
                } else {
                    break;
                }
            }
            let alpha = raster(icon, size);
            self.bytes += alpha.len();
            self.masks.push_back(Mask { icon, size, alpha });
        }
        self.masks.back().unwrap()
    }
}
pub(super) fn draw(buf: &mut [u32], stride: usize, icon: Icon, center: (f32, f32), size: usize, color: u32) {
    if stride == 0 || size == 0 {
        return;
    }
    let size = size.min(256);
    let left = (center.0 - size as f32 / 2.).floor() as i64;
    let top = (center.1 - size as f32 / 2.).floor() as i64;
    let rows = buf.len() / stride;
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mask = cache.get(icon, size);
        for y in 0..size {
            let py = top + y as i64;
            if py < 0 || py >= rows as i64 {
                continue;
            }
            for x in 0..size {
                let px = left + x as i64;
                if px < 0 || px >= stride as i64 {
                    continue;
                }
                let alpha = mask.alpha[y * size + x];
                if alpha > 0 {
                    let pixel = &mut buf[py as usize * stride + px as usize];
                    *pixel = chrome::mix(*pixel, color, alpha);
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn svg_masks_are_distinct_scaled_clipped_and_cache_bounded() {
        let mut cache = Cache::default();
        let mut hashes = Vec::new();
        for icon in [
            Icon::Close,
            Icon::Open,
            Icon::Save,
            Icon::Search,
            Icon::Undo,
            Icon::Redo,
            Icon::Settings,
            Icon::Markdown,
            Icon::Split,
        ] {
            let mask = cache.get(icon, 24);
            assert!(mask.alpha.iter().any(|a| *a > 0));
            hashes.push(mask.alpha.clone());
        }
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        for size in [16, 24, 32, 48, 64, 96, 128, 256] {
            cache.get(Icon::Search, size);
            assert!(cache.bytes <= LIMIT && cache.masks.len() <= ALL.len() * 2);
        }
        let mut pixels = vec![0x101010; 49];
        draw(&mut pixels, 7, Icon::Search, (0., 0.), 32, 0xffffff);
        assert_eq!(pixels.len(), 49);
    }
}
