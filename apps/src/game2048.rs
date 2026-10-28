//! 2048: slide numbered tiles on a 4x4 grid; equal tiles merge. Swipe on
//! the board or use the arrow keys. The best score is kept on the SD card in
//! `/sd/2048-best.txt` when the card is present.

use alloc::boxed::Box;
use alloc::format;
use p4os_sdk::gui::font::Font;
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "game2048", name: "2048", icon, create };

const BEST_PATH: &str = "/sd/2048-best.txt";
/// Minimum swipe length in pixels before it counts as a move.
const SWIPE_MIN: i32 = 30;

fn create() -> Box<dyn App> {
    Box::new(Game2048::default())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xbbada0), Color::hex(0x8f7a66));
    let inner = r.inset(r.w / 6);
    let colors = [0xeee4da, 0xf2b179, 0xf67c5f, 0xedc22e];
    for (i, col) in colors.iter().enumerate() {
        let cell = inner.grid_cell(2, 2, r.w / 14, i as i32 % 2, i as i32 / 2);
        c.fill_round_rect(cell, cell.w / 5, Color::hex(*col));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

pub type Board = [[u32; 4]; 4];

/// Slide one row towards index 0, merging equal neighbours once per move.
/// Returns the new row and the points gained (sum of merged tiles).
pub fn slide_row(row: [u32; 4]) -> ([u32; 4], u32) {
    let mut out = [0u32; 4];
    let mut n = 0;
    let mut score = 0;
    // `out[n - 1]` may still absorb the next equal tile.
    let mut can_merge = false;
    for v in row.into_iter().filter(|&v| v != 0) {
        if can_merge && out[n - 1] == v {
            out[n - 1] *= 2;
            score += out[n - 1];
            can_merge = false;
        } else {
            out[n] = v;
            n += 1;
            can_merge = true;
        }
    }
    (out, score)
}

/// Apply a move to the whole board. Returns (board, points, changed).
pub fn apply_move(b: &Board, dir: Dir) -> (Board, u32, bool) {
    let mut out = *b;
    let mut score = 0;
    for i in 0..4 {
        // Read the line in the direction tiles move towards.
        let idx = |k: usize| -> (usize, usize) {
            match dir {
                Dir::Left => (i, k),
                Dir::Right => (i, 3 - k),
                Dir::Up => (k, i),
                Dir::Down => (3 - k, i),
            }
        };
        let line = [0, 1, 2, 3].map(|k| {
            let (r, c) = idx(k);
            b[r][c]
        });
        let (slid, s) = slide_row(line);
        score += s;
        for (k, v) in slid.into_iter().enumerate() {
            let (r, c) = idx(k);
            out[r][c] = v;
        }
    }
    let changed = out != *b;
    (out, score, changed)
}

pub fn can_move(b: &Board) -> bool {
    [Dir::Left, Dir::Right, Dir::Up, Dir::Down].iter().any(|&d| apply_move(b, d).2)
}

/// Xorshift32 pseudo-random generator.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u64) -> Self {
        let s = (seed ^ (seed >> 32)) as u32;
        Rng(if s == 0 { 0x9e37_79b9 } else { s })
    }

    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// Put a 2 (90%) or 4 (10%) on a random empty cell.
fn spawn(b: &mut Board, rng: &mut Rng) {
    let empty: alloc::vec::Vec<(usize, usize)> =
        (0..16).map(|i| (i / 4, i % 4)).filter(|&(r, c)| b[r][c] == 0).collect();
    if empty.is_empty() {
        return;
    }
    let (r, c) = empty[rng.next() as usize % empty.len()];
    b[r][c] = if rng.next() % 10 == 0 { 4 } else { 2 };
}

#[derive(Default)]
struct Game2048 {
    /// Created on the first frame, when the uptime is available for seeding.
    rng: Option<Rng>,
    board: Board,
    score: u32,
    best: u32,
    /// Best score last written to the SD card.
    saved_best: u32,
    won: bool,
    /// The player chose to continue after reaching 2048.
    keep_going: bool,
    over: bool,
}

