//! What the home screen shows: the app catalogue split into pages.
//!
//! Apps belong to a group. The built-in apps form the main group, shown
//! first; every other group (for example "Examples") starts on its own
//! page with the group name as its title. Each group fills as many pages
//! of the fixed icon grid as it needs.

use crate::APPS;
use alloc::vec::Vec;
use p4os_sdk::{AppInfo, Rect};

/// Group of the built-in apps; its pages have no title.
pub const MAIN: &str = "";

pub const ICON: i32 = 112;
const MIN_CELL_W: i32 = 150;
const CELL_H: i32 = ICON + 56;
/// Room above the grid (the group title goes here).
pub const TOP: i32 = 40;
/// Room below the grid for the page dots.
pub const DOTS_H: i32 = 28;

#[derive(Clone, Copy)]
pub struct Entry {
    pub group: &'static str,
    pub app: &'static AppInfo,
}

pub struct Catalogue {
    entries: Vec<Entry>,
    /// Changes whenever the catalogue does (for caches).
    pub version: u32,
}

impl Catalogue {
    /// The built-in apps in the main group.
    pub fn builtin() -> Self {
        Self { entries: APPS.iter().map(|app| Entry { group: MAIN, app }).collect(), version: 0 }
    }

    /// Add `app` to `group`, or move it there if its id is already known.
    pub fn register(&mut self, group: &'static str, app: &'static AppInfo) {
        self.entries.retain(|e| e.app.id != app.id);
        self.entries.push(Entry { group, app });
        self.version += 1;
    }

    pub fn find(&self, id: &str) -> Option<&'static AppInfo> {
        self.entries.iter().find(|e| e.app.id == id).map(|e| e.app)
    }

    /// Groups in order of first appearance, the main group first.
    fn groups(&self) -> Vec<&'static str> {
        let mut out = alloc::vec![MAIN];
        for e in &self.entries {
            if !out.contains(&e.group) {
                out.push(e.group);
            }
        }
        out
    }

    pub fn pages(&self, per_page: usize) -> Vec<Page> {
        let per_page = per_page.max(1);
        let mut pages = Vec::new();
        for g in self.groups() {
            let apps: Vec<&'static AppInfo> = self.entries.iter().filter(|e| e.group == g).map(|e| e.app).collect();
            if apps.is_empty() && g != MAIN {
                continue;
            }
            if apps.is_empty() {
                pages.push(Page { group: g, apps });
                continue;
            }
            for chunk in apps.chunks(per_page) {
                pages.push(Page { group: g, apps: chunk.to_vec() });
            }
        }
        pages
    }
}

pub struct Page {
    pub group: &'static str,
    pub apps: Vec<&'static AppInfo>,
}

/// Page and slot of app `id`.
pub fn locate(pages: &[Page], id: &str) -> Option<(usize, usize)> {
    pages.iter().enumerate().find_map(|(p, page)| page.apps.iter().position(|a| a.id == id).map(|s| (p, s)))
}

/// The icon grid for a home screen area: cells at least 150 px wide with
/// full-size icons, as many rows as fit above the page dots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub cols: i32,
    pub rows: i32,
}

impl Grid {
    pub fn for_area(body: Rect) -> Self {
        let cols = (body.w / MIN_CELL_W).max(1);
        let rows = ((body.h - TOP - DOTS_H) / CELL_H).max(1);
        Self { cols, rows }
    }

    pub fn per_page(&self) -> usize {
        (self.cols * self.rows) as usize
    }

    /// Cell and icon rectangles of `slot` on a page whose left edge is at
    /// `body.x`.
    pub fn cell(&self, body: Rect, slot: usize) -> (Rect, Rect) {
        let cell_w = body.w / self.cols;
        let (col, row) = (slot as i32 % self.cols, slot as i32 / self.cols);
        let cell = Rect::new(body.x + col * cell_w, body.y + TOP + row * CELL_H, cell_w, CELL_H);
        let icon = Rect::new(cell.x + (cell.w - ICON) / 2, cell.y, ICON, ICON);
        (cell, icon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p4os_sdk::{App, Canvas, Capabilities};

    fn icon(_: &mut Canvas, _: Rect) {}
    fn create() -> alloc::boxed::Box<dyn App> {
        unreachable!()
    }
    static DEMO_A: AppInfo = AppInfo { id: "demo-a", name: "Demo A", icon, create, background: Capabilities::NONE };
    static DEMO_B: AppInfo = AppInfo { id: "demo-b", name: "Demo B", icon, create, background: Capabilities::NONE };

    #[test]
    fn grids_keep_full_size_icons() {
        // Portrait body (480x800 minus status bar and home strip).
        assert_eq!(Grid::for_area(Rect::new(0, 36, 480, 740)), Grid { cols: 3, rows: 4 });
        // Landscape body (800x480 minus the same).
        assert_eq!(Grid::for_area(Rect::new(0, 36, 800, 420)), Grid { cols: 5, rows: 2 });
    }

    #[test]
    fn builtin_apps_fill_pages_in_order() {
        let c = Catalogue::builtin();
        let one = c.pages(12);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].apps.len(), APPS.len());
        let two = c.pages(10);
        assert_eq!(two.len(), 2);
        assert_eq!(two[1].apps.len(), APPS.len() - 10);
        assert_eq!(two[1].apps[0].id, APPS[10].id);
        assert_eq!(locate(&two, APPS[11].id), Some((1, 1)));
    }

    #[test]
    fn other_groups_start_their_own_titled_page() {
        let mut c = Catalogue::builtin();
        let v = c.version;
        c.register("Examples", &DEMO_A);
        c.register("Examples", &DEMO_B);
        assert!(c.version > v);
        let pages = c.pages(12);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].group, MAIN);
        assert_eq!(pages[1].group, "Examples");
        assert_eq!(pages[1].apps.len(), 2);
        assert_eq!(c.find("demo-b").map(|a| a.name), Some("Demo B"));
        // Registering again moves rather than duplicates.
        c.register("Examples", &DEMO_A);
        assert_eq!(c.pages(12)[1].apps.len(), 2);
    }

    #[test]
    fn cells_follow_the_grid() {
        let g = Grid { cols: 3, rows: 4 };
        let body = Rect::new(0, 36, 480, 740);
        let (cell, icon) = g.cell(body, 4);
        assert_eq!(cell, Rect::new(160, 36 + TOP + 168, 160, 168));
        assert_eq!(icon, Rect::new(184, 36 + TOP + 168, ICON, ICON));
        // A page further right only moves horizontally.
        let (c2, _) = g.cell(Rect::new(480, 36, 480, 740), 4);
        assert_eq!(c2.x, cell.x + 480);
    }
}
