//! System monitor: uptime, CPU load history, memory usage and the kernel
//! thread table, refreshed once a second from [`System::stats`].

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, Point, Rect, System, SystemStats, Ui};

pub const INFO: AppInfo = AppInfo { id: "monitor", name: "Monitor", icon, create };

/// Samples kept for the load graph (one per second).
const HISTORY_LEN: usize = 60;
const SAMPLE_MS: u64 = 1000;
/// Placeholder for unknown values (the bundled fonts have no em dash).
const DASH: &str = "--";

fn create() -> Box<dyn App> {
    Box::new(Monitor::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x26de81), Color::hex(0x0f8a5f));
    let s = r.w as f32 / 64.0;
    let p = |x: f32, y: f32| Point::new(r.x + (x * s) as i32, r.y + (y * s) as i32);
    let pts = [p(12.0, 38.0), p(22.0, 38.0), p(28.0, 22.0), p(36.0, 46.0), p(42.0, 32.0), p(52.0, 32.0)];
    for w in pts.windows(2) {
        c.line(w[0], w[1], 4.0 * s, Color::WHITE);
    }
}

/// Fixed-length rolling window of CPU load samples in 0..=1.
struct LoadHistory {
    samples: VecDeque<f32>,
    cap: usize,
}

impl LoadHistory {
    fn new(cap: usize) -> Self {
        Self { samples: VecDeque::with_capacity(cap), cap }
    }

    fn push(&mut self, load: f32) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(load.clamp(0.0, 1.0));
    }

    fn average(&self) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        Some(self.samples.iter().sum::<f32>() / self.samples.len() as f32)
    }
}

struct Monitor {
    stats: SystemStats,
    history: LoadHistory,
    /// Uptime of the last sample; `None` before the first one.
    last_sample: Option<u64>,
    table_offset: i32,
}

impl Default for Monitor {
    fn default() -> Self {
        Self {
            stats: SystemStats::default(),
            history: LoadHistory::new(HISTORY_LEN),
            last_sample: None,
            table_offset: 0,
        }
    }
}

/// The kernel reports per-thread data whenever it measures load, so an
/// empty thread list means the platform has no load figures at all.
fn has_load(s: &SystemStats) -> bool {
    !s.threads.is_empty()
}

/// "3d 04:05:06", "04:05:06" or a dash when unknown.
fn fmt_uptime(ms: u64) -> String {
    if ms == 0 {
        return DASH.to_string();
    }
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86_400, s / 3600 % 24, s / 60 % 60, s % 60);
    if d > 0 {
        format!("{d}d {h:02}:{m:02}:{sec:02}")
    } else {
        format!("{h:02}:{m:02}:{sec:02}")
    }
}

/// Human-readable byte count with one decimal above 1 KiB.
fn fmt_bytes(n: usize) -> String {
    const K: f32 = 1024.0;
    let f = n as f32;
    if f < K {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KB", f / K)
    } else {
        format!("{:.1} MB", f / (K * K))
    }
}

/// "used / total" or a dash when the total is unknown.
fn fmt_usage(used: usize, total: usize) -> String {
    if total == 0 {
        DASH.to_string()
    } else {
        format!("{} / {}", fmt_bytes(used), fmt_bytes(total))
    }
}

fn ratio(used: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f32 / total as f32).clamp(0.0, 1.0)
    }
}

impl Monitor {
    /// Take a sample when a second has passed since the previous one.
    fn sample(&mut self, sys: &mut dyn System) {
        let now = sys.uptime_ms();
        if self.last_sample.is_some_and(|t| now.saturating_sub(t) < SAMPLE_MS) {
            return;
        }
        self.last_sample = Some(now);
        self.stats = sys.stats();
        if has_load(&self.stats) {
            self.history.push(self.stats.cpu_load);
        }
    }

    fn card(ui: &mut Ui, r: Rect, label: &str, value: &str) {
        let t = *ui.theme;
        ui.panel(r);
        let inner = r.inset(14);
        let (lab_r, val_r) = inner.split_top(20);
        let (small, large) = (ui.fonts.small, ui.fonts.large);
        ui.text(lab_r, label, &small, t.text_dim, Align::Left);
        ui.text(val_r, value, &large, t.text, Align::Left);
    }