impl Game2048 {
    fn new_game(&mut self) {
        let rng = self.rng.as_mut().expect("rng seeded");
        self.board = [[0; 4]; 4];
        spawn(&mut self.board, rng);
        spawn(&mut self.board, rng);
        self.score = 0;
        self.won = false;
        self.keep_going = false;
        self.over = false;
    }

    fn step(&mut self, dir: Dir, sys: &mut dyn System) {
        if self.over || (self.won && !self.keep_going) {
            return;
        }
        let (b, pts, changed) = apply_move(&self.board, dir);
        if !changed {
            return;
        }
        self.board = b;
        self.score += pts;
        self.best = self.best.max(self.score);
        spawn(&mut self.board, self.rng.as_mut().unwrap());
        if !self.keep_going && self.board.iter().flatten().any(|&v| v >= 2048) {
            self.won = true;
            self.save_best(sys);
        }
        if !can_move(&self.board) {
            self.over = true;
            self.save_best(sys);
        }
    }

    fn load_best(&mut self, sys: &mut dyn System) {
        if let Ok(data) = sys.fs().read(BEST_PATH) {
            if let Some(v) = core::str::from_utf8(&data).ok().and_then(|s| s.trim().parse().ok()) {
                self.best = v;
                self.saved_best = v;
            }
        }
    }

    /// Persist the best score if it improved; a missing card is ignored.
    fn save_best(&mut self, sys: &mut dyn System) {
        if self.best > self.saved_best && sys.fs().write(BEST_PATH, format!("{}\n", self.best).as_bytes()).is_ok() {
            self.saved_best = self.best;
        }
    }

    fn swipe_dir(ui: &Ui, board_r: Rect) -> Option<Dir> {
        let i = ui.input;
        if !(i.released && board_r.contains(i.press_pos)) {
            return None;
        }
        let (dx, dy) = (i.pos.x - i.press_pos.x, i.pos.y - i.press_pos.y);
        if dx.abs().max(dy.abs()) < SWIPE_MIN {
            return None;
        }
        Some(if dx.abs() > dy.abs() {
            if dx > 0 { Dir::Right } else { Dir::Left }
        } else if dy > 0 {
            Dir::Down
        } else {
            Dir::Up
        })
    }

    fn draw_board(&self, ui: &mut Ui, r: Rect) {
        ui.canvas.fill_round_rect(r, 14, Color::hex(0xbbada0));
        let grid = r.inset(12);
        for row in 0..4 {
            for col in 0..4 {
                let cell = grid.grid_cell(4, 4, 12, col, row);
                let v = self.board[row as usize][col as usize];
                let (bg, fg) = tile_colors(v);
                ui.canvas.fill_round_rect(cell, 8, bg);
                if v != 0 {
                    let text = format!("{v}");
                    let f = fit_font(ui, &text, cell.w - 12);
                    ui.text(cell, &text, &f, fg, Align::Center);
                }
            }
        }
    }

    /// Translucent overlay over the board with a message and one button.
    fn overlay(&mut self, ui: &mut Ui, r: Rect, title: &str, button: &str) -> bool {
        // Inset so the square overlay stays inside the board's rounded corners.
        ui.canvas.fill_rect_alpha(r.inset(6), Color::hex(0xeee4da), 225);
        let f = ui.fonts.huge;
        ui.text(Rect::new(r.x, r.y + r.h / 2 - 90, r.w, 70), title, &f, Color::hex(0x776e65), Align::Center);
        let b = Rect::new(r.x + (r.w - 220) / 2, r.y + r.h / 2 + 10, 220, 60);
        ui.button_colored(b, button, Color::hex(0x8f7a66), Color::WHITE)
    }
}

/// Screen regions, derived from the app area: a stacked layout in portrait
/// and board-left / panel-right in landscape.
struct Layout {
    board: Rect,
    title: Rect,
    scores: [Rect; 2],
    new_game: Rect,
    hint: Rect,
}

