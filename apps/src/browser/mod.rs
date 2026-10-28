//! Browser: a text-mode web browser for http:// and https:// pages.
//!
//! Pages are fetched through `System::net`, tokenized by [`html`] and laid
//! out into styled lines by [`layout`]. Links are tappable, images become
//! "[image: alt]" placeholders, and there is a back/forward history that
//! remembers the scroll position of each entry.

mod html;
mod layout;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use layout::{layout_html, layout_plain, Page, RunKind};
use p4os_sdk::gui::ui::{Align, Key};
use p4os_sdk::{App, AppInfo, Canvas, Color, FetchId, FetchState, Point, Rect, System, Ui};

pub const INFO: AppInfo = AppInfo { id: "browser", name: "Browser", icon, create };

fn create() -> Box<dyn App> {
    Box::new(Browser::new())
}

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0x48c6ef), Color::hex(0x2463c9));
    // A globe: outline, equator, meridian and two curved meridians.
    let ctr = r.center();
    let rad = r.w as f32 * 0.3;
    let w = (r.w as f32 / 28.0).max(2.0);
    c.stroke_circle(ctr, rad, w, Color::WHITE);
    let ri = rad as i32;
    c.line(Point::new(ctr.x - ri, ctr.y), Point::new(ctr.x + ri, ctr.y), w, Color::WHITE);
    c.line(Point::new(ctr.x, ctr.y - ri), Point::new(ctr.x, ctr.y + ri), w, Color::WHITE);
    for lat in [-0.5f32, 0.5] {
        let y = ctr.y + (rad * lat) as i32;
        let half = (rad * libm::sqrtf(1.0 - lat * lat)) as i32;
        c.line(Point::new(ctr.x - half, y), Point::new(ctr.x + half, y), w * 0.7, Color::WHITE);
    }
    // Ellipse for the curved meridians.
    let steps = 24;
    let pt = |i: i32| {
        let a = i as f32 * core::f32::consts::TAU / steps as f32;
        Point::new(ctr.x + (libm::sinf(a) * rad * 0.45) as i32, ctr.y + (libm::cosf(a) * rad) as i32)
    };
    for i in 0..steps {
        c.line(pt(i), pt(i + 1), w * 0.7, Color::WHITE);
    }
}

// ---------------------------------------------------------------------------
// URLs

const HOME_URL: &str = "about:home";

fn strip_fragment(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

fn fragment(url: &str) -> Option<&str> {
    url.split_once('#').map(|(_, f)| f).filter(|f| !f.is_empty())
}

/// True when `s` starts with `scheme:` (letters, then letters, digits, +-.).
fn has_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else { return false };
    let scheme = &s[..colon];
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Split "scheme://host:port/path?q" into ("scheme://host:port", "/path?q").
fn split_origin(url: &str) -> (&str, &str) {
    let Some(p) = url.find("://") else { return ("", url) };
    let host_end = url[p + 3..].find(['/', '?', '#']).map_or(url.len(), |i| i + p + 3);
    (&url[..host_end], &url[host_end..])
}

/// Host part of an absolute URL, for display.
fn host_of(url: &str) -> &str {
    let (origin, _) = split_origin(url);
    origin.split_once("://").map_or(url, |(_, h)| h)
}

/// RFC 3986 dot-segment removal; keeps any query string untouched.
fn normalize_path(p: &str) -> String {
    let (path, query) = match p.find('?') {
        Some(i) => (&p[..i], &p[i..]),
        None => (p, ""),
    };
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        match *seg {
            "." => {}
            ".." => {
                out.pop();
            }
            s => {
                out.push(s);
                continue;
            }
        }
        if last {
            out.push("");
        }
    }
    format!("/{}{}", out.join("/"), query)
}

