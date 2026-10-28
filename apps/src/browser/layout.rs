//! Turns the token stream into positioned lines of styled text.
//!
//! The layout is a single pass with a small amount of state (open lists,
//! quotes, the current link, heading level). Coordinates are relative to
//! the page's content origin; the app adds margins and scroll offsets.

use super::html::{tokenize, Token};
use super::resolve_url;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::gui::font::builtin::Fonts;
use p4os_sdk::gui::Font;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Face {
    Small,
    Body,
    Large,
    Bold,
    Title,
    Mono,
}

impl Face {
    pub fn font<'a>(self, f: &'a Fonts) -> &'a Font {
        match self {
            Face::Small => &f.small,
            Face::Body => &f.body,
            Face::Large => &f.large,
            Face::Bold => &f.bold,
            Face::Title => &f.title,
            Face::Mono => &f.mono,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    pub face: Face,
    /// Faux bold (drawn twice, 1 px apart) for faces without a bold cut.
    pub bold: bool,
    /// Secondary text such as image placeholders.
    pub dim: bool,
    /// Index into [`Page::links`].
    pub link: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunKind {
    Text,
    /// List bullet; `true` for the hollow variant used by nested lists.
    Bullet(bool),
}

#[derive(Clone, Debug)]
pub struct Run {
    pub x: i32,
    pub w: i32,
    pub text: String,
    pub style: Style,
    pub kind: RunKind,
}

#[derive(Clone, Debug, Default)]
pub struct Line {
    pub y: i32,
    pub h: i32,
    /// Baseline offset from `y`.
    pub ascent: i32,
    pub indent: i32,
    pub runs: Vec<Run>,
    /// Part of a preformatted block (drawn on a tinted background).
    pub pre: bool,
    /// Horizontal rule instead of text.
    pub rule: bool,
    /// x positions of blockquote bars.
    pub bars: Vec<i32>,
}

#[derive(Default)]
pub struct Page {
    pub title: String,
    pub lines: Vec<Line>,
    /// Absolute URLs of links, referenced by [`Style::link`].
    pub links: Vec<String>,
    /// Fragment targets (`id` / `<a name>`) and their y position.
    pub anchors: Vec<(String, i32)>,
    pub height: i32,
    /// Widest line; larger than the layout width only for unwrapped text.
    pub width: i32,
}

impl Page {
    pub fn anchor_y(&self, name: &str) -> Option<i32> {
        self.anchors.iter().find(|(n, _)| n == name).map(|(_, y)| *y)
    }
}

/// Replace typographic characters the bundled fonts lack (they cover
/// ASCII, Latin-1, Vietnamese and a few symbols) with close ASCII forms,
/// and drop invisible ones.
pub fn displayable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' => out.push(' '),
            '\u{ad}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}' => {}
            '\u{2010}'..='\u{2015}' | '\u{2043}' => out.push('-'),
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2032}' => out.push('\''),
            '\u{201c}'..='\u{201f}' | '\u{2033}' => out.push('"'),
            '\u{2039}' => out.push('<'),
            '\u{203a}' => out.push('>'),
            '\u{2122}' => out.push_str("TM"),
            '\u{20ac}' => out.push_str("EUR"),
            _ => out.push(c),
        }
    }
    out
}

const TOP: i32 = 12;
const INDENT: i32 = 32;
const CELL_GAP: i32 = 24;
const LINE_GAP: i32 = 2;

struct ListState {
    ordered: bool,
    next: i32,
}

struct Builder<'f> {
    fonts: &'f Fonts,
    width: i32,
    base: String,
    page: Page,
    y: i32,
    x: i32,
    cur: Line,
    pending_space: bool,
    pending_gap: i32,
    indent: i32,
    // Element state.
    bold: u32,
    mono: u32,
    small: u32,
    heading: u8,
    pre: u32,
    /// Drop one leading newline right after `<pre>`.
    pre_start: bool,
    link: Option<usize>,
    lists: Vec<ListState>,
    quotes: Vec<i32>,
    in_dd: bool,
    in_dt: bool,
    in_th: bool,
    in_head: bool,
    in_title: bool,
    skip: u32,
}

/// Lay out an HTML document for a content area `width` pixels wide.
pub fn layout_html(src: &str, base: &str, fonts: &Fonts, width: i32) -> Page {
    let mut b = Builder::new(fonts, width, base);
    for tok in tokenize(src) {
        match tok {
            Token::Text(t) => b.text(&t),
            Token::Start { name, attrs, self_closing } => b.start(&name, &attrs, self_closing),
            Token::End(name) => b.end(&name),
        }
    }
    b.finish()
}