impl Layout {
    fn new(area: Rect) -> Self {
        let pad = 24;
        if area.h >= area.w {
            let inner = area.inset(pad);
            let (head, rest) = inner.split_top(96);
            let boxes = Rect::new(head.right() - 312, head.y, 312, 96);
            let new_game = Rect::new(inner.x, head.bottom() + 20, inner.w, 64);
            let below = new_game.bottom() + 24;
            let side = inner.w.min(rest.bottom() - below - 60).max(0);
            let board = Rect::new(inner.x + (inner.w - side) / 2, below, side, side);
            Layout {
                board,
                title: Rect::new(head.x, head.y, head.w - boxes.w, head.h),
                scores: [boxes.grid_cell(2, 1, 16, 0, 0), boxes.grid_cell(2, 1, 16, 1, 0)],
                new_game,
                hint: Rect::new(inner.x, board.bottom() + 20, inner.w, inner.bottom() - board.bottom() - 20),
            }
        } else {
            let side = (area.h - 2 * pad).min(area.w * 11 / 20);
            let board = Rect::new(area.x + pad * 2, area.y + (area.h - side) / 2, side, side);
            let panel = Rect::new(board.right() + 2 * pad, board.y, area.right() - board.right() - 4 * pad, side);
            let boxes = Rect::new(panel.x, panel.y + 90, panel.w, 96);
            let new_game = Rect::new(panel.x, boxes.bottom() + 24, panel.w, 64);
            Layout {
                board,
                title: Rect::new(panel.x, panel.y, panel.w, 70),
                scores: [boxes.grid_cell(2, 1, 16, 0, 0), boxes.grid_cell(2, 1, 16, 1, 0)],
                new_game,
                hint: Rect::new(panel.x, new_game.bottom() + 28, panel.w, panel.bottom() - new_game.bottom() - 28),
            }
        }
    }
}

/// Largest font in which `text` fits into `width`.
fn fit_font(ui: &Ui, text: &str, width: i32) -> Font {
    let f = ui.fonts;
    *[&f.huge, &f.title, &f.bold, &f.body].into_iter().find(|f| f.measure(text) <= width).unwrap_or(&f.small)
}

/// Background and text colour of a tile, after the original game.
fn tile_colors(v: u32) -> (Color, Color) {
    let dark = Color::hex(0x776e65);
    let light = Color::hex(0xf9f6f2);
    match v {
        0 => (Color::hex(0xcdc1b4), dark),
        2 => (Color::hex(0xeee4da), dark),
        4 => (Color::hex(0xede0c8), dark),
        8 => (Color::hex(0xf2b179), light),
        16 => (Color::hex(0xf59563), light),
        32 => (Color::hex(0xf67c5f), light),
        64 => (Color::hex(0xf65e3b), light),
        128 => (Color::hex(0xedcf72), light),
        256 => (Color::hex(0xedcc61), light),
        512 => (Color::hex(0xedc850), light),
        1024 => (Color::hex(0xedc53f), light),
        2048 => (Color::hex(0xedc22e), light),
        _ => (Color::hex(0x3c3a32), light),
    }
}

impl App for Game2048 {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if self.rng.is_none() {
            self.rng = Some(Rng::new(sys.uptime_ms().wrapping_mul(0x9e37_79b9_7f4a_7c15)));
            self.load_best(sys);
            self.new_game();
        }
        let t = *ui.theme;
        ui.fill(area, t.bg);

        let l = Layout::new(area);
        let board_r = l.board;

        // Moves: arrow keys or a swipe that started on the board.
        let mut dir = Self::swipe_dir(ui, board_r);
        for k in &ui.input.keys {
            match k {
                Key::Left => dir = Some(Dir::Left),
                Key::Right => dir = Some(Dir::Right),
                Key::Up => dir = Some(Dir::Up),
                Key::Down => dir = Some(Dir::Down),
                _ => {}
            }
        }
        if let Some(d) = dir {
            self.step(d, sys);
        }