/// Resolve `href` relative to the absolute URL `base`.
pub fn resolve_url(base: &str, href: &str) -> String {
    let href: String = href.trim().chars().filter(|c| !matches!(c, '\n' | '\r' | '\t')).collect();
    let base = strip_fragment(base);
    if href.is_empty() {
        return base.to_string();
    }
    if has_scheme(&href) {
        return href;
    }
    if let Some(rest) = href.strip_prefix("//") {
        let scheme = base.split_once("://").map_or("http", |(s, _)| s);
        return format!("{scheme}://{rest}");
    }
    if href.starts_with('#') {
        return format!("{base}{href}");
    }
    let (origin, path) = split_origin(base);
    let path = if path.is_empty() || path.starts_with('?') { "/" } else { path };
    let path_only = path.split('?').next().unwrap_or(path);
    // Keep the fragment of href out of dot-segment processing.
    let (href_main, frag) = match href.find('#') {
        Some(i) => (&href[..i], &href[i..]),
        None => (href.as_str(), ""),
    };
    let joined = if href_main.starts_with('?') {
        format!("{path_only}{href_main}")
    } else if href_main.starts_with('/') {
        href_main.to_string()
    } else {
        let dir = &path_only[..path_only.rfind('/').map_or(0, |i| i + 1)];
        format!("{dir}{href_main}")
    };
    format!("{origin}{}{frag}", normalize_path(&joined))
}

