//! QR codes for links the user opens on a phone, using Nayuki's
//! `qrcodegen-no-heap` (no_std, no allocation, MIT).

use alloc::vec;
use alloc::vec::Vec;
use p4os_sdk::{Canvas, Color, Rect};
use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};

/// Version 10 holds 213 bytes at medium error correction, plenty for a
/// verification URL or a youtu.be link, and keeps the buffers at 300 bytes.
const MAX_VERSION: Version = Version::new(10);
/// Light border required around the code, in modules.
const QUIET: i32 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Qr {
    size: i32,
    dark: Vec<bool>,
}

impl Qr {
    pub fn encode(text: &str) -> Option<Qr> {
        let mut out = vec![0u8; MAX_VERSION.buffer_len()];
        let mut temp = vec![0u8; MAX_VERSION.buffer_len()];
        let code =
            QrCode::encode_text(text, &mut temp, &mut out, QrCodeEcc::Medium, Version::MIN, MAX_VERSION, None, true).ok()?;
        let size = code.size();
        let dark = (0..size * size).map(|i| code.get_module(i % size, i / size)).collect();
        Some(Qr { size, dark })
    }

    /// Modules per side, without the quiet zone.
    pub fn size(&self) -> i32 {
        self.size
    }

    pub fn is_dark(&self, x: i32, y: i32) -> bool {
        (0..self.size).contains(&x) && (0..self.size).contains(&y) && self.dark[(y * self.size + x) as usize]
    }

    /// Draw centred in `r` with whole-pixel modules and a white quiet
    /// zone; returns the area used.
    pub fn draw(&self, c: &mut Canvas, r: Rect) -> Rect {
        let total = self.size + 2 * QUIET;
        let scale = (r.w.min(r.h) / total).max(1);
        let side = total * scale;
        let area = Rect::new(r.x + (r.w - side) / 2, r.y + (r.h - side) / 2, side, side);
        c.fill_rect(area, Color::WHITE);
        let origin = (area.x + QUIET * scale, area.y + QUIET * scale);
        for y in 0..self.size {
            let mut x = 0;
            while x < self.size {
                if !self.is_dark(x, y) {
                    x += 1;
                    continue;
                }
                // One rectangle per horizontal run of dark modules.
                let start = x;
                while x < self.size && self.is_dark(x, y) {
                    x += 1;
                }
                let run = Rect::new(origin.0 + start * scale, origin.1 + y * scale, (x - start) * scale, scale);
                c.fill_rect(run, Color::BLACK);
            }
        }
        area
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_links() {
        let qr = Qr::encode("https://youtu.be/p4Os_Vid001").unwrap();
        // Sizes are 17 + 4 * version modules.
        assert!(qr.size() >= 21 && (qr.size() - 17) % 4 == 0 && qr.size() <= 57);
        // The three finder patterns: dark corners with a light separator.
        for (x, y) in [(0, 0), (qr.size() - 1, 0), (0, qr.size() - 1)] {
            assert!(qr.is_dark(x, y));
        }
        assert!(!qr.is_dark(7, 7));
        assert!(!qr.is_dark(-1, 0) && !qr.is_dark(0, qr.size()));
        assert!(Qr::encode(&"x".repeat(400)).is_none(), "too long for version 10");
    }

    #[test]
    fn draws_with_quiet_zone() {
        let qr = Qr::encode("https://www.google.com/device").unwrap();
        let mut buf = vec![Color::hex(0x123456); 200 * 200];
        let mut c = Canvas::new(&mut buf, 200, 200);
        let used = qr.draw(&mut c, Rect::new(0, 0, 200, 200));
        let scale = used.w / (qr.size() + 2 * QUIET);
        assert!(scale >= 5);
        assert_eq!(c.get(used.x + 1, used.y + 1), Color::WHITE, "quiet zone");
        let (ox, oy) = (used.x + QUIET * scale, used.y + QUIET * scale);
        assert_eq!(c.get(ox, oy), Color::BLACK, "top-left finder");
        assert_eq!(c.get(0, 0), Color::hex(0x123456), "outside untouched");
    }
}