        self.draw_board(ui, board_r);
        if self.over {
            if self.overlay(ui, board_r, "Game over!", "Try again") {
                self.new_game();
            }
        } else if self.won && !self.keep_going {
            if self.overlay(ui, board_r, "You win!", "Keep going") {
                self.keep_going = true;
            }
        }

        // Title, scores, new game, hint.
        let huge = ui.fonts.huge;
        ui.text(l.title, "2048", &huge, t.text, Align::Left);
        for (b, (label, value)) in l.scores.into_iter().zip([("SCORE", self.score), ("BEST", self.best)]) {
            ui.panel(b);
            let (small, title) = (ui.fonts.body, ui.fonts.title);
            ui.text(Rect::new(b.x, b.y + 8, b.w, 26), label, &small, t.text_dim, Align::Center);
            ui.text(Rect::new(b.x, b.y + 36, b.w, 48), &format!("{value}"), &title, t.text, Align::Center);
        }
        if ui.button_primary(l.new_game, "New game") {
            self.save_best(sys);
            self.new_game();
        }
        let body = ui.fonts.body;
        let hint = "Swipe on the board or use the arrow keys. Tiles with the same number merge into one. Reach 2048 to win!";
        let mut y = l.hint.y;
        for line in body.wrap(hint, l.hint.w) {
            if y + body.line_height > l.hint.bottom() {
                break;
            }
            ui.text(Rect::new(l.hint.x, y, l.hint.w, body.line_height), &line, &body, t.text_dim, Align::Left);
            y += body.line_height + 4;
        }
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        self.save_best(sys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_pairs_once() {
        assert_eq!(slide_row([2, 2, 2, 2]), ([4, 4, 0, 0], 8));
        assert_eq!(slide_row([2, 2, 4, 0]), ([4, 4, 0, 0], 4));
        assert_eq!(slide_row([4, 4, 8, 0]), ([8, 8, 0, 0], 8));
        assert_eq!(slide_row([0, 2, 0, 2]), ([4, 0, 0, 0], 4));
        assert_eq!(slide_row([2, 0, 0, 4]), ([2, 4, 0, 0], 0));
        assert_eq!(slide_row([8, 4, 4, 4]), ([8, 8, 4, 0], 8));
    }

    #[test]
    fn moves_in_all_directions() {
        let b: Board = [[2, 0, 0, 2], [0, 0, 0, 0], [0, 4, 0, 0], [0, 4, 0, 0]];
        let (l, s, ch) = apply_move(&b, Dir::Left);
        assert!(ch);
        assert_eq!(s, 4);
        assert_eq!(l[0], [4, 0, 0, 0]);
        let (r, _, _) = apply_move(&b, Dir::Right);
        assert_eq!(r[0], [0, 0, 0, 4]);
        let (d, s, _) = apply_move(&b, Dir::Down);
        assert_eq!(s, 8);
        assert_eq!(d[3], [2, 8, 0, 2]);
        let (u, _, _) = apply_move(&b, Dir::Up);
        assert_eq!(u[0], [2, 8, 0, 2]);
    }

    #[test]
    fn unchanged_move_and_game_over() {
        let b: Board = [[2, 4, 2, 4], [4, 2, 4, 2], [2, 4, 2, 4], [4, 2, 4, 2]];
        assert!(!apply_move(&b, Dir::Left).2);
        assert!(!can_move(&b));
        let mut b2 = b;
        b2[0][0] = 4;
        assert!(can_move(&b2));
    }

    #[test]
    fn rng_is_deterministic_and_spawns() {
        let (mut a, mut b) = (Rng::new(42), Rng::new(42));
        assert_eq!(a.next(), b.next());
        let mut board = [[0; 4]; 4];
        spawn(&mut board, &mut a);
        let tiles: alloc::vec::Vec<u32> = board.iter().flatten().copied().filter(|&v| v != 0).collect();
        assert_eq!(tiles.len(), 1);
        assert!(tiles[0] == 2 || tiles[0] == 4);
    }
}