/// Turn what the user typed into a URL ("example.com" -> "http://example.com").
fn normalize_input(s: &str) -> String {
    let s = s.trim();
    // "host:port" also looks like a scheme; a digit after the colon tells them apart.
    let port_follows = s.split_once(':').is_some_and(|(_, r)| r.starts_with(|c: char| c.is_ascii_digit()));
    if s.is_empty() || s.contains("://") || (has_scheme(s) && !port_follows) {
        return s.to_string();
    }
    format!("http://{s}")
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

// ---------------------------------------------------------------------------
// App state

const START_PAGE: &str = r#"<html><head><title>Start page</title></head><body>
<h1>Browser</h1>
<p>A small text-mode web browser for p4os. It opens <b>http://</b> and secure
<b>https://</b> pages; certificates are checked against the built-in list of trusted
authorities. Pages are shown as text, links are underlined and images appear as
placeholders.</p>
<h2>Sites to try</h2>
<ul>
<li><a href="http://info.cern.ch/">info.cern.ch</a> &mdash; the first website, from CERN</li>
<li><a href="http://info.cern.ch/hypertext/WWW/TheProject.html">The WorldWideWeb project</a> &mdash; the original 1991 pages</li>
<li><a href="https://example.com/">example.com</a> &mdash; the classic example domain</li>
<li><a href="https://text.npr.org/">text.npr.org</a> &mdash; NPR news in plain text</li>
<li><a href="https://lite.cnn.com/">lite.cnn.com</a> &mdash; CNN headlines, text only</li>
<li><a href="http://neverssl.com/">neverssl.com</a> &mdash; a site that never uses HTTPS</li>
<li><a href="http://68k.news/">68k.news</a> &mdash; news headlines for vintage computers</li>
<li><a href="http://textfiles.com/">textfiles.com</a> &mdash; an archive of BBS-era text files</li>
<li><a href="http://frogfind.com/">frogfind.com</a> &mdash; a search engine for vintage computers</li>
</ul>
<h2>Tips</h2>
<ul>
<li>Tap the address bar, type an address and press Enter or <b>Go</b>. Without a prefix, http:// is assumed; sites that need HTTPS redirect automatically.</li>
<li>Drag the page, or use the arrow keys, to scroll.</li>
<li>Back and Forward remember where you were on each page.</li>
</ul>
</body></html>"#;

enum Content {
    Html(String),
    Plain(String),
}

/// A loaded document and its layout for the current width.
struct Doc {
    url: String,
    content: Content,
    page: Option<Page>,
    laid_w: i32,
    /// Short description shown under the title, e.g. "example.com · 1 KB".
    info: String,
}

enum Action {
    Reload,
}

/// A full-page message (errors, unsupported addresses).
struct Notice {
    title: String,
    detail: String,
    action: Option<(String, Action)>,
}

enum View {
    Doc(Doc),
    Notice(Notice),
}

struct Entry {
    url: String,
    scroll: i32,
}

enum Nav {
    Go(String),
    Back,
    Forward,
    Reload,
    Stop,
}

struct Browser {
    url_text: String,
    field_was_focused: bool,
    /// The whole address is "selected": the next key replaces it.
    select_all: bool,
    history: Vec<Entry>,
    pos: usize,
    loading: Option<(FetchId, String)>,
    view: View,
    scroll: Point,
    /// Applied once the next document has been laid out.
    pending_scroll: Option<i32>,
    pending_anchor: Option<String>,
    /// Navigation changed what is shown; ask for a redraw right away.
    redraw: bool,
}

impl Browser {
    fn new() -> Self {
        let mut b = Self {
            url_text: String::new(),
            field_was_focused: false,
            select_all: false,
            history: Vec::new(),
            pos: 0,
            loading: None,
            view: View::Notice(Notice { title: String::new(), detail: String::new(), action: None }),
            scroll: Point::default(),
            pending_scroll: None,
            pending_anchor: None,
            redraw: false,
        };
        b.history.push(Entry { url: HOME_URL.into(), scroll: 0 });
        b.show_home();
        b
    }

    fn current_url(&self) -> &str {
        &self.history[self.pos].url
    }

    fn show_home(&mut self) {
        self.url_text.clear();
        self.view = View::Doc(Doc {
            url: HOME_URL.into(),
            content: Content::Html(START_PAGE.into()),
            page: None,
            laid_w: 0,
            info: "Built-in start page".into(),
        });
    }

    fn show_notice(&mut self, title: &str, detail: String, action: Option<(String, Action)>) {
        self.view = View::Notice(Notice { title: title.into(), detail, action });
        self.scroll = Point::default();
    }

    fn navigate(&mut self, url: String, sys: &mut dyn System) {
        // Same document, different fragment: just scroll.
        if let (View::Doc(d), Some(frag)) = (&self.view, fragment(&url)) {
            if self.loading.is_none() && strip_fragment(&d.url) == strip_fragment(&url) {
                let frag = frag.to_string();
                self.history[self.pos].scroll = self.scroll.y;
                self.history.truncate(self.pos + 1);
                self.history.push(Entry { url: url.clone(), scroll: 0 });
                self.pos += 1;
                self.url_text = url;
                self.pending_anchor = Some(frag);
                return;
            }
        }
        self.history[self.pos].scroll = self.scroll.y;
        self.history.truncate(self.pos + 1);
        self.history.push(Entry { url, scroll: 0 });
        self.pos += 1;
        self.load(sys, None);
    }

    /// Start loading the current history entry.
    fn load(&mut self, sys: &mut dyn System, restore: Option<i32>) {
        let url = self.current_url().to_string();
        self.loading = None;
        self.pending_scroll = restore;
        self.pending_anchor = fragment(&url).map(String::from);
        if url == HOME_URL {
            self.show_home();
            return;
        }
        self.url_text = url.clone();
        if !starts_with_ci(&url, "http://") && !starts_with_ci(&url, "https://") {
            self.show_notice(
                "Can't open this address",
                format!(
                    "\"{url}\" is not a web address this browser understands. Only http:// and https:// links can be opened."
                ),
                None,
            );
            return;
        }
        let id = sys.net().fetch(strip_fragment(&url));
        self.loading = Some((id, url));
    }

    fn poll(&mut self, sys: &mut dyn System) {
        let Some((id, url)) = &self.loading else { return };
        let (id, url) = (*id, url.clone());
        let host = host_of(&url).to_string();
        match sys.net().poll(id) {
            FetchState::Pending => return,
            FetchState::Failed(msg) => {
                if msg.contains("certificate") || msg.contains("TLS") || msg.contains("clock not set") {
                    self.show_notice(
                        "Secure connection failed",
                        format!("{host}: {msg}"),
                        Some(("Try again".into(), Action::Reload)),
                    );
                } else {
                    self.show_notice(
                        "Can't reach this page",
                        format!("{host}: {msg}"),
                        Some(("Try again".into(), Action::Reload)),
                    );
                }
            }
            FetchState::Done { status, content_type, body } => {
                self.loading = None;
                self.show_response(url, status, &content_type, body);
                return;
            }
        }
        self.loading = None;
    }

    fn show_response(&mut self, url: String, status: u16, content_type: &str, body: Vec<u8>) {
        let host = host_of(&url).to_string();
        if !(200..300).contains(&status) {
            let title = format!("HTTP {status} {}", reason(status));
            self.show_notice(
                title.trim_end(),
                format!("The server at {host} answered with an error, so there is no page to show."),
                Some(("Try again".into(), Action::Reload)),
            );
            return;
        }
        let ct = content_type.to_ascii_lowercase();
        let text = String::from_utf8_lossy(&body).into_owned();
        let looks_html = text.trim_start().starts_with('<');
        let content = if ct.contains("html") || (ct.is_empty() && looks_html) {
            Content::Html(text)
        } else if ct.starts_with("text/") || ct.is_empty() {
            Content::Plain(text)
        } else {
            let kind = ct.split(';').next().unwrap_or("").trim().to_string();
            self.show_notice(
                "Can't display this file",
                format!("{host} sent {} of type {kind}, which is not text.", size_text(body.len())),
                None,
            );
            return;
        };
        let kind = if matches!(content, Content::Plain(_)) { " · plain text" } else { "" };
        let secure = if starts_with_ci(&url, "https://") { " · secure" } else { "" };
        let info = format!("{host}{secure} · {}{kind}", size_text(body.len()));
        self.view = View::Doc(Doc { url, content, page: None, laid_w: 0, info });
        self.scroll = Point::default();
    }

    fn apply(&mut self, nav: Nav, sys: &mut dyn System) {
        match nav {
            Nav::Go(url) => self.navigate(url, sys),
            Nav::Back if self.pos > 0 => {
                self.history[self.pos].scroll = self.scroll.y;
                self.pos -= 1;
                let restore = self.history[self.pos].scroll;
                self.load(sys, Some(restore));
            }
            Nav::Forward if self.pos + 1 < self.history.len() => {
                self.history[self.pos].scroll = self.scroll.y;
                self.pos += 1;
                let restore = self.history[self.pos].scroll;
                self.load(sys, Some(restore));
            }
            Nav::Reload => {
                let y = self.scroll.y;
                self.load(sys, Some(y));
            }
            Nav::Stop => {
                self.loading = None;
                self.url_text = self.displayed_url();
            }
            _ => {}
        }
    }

    fn displayed_url(&self) -> String {
        let u = self.current_url();
        if u == HOME_URL { String::new() } else { u.to_string() }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        301 | 302 | 303 | 307 | 308 => "Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn size_text(n: usize) -> String {
    if n < 1024 { format!("{n} B") } else { format!("{:.1} KB", n as f32 / 1024.0) }
}

// ---------------------------------------------------------------------------
// Drawing

const MARGIN: i32 = 18;
const BTN: i32 = 52;
const ROW_H: i32 = 48;
const GAP: i32 = 8;

/// Positions of the browser chrome, derived from the app area.
struct Chrome {
    back: Rect,
    fwd: Rect,
    reload: Rect,
    home: Rect,
    url: Rect,
    go: Rect,
    title: Rect,
    /// Background of the whole toolbar region.
    bar: Rect,
    content: Rect,
}

fn chrome(area: Rect) -> Chrome {
    let x = area.x + GAP;
    let y = area.y + GAP;
    let btn = |i: i32, y: i32| Rect::new(x + i * (BTN + 6), y, BTN, ROW_H);
    let go_w = 72;
    let right = area.right() - GAP;
    let (back, fwd, reload, home, url, go, title);
    if area.w > area.h && area.w >= 760 {
        // Wide: buttons, address and Go on one row, title strip below.
        back = btn(0, y);
        fwd = btn(1, y);
        reload = btn(2, y);
        home = btn(3, y);
        go = Rect::new(right - go_w, y, go_w, ROW_H);
        url = Rect::new(home.right() + GAP, y, go.x - GAP - home.right() - GAP, ROW_H);
        title = Rect::new(x + 4, y + ROW_H + 2, area.w - 2 * GAP - 8, 30);
    } else {
        // Narrow: address and Go on top, buttons and title below.
        go = Rect::new(right - go_w, y, go_w, ROW_H);
        url = Rect::new(x, y, go.x - GAP - x, ROW_H);
        let y2 = y + ROW_H + 6;
        back = btn(0, y2);
        fwd = btn(1, y2);
        reload = btn(2, y2);
        home = btn(3, y2);
        title = Rect::new(home.right() + 12, y2, right - home.right() - 12, ROW_H);
    }
    let bar_h = title.bottom() + 4 - area.y;
    let bar = Rect::new(area.x, area.y, area.w, bar_h);
    let content = Rect::new(area.x, bar.bottom(), area.w, area.h - bar_h);
    Chrome { back, fwd, reload, home, url, go, title, bar, content }
}

#[derive(Clone, Copy)]
enum Glyph {
    Back,
    Forward,
    Reload,
    Stop,
    Home,
}

fn draw_glyph(c: &mut Canvas, r: Rect, g: Glyph, fg: Color, bg: Color) {
    let ctr = r.center();
    let (cx, cy) = (ctr.x, ctr.y);
    match g {
        Glyph::Back | Glyph::Forward => {
            let d = if matches!(g, Glyph::Back) { 1 } else { -1 };
            c.line(Point::new(cx + 5 * d, cy - 10), Point::new(cx - 5 * d, cy), 3.0, fg);
            c.line(Point::new(cx - 5 * d, cy), Point::new(cx + 5 * d, cy + 10), 3.0, fg);
        }
        Glyph::Reload => {
            c.stroke_circle(ctr, 10.0, 3.0, fg);
            // Open the ring in the upper right and add an arrowhead.
            c.fill_rect(Rect::new(cx + 1, cy - 14, 14, 12), bg);
            c.fill_triangle([Point::new(cx - 1, cy - 16), Point::new(cx - 1, cy - 4), Point::new(cx + 7, cy - 10)], fg);
        }
        Glyph::Stop => {
            c.line(Point::new(cx - 8, cy - 8), Point::new(cx + 8, cy + 8), 3.0, fg);
            c.line(Point::new(cx + 8, cy - 8), Point::new(cx - 8, cy + 8), 3.0, fg);
        }
        Glyph::Home => {
            c.fill_triangle([Point::new(cx - 13, cy - 1), Point::new(cx + 13, cy - 1), Point::new(cx, cy - 13)], fg);
            c.fill_rect(Rect::new(cx - 9, cy - 2, 18, 13), fg);
            c.fill_rect(Rect::new(cx - 3, cy + 3, 6, 8), bg);
        }
    }
}

fn tool_button(ui: &mut Ui, r: Rect, enabled: bool, g: Glyph) -> bool {
    let t = *ui.theme;
    let (held, clicked) = if enabled { ui.interact(r) } else { (false, false) };
    let bg = if held { t.surface_alt.lighten(40) } else { t.surface_alt };
    ui.canvas.fill_round_rect(r, t.radius, bg);
    let fg = if enabled { t.text } else { t.text_dim.darken(90) };
    draw_glyph(ui.canvas, r, g, fg, bg);
    clicked
}

/// Index of the link under `p` (page coordinates), with a little slack
/// around each run so small links are easier to hit with a finger.
fn link_at(page: &Page, p: Point) -> Option<usize> {
    let first = page.lines.partition_point(|l| l.y + l.h + 8 < p.y);
    for line in &page.lines[first..] {
        if line.y - 8 > p.y {
            break;
        }
        for run in &line.runs {
            let Some(link) = run.style.link else { continue };
            if Rect::new(run.x - 4, line.y - 6, run.w + 8, line.h + 12).contains(p) {
                return Some(link);
            }
        }
    }
    None
}

impl Browser {
    fn draw_chrome(&mut self, ui: &mut Ui, ch: &Chrome, sys: &mut dyn System) -> Option<Nav> {
        let t = *ui.theme;
        let mut nav = None;
        ui.fill(ch.bar, t.surface);
        ui.canvas.fill_rect(Rect::new(ch.bar.x, ch.bar.bottom() - 1, ch.bar.w, 1), t.border);

        if tool_button(ui, ch.back, self.pos > 0, Glyph::Back) {
            nav = Some(Nav::Back);
        }
        if tool_button(ui, ch.fwd, self.pos + 1 < self.history.len(), Glyph::Forward) {
            nav = Some(Nav::Forward);
        }
        let loading = self.loading.is_some();
        if tool_button(ui, ch.reload, true, if loading { Glyph::Stop } else { Glyph::Reload }) {
            nav = Some(if loading { Nav::Stop } else { Nav::Reload });
        }
        if tool_button(ui, ch.home, true, Glyph::Home) {
            nav = Some(Nav::Go(HOME_URL.into()));
        }

        // Address field. The first key after focusing replaces the address.
        let focused_before = ui.state.focus == Some(ch.url);
        if focused_before && self.select_all && ui.input.keys.iter().any(|k| matches!(k, Key::Char(_) | Key::Backspace)) {
            self.url_text.clear();
            self.select_all = false;
        }
        let submitted = ui.text_field(ch.url, &mut self.url_text, "Type a web address");
        let focused = ui.state.focus == Some(ch.url);
        if focused && !self.field_was_focused {
            self.select_all = !self.url_text.is_empty();
        }
        if !focused {
            self.select_all = false;
            if self.field_was_focused && !submitted {
                self.url_text = self.displayed_url();
            }
        }
        self.field_was_focused = focused;
        if self.select_all {
            let f = ui.fonts.body;
            let inner = ch.url.inset(12);
            let w = f.measure(&self.url_text).min(inner.w);
            let ty = inner.y + (inner.h - f.line_height) / 2;
            ui.canvas.fill_rect_alpha(Rect::new(inner.x - 2, ty, w + 4, f.line_height), t.accent, 90);
        }
        if ui.button_primary(ch.go, "Go") || submitted {
            let url = normalize_input(&self.url_text);
            if !url.is_empty() {
                nav = Some(Nav::Go(url));
            }
        }

        // Title and status.
        let (title, info) = match (&self.loading, &self.view) {
            (Some((_, url)), _) => (format!("Loading {}…", host_of(url)), String::from("Waiting for the server")),
            (None, View::Doc(d)) => {
                let title = d.page.as_ref().map(|p| p.title.clone()).filter(|t| !t.is_empty());
                (title.unwrap_or_else(|| host_of(&d.url).to_string()), d.info.clone())
            }
            (None, View::Notice(n)) => (n.title.clone(), host_of(self.current_url()).to_string()),
        };
        let (bf, sf) = (ui.fonts.body, ui.fonts.small);
        let tr = ch.title;
        if tr.h >= 40 {
            // Two lines: title above, details below.
            let (t1, t2) = tr.split_top(tr.h / 2 + 3);
            ui.text(t1, &title, &bf, t.text, Align::Left);
            ui.text(Rect::new(t2.x, t2.y - 2, t2.w, t2.h), &info, &sf, t.text_dim, Align::Left);
        } else {
            // One line: title on the left, details on the right.
            let info_w = sf.measure(&info);
            ui.text(Rect::new(tr.x, tr.y, tr.w - info_w - 24, tr.h), &title, &bf, t.text, Align::Left);
            ui.text(tr, &info, &sf, t.text_dim, Align::Right);
        }

        if loading {
            // Indeterminate progress: a segment sweeping along the bottom edge.
            let track = Rect::new(ch.bar.x, ch.bar.bottom() - 3, ch.bar.w, 3);
            ui.canvas.fill_rect(track, t.surface_alt);
            let seg = track.w / 4;
            let phase = (sys.uptime_ms() % 1400) as i32;
            let x = track.x - seg + (track.w + seg) * phase / 1400;
            let old = ui.canvas.set_clip(track.intersect(&ui.canvas.clip()));
            ui.canvas.fill_rect(Rect::new(x, track.y, seg, 3), t.accent);
            ui.canvas.restore_clip(old);
        }
        nav
    }

    fn draw_notice(&mut self, ui: &mut Ui, area: Rect) -> Option<Nav> {
        let View::Notice(n) = &self.view else { return None };
        let t = *ui.theme;
        let (lf, bf) = (ui.fonts.large, ui.fonts.body);
        let w = (area.w - 48).min(560);
        let text_w = w - 48;
        let lines = bf.wrap(&n.detail, text_w);
        let btn_h = if n.action.is_some() { 56 + 20 } else { 0 };
        let h = 24 + lf.line_height + 12 + lines.len() as i32 * bf.line_height + 24 + btn_h;
        let card = Rect::new(area.x + (area.w - w) / 2, area.y + ((area.h - h) / 3).max(16), w, h);
        ui.panel(card);
        let mut y = card.y + 24;
        let tw = Rect::new(card.x + 24, y, text_w, lf.line_height);
        ui.text(tw, &n.title, &lf, t.text, Align::Left);
        y += lf.line_height + 12;
        for l in &lines {
            ui.text(Rect::new(card.x + 24, y, text_w, bf.line_height), l, &bf, t.text_dim, Align::Left);
            y += bf.line_height;
        }
        y += 24;
        let mut nav = None;
        if let Some((label, action)) = &n.action {
            let bw = (lf.measure(label) + 48).min(text_w);
            if ui.button_primary(Rect::new(card.x + 24, y, bw, 56), label) {
                nav = Some(match action {
                    Action::Reload => Nav::Reload,
                });
            }
        }
        nav
    }

    fn draw_doc(&mut self, ui: &mut Ui, area: Rect) -> Option<Nav> {
        let fonts = ui.fonts;
        let t = *ui.theme;
        let View::Doc(doc) = &mut self.view else { return None };
        let lw = area.w - 2 * MARGIN - 8;
        if doc.page.is_none() || doc.laid_w != lw {
            let page = match &doc.content {
                Content::Html(src) => layout_html(src, &doc.url, fonts, lw),
                Content::Plain(src) => layout_plain(src, fonts, lw),
            };
            doc.page = Some(page);
            doc.laid_w = lw;
        }
        let page = doc.page.as_ref().unwrap();
        // While the next page loads, the old one stays on screen; scroll
        // targets belong to the incoming page.
        if self.loading.is_none() {
            if let Some(y) = self.pending_scroll.take() {
                self.scroll.y = y;
            }
            if let Some(a) = self.pending_anchor.take() {
                self.scroll.y = page.anchor_y(&a).map_or(0, |y| y - 8);
            }
        }

        // Scrolling: drag (both axes when the page is wider) and arrow keys.
        let input = ui.input;
        let max_y = (page.height - area.h).max(0);
        let max_x = (page.width - lw).max(0);
        if input.down && area.contains(input.press_pos) && input.is_drag() {
            self.scroll.y -= input.delta.y;
            if max_x > 0 {
                self.scroll.x -= input.delta.x;
            }
        }
        for k in &input.keys {
            match k {
                Key::Down => self.scroll.y += 48,
                Key::Up => self.scroll.y -= 48,
                Key::Right => self.scroll.x += 48,
                Key::Left => self.scroll.x -= 48,
                _ => {}
            }
        }
        self.scroll.y = self.scroll.y.clamp(0, max_y);
        self.scroll.x = self.scroll.x.clamp(0, max_x);

        let origin = Point::new(area.x + MARGIN - self.scroll.x, area.y - self.scroll.y);
        let rel = |p: Point| Point::new(p.x - origin.x, p.y - origin.y);
        let pressed = (input.down && !input.is_drag() && area.contains(input.press_pos))
            .then(|| link_at(page, rel(input.press_pos)))
            .flatten();
        let mut nav = None;
        if input.released && !input.is_drag() && area.contains(input.pos) && area.contains(input.press_pos) {
            if let Some(i) = link_at(page, rel(input.pos)) {
                nav = Some(Nav::Go(page.links[i].clone()));
            }
        }

        let old = ui.canvas.set_clip(area.intersect(&ui.canvas.clip()));
        let c = &mut *ui.canvas;
        let first = page.lines.partition_point(|l| l.y + l.h < self.scroll.y);
        for line in &page.lines[first..] {
            let ly = origin.y + line.y;
            if ly > area.bottom() {
                break;
            }
            if line.pre {
                c.fill_rect(Rect::new(area.x, ly, area.w, line.h), t.surface);
            }
            for &bx in &line.bars {
                c.fill_rect(Rect::new(origin.x + bx, ly, 3, line.h), t.accent.darken(120));
            }
            if line.rule {
                c.fill_rect(Rect::new(origin.x + line.indent, ly + line.h / 2, lw - line.indent, 1), t.border);
                continue;
            }
            for run in &line.runs {
                let x = origin.x + run.x;
                if let RunKind::Bullet(hollow) = run.kind {
                    let b = fonts.body;
                    let ctr = Point::new(x + 6, ly + line.ascent - b.ascent * 3 / 10);
                    if hollow {
                        c.stroke_circle(ctr, 3.0, 1.5, t.text_dim);
                    } else {
                        c.fill_circle(ctr, 3.5, t.text_dim);
                    }
                    continue;
                }
                let font = run.style.face.font(fonts);
                let ty = ly + line.ascent - font.ascent;
                let color = if run.style.link.is_some() {
                    t.accent.lighten(30)
                } else if run.style.dim {
                    t.text_dim
                } else {
                    t.text
                };
                if run.style.link.is_some() && run.style.link == pressed {
                    c.fill_round_rect(Rect::new(x - 3, ly, run.w + 6, line.h), 6, t.accent.darken(140));
                }
                font.draw(c, Point::new(x, ty), &run.text, color);
                if run.style.bold {
                    font.draw(c, Point::new(x + 1, ty), &run.text, color);
                }
                if run.style.link.is_some() {
                    c.fill_rect(Rect::new(x, ty + font.ascent + 2, run.w, 1), color);
                }
            }
        }
        // Scroll indicator.
        if max_y > 0 {
            let bar_h = (area.h * area.h / page.height).max(30);
            let bar_y = area.y + (area.h - bar_h) * self.scroll.y / max_y;
            c.fill_round_rect(Rect::new(area.right() - 6, bar_y, 4, bar_h), 2, t.text_dim);
        }
        ui.canvas.restore_clip(old);
        nav
    }
}

impl App for Browser {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        self.redraw = false;
        self.poll(sys);
        let t = *ui.theme;
        ui.fill(area, t.bg);
        let ch = chrome(area);
        let mut nav = match self.view {
            View::Doc(_) => self.draw_doc(ui, ch.content),
            View::Notice(_) => self.draw_notice(ui, ch.content),
        };
        // Chrome last so it draws over content and wins its own taps.
        if let Some(n) = self.draw_chrome(ui, &ch, sys) {
            nav = Some(n);
        }
        if let Some(n) = nav {
            ui.state.focus = None;
            self.field_was_focused = false;
            self.select_all = false;
            self.apply(n, sys);
            self.redraw = true;
            if self.loading.is_none() && !matches!(self.view, View::Doc(_)) {
                self.scroll = Point::default();
            }
        }
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        if self.redraw {
            Some(0)
        } else {
            self.loading.as_ref().map(|_| 50)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_urls() {
        let base = "http://h.com/a/b/c.html?q=1#top";
        assert_eq!(resolve_url(base, "d.html"), "http://h.com/a/b/d.html");
        assert_eq!(resolve_url(base, "./d.html"), "http://h.com/a/b/d.html");
        assert_eq!(resolve_url(base, "../d.html"), "http://h.com/a/d.html");
        assert_eq!(resolve_url(base, "../../../../d.html"), "http://h.com/d.html");
        assert_eq!(resolve_url(base, "/x/y"), "http://h.com/x/y");
        assert_eq!(resolve_url(base, "/x/./z/../y/"), "http://h.com/x/y/");
        assert_eq!(resolve_url(base, "sub/"), "http://h.com/a/b/sub/");
        assert_eq!(resolve_url(base, ".."), "http://h.com/a/");
        assert_eq!(resolve_url(base, "?p=2"), "http://h.com/a/b/c.html?p=2");
        assert_eq!(resolve_url(base, "#sec"), "http://h.com/a/b/c.html?q=1#sec");
        assert_eq!(resolve_url(base, "e.html#s"), "http://h.com/a/b/e.html#s");
        assert_eq!(resolve_url(base, "//other.org/p"), "http://other.org/p");
        assert_eq!(resolve_url(base, "https://s.com/"), "https://s.com/");
        assert_eq!(resolve_url(base, "mailto:a@b.c"), "mailto:a@b.c");
        assert_eq!(resolve_url(base, "  d.html\n"), "http://h.com/a/b/d.html");
        assert_eq!(resolve_url(base, ""), "http://h.com/a/b/c.html?q=1");
    }

    #[test]
    fn resolves_against_bare_hosts() {
        assert_eq!(resolve_url("http://h.com", "x.html"), "http://h.com/x.html");
        assert_eq!(resolve_url("http://h.com:8080", "../x"), "http://h.com:8080/x");
        assert_eq!(resolve_url("http://h.com/dir/", "x?a=../b"), "http://h.com/dir/x?a=../b");
    }

    #[test]
    fn normalizes_typed_addresses() {
        assert_eq!(normalize_input(" example.com "), "http://example.com");
        assert_eq!(normalize_input("http://a.b/c"), "http://a.b/c");
        assert_eq!(normalize_input("https://a.b"), "https://a.b");
        assert_eq!(normalize_input("about:home"), "about:home");
        assert_eq!(normalize_input("localhost:8080/x"), "http://localhost:8080/x");
        assert_eq!(host_of("http://h.com:81/x?y"), "h.com:81");
        assert_eq!(fragment("http://a/b#c"), Some("c"));
        assert_eq!(fragment("http://a/b#"), None);
    }
}