/// Lay out a plain-text document as preformatted text.
pub fn layout_plain(src: &str, fonts: &Fonts, width: i32) -> Page {
    let mut b = Builder::new(fonts, width, "");
    b.pre = 1;
    b.text(src);
    let mut page = b.finish();
    // The whole document is preformatted; no need to tint it as a block.
    for l in &mut page.lines {
        l.pre = false;
    }
    page
}

impl<'f> Builder<'f> {
    fn new(fonts: &'f Fonts, width: i32, base: &str) -> Self {
        Self {
            fonts,
            width: width.max(200),
            base: base.into(),
            page: Page::default(),
            y: TOP,
            x: 0,
            cur: Line::default(),
            pending_space: false,
            pending_gap: 0,
            indent: 0,
            bold: 0,
            mono: 0,
            small: 0,
            heading: 0,
            pre: 0,
            pre_start: false,
            link: None,
            lists: Vec::new(),
            quotes: Vec::new(),
            in_dd: false,
            in_dt: false,
            in_th: false,
            in_head: false,
            in_title: false,
            skip: 0,
        }
    }

    fn finish(mut self) -> Page {
        self.flush();
        self.page.height = self.y + TOP * 2;
        let t = displayable(&self.page.title).split_whitespace().collect::<Vec<_>>().join(" ");
        self.page.title = t;
        self.page
    }

    fn style(&self) -> Style {
        let (face, heading_bold) = match self.heading {
            1 => (Face::Title, false),
            2 | 3 => (Face::Bold, false),
            4 => (Face::Large, false),
            5 | 6 => (Face::Body, true),
            _ if self.pre > 0 || self.mono > 0 => (Face::Mono, false),
            _ if self.small > 0 => (Face::Small, false),
            _ => (Face::Body, false),
        };
        let bold = heading_bold || ((self.bold > 0 || self.in_dt || self.in_th) && !matches!(face, Face::Bold | Face::Title));
        Style { face, bold, dim: false, link: self.link }
    }

    fn line_empty(&self) -> bool {
        self.cur.runs.is_empty()
    }

    /// Prepare the current line for its first run.
    fn begin_line(&mut self) {
        if self.line_empty() {
            if !self.page.lines.is_empty() {
                self.y += self.pending_gap;
            }
            self.pending_gap = 0;
            self.x = self.indent.min(self.width / 2);
            self.cur.indent = self.x;
        }
    }

    /// Finish the current line if it has content.
    fn flush(&mut self) {
        if self.line_empty() {
            return;
        }
        let mut line = core::mem::take(&mut self.cur);
        let (mut h, mut ascent) = (0, 0);
        for r in &line.runs {
            let f = r.style.face.font(self.fonts);
            h = h.max(f.line_height);
            ascent = ascent.max(f.ascent);
        }
        // Keep descenders of the largest font inside the line box.
        let below = line.runs.iter().map(|r| {
            let f = r.style.face.font(self.fonts);
            f.line_height - f.ascent
        });
        h = h.max(ascent + below.max().unwrap_or(0)) + LINE_GAP;
        line.y = self.y;
        line.h = h;
        line.ascent = ascent;
        line.pre = self.pre > 0;
        line.bars = self.quotes.clone();
        self.page.width = self.page.width.max(line.runs.iter().map(|r| r.x + r.w).max().unwrap_or(0));
        self.y += h;
        self.page.lines.push(line);
        self.pending_space = false;
    }

    /// End the current block and leave at least `gap` pixels before the next.
    fn block(&mut self, gap: i32) {
        self.flush();
        self.pending_gap = self.pending_gap.max(gap);
        self.pending_space = false;
    }

    /// `<br>` or a newline in preformatted text.
    fn hard_break(&mut self) {
        if self.line_empty() {
            self.begin_line();
            let f = self.style().face.font(self.fonts);
            let h = f.line_height + LINE_GAP;
            // An empty line still belongs to its block (pre tint, quote bars).
            let line = Line { y: self.y, h, ascent: f.ascent, indent: self.x, pre: self.pre > 0, bars: self.quotes.clone(), ..Line::default() };
            self.page.lines.push(line);
            self.y += h;
        } else {
            self.flush();
        }
    }

    fn push_run(&mut self, text: String, w: i32, style: Style, kind: RunKind, x: i32) {
        self.cur.runs.push(Run { x, w, text, style, kind });
    }

