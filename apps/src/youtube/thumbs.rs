//! Thumbnail loading and a small LRU cache of decoded images.
//!
//! Rows ask for their thumbnail every frame with [`Thumbs::get`]; only
//! images asked for recently are fetched, so scrolling past a row does not
//! queue its download. [`Thumbs::pump`] runs between frames: it collects
//! downloads, starts new ones (at most [`MAX_FETCHES`] at a time) and
//! decodes at most one JPEG per call, never while a finger is on the
//! screen, so dragging stays smooth. The SDK has no worker threads for
//! apps, so this time-slicing is what keeps decoding off the touch path.
//!
//! Memory is bounded by [`MAX_ENTRIES`] decoded images (a 160x90 RGB565
//! list thumbnail is 28 KB; the kernel heap places such blocks in PSRAM).

use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::{FetchId, FetchState, Image, System};

pub const MAX_ENTRIES: usize = 40;
pub const MAX_FETCHES: usize = 2;
/// Largest download accepted for one thumbnail.
const MAX_BYTES: usize = 256 * 1024;
/// A request counts as wanted for this many frames after it was asked for.
const WANT_FRAMES: u64 = 2;

enum Slot {
    Wanted,
    Fetching(FetchId),
    Encoded(Vec<u8>),
    Ready(Image),
    Failed,
}

struct Entry {
    url: String,
    w: i32,
    h: i32,
    slot: Slot,
    used: u64,
}

#[derive(Default)]
pub struct Thumbs {
    entries: Vec<Entry>,
    frame: u64,
}

