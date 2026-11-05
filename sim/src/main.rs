//! Run the p4os desktop on the host. Input comes from a script, output is
//! written as PNG screenshots, so UI changes can be checked without the
//! board.
//!
//! Script commands (one per argument or line):
//!   tap X Y | drag X1 Y1 X2 Y2 | type TEXT | key enter|backspace|esc|up|down
//!   wait MS | launch APP_ID | shot FILE.png | rotate (toggle orientation)
//!   swipe X1 Y1 X2 Y2 MS [hold]   (real time; `hold` rests 400 ms before lifting)
//!   down X Y | move X Y [MS] | up   (a touch in steps, e.g. for mid-gesture shots)
//!   yt play ID [144|240|audio] | yt pause | yt seek N | yt stop | yt stat
//!   config KEY VALUE (store a setting) | lock (lock the screen now)
//!   mock URL_PART FILE [STATUS]
//!
//! `mock` answers every request whose URL contains URL_PART with FILE
//! (relative paths start at the workspace root) instead of the network;
//! the most recent matching mock wins.
//!
//! The screen starts in portrait (480x800); `--landscape` starts in 800x480.
//!
//! Usage: p4os-sim [--sd DIR] [--script FILE] [COMMAND ...]
//! Commands given as arguments use `;` as separator.

mod host;
mod memtag;

#[global_allocator]
static ALLOC: memtag::Counting = memtag::Counting;

use p4os_apps::desktop::Desktop;
use p4os_apps::wm::gesture::swipe_point;
use p4os_sdk::gui::ui::{Input, Key};
use p4os_sdk::{Canvas, Color, Orientation, Point, System};
use std::time::{Duration, Instant};


/// Touch sample interval of scripted gestures (the GUI loop polls every
/// 16 ms on the board).
const SAMPLE_MS: u64 = 16;
/// Rest before lifting in `swipe ... hold`, above the switcher's 300 ms.
const HOLD_MS: u64 = 400;

struct Sim {
    desktop: Desktop,
    sys: host::HostSystem,
    fb: Vec<Color>,
    input: Input,
    next_tick: Option<Instant>,
}

impl Sim {
    fn size(&self) -> (i32, i32) {
        self.sys.orientation().size()
    }

    fn frame(&mut self) {
        let (w, h) = self.size();
        let mut canvas = Canvas::new(&mut self.fb, w, h);
        let before = self.sys.orientation();
        let next = self.desktop.frame(&mut canvas, &self.input, &mut self.sys);
        self.next_tick = next.map(|ms| Instant::now() + Duration::from_millis(ms as u64));
        // Edge-triggered fields only last one frame.
        self.input.pressed = false;
        self.input.released = false;
        self.input.delta = Point::default();
        self.input.keys.clear();
        // Run follow-up frames requested immediately (layout changes).
        if next == Some(0) || self.sys.orientation() != before {
            self.frame();
        }
    }

    fn touch(&mut self, p: Point, down: bool) {
        let was_down = self.input.down;
        if down && !was_down {
            self.input.press_pos = p;
            self.input.pressed = true;
        }
        if !down && was_down {
            self.input.released = true;
        }
        self.input.delta = Point::new(p.x - self.input.pos.x, p.y - self.input.pos.y);
        self.input.pos = p;
        self.input.down = down;
        self.input.fingers = down as u8;
        self.frame();
    }

    /// Move the finger along a path in real time, one sample per frame
    /// interval, so the gesture recogniser sees realistic speeds.
    fn glide(&mut self, from: Point, to: Point, ms: u64, hold: u64, lift: bool) {
        let start = Instant::now();
        loop {
            let el = start.elapsed().as_millis() as u64;
            match swipe_point(from, to, ms, hold, el) {
                Some(p) => self.touch(p, true),
                None => break,
            }
            std::thread::sleep(Duration::from_millis(SAMPLE_MS));
        }
        if lift {
            self.touch(to, false);
        }
    }