    fn draw_graph(&self, ui: &mut Ui, r: Rect) {
        let t = *ui.theme;
        ui.panel(r);
        let inner = r.inset(14);
        let (head, plot) = inner.split_top(28);
        let (body, small) = (ui.fonts.body, ui.fonts.small);
        ui.text(head, "CPU load (last 60 s)", &body, t.text, Align::Left);
        let avg = match self.history.average() {
            Some(a) => format!("avg {:.0}%", a * 100.0),
            None => DASH.to_string(),
        };
        ui.text(head, &avg, &small, t.text_dim, Align::Right);

        // Leave room on the left for the axis labels.
        let plot = Rect::new(plot.x + 52, plot.y + 10, plot.w - 52, plot.h - 20);
        for pct in [0, 50, 100] {
            let y = plot.bottom() - plot.h * pct / 100;
            ui.canvas.fill_rect(Rect::new(plot.x, y, plot.w, 1), t.border);
            let lab = Rect::new(inner.x, y - 10, 44, 20);
            ui.text(lab, &format!("{pct}%"), &small, t.text_dim, Align::Right);
        }

        let n = self.history.samples.len();
        if n == 0 {
            ui.text(plot, "No load data from this platform", &body, t.text_dim, Align::Center);
            return;
        }
        // Newest sample at the right edge; one slot per possible sample.
        let step = plot.w as f32 / (HISTORY_LEN - 1) as f32;
        let start = HISTORY_LEN - n;
        let pt = |i: usize, v: f32| {
            let x = plot.x + ((start + i) as f32 * step) as i32;
            let y = plot.bottom() - (v * plot.h as f32) as i32;
            Point::new(x, y)
        };
        let fill = t.accent.blend(t.surface, 70);
        let pts: alloc::vec::Vec<Point> = self.history.samples.iter().enumerate().map(|(i, &v)| pt(i, v)).collect();
        // Area under the curve, one pixel column at a time.
        let base = plot.bottom();
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let span = (b.x - a.x).max(1);
            for x in a.x..b.x {
                let y = a.y + (b.y - a.y) * (x - a.x) / span;
                ui.canvas.fill_rect(Rect::new(x, y, 1, base - y), fill);
            }
        }
        for w in pts.windows(2) {
            ui.canvas.line(w[0], w[1], 2.5, t.accent);
        }
        if let Some(&last) = pts.last() {
            ui.canvas.fill_circle(last, 4.0, t.accent);
        }
    }

    fn draw_memory(&self, ui: &mut Ui, r: Rect) {
        let t = *ui.theme;
        ui.panel(r);
        let inner = r.inset(14);
        let (head, rest) = inner.split_top(28);
        let (body, small) = (ui.fonts.body, ui.fonts.small);
        ui.text(head, "Memory", &body, t.text, Align::Left);
        let s = &self.stats;
        let rows = [("Heap", s.heap_used, s.heap_total), ("PSRAM", s.psram_used, s.psram_total)];
        for (i, (name, used, total)) in rows.into_iter().enumerate() {
            let row = Rect::new(rest.x, rest.y + 10 + i as i32 * 66, rest.w, 60);
            let (lab, bar) = row.split_top(28);
            ui.text(lab, name, &body, t.text, Align::Left);
            ui.text(lab, &fmt_usage(used, total), &small, t.text_dim, Align::Right);
            ui.progress(Rect::new(bar.x, bar.y + 6, bar.w, 14), ratio(used, total));
        }
    }

    fn draw_threads(&mut self, ui: &mut Ui, r: Rect) {
        let t = *ui.theme;
        ui.panel(r);
        let inner = r.inset(14);
        let (head, list) = inner.split_top(30);
        let (body, small) = (ui.fonts.body, ui.fonts.small);

        // Column x positions as a share of the table width.
        let cols = [0, 26, 36, 52, 64].map(|pct| inner.w * pct / 100);
        let titles = ["Thread", "Prio", "State", "CPU ms", "Stack used / size"];
        for (i, title) in titles.iter().enumerate() {
            let w = cols.get(i + 1).copied().unwrap_or(inner.w) - cols[i];
            ui.text(Rect::new(head.x + cols[i], head.y, w, head.h), title, &small, t.text_dim, Align::Left);
        }
        ui.canvas.fill_rect(Rect::new(head.x, head.bottom() - 1, head.w, 1), t.border);

        let threads = &self.stats.threads;
        if threads.is_empty() {
            ui.text(list, "No thread data from this platform", &body, t.text_dim, Align::Center);
            return;
        }
        let row_h = 34;
        let content_h = threads.len() as i32 * row_h;
        let off = ui.scroll(list, content_h, &mut self.table_offset);
        let old = ui.canvas.set_clip(list.intersect(&ui.canvas.clip()));
        for (i, th) in threads.iter().enumerate() {
            let y = list.y + i as i32 * row_h - off;
            if y + row_h < list.y {
                continue;
            }
            if y > list.bottom() {
                break;
            }
            let cell = |x: i32, w: i32| Rect::new(list.x + x, y, w, row_h);
            let name = if th.name.is_empty() { DASH } else { th.name.as_str() };
            let width = |i: usize| cols[i + 1] - cols[i] - 8;
            ui.text(cell(cols[0], width(0)), name, &body, t.text, Align::Left);
            ui.text(cell(cols[1], width(1)), &format!("{}", th.priority), &body, t.text, Align::Left);
            let state = if th.state.is_empty() { DASH } else { th.state.as_str() };
            ui.text(cell(cols[2], width(2)), state, &body, t.text_dim, Align::Left);
            ui.text(cell(cols[3], width(3)), &format!("{}", th.cpu_ms), &body, t.text, Align::Left);

            let stack = cell(cols[4], list.w - cols[4] - 12);
            let bar = Rect::new(stack.x, y + (row_h - 10) / 2, (stack.w / 4).min(120), 10);
            let frac = ratio(th.stack_used as usize, th.stack_size as usize);
            ui.progress(bar, frac);
            if frac > 0.85 {
                // Nearly exhausted stacks are worth a warning colour.
                let w = (bar.w as f32 * frac) as i32;
                ui.canvas.fill_round_rect(Rect::new(bar.x, bar.y, w, bar.h), bar.h / 2, t.danger);
            }
            let txt = if th.stack_size == 0 {
                DASH.to_string()
            } else {
                format!("{} / {}", fmt_bytes(th.stack_used as usize), fmt_bytes(th.stack_size as usize))
            };
            ui.text(Rect::new(bar.right() + 8, y, stack.right() - bar.right() - 8, row_h), &txt, &small, t.text_dim, Align::Left);
            if i + 1 < threads.len() {
                ui.canvas.fill_rect(Rect::new(list.x, y + row_h - 1, list.w - 12, 1), t.border);
            }
        }
        ui.canvas.restore_clip(old);
    }
}