impl Thumbs {
    /// The decoded image for `url` fitted into `w`x`h`, if ready; asks
    /// for it otherwise.
    pub fn get(&mut self, url: &str, w: i32, h: i32) -> Option<&Image> {
        let frame = self.frame;
        let i = match self.entries.iter().position(|e| e.url == url && e.w == w && e.h == h) {
            Some(i) => i,
            None => {
                self.entries.push(Entry { url: url.into(), w, h, slot: Slot::Wanted, used: frame });
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[i];
        e.used = frame;
        match &e.slot {
            Slot::Ready(img) => Some(img),
            _ => None,
        }
    }

    /// Work left to do (downloads or decodes).
    pub fn busy(&self) -> bool {
        self.entries.iter().any(|e| self.is_live(e) && matches!(e.slot, Slot::Wanted | Slot::Encoded(_)))
            || self.entries.iter().any(|e| matches!(e.slot, Slot::Fetching(_)))
    }

    fn is_live(&self, e: &Entry) -> bool {
        e.used + WANT_FRAMES >= self.frame
    }

    /// Advance downloads and decode one image unless `touching`. Call
    /// once per frame, after drawing. Returns true if the screen changed.
    pub fn pump(&mut self, sys: &mut dyn System, touching: bool) -> bool {
        let mut changed = false;
        for e in &mut self.entries {
            let Slot::Fetching(id) = e.slot else { continue };
            e.slot = match sys.net().poll(id) {
                FetchState::Pending => continue,
                FetchState::Done { status: 200, body, .. } if body.len() <= MAX_BYTES => Slot::Encoded(body),
                _ => Slot::Failed,
            };
        }
        let fetching = self.entries.iter().filter(|e| matches!(e.slot, Slot::Fetching(_))).count();
        let frame = self.frame;
        let mut wanted: Vec<usize> = (0..self.entries.len())
            .filter(|&i| matches!(self.entries[i].slot, Slot::Wanted) && self.entries[i].used + WANT_FRAMES >= frame)
            .collect();
        wanted.sort_by_key(|&i| core::cmp::Reverse(self.entries[i].used));
        for i in wanted.into_iter().take(MAX_FETCHES.saturating_sub(fetching)) {
            let id = sys.net().fetch(&self.entries[i].url);
            self.entries[i].slot = Slot::Fetching(id);
        }
        if !touching {
            let next = (0..self.entries.len())
                .filter(|&i| matches!(self.entries[i].slot, Slot::Encoded(_)))
                .max_by_key(|&i| self.entries[i].used);
            if let Some(i) = next {
                let e = &mut self.entries[i];
                let Slot::Encoded(data) = core::mem::replace(&mut e.slot, Slot::Failed) else { unreachable!() };
                if let Some(img) = sys.decode_image(&data, e.w, e.h) {
                    e.slot = Slot::Ready(img);
                    changed = true;
                }
            }
        }
        self.evict();
        self.frame += 1;
        changed
    }

    /// Drop the least recently used entries beyond the limit. Downloads in
    /// progress are kept so their results are collected.
    fn evict(&mut self) {
        while self.entries.len() > MAX_ENTRIES {
            let victim = (0..self.entries.len())
                .filter(|&i| !matches!(self.entries[i].slot, Slot::Fetching(_)))
                .min_by_key(|&i| self.entries[i].used);
            match victim {
                Some(i) => {
                    self.entries.swap_remove(i);
                }
                None => break,
            }
        }
    }

    /// Stop downloads and free memory (the app closes).
    pub fn clear(&mut self, sys: &mut dyn System) {
        // Collect what can be collected; the OS frees the rest when done.
        for e in &self.entries {
            if let Slot::Fetching(id) = e.slot {
                let _ = sys.net().poll(id);
            }
        }
        self.entries.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::TestSys as Sys;
    use super::*;
    use p4os_sdk::FetchId;

    fn url(i: usize) -> String {
        alloc::format!("https://i.ytimg.com/vi/v{i}/mqdefault.jpg")
    }

    #[test]
    fn fetches_visible_thumbnails_and_decodes_one_per_frame() {
        let mut sys = Sys::default();
        let mut t = Thumbs::default();
        for i in 0..4 {
            assert!(t.get(&url(i), 160, 90).is_none());
        }
        t.pump(&mut sys, false);
        assert_eq!(sys.net.sent(), MAX_FETCHES);
        sys.net.respond(1, 200, "jpeg");
        sys.net.respond(2, 200, "jpeg");
        for i in 0..4 {
            t.get(&url(i), 160, 90);
        }
        // Touching: downloads continue but nothing is decoded.
        assert!(!t.pump(&mut sys, true));
        assert_eq!(sys.decoded, 0);
        assert_eq!(sys.net.sent(), 4);
        for i in 0..4 {
            t.get(&url(i), 160, 90);
        }
        assert!(t.pump(&mut sys, false));
        assert_eq!(sys.decoded, 1);
        for i in 0..4 {
            t.get(&url(i), 160, 90);
        }
        t.pump(&mut sys, false);
        assert_eq!(sys.decoded, 2);
        assert!(t.get(&url(0), 160, 90).is_some() && t.get(&url(1), 160, 90).is_some());
    }

    #[test]
    fn rows_scrolled_away_are_not_fetched() {
        let mut sys = Sys::default();
        let mut t = Thumbs::default();
        t.get(&url(0), 160, 90);
        t.get(&url(1), 160, 90);
        t.get(&url(2), 160, 90);
        t.pump(&mut sys, false);
        for _ in 0..5 {
            t.pump(&mut sys, false);
        }
        assert_eq!(sys.net.sent(), 2, "the third row was no longer on screen");
        assert!(t.busy(), "two downloads are still running");
    }

    #[test]
    fn failures_and_oversized_images_are_not_retried() {
        let mut sys = Sys::default();
        let mut t = Thumbs::default();
        t.get(&url(0), 160, 90);
        t.get(&url(1), 160, 90);
        t.pump(&mut sys, false);
        sys.net.respond(1, 404, "");
        sys.net.respond(2, 200, &"x".repeat(MAX_BYTES + 1));
        for _ in 0..3 {
            t.get(&url(0), 160, 90);
            t.get(&url(1), 160, 90);
            t.pump(&mut sys, false);
        }
        assert_eq!(sys.net.sent(), 2);
        assert_eq!(sys.decoded, 0);
        assert!(!t.busy());
    }

    #[test]
    fn cache_is_bounded() {
        let mut sys = Sys::default();
        let mut t = Thumbs::default();
        for i in 0..(MAX_ENTRIES * 2) {
            t.get(&url(i), 160, 90);
            t.pump(&mut sys, false);
            let id = sys.net.sent() as FetchId;
            sys.net.respond(id, 200, "jpeg");
        }
        for _ in 0..4 {
            t.pump(&mut sys, false);
        }
        assert!(t.len() <= MAX_ENTRIES + MAX_FETCHES);
        assert_eq!(sys.net.unpolled(), 0, "every download was collected");
    }
}