    fn text(&mut self, s: &str) {
        if self.skip > 0 || (self.in_head && !self.in_title) {
            return;
        }
        if self.in_title {
            self.page.title.push_str(s);
            return;
        }
        if self.pre > 0 {
            self.pre_text(s);
            return;
        }
        if s.starts_with(|c: char| c.is_ascii_whitespace()) && !self.line_empty() {
            self.pending_space = true;
        }
        let style = self.style();
        for (i, word) in s.split_ascii_whitespace().enumerate() {
            if i > 0 {
                self.pending_space = true;
            }
            self.word(word, style);
        }
        if s.ends_with(|c: char| c.is_ascii_whitespace()) && !self.line_empty() {
            self.pending_space = true;
        }
    }

    /// Place one word, wrapping (or splitting an over-long word) as needed.
    fn word(&mut self, word: &str, style: Style) {
        let word = displayable(word);
        if word.is_empty() {
            return;
        }
        let font = style.face.font(self.fonts);
        let w = font.measure(&word);
        let mut space = if self.pending_space && !self.line_empty() { font.char_width(' ') } else { 0 };
        let right = self.width;
        if !self.line_empty() && self.x + space + w > right {
            self.flush();
            space = 0;
        }
        self.begin_line();
        if w > right - self.x {
            // Longer than a whole line: break it at character boundaries.
            let mut chunk = String::new();
            let mut cw = 0;
            for ch in word.chars() {
                let chw = font.char_width(ch);
                if cw + chw > right - self.x - space && !chunk.is_empty() {
                    let x = self.x + space;
                    self.push_run(core::mem::take(&mut chunk), cw, style, RunKind::Text, x);
                    self.flush();
                    self.begin_line();
                    cw = 0;
                    space = 0;
                }
                chunk.push(ch);
                cw += chw;
            }
            let x = self.x + space;
            self.push_run(chunk, cw, style, RunKind::Text, x);
            self.x = x + cw;
            self.pending_space = false;
            return;
        }
        // Extend the previous run when the style matches to keep runs few.
        if let Some(last) = self.cur.runs.last_mut() {
            if last.style == style && last.kind == RunKind::Text && last.x + last.w == self.x {
                if space > 0 {
                    last.text.push(' ');
                }
                last.text.push_str(&word);
                last.w += space + w;
                self.x += space + w;
                self.pending_space = false;
                return;
            }
        }
        let x = self.x + space;
        self.push_run(word, w, style, RunKind::Text, x);
        self.x = x + w;
        self.pending_space = false;
    }

    fn pre_text(&mut self, s: &str) {
        let mut s = s;
        if core::mem::take(&mut self.pre_start) {
            s = s.strip_prefix("\r\n").or_else(|| s.strip_prefix('\n')).unwrap_or(s);
        }
        let style = self.style();
        let font = style.face.font(self.fonts);
        for (i, seg) in s.split('\n').enumerate() {
            if i > 0 {
                self.hard_break();
            }
            let seg = seg.strip_suffix('\r').unwrap_or(seg);
            if seg.is_empty() {
                continue;
            }
            self.begin_line();
            // Expand tabs to 8-column stops relative to the line start.
            let col0 = self.cur.runs.iter().map(|r| r.text.chars().count()).sum::<usize>();
            let mut text = String::new();
            let mut col = col0;
            for ch in displayable(seg).chars() {
                if ch == '\t' {
                    let n = 8 - col % 8;
                    text.extend(core::iter::repeat_n(' ', n));
                    col += n;
                } else {
                    text.push(ch);
                    col += 1;
                }
            }
            let w = font.measure(&text);
            let x = self.x;
            self.push_run(text, w, style, RunKind::Text, x);
            self.x += w;
        }
    }

    /// Insert horizontal space between table cells.
    fn cell_gap(&mut self) {
        if !self.line_empty() {
            self.x += CELL_GAP;
            self.pending_space = false;
        }
    }

    fn list_marker(&mut self) {
        let depth = self.lists.len();
        let marker = match self.lists.last_mut() {
            Some(l) if l.ordered => {
                let n = l.next;
                l.next += 1;
                Some(alloc::format!("{n}."))
            }
            _ => None,
        };
        self.begin_line();
        let style = Style { face: Face::Body, bold: false, dim: false, link: None };
        let ix = self.x;
        match marker {
            Some(text) => {
                let w = self.fonts.body.measure(&text);
                self.push_run(text, w, style, RunKind::Text, (ix - 8 - w).max(0));
            }
            None => self.push_run(String::new(), 12, style, RunKind::Bullet(depth % 2 == 0), (ix - 20).max(0)),
        }
        self.x = ix;
        self.pending_space = false;
    }