impl App for Monitor {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        self.sample(sys);
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let area = area.inset(12);
        let gap = 12;

        let portrait = area.h > area.w;

        // Headline numbers: a 2x2 grid in portrait, one row in landscape.
        let (card_cols, card_rows) = if portrait { (2, 2) } else { (4, 1) };
        let (top, rest) = area.split_top(76 * card_rows + gap * (card_rows - 1));
        let s = &self.stats;
        let load = if has_load(s) { format!("{:.0}%", s.cpu_load * 100.0) } else { DASH.to_string() };
        let mhz = if s.cpu_mhz == 0 { DASH.to_string() } else { format!("{} MHz", s.cpu_mhz) };
        let threads = if s.threads.is_empty() { DASH.to_string() } else { format!("{}", s.threads.len()) };
        let cards = [("Uptime", fmt_uptime(s.uptime_ms)), ("CPU clock", mhz), ("CPU load", load), ("Threads", threads)];
        for (i, (label, value)) in cards.iter().enumerate() {
            let (col, row) = (i as i32 % card_cols, i as i32 / card_cols);
            Self::card(ui, top.grid_cell(card_cols, card_rows, gap, col, row), label, value);
        }

        // Load graph and memory: stacked in portrait, side by side otherwise.
        let rest = Rect::new(rest.x, rest.y + gap, rest.w, rest.h - gap);
        let bottom = if portrait {
            let (graph, rest) = rest.split_top(240);
            self.draw_graph(ui, graph);
            let (mem, rest) = Rect::new(rest.x, rest.y + gap, rest.w, rest.h - gap).split_top(170);
            self.draw_memory(ui, mem);
            rest
        } else {
            let (mid, rest) = rest.split_top(180);
            let (graph, mem) = mid.split_left(mid.w * 3 / 5);
            self.draw_graph(ui, graph);
            self.draw_memory(ui, Rect::new(mem.x + gap, mem.y, mem.w - gap, mem.h));
            rest
        };

        let bottom = Rect::new(bottom.x, bottom.y + gap, bottom.w, bottom.h - gap);
        if bottom.h > 80 {
            self.draw_threads(ui, bottom);
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        Some(SAMPLE_MS as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_rolls_and_clamps() {
        let mut h = LoadHistory::new(3);
        assert_eq!(h.average(), None);
        for v in [0.1, 0.2, 0.3, 1.5] {
            h.push(v);
        }
        let v: alloc::vec::Vec<f32> = h.samples.iter().copied().collect();
        assert_eq!(v, [0.2, 0.3, 1.0]);
        assert!((h.average().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn uptime_formatting() {
        assert_eq!(fmt_uptime(0), DASH);
        assert_eq!(fmt_uptime(61_000), "00:01:01");
        assert_eq!(fmt_uptime(3 * 86_400_000 + 4 * 3_600_000 + 5_000), "3d 04:00:05");
    }

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(32 * 1024 * 1024), "32.0 MB");
        assert_eq!(fmt_usage(10, 0), DASH);
        assert_eq!(fmt_usage(1024, 2048), "1.0 KB / 2.0 KB");
    }

    #[test]
    fn ratio_handles_unknown_total() {
        assert_eq!(ratio(5, 0), 0.0);
        assert_eq!(ratio(5, 10), 0.5);
        assert_eq!(ratio(20, 10), 1.0);
    }
}