    fn wait(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            let now = Instant::now();
            let due = self.next_tick.filter(|t| *t <= end);
            match due {
                Some(t) => {
                    if t > now {
                        std::thread::sleep(t - now);
                    }
                    self.frame();
                }
                None => {
                    if end > now {
                        std::thread::sleep(end - now);
                    }
                    return;
                }
            }
        }
    }

    fn shot(&self, path: &str) {
        let file = std::fs::File::create(path).expect("create screenshot");
        let (w, h) = self.size();
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        let data: Vec<u8> = self.fb[..(w * h) as usize].iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect();
        writer.write_image_data(&data).unwrap();
        eprintln!("wrote {path}");
    }

    fn run(&mut self, cmd: &str) {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let num = |i: usize| -> i32 { parts.get(i).and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("bad number in `{cmd}`")) };
        match parts.first().copied() {
            None => {}
            Some("tap") => {
                let p = Point::new(num(1), num(2));
                self.touch(p, true);
                self.touch(p, false);
            }
            Some("drag") => {
                let (a, b) = (Point::new(num(1), num(2)), Point::new(num(3), num(4)));
                self.touch(a, true);
                for i in 1..=10 {
                    let p = Point::new(a.x + (b.x - a.x) * i / 10, a.y + (b.y - a.y) * i / 10);
                    self.touch(p, true);
                }
                self.touch(b, false);
            }
            Some("type") => {
                let text = cmd.trim_start()[4..].trim_start();
                for ch in text.chars() {
                    self.input.keys.push(Key::Char(ch));
                    self.frame();
                }
            }
            Some("key") => {
                let k = match parts.get(1).copied() {
                    Some("enter") => Key::Enter,
                    Some("backspace") => Key::Backspace,
                    Some("esc") => Key::Escape,
                    Some("up") => Key::Up,
                    Some("down") => Key::Down,
                    Some("left") => Key::Left,
                    Some("right") => Key::Right,
                    other => panic!("unknown key {other:?}"),
                };
                self.input.keys.push(k);
                self.frame();
            }
            Some("swipe") => {
                let (a, b) = (Point::new(num(1), num(2)), Point::new(num(3), num(4)));
                let hold = if parts.get(6) == Some(&"hold") { HOLD_MS } else { 0 };
                self.touch(a, true);
                self.glide(a, b, num(5) as u64, hold, true);
            }
            Some("down") => self.touch(Point::new(num(1), num(2)), true),
            Some("move") => {
                let to = Point::new(num(1), num(2));
                let ms = parts.get(3).map_or(160, |_| num(3) as u64);
                self.glide(self.input.pos, to, ms, 0, false);
            }
            Some("up") => {
                let p = self.input.pos;
                self.touch(p, false);
            }
            Some("wait") => self.wait(num(1) as u64),
            Some("launch") => {
                self.desktop.launch(parts[1], &mut self.sys);
                self.frame();
            }
            Some("shot") => self.shot(parts[1]),
            Some("yt") if parts.get(1) == Some(&"stat") => {
                eprintln!("{}", p4os_ytplay::remote::stats().unwrap_or_else(|| "yt: no player yet".into()));
            }
            Some("yt") => {
                let c = p4os_ytplay::remote::parse_command(&cmd.trim_start()[2..]).unwrap_or_else(|e| panic!("`{cmd}`: {e}"));
                assert!(p4os_ytplay::remote::post(c), "player command queue full");
                self.frame();
            }
            Some("config") if parts.len() >= 3 => {
                self.sys.config_set(parts[1], &parts[2..].join(" "));
                self.frame();
            }
            Some("lock") => {
                self.desktop.lock_now(&mut self.sys);
                self.frame();
            }
            Some("mock") => {
                let file = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).join(parts[2]);
                let status = parts.get(3).map_or(200, |_| num(3) as u16);
                self.sys.mock(parts[1], status, file);
            }
            Some("rotate") => {
                let o = self.sys.orientation().toggled();
                self.sys.set_orientation(o);
                self.frame();
            }
            Some(other) => panic!("unknown command `{other}`"),
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut orientation = Orientation::Portrait;
    let mut sd = String::from(concat!(env!("CARGO_MANIFEST_DIR"), "/sdcard"));
    let mut script = String::new();
    while let Some(a) = args.first().cloned() {
        match a.as_str() {
            "--sd" => {
                sd = args[1].clone();
                args.drain(..2);
            }
            "--landscape" => {
                orientation = Orientation::Landscape;
                args.remove(0);
            }
            "--script" => {
                script = std::fs::read_to_string(&args[1]).expect("read script");
                args.drain(..2);
            }
            _ => break,
        }
    }
    script.push('\n');
    script.push_str(&args.join(" ").replace(';', "\n"));

    let mut sys = host::HostSystem::new(sd.into());
    sys.orientation = orientation;
    let mut sim = Sim {
        desktop: Desktop::new(),
        sys,
        // Large enough for either orientation.
        fb: vec![Color::BLACK; 480 * 800],
        input: Input::default(),
        next_tick: None,
    };
    sim.frame();
    for line in script.lines() {
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            sim.run(line);
        }
    }
}