    fn inline_placeholder(&mut self, text: &str) {
        let mut style = self.style();
        style.dim = true;
        if !self.line_empty() {
            self.pending_space = true;
        }
        for (i, w) in text.split(' ').enumerate() {
            if i > 0 {
                self.pending_space = true;
            }
            self.word(w, style);
        }
        self.pending_space = true;
    }

    fn start(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        let attr = |k: &str| Token::attr(attrs, k);
        if let Some(id) = attr("id") {
            self.page.anchors.push((id.into(), self.y));
        }
        if self.skip > 0 {
            if !self_closing && matches!(name, "svg" | "select" | "template" | "iframe" | "object" | "video" | "audio") {
                self.skip += 1;
            }
            return;
        }
        match name {
            "head" => self.in_head = true,
            "body" => self.in_head = false,
            "title" => self.in_title = true,
            "base" => {
                if let Some(h) = attr("href") {
                    self.base = resolve_url(&self.base, h);
                }
            }
            "p" | "figure" | "fieldset" => self.block(12),
            "div" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside" | "address" | "center"
            | "caption" | "figcaption" | "details" | "summary" | "legend" | "form" | "noscript" | "hgroup" => self.block(0),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.heading = name.as_bytes()[1] - b'0';
                self.block(if self.heading <= 2 { 18 } else { 12 });
            }
            "pre" | "listing" | "plaintext" => {
                self.block(10);
                self.pre += 1;
                self.pre_start = true;
            }
            "ul" | "ol" | "menu" | "dir" => {
                self.block(if self.lists.is_empty() { 12 } else { 0 });
                let next = attr("start").and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                self.lists.push(ListState { ordered: name == "ol", next });
                self.indent += INDENT;
            }
            "li" => {
                self.block(4);
                if let Some(v) = attr("value").and_then(|s| s.trim().parse().ok()) {
                    if let Some(l) = self.lists.last_mut() {
                        l.next = v;
                    }
                }
                self.list_marker();
            }
            "dl" => self.block(12),
            "dt" => {
                self.block(4);
                self.set_dd(false);
                self.in_dt = true;
            }
            "dd" => {
                self.block(0);
                self.in_dt = false;
                self.set_dd(true);
            }
            "blockquote" => {
                self.block(12);
                self.quotes.push(self.indent + 4);
                self.indent += INDENT;
            }
            "hr" => {
                self.block(8);
                self.begin_line();
                let indent = self.cur.indent;
                self.page.lines.push(Line { y: self.y, h: 17, indent, rule: true, bars: self.quotes.clone(), ..Line::default() });
                self.y += 17;
                self.block(8);
            }
            "br" => self.hard_break(),
            "table" => self.block(10),
            "tr" => {
                self.block(4);
                self.in_th = false;
            }
            "td" => {
                self.in_th = false;
                self.cell_gap();
            }
            "th" => {
                self.cell_gap();
                self.in_th = true;
            }
            "b" | "strong" => self.bold += 1,
            "code" | "tt" | "kbd" | "samp" | "var" => self.mono += 1,
            "small" | "sub" | "sup" => self.small += 1,
            "a" => {
                if let Some(n) = attr("name") {
                    self.page.anchors.push((n.into(), self.y));
                }
                self.link = attr("href").map(|h| {
                    self.page.links.push(resolve_url(&self.base, h));
                    self.page.links.len() - 1
                });
            }
            "img" => {
                let alt = attr("alt").map(str::trim).unwrap_or("");
                let label = if alt.is_empty() { String::from("[image]") } else { alloc::format!("[image: {alt}]") };
                self.inline_placeholder(&label);
            }
            "input" => {
                let kind = attr("type").unwrap_or("text").to_ascii_lowercase();
                let label = match kind.as_str() {
                    "hidden" => return,
                    "submit" | "button" | "reset" => alloc::format!("[{}]", attr("value").unwrap_or("Submit")),
                    "checkbox" => String::from("[ ]"),
                    "radio" => String::from("( )"),
                    _ => String::from("[______]"),
                };
                self.inline_placeholder(&label);
            }
            "textarea" => {
                self.inline_placeholder("[text area]");
                self.skip += 1;
            }
            "svg" | "select" | "template" | "iframe" | "object" | "video" | "audio" if !self_closing => {
                if name == "select" {
                    self.inline_placeholder("[menu]");
                }
                self.skip += 1;
            }
            _ => {}
        }
    }

    fn set_dd(&mut self, on: bool) {
        if on != self.in_dd {
            self.in_dd = on;
            if on { self.indent += INDENT } else { self.indent -= INDENT }
        }
    }

    fn end(&mut self, name: &str) {
        if self.skip > 0 {
            if matches!(name, "svg" | "select" | "template" | "iframe" | "object" | "video" | "audio" | "textarea") {
                self.skip -= 1;
            }
            return;
        }
        match name {
            "head" => self.in_head = false,
            "title" => self.in_title = false,
            "p" | "figure" | "fieldset" | "dl" => {
                if name == "dl" {
                    self.in_dt = false;
                    self.set_dd(false);
                }
                self.block(12)
            }
            "div" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside" | "address" | "center"
            | "caption" | "figcaption" | "details" | "summary" | "legend" | "form" | "noscript" | "hgroup" | "li"
            | "tr" => self.block(0),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let gap = if self.heading <= 2 { 12 } else { 8 };
                self.block(gap);
                self.heading = 0;
            }
            "pre" | "listing" | "plaintext" => {
                self.flush();
                self.pre = self.pre.saturating_sub(1);
                self.block(10);
            }
            "ul" | "ol" | "menu" | "dir" => {
                if self.lists.pop().is_some() {
                    self.indent -= INDENT;
                }
                self.block(if self.lists.is_empty() { 12 } else { 0 });
            }
            "dt" => {
                self.in_dt = false;
                self.block(0);
            }
            "dd" => {
                self.block(4);
                self.set_dd(false);
            }
            "blockquote" => {
                self.block(12);
                if self.quotes.pop().is_some() {
                    self.indent -= INDENT;
                }
            }
            "table" => {
                self.in_th = false;
                self.block(10);
            }
            "th" => self.in_th = false,
            "b" | "strong" => self.bold = self.bold.saturating_sub(1),
            "code" | "tt" | "kbd" | "samp" | "var" => self.mono = self.mono.saturating_sub(1),
            "small" | "sub" | "sup" => self.small = self.small.saturating_sub(1),
            "a" => self.link = None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p4os_sdk::gui::font::builtin;

    fn texts(p: &Page) -> Vec<String> {
        p.lines.iter().map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<Vec<_>>().join("|")).collect()
    }

    #[test]
    fn blocks_links_and_title() {
        let fonts = builtin::load();
        let p = layout_html(
            "<html><head><title> My  Page </title><meta charset=utf-8></head><body>\
             <h1>Hello</h1><p>Go <a href=\"../x.html\">there</a> now.</p>\
             <ul><li>one<li>two</ul><img src=a.png alt=Logo></body>",
            "http://h.com/a/b/c.html",
            &fonts,
            560,
        );
        assert_eq!(p.title, "My Page");
        assert_eq!(p.links, ["http://h.com/a/x.html"]);
        assert_eq!(texts(&p), ["Hello", "Go|there|now.", "|one", "|two", "[image: Logo]"]);
        assert_eq!(p.lines[0].runs[0].style.face, Face::Title);
        assert_eq!(p.lines[1].runs[1].style.link, Some(0));
        assert!(matches!(p.lines[2].runs[0].kind, RunKind::Bullet(_)));
        // Lines are stacked top to bottom.
        assert!(p.lines.windows(2).all(|w| w[1].y >= w[0].y + w[0].h));
    }

    #[test]
    fn wraps_and_keeps_pre_unwrapped() {
        let fonts = builtin::load();
        let long = "word ".repeat(100);
        let p = layout_html(&alloc::format!("<p>{long}</p><pre>\na  b\n\tc {long}</pre>"), "", &fonts, 300);
        let pre: Vec<_> = p.lines.iter().filter(|l| l.pre).collect();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0].runs[0].text, "a  b");
        assert!(pre[1].runs[0].text.starts_with("        c word"));
        assert!(p.width > 300);
        for l in p.lines.iter().filter(|l| !l.pre) {
            assert!(l.runs.iter().all(|r| r.x + r.w <= 300), "line overflows");
        }
    }

    #[test]
    fn ordered_lists_and_plain_text() {
        let fonts = builtin::load();
        let p = layout_html("<ol start=3><li>a</li><li>b</li></ol>", "", &fonts, 400);
        assert_eq!(texts(&p), ["3.|a", "4.|b"]);
        let p = layout_plain("x\n\ny <b>", &fonts, 400);
        assert_eq!(texts(&p), ["x", "", "y <b>"]);
        assert!(p.lines[2].y >= p.lines[1].y + p.lines[1].h);
    }
}
