//! YouTube: browse with the official Data API v3.
//!
//! Tabs: Home (subscriptions feed when signed in, trending otherwise),
//! Search, Library (liked videos, playlists, subscribed channels) and
//! Account (Google sign-in with a code entered on a phone). Videos open a
//! detail page; "Play" goes through the [`player`] seam.
//!
//! Settings (set on the serial console with `config set`):
//! `youtube.client_id` and `youtube.client_secret` for sign-in,
//! `youtube.api_key` for browsing without an account, `youtube.region`
//! for the trending chart. The app stores `youtube.refresh_token` after
//! sign-in and `youtube.quota` (units spent today).

pub mod api;
mod client;
pub mod feed;
pub mod json;
pub mod oauth;
pub mod player;
pub mod qr;
pub mod quota;
mod thumbs;

#[cfg(test)]
mod testing;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use api::{Collection, CollectionKind, Video};
use client::Client;
use oauth::{Auth, Credentials, Event, State};
use p4os_sdk::config;
use p4os_sdk::gui::font::Font;
use p4os_sdk::gui::ui::Align;
use p4os_sdk::{App, AppInfo, Canvas, Color, DateTime, Image, Orientation, Point, Rect, System, Ui};
use player::Player;
use qr::Qr;
use quota::Quota;
use thumbs::Thumbs;

pub const INFO: AppInfo = AppInfo { id: "youtube", name: "YouTube", icon, create };

fn create() -> Box<dyn App> {
    Box::new(YouTube::new())
}

const RED: Color = Color::hex(0xff0033);

fn icon(c: &mut Canvas, r: Rect) {
    crate::icons::tile(c, r, Color::hex(0xff4e45), Color::hex(0xc4002b));
    let w = r.w * 6 / 10;
    let h = w * 7 / 10;
    let body = Rect::new(r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h);
    c.fill_round_rect(body, h / 4, Color::WHITE);
    let ctr = body.center();
    let s = h / 4;
    c.fill_triangle([Point::new(ctr.x - s * 3 / 4, ctr.y - s), Point::new(ctr.x - s * 3 / 4, ctr.y + s), Point::new(ctr.x + s, ctr.y)], RED);
}

// ---------------------------------------------------------------------------
// State

const PAD: i32 = 12;
const TAB_H: i32 = 64;
/// Width of the tab rail in landscape.
const RAIL_W: i32 = 96;
const HEADER_H: i32 = 56;
const ROW_H: i32 = 108;
const THUMB_W: i32 = 160;
const THUMB_H: i32 = 90;
const FOOTER_H: i32 = 72;
/// Shown instead of trending when there is neither an API key nor a
/// signed-in account.
const NO_KEY_TITLE: &str = "No API key set";
const NO_KEY_DETAIL: &str = "Trending and search without an account need a YouTube Data API key. On the serial console run: config set youtube.api_key <KEY>. Or sign in on the Account tab.";
/// Save the quota count at least every this many units.
const QUOTA_SAVE_STEP: u32 = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Home,
    Search,
    Library,
    Account,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shelf {
    Liked,
    Playlists,
    Channels,
}

/// What an API answer is for. Lists are found by their generation number,
/// which changes on every reload so late answers are dropped.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Videos,
    Search,
    PlaylistItems,
    Stats,
    Collections,
    Uploads,
    Feed(feed::Step),
    Detail,
}

type Tag = (u32, Kind);

struct Listing<T> {
    items: Vec<T>,
    next: Option<String>,
    loading: bool,
    loaded: bool,
    /// The request in flight adds a page instead of replacing the list.
    appending: bool,
    error: Option<String>,
    scroll: i32,
    epoch: u32,
}

impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self { items: Vec::new(), next: None, loading: false, loaded: false, appending: false, error: None, scroll: 0, epoch: 0 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Feed,
    Trending,
    Search(String),
    Liked,
    Playlist(String),
    /// A channel: its uploads playlist is looked up first.
    Channel(String),
}

struct VideoList {
    source: Source,
    list: Listing<Video>,
}

impl VideoList {
    fn new(source: Source) -> Self {
        Self { source, list: Listing::default() }
    }
}

struct Detail {
    video: Video,
    epoch: u32,
    scroll: i32,
    loading: bool,
    error: Option<String>,
}

enum Screen {
    Videos { title: String, videos: VideoList },
    Detail(Detail),
    Player(Box<dyn Player>),
}

/// What the user did this frame; applied after drawing.
enum Action {
    Tab(Tab),
    Shelf(Shelf),
    Back,
    OpenVideo(Video),
    OpenCollection(Collection),
    Play(Video),
    /// Load a list again (epoch), or its next page.
    Reload(u32),
    More(u32),
    Search,
    SignIn,
    CancelSignIn,
    SignOut,
}

pub struct YouTube {
    started: bool,
    client: Client<Tag>,
    thumbs: Thumbs,
    region: String,
    tab: Tab,
    shelf: Shelf,
    home_feed: VideoList,
    feed_builder: Option<feed::Builder>,
    feed_at: Option<u64>,
    trending: VideoList,
    search: VideoList,
    query: String,
    liked: VideoList,
    playlists: Listing<Collection>,
    channels: Listing<Collection>,
    stack: Vec<Screen>,
    device_qr: Option<(String, Qr)>,
    notice: Option<String>,
    next_epoch: u32,
    quota_saved: u32,
    now_ms: u64,
    now: Option<DateTime>,
    /// State changed after drawing; draw again at once.
    redraw: bool,
}

impl YouTube {
    fn new() -> Self {
        Self {
            started: false,
            client: Client::new(Auth::new(None, None), None, Quota::default()),
            thumbs: Thumbs::default(),
            region: "VN".into(),
            tab: Tab::Home,
            shelf: Shelf::Liked,
            home_feed: VideoList::new(Source::Feed),
            feed_builder: None,
            feed_at: None,
            trending: VideoList::new(Source::Trending),
            search: VideoList::new(Source::Search(String::new())),
            query: String::new(),
            liked: VideoList::new(Source::Liked),
            playlists: Listing::default(),
            channels: Listing::default(),
            stack: Vec::new(),
            device_qr: None,
            notice: None,
            next_epoch: 1,
            quota_saved: 0,
            now_ms: 0,
            now: None,
            redraw: false,
        }
    }

    /// Read settings on the first frame (`create` has no `System`).
    fn start(&mut self, sys: &mut dyn System) {
        self.started = true;
        let get = |k: &str| sys.config_get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let creds = match (get(config::YOUTUBE_CLIENT_ID), get(config::YOUTUBE_CLIENT_SECRET)) {
            (Some(client_id), Some(client_secret)) => Some(Credentials { client_id, client_secret }),
            _ => None,
        };
        let quota = get(config::YOUTUBE_QUOTA).map_or_else(Quota::default, |v| Quota::from_config(&v, quota::DAILY_LIMIT));
        self.quota_saved = quota.used();
        self.region = get(config::YOUTUBE_REGION).unwrap_or_else(|| "VN".into());
        let auth = Auth::new(creds, get(config::YOUTUBE_REFRESH_TOKEN));
        self.client = Client::new(auth, get(config::YOUTUBE_API_KEY), quota);
    }

    fn new_epoch(&mut self) -> u32 {
        self.next_epoch += 1;
        self.next_epoch
    }

    fn save_quota(&mut self, sys: &mut dyn System, force: bool) {
        let used = self.client.quota.used();
        if used != self.quota_saved && (force || used >= self.quota_saved + QUOTA_SAVE_STEP || used < self.quota_saved) {
            if let Some(v) = self.client.quota.to_config() {
                sys.config_set(config::YOUTUBE_QUOTA, &v);
                self.quota_saved = used;
            }
        }
    }

    /// Collect finished calls and send queued ones; true if anything
    /// changed.
    fn pump_client(&mut self, sys: &mut dyn System) -> bool {
        let (done, event) = self.client.tick(sys.net(), self.now_ms, self.now);
        let changed = event.is_some() || !done.is_empty();
        if let Some(e) = event {
            self.handle_event(e, sys);
        }
        for (tag, result) in done {
            self.handle_done(tag, result);
        }
        changed
    }

    /// Forget everything that belongs to the signed-in user.
    fn forget_user_data(&mut self) {
        self.home_feed = VideoList::new(Source::Feed);
        self.feed_builder = None;
        self.feed_at = None;
        self.liked = VideoList::new(Source::Liked);
        self.playlists = Listing::default();
        self.channels = Listing::default();
        self.stack.clear();
    }

    // -----------------------------------------------------------------
    // Requests

    fn request_videos(&mut self, which: ListRef, more: bool) {
        let epoch = self.new_epoch();
        let region = self.region.clone();
        let Some(vl) = self.video_list(which) else { return };
        let page = if more { vl.list.next.clone() } else { None };
        if more && page.is_none() {
            return;
        }
        if !more {
            vl.list.epoch = epoch;
        }
        vl.list.loading = true;
        vl.list.appending = more;
        vl.list.error = None;
        let tag_epoch = vl.list.epoch;
        let page = page.as_deref();
        let (call, kind) = match &vl.source {
            Source::Feed => {
                let (builder, (call, step)) = feed::Builder::start();
                self.feed_builder = Some(builder);
                self.client.request(call, (tag_epoch, Kind::Feed(step)));
                return;
            }
            Source::Trending => (api::trending(&region, page), Kind::Videos),
            Source::Search(q) => (api::search(q, page), Kind::Search),
            Source::Liked => (api::liked(page), Kind::Videos),
            Source::Playlist(id) => (api::playlist_items(id, api::PAGE_SIZE, page), Kind::PlaylistItems),
            Source::Channel(id) => (api::uploads(&[id.as_str()]), Kind::Uploads),
        };
        self.client.request(call, (tag_epoch, kind));
    }

    fn request_collections(&mut self, shelf: Shelf, more: bool) {
        let epoch = self.new_epoch();
        let l = if shelf == Shelf::Playlists { &mut self.playlists } else { &mut self.channels };
        let page = if more { l.next.clone() } else { None };
        if more && page.is_none() {
            return;
        }
        if !more {
            l.epoch = epoch;
        }
        l.loading = true;
        l.appending = more;
        l.error = None;
        let call = match shelf {
            Shelf::Playlists => api::my_playlists(page.as_deref()),
            _ => api::subscriptions(api::PAGE_SIZE, page.as_deref()),
        };
        self.client.request(call, (l.epoch, Kind::Collections));
    }

    fn video_list(&mut self, which: ListRef) -> Option<&mut VideoList> {
        match which {
            ListRef::Feed => Some(&mut self.home_feed),
            ListRef::Trending => Some(&mut self.trending),
            ListRef::Search => Some(&mut self.search),
            ListRef::Liked => Some(&mut self.liked),
            ListRef::Screen(i) => match self.stack.get_mut(i) {
                Some(Screen::Videos { videos, .. }) => Some(videos),
                _ => None,
            },
        }
    }

    /// The list a generation number belongs to.
    fn find_list(&mut self, epoch: u32) -> Option<ListRef> {
        let lists = [
            (ListRef::Feed, self.home_feed.list.epoch),
            (ListRef::Trending, self.trending.list.epoch),
            (ListRef::Search, self.search.list.epoch),
            (ListRef::Liked, self.liked.list.epoch),
        ];
        if let Some((r, _)) = lists.iter().find(|(_, g)| *g == epoch) {
            return Some(*r);
        }
        // A playlist or channel screen, possibly below a video page.
        self.stack
            .iter()
            .position(|s| matches!(s, Screen::Videos { videos, .. } if videos.list.epoch == epoch))
            .map(ListRef::Screen)
    }

    /// Start loading whatever the visible page shows and has not loaded.
    fn ensure_loaded(&mut self) {
        if !self.stack.is_empty() {
            return;
        }
        let signed_in = self.client.auth.is_signed_in();
        match self.tab {
            Tab::Home if signed_in => {
                let stale = self.feed_at.is_some_and(|t| self.now_ms.saturating_sub(t) > feed::CACHE_MS);
                let l = &self.home_feed.list;
                if !l.loading && l.error.is_none() && (!l.loaded || stale) {
                    self.request_videos(ListRef::Feed, false);
                }
            }
            Tab::Home => {
                let l = &self.trending.list;
                if self.client.can_browse() && !l.loading && !l.loaded && l.error.is_none() {
                    self.request_videos(ListRef::Trending, false);
                }
            }
            Tab::Library if signed_in => {
                let (loading, loaded, failed) = match self.shelf {
                    Shelf::Liked => (self.liked.list.loading, self.liked.list.loaded, self.liked.list.error.is_some()),
                    Shelf::Playlists => (self.playlists.loading, self.playlists.loaded, self.playlists.error.is_some()),
                    Shelf::Channels => (self.channels.loading, self.channels.loaded, self.channels.error.is_some()),
                };
                if !loading && !loaded && !failed {
                    match self.shelf {
                        Shelf::Liked => self.request_videos(ListRef::Liked, false),
                        s => self.request_collections(s, false),
                    }
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------
    // Responses

    fn handle_event(&mut self, event: Event, sys: &mut dyn System) {
        match event {
            Event::CodeReady => {
                if let State::WaitingForUser(code) = self.client.auth.state() {
                    let url = code.verification_url.clone();
                    self.device_qr = Qr::encode(&url).map(|q| (url, q));
                }
                self.notice = None;
            }
            Event::SignedIn { refresh_token } => {
                sys.config_set(config::YOUTUBE_REFRESH_TOKEN, &refresh_token);
                if self.tab == Tab::Account && self.device_qr.is_some() {
                    self.forget_user_data();
                    self.tab = Tab::Home;
                }
                self.device_qr = None;
                self.notice = None;
            }
            Event::Denied => self.sign_in_ended("Sign-in was declined on the phone."),
            Event::Expired => self.sign_in_ended("The code expired before sign-in finished. Try again."),
            Event::SessionEnded => {
                sys.config_remove(config::YOUTUBE_REFRESH_TOKEN);
                self.forget_user_data();
                self.notice = Some("Your Google sign-in expired or was revoked. Sign in again.".into());
            }
            Event::Failed(msg) => {
                self.device_qr = None;
                self.notice = Some(msg);
            }
        }
    }

    fn sign_in_ended(&mut self, msg: &str) {
        self.device_qr = None;
        self.notice = Some(msg.into());
    }

    fn handle_done(&mut self, (epoch, kind): Tag, result: Result<Vec<u8>, String>) {
        match kind {
            Kind::Collections => {
                if self.playlists.epoch == epoch {
                    apply_page(&mut self.playlists, result.and_then(|b| api::parse_playlists(&b)));
                } else if self.channels.epoch == epoch {
                    apply_page(&mut self.channels, result.and_then(|b| api::parse_subscriptions(&b)));
                }
            }
            Kind::Detail => {
                let Some(Screen::Detail(d)) = self.stack.iter_mut().rev().find(|s| matches!(s, Screen::Detail(d) if d.epoch == epoch))
                else {
                    return;
                };
                d.loading = false;
                match result.and_then(|b| api::parse_videos(&b)) {
                    Ok(page) => match page.items.into_iter().find(|v| v.id == d.video.id) {
                        Some(v) => d.video = v,
                        None => d.error = Some("This video is private or was removed.".into()),
                    },
                    Err(e) => d.error = Some(e),
                }
            }
            Kind::Feed(step) => {
                if self.home_feed.list.epoch != epoch {
                    return;
                }
                let Some(builder) = self.feed_builder.as_mut() else { return };
                match builder.on_response(step, result) {
                    feed::Progress::Calls(calls) => {
                        for (call, step) in calls {
                            self.client.request(call, (epoch, Kind::Feed(step)));
                        }
                    }
                    feed::Progress::Wait => {}
                    feed::Progress::Done(videos) => {
                        self.feed_builder = None;
                        self.feed_at = Some(self.now_ms);
                        apply_page(&mut self.home_feed.list, Ok(api::Page { items: videos, next: None }));
                    }
                    feed::Progress::Failed(e) => {
                        self.feed_builder = None;
                        apply_page::<Video>(&mut self.home_feed.list, Err(e));
                    }
                }
            }
            Kind::Videos | Kind::Search | Kind::PlaylistItems | Kind::Stats | Kind::Uploads => {
                let Some(which) = self.find_list(epoch) else { return };
                self.handle_video_page(which, kind, result);
            }
        }
    }

    fn handle_video_page(&mut self, which: ListRef, kind: Kind, result: Result<Vec<u8>, String>) {
        let Some(vl) = self.video_list(which) else { return };
        let epoch = vl.list.epoch;
        match kind {
            Kind::Stats => {
                if let Ok(page) = result.and_then(|b| api::parse_videos(&b)) {
                    api::merge_stats(&mut vl.list.items, &page.items);
                }
            }
            Kind::Uploads => {
                let Source::Channel(channel) = &vl.source else { return };
                let uploads = result
                    .and_then(|b| api::parse_uploads(&b))
                    .map(|pairs| pairs.into_iter().find(|(c, _)| c == channel).map(|(_, playlist)| playlist));
                match uploads {
                    Ok(Some(playlist)) => {
                        let call = api::playlist_items(&playlist, api::PAGE_SIZE, None);
                        vl.source = Source::Playlist(playlist);
                        self.client.request(call, (epoch, Kind::PlaylistItems));
                    }
                    Ok(None) => apply_page::<Video>(&mut vl.list, Err("This channel has no public uploads.".into())),
                    Err(e) => apply_page::<Video>(&mut vl.list, Err(e)),
                }
            }
            _ => {
                let parsed = result.and_then(|b| match kind {
                    Kind::Search => api::parse_search(&b),
                    Kind::PlaylistItems => api::parse_playlist_items(&b),
                    _ => api::parse_videos(&b),
                });
                let first_new = if vl.list.appending { vl.list.items.len() } else { 0 };
                apply_page(&mut vl.list, parsed);
                // Search results and playlist items lack durations and views.
                if matches!(kind, Kind::Search | Kind::PlaylistItems) {
                    let ids: Vec<&str> = vl.list.items[first_new.min(vl.list.items.len())..].iter().map(|v| v.id.as_str()).collect();
                    if !ids.is_empty() {
                        let call = api::stats(&ids);
                        self.client.request(call, (epoch, Kind::Stats));
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Actions

    fn apply(&mut self, action: Action, sys: &mut dyn System) {
        match action {
            Action::Tab(t) => {
                self.tab = t;
                self.stack.clear();
            }
            Action::Shelf(s) => self.shelf = s,
            Action::Back => {
                if let Some(Screen::Player(mut p)) = self.stack.pop() {
                    p.close(sys);
                }
            }
            Action::OpenVideo(video) => {
                let epoch = self.new_epoch();
                self.client.request(api::detail(&video.id), (epoch, Kind::Detail));
                self.stack.push(Screen::Detail(Detail { video, epoch, scroll: 0, loading: true, error: None }));
            }
            Action::OpenCollection(c) => {
                let source = match c.kind {
                    CollectionKind::Playlist => Source::Playlist(c.id),
                    CollectionKind::Channel => Source::Channel(c.id),
                };
                self.stack.push(Screen::Videos { title: c.title, videos: VideoList::new(source) });
                self.request_videos(ListRef::Screen(self.stack.len() - 1), false);
            }
            Action::Play(video) => self.stack.push(Screen::Player(player::open(&video))),
            Action::Reload(epoch) | Action::More(epoch) => {
                let more = matches!(action, Action::More(_));
                if let Some(which) = self.find_list(epoch) {
                    if which == ListRef::Feed {
                        self.feed_at = None;
                    }
                    self.request_videos(which, more);
                } else if epoch == self.playlists.epoch {
                    self.request_collections(Shelf::Playlists, more);
                } else if epoch == self.channels.epoch {
                    self.request_collections(Shelf::Channels, more);
                }
            }
            Action::Search => {
                let q = self.query.trim().to_string();
                if !q.is_empty() {
                    self.search = VideoList::new(Source::Search(q));
                    self.request_videos(ListRef::Search, false);
                }
            }
            Action::SignIn => {
                self.notice = None;
                if let Err(e) = self.client.auth.begin_sign_in(sys.net()) {
                    self.notice = Some(e);
                }
            }
            Action::CancelSignIn => {
                self.client.auth.cancel();
                self.device_qr = None;
            }
            Action::SignOut => {
                self.client.auth.sign_out(sys.net());
                sys.config_remove(config::YOUTUBE_REFRESH_TOKEN);
                self.forget_user_data();
                self.notice = Some("Signed out. Access was revoked and the token deleted.".into());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListRef {
    Feed,
    Trending,
    Search,
    Liked,
    /// A video list screen at this index of the screen stack.
    Screen(usize),
}

fn apply_page<T>(l: &mut Listing<T>, result: Result<api::Page<T>, String>) {
    l.loading = false;
    match result {
        Ok(page) => {
            if l.appending {
                l.items.extend(page.items);
            } else {
                l.items = page.items;
                l.scroll = 0;
            }
            l.next = page.next;
            l.loaded = true;
            l.error = None;
        }
        Err(e) => l.error = Some(e),
    }
    l.appending = false;
}

// ---------------------------------------------------------------------------
// Drawing helpers

/// `text` cut to fit `w` pixels, with "..." when shortened.
fn ellipsize(f: &Font, text: &str, w: i32) -> String {
    if f.measure(text) <= w {
        return text.into();
    }
    let dots = f.measure("...");
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cw = f.char_width(c);
        if used + cw + dots > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.truncate(out.trim_end().len());
    out.push_str("...");
    out
}

/// Word-wrapped `text`, at most `max` lines, the last one ellipsized.
fn wrap_lines(f: &Font, text: &str, w: i32, max: usize) -> Vec<String> {
    let mut lines = f.wrap(text, w);
    lines.retain(|l| !l.is_empty());
    if lines.len() > max {
        let rest = lines[max - 1..].join(" ");
        lines.truncate(max - 1);
        lines.push(ellipsize(f, &rest, w));
    }
    lines
}

fn draw_lines(ui: &mut Ui, x: i32, y: &mut i32, w: i32, lines: &[String], f: &Font, color: Color) {
    for line in lines {
        ui.text(Rect::new(x, *y, w, f.line_height), line, f, color, Align::Left);
        *y += f.line_height;
    }
}

fn draw_image(c: &mut Canvas, r: Rect, img: Option<&Image>, bg: Color) {
    match img {
        Some(img) if img.width == r.w && img.height == r.h => c.blit(Point::new(r.x, r.y), &img.pixels, img.width, img.height),
        Some(img) => {
            c.fill_rect(r, Color::BLACK);
            // Keep the aspect ratio inside r.
            let (w, h) = if img.width * r.h > img.height * r.w {
                (r.w, img.height * r.w / img.width.max(1))
            } else {
                (img.width * r.h / img.height.max(1), r.h)
            };
            let dst = Rect::new(r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h);
            if w == img.width && h == img.height {
                c.blit(Point::new(dst.x, dst.y), &img.pixels, w, h);
            } else {
                c.blit_scaled(dst, &img.pixels, img.width, img.height);
            }
        }
        None => {
            c.fill_rect(r, bg);
            let ctr = r.center();
            let s = r.h.min(r.w) / 8;
            c.fill_triangle(
                [Point::new(ctr.x - s, ctr.y - s), Point::new(ctr.x - s, ctr.y + s), Point::new(ctr.x + s, ctr.y)],
                bg.lighten(40),
            );
        }
    }
}

fn badge(ui: &mut Ui, thumb: Rect, text: &str, bg: Color) {
    let f = ui.fonts.small;
    let w = f.measure(text) + 10;
    let r = Rect::new(thumb.right() - w - 4, thumb.bottom() - f.line_height - 6, w, f.line_height + 2);
    ui.canvas.fill_round_rect(r, 4, bg);
    ui.text(r, text, &f, Color::WHITE, Align::Center);
}

fn video_row(ui: &mut Ui, row: Rect, v: &Video, thumbs: &mut Thumbs, now: Option<DateTime>) {
    let t = *ui.theme;
    let th = Rect::new(row.x + PAD, row.y + (row.h - THUMB_H) / 2, THUMB_W, THUMB_H);
    draw_image(ui.canvas, th, thumbs.get(&api::thumb_url(&v.id), THUMB_W, THUMB_H), t.surface_alt);
    match v.duration_s {
        Some(0) => badge(ui, th, "LIVE", RED),
        Some(d) => badge(ui, th, &api::format_duration(d), Color::hex(0x000000)),
        None => {}
    }
    let x = th.right() + PAD;
    let w = row.right() - PAD - x;
    let (body, small) = (ui.fonts.body, ui.fonts.small);
    let mut y = row.y + 6;
    draw_lines(ui, x, &mut y, w, &wrap_lines(&body, &v.title, w, 2), &body, t.text);
    y += 2;
    let lines = [ellipsize(&small, &v.channel, w), ellipsize(&small, &api::video_meta(v, now), w)];
    draw_lines(ui, x, &mut y, w, &lines, &small, t.text_dim);
}

fn collection_row(ui: &mut Ui, row: Rect, c: &Collection, thumbs: &mut Thumbs) {
    let t = *ui.theme;
    let th = Rect::new(row.x + PAD, row.y + (row.h - THUMB_H) / 2, THUMB_W, THUMB_H);
    let img = c.thumb.as_deref().and_then(|u| thumbs.get(u, THUMB_W, THUMB_H));
    draw_image(ui.canvas, th, img, t.surface_alt);
    let x = th.right() + PAD;
    let w = row.right() - PAD - x;
    let (body, small) = (ui.fonts.body, ui.fonts.small);
    let mut y = row.y + 14;
    draw_lines(ui, x, &mut y, w, &wrap_lines(&body, &c.title, w, 2), &body, t.text);
    y += 4;
    draw_lines(ui, x, &mut y, w, &[ellipsize(&small, &c.subtitle, w)], &small, t.text_dim);
}

/// Centred message with an optional button; returns true when tapped.
fn message(ui: &mut Ui, r: Rect, title: &str, detail: &str, button: Option<&str>) -> bool {
    let t = *ui.theme;
    let (bold, body) = (ui.fonts.bold, ui.fonts.body);
    let w = (r.w - 2 * 24).min(560);
    let x = r.x + (r.w - w) / 2;
    let lines = wrap_lines(&body, detail, w, 8);
    let h = bold.line_height + 10 + lines.len() as i32 * body.line_height + if button.is_some() { 76 } else { 0 };
    let mut y = r.y + ((r.h - h) / 2).max(12);
    ui.text(Rect::new(x, y, w, bold.line_height), title, &bold, t.text, Align::Center);
    y += bold.line_height + 10;
    for line in &lines {
        ui.text(Rect::new(x, y, w, body.line_height), line, &body, t.text_dim, Align::Center);
        y += body.line_height;
    }
    match button {
        Some(label) => ui.button(Rect::new(r.x + (r.w - 220) / 2, y + 20, 220, 56), label),
        None => false,
    }
}

enum ListAction {
    Open(usize),
    More,
    Reload,
}

/// A scrolling list with loading, error and "load more" states.
fn listing<T>(
    ui: &mut Ui,
    r: Rect,
    l: &mut Listing<T>,
    empty: &str,
    more_label: &str,
    mut row: impl FnMut(&mut Ui, Rect, &T),
) -> Option<ListAction> {
    if l.items.is_empty() {
        if l.loading {
            message(ui, r, "Loading...", "", None);
        } else if let Some(e) = l.error.clone() {
            if message(ui, r, "Something went wrong", &e, Some("Try again")) {
                return Some(ListAction::Reload);
            }
        } else if l.loaded {
            message(ui, r, "Nothing here", empty, None);
        }
        return None;
    }
    let footer = l.loading || l.error.is_some() || l.next.is_some();
    let content_h = l.items.len() as i32 * ROW_H + if footer { FOOTER_H } else { 0 };
    let off = ui.scroll(r, content_h, &mut l.scroll);
    let t = *ui.theme;
    let old = ui.canvas.set_clip(r.intersect(&ui.canvas.clip()));
    let mut action = None;
    let first = (off / ROW_H) as usize;
    for (i, item) in l.items.iter().enumerate().skip(first) {
        let y = r.y + i as i32 * ROW_H - off;
        if y > r.bottom() {
            break;
        }
        let rr = Rect::new(r.x, y, r.w - 8, ROW_H);
        let i_ = ui.input;
        if i_.down && rr.contains(i_.pos) && rr.contains(i_.press_pos) && !i_.is_drag() && r.contains(i_.pos) {
            ui.canvas.fill_rect(rr, t.surface);
        }
        if i_.tapped_in(rr) && r.contains(i_.pos) {
            action = Some(ListAction::Open(i));
        }
        row(ui, rr, item);
        ui.canvas.fill_rect(Rect::new(rr.x + PAD, rr.bottom() - 1, rr.w - 2 * PAD, 1), t.border);
    }
    if footer {
        let fy = r.y + l.items.len() as i32 * ROW_H - off;
        let fr = Rect::new(r.x + PAD, fy + 10, r.w - 2 * PAD - 8, FOOTER_H - 20);
        if fr.y < r.bottom() {
            let small = ui.fonts.small;
            if l.loading {
                ui.text(fr, "Loading more...", &small, t.text_dim, Align::Center);
            } else if let Some(e) = &l.error {
                let label = ellipsize(&small, &format!("{e} Tap to retry."), fr.w);
                if ui.button_colored(fr, "", t.surface, t.text) {
                    action = Some(ListAction::More);
                }
                ui.text(fr, &label, &small, t.danger, Align::Center);
            } else if ui.button(fr, more_label) {
                action = Some(ListAction::More);
            }
        }
    }
    ui.canvas.restore_clip(old);
    action
}

fn header(ui: &mut Ui, r: Rect, title: &str, back: bool, refresh: bool) -> (bool, bool) {
    let t = *ui.theme;
    ui.fill(r, t.bg);
    let mut x = r.x + PAD;
    let mut back_tapped = false;
    if back {
        let b = Rect::new(r.x + 4, r.y + 4, 52, r.h - 8);
        back_tapped = ui.icon_button(b, |c, b, col| {
            let cy = b.y + b.h / 2;
            let cx = b.x + b.w / 2 + 3;
            c.line(Point::new(cx + 5, cy - 10), Point::new(cx - 5, cy), 3.0, col);
            c.line(Point::new(cx - 5, cy), Point::new(cx + 5, cy + 10), 3.0, col);
        });
        x = b.right() + 4;
    }
    let mut refresh_tapped = false;
    let mut right = r.right() - PAD;
    if refresh {
        let b = Rect::new(r.right() - 56, r.y + 4, 52, r.h - 8);
        refresh_tapped = ui.icon_button(b, |c, b, col| {
            let ctr = b.center();
            c.stroke_circle(ctr, 10.0, 3.0, col);
            c.fill_rect(Rect::new(ctr.x + 1, ctr.y - 14, 14, 12), t.bg);
            c.fill_triangle([Point::new(ctr.x - 1, ctr.y - 16), Point::new(ctr.x - 1, ctr.y - 4), Point::new(ctr.x + 7, ctr.y - 10)], col);
        });
        right = b.x - 4;
    }
    let bold = ui.fonts.bold;
    let label = ellipsize(&bold, title, right - x);
    ui.text(Rect::new(x, r.y, right - x, r.h), &label, &bold, t.text, Align::Left);
    (back_tapped, refresh_tapped)
}

fn tab_glyph(c: &mut Canvas, tab: Tab, ctr: Point, col: Color, bg: Color) {
    let (x, y) = (ctr.x, ctr.y);
    match tab {
        Tab::Home => {
            c.fill_triangle([Point::new(x - 12, y - 1), Point::new(x + 12, y - 1), Point::new(x, y - 12)], col);
            c.fill_rect(Rect::new(x - 8, y - 2, 16, 12), col);
            c.fill_rect(Rect::new(x - 3, y + 3, 6, 7), bg);
        }
        Tab::Search => {
            c.stroke_circle(Point::new(x - 3, y - 3), 7.0, 2.5, col);
            c.line(Point::new(x + 2, y + 2), Point::new(x + 9, y + 9), 3.0, col);
        }
        Tab::Library => {
            for i in 0..3 {
                c.fill_rect(Rect::new(x - 11, y - 10 + i * 8, 22, 4), col);
            }
            c.fill_triangle([Point::new(x + 4, y + 2), Point::new(x + 4, y + 12), Point::new(x + 12, y + 7)], col);
        }
        Tab::Account => {
            c.fill_circle(Point::new(x, y - 5), 5.5, col);
            c.fill_round_rect(Rect::new(x - 10, y + 3, 20, 10), 5, col);
        }
    }
}

/// Tabs along the bottom, or down the left side (`rail`) in landscape
/// where height is scarce.
fn tab_bar(ui: &mut Ui, r: Rect, current: Tab, rail: bool) -> Option<Tab> {
    let t = *ui.theme;
    ui.fill(r, t.surface);
    let edge = if rail { Rect::new(r.right() - 1, r.y, 1, r.h) } else { Rect::new(r.x, r.y, r.w, 1) };
    ui.canvas.fill_rect(edge, t.border);
    let tabs = [(Tab::Home, "Home"), (Tab::Search, "Search"), (Tab::Library, "Library"), (Tab::Account, "Account")];
    let n = tabs.len() as i32;
    let mut picked = None;
    for (i, (tab, label)) in tabs.iter().enumerate() {
        let i = i as i32;
        let cell = if rail {
            Rect::new(r.x, r.y + i * r.h / n, r.w - 1, r.h / n)
        } else {
            Rect::new(r.x + i * r.w / n, r.y, r.w / n, r.h)
        };
        let top = cell.y + (cell.h - TAB_H) / 2;
        let (held, clicked) = ui.interact(cell);
        if held {
            ui.canvas.fill_rect(cell, t.surface_alt);
        }
        let col = if *tab == current { t.text } else { t.text_dim };
        let bg = if held { t.surface_alt } else { t.surface };
        tab_glyph(ui.canvas, *tab, Point::new(cell.center().x, top + 22), col, bg);
        let f = ui.fonts.small;
        ui.text(Rect::new(cell.x, top + 38, cell.w, 20), label, &f, col, Align::Center);
        if *tab == current {
            let mark = if rail {
                Rect::new(cell.x, cell.y + cell.h / 4, 3, cell.h / 2)
            } else {
                Rect::new(cell.x + cell.w / 4, cell.y + 1, cell.w / 2, 3)
            };
            ui.canvas.fill_rect(mark, RED);
        }
        if clicked {
            picked = Some(*tab);
        }
    }
    picked
}

// ---------------------------------------------------------------------------
// Pages

impl YouTube {
    fn home(&mut self, ui: &mut Ui, r: Rect) -> Option<Action> {
        let (head, body) = r.split_top(HEADER_H);
        let signed_in = self.client.auth.is_signed_in();
        if !signed_in && !self.client.can_browse() {
            header(ui, head, &format!("Trending in {}", self.region.to_ascii_uppercase()), false, false);
            return message(ui, body, NO_KEY_TITLE, NO_KEY_DETAIL, Some("Account")).then_some(Action::Tab(Tab::Account));
        }
        let title = if signed_in { "Subscriptions".to_string() } else { format!("Trending in {}", self.region.to_ascii_uppercase()) };
        let (_, refresh) = header(ui, head, &title, false, true);
        let now = self.now;
        let (vl, empty) = if signed_in {
            (&mut self.home_feed, "No recent uploads from your subscriptions.")
        } else {
            (&mut self.trending, "No trending videos right now.")
        };
        let epoch = vl.list.epoch;
        if refresh {
            return Some(Action::Reload(epoch));
        }
        let thumbs = &mut self.thumbs;
        let act = listing(ui, body, &mut vl.list, empty, "Load more", |ui, rr, v| video_row(ui, rr, v, thumbs, now));
        list_action(act, epoch, &vl.list.items)
    }

    fn search_page(&mut self, ui: &mut Ui, r: Rect) -> Option<Action> {
        let t = *ui.theme;
        ui.fill(r, t.bg);
        let bar = Rect::new(r.x + PAD, r.y + 8, r.w - 2 * PAD, 52);
        let go = Rect::new(bar.right() - 100, bar.y, 100, bar.h);
        let field = Rect::new(bar.x, bar.y, go.x - 8 - bar.x, bar.h);
        let mut submit = ui.text_field(field, &mut self.query, "Search YouTube");
        submit |= ui.button_primary(go, "Search");
        let body = Rect::new(r.x, bar.bottom() + 8, r.w, r.bottom() - bar.bottom() - 8);
        if submit && !self.query.trim().is_empty() {
            ui.state.focus = None;
            return Some(Action::Search);
        }
        if matches!(&self.search.source, Source::Search(q) if q.is_empty()) {
            let detail = if self.client.can_browse() {
                "Each search uses 100 of the 10,000 quota units YouTube allows per day."
            } else {
                "Sign in, or set youtube.api_key on the console, to search."
            };
            message(ui, body, "Search videos", detail, None);
            return None;
        }
        let now = self.now;
        let epoch = self.search.list.epoch;
        let thumbs = &mut self.thumbs;
        let act = listing(ui, body, &mut self.search.list, "No videos found.", "More results (100 units)", |ui, rr, v| {
            video_row(ui, rr, v, thumbs, now)
        });
        list_action(act, epoch, &self.search.list.items)
    }

    fn library(&mut self, ui: &mut Ui, r: Rect) -> Option<Action> {
        let t = *ui.theme;
        ui.fill(r, t.bg);
        if !self.client.auth.is_signed_in() {
            let detail = "Liked videos, playlists and subscriptions appear here after you sign in.";
            return message(ui, r, "Your library", detail, Some("Sign in")).then_some(Action::Tab(Tab::Account));
        }
        let seg = Rect::new(r.x + PAD, r.y + 8, r.w - 2 * PAD, 48);
        let shelves = [(Shelf::Liked, "Liked"), (Shelf::Playlists, "Playlists"), (Shelf::Channels, "Channels")];
        let w = seg.w / shelves.len() as i32;
        let mut action = None;
        ui.canvas.fill_round_rect(seg, 10, t.surface);
        for (i, (shelf, label)) in shelves.iter().enumerate() {
            let cell = Rect::new(seg.x + i as i32 * w, seg.y, w, seg.h).inset(3);
            let on = *shelf == self.shelf;
            if ui.button_colored(cell, "", if on { t.surface_alt.lighten(20) } else { t.surface }, t.text) {
                action = Some(Action::Shelf(*shelf));
            }
            let f = ui.fonts.body;
            ui.text(cell, label, &f, if on { t.text } else { t.text_dim }, Align::Center);
        }
        let body = Rect::new(r.x, seg.bottom() + 8, r.w, r.bottom() - seg.bottom() - 8);
        let thumbs = &mut self.thumbs;
        let act = match self.shelf {
            Shelf::Liked => {
                let now = self.now;
                let epoch = self.liked.list.epoch;
                let act = listing(ui, body, &mut self.liked.list, "Videos you like appear here.", "Load more", |ui, rr, v| {
                    video_row(ui, rr, v, thumbs, now)
                });
                list_action(act, epoch, &self.liked.list.items)
            }
            shelf => {
                let l = if shelf == Shelf::Playlists { &mut self.playlists } else { &mut self.channels };
                let empty = if shelf == Shelf::Playlists { "You have no playlists." } else { "You have no subscriptions." };
                let epoch = l.epoch;
                match listing(ui, body, l, empty, "Load more", |ui, rr, c| collection_row(ui, rr, c, thumbs)) {
                    Some(ListAction::Open(i)) => l.items.get(i).cloned().map(Action::OpenCollection),
                    Some(ListAction::More) => Some(Action::More(epoch)),
                    Some(ListAction::Reload) => Some(Action::Reload(epoch)),
                    None => None,
                }
            }
        };
        action.or(act)
    }

    fn account(&mut self, ui: &mut Ui, r: Rect) -> Option<Action> {
        let t = *ui.theme;
        ui.fill(r, t.bg);
        let (bold, body, small, huge) = (ui.fonts.bold, ui.fonts.body, ui.fonts.small, ui.fonts.huge);
        let inner = r.inset(20);
        let mut y = inner.y;
        let mut action = None;
        if let Some(n) = self.notice.clone() {
            let lines = wrap_lines(&body, &n, inner.w - 24, 4);
            let h = lines.len() as i32 * body.line_height + 20;
            ui.canvas.fill_round_rect(Rect::new(inner.x, y, inner.w, h), 10, t.surface_alt);
            let mut ty = y + 10;
            draw_lines(ui, inner.x + 12, &mut ty, inner.w - 24, &lines, &body, t.text);
            y += h + 16;
        }
        match self.client.auth.state().clone() {
            State::SignedOut => {
                ui.text(Rect::new(inner.x, y, inner.w, bold.line_height), "Sign in to YouTube", &bold, t.text, Align::Left);
                y += bold.line_height + 10;
                let detail = if self.client.auth.has_credentials() {
                    "You will get a short code to enter on your phone or computer. p4os only asks for read-only access to your YouTube account."
                } else {
                    "Sign-in needs a Google OAuth client of type \"TVs and Limited Input devices\". On the serial console run:\n config set youtube.client_id <ID>\n config set youtube.client_secret <SECRET>\nthen reopen this app."
                };
                for para in detail.split('\n') {
                    draw_lines(ui, inner.x, &mut y, inner.w, &wrap_lines(&body, para, inner.w, 6), &body, t.text_dim);
                }
                y += 20;
                if self.client.auth.has_credentials() && ui.button_primary(Rect::new(inner.x, y, inner.w.min(320), 60), "Sign in with Google") {
                    action = Some(Action::SignIn);
                }
                y += 84;
                let key = if self.client.has_api_key() {
                    "Without an account: trending and search use the API key in youtube.api_key."
                } else {
                    "Without an account: set youtube.api_key to browse trending videos and search."
                };
                draw_lines(ui, inner.x, &mut y, inner.w, &wrap_lines(&small, key, inner.w, 3), &small, t.text_dim);
            }
            State::RequestingCode => {
                message(ui, Rect::new(r.x, y, r.w, 200), "Contacting Google...", "Asking for a sign-in code.", None);
                if ui.button(Rect::new(r.x + (r.w - 200) / 2, y + 220, 200, 56), "Cancel") {
                    action = Some(Action::CancelSignIn);
                }
            }
            State::WaitingForUser(code) => {
                let wide = inner.w > inner.h;
                let qr_side = if wide { (inner.h - (y - inner.y)).min(inner.w / 2 - 20) } else { 240.min(inner.w) };
                let (qr_rect, text) = if wide {
                    (Rect::new(inner.x, y, qr_side, qr_side), Rect::new(inner.x + qr_side + 24, y, inner.w - qr_side - 24, inner.bottom() - y))
                } else {
                    let text_h = inner.bottom() - y - qr_side - 16;
                    (Rect::new(inner.x + (inner.w - qr_side) / 2, y + text_h + 16, qr_side, qr_side), Rect::new(inner.x, y, inner.w, text_h))
                };
                let mut ty = text.y;
                ui.text(Rect::new(text.x, ty, text.w, body.line_height), "On your phone or computer, open", &body, t.text_dim, Align::Left);
                ty += body.line_height + 2;
                let url = code.verification_url.trim_start_matches("https://").to_string();
                ui.text(Rect::new(text.x, ty, text.w, bold.line_height), &url, &bold, t.accent, Align::Left);
                ty += bold.line_height + 10;
                ui.text(Rect::new(text.x, ty, text.w, body.line_height), "and enter this code:", &body, t.text_dim, Align::Left);
                ty += body.line_height + 6;
                let code_box = Rect::new(text.x, ty, text.w, huge.line_height + 16);
                ui.canvas.fill_round_rect(code_box, 12, t.surface);
                ui.text(code_box, &code.user_code, &huge, t.text, Align::Center);
                ty = code_box.bottom() + 10;
                let left = code.expires_at_ms.saturating_sub(self.now_ms) / 1000;
                let status = format!("Waiting for approval... code expires in {}:{:02}", left / 60, left % 60);
                ui.text(Rect::new(text.x, ty, text.w, small.line_height), &status, &small, t.text_dim, Align::Left);
                ty += small.line_height + 12;
                if ui.button(Rect::new(text.x, ty, 160, 52), "Cancel") {
                    action = Some(Action::CancelSignIn);
                }
                if let Some((u, qr)) = &self.device_qr {
                    if *u == code.verification_url {
                        qr.draw(ui.canvas, qr_rect);
                    }
                }
            }
            State::SignedIn => {
                ui.text(Rect::new(inner.x, y, inner.w, bold.line_height), "Signed in", &bold, t.text, Align::Left);
                y += bold.line_height + 8;
                let lines = [
                    "Read-only access to your YouTube account (youtube.readonly).".to_string(),
                    format!(
                        "Quota used today: {} of {} units. A search costs 100, every other request 1.",
                        self.client.quota.used(),
                        self.client.quota.limit()
                    ),
                ];
                for l in &lines {
                    draw_lines(ui, inner.x, &mut y, inner.w, &wrap_lines(&body, l, inner.w, 4), &body, t.text_dim);
                    y += 8;
                }
                y += 16;
                if ui.button_colored(Rect::new(inner.x, y, inner.w.min(260), 58), "Sign out", t.danger, Color::WHITE) {
                    action = Some(Action::SignOut);
                }
            }
        }
        action
    }

    fn detail(d: &mut Detail, ui: &mut Ui, r: Rect, thumbs: &mut Thumbs, now: Option<DateTime>) -> Option<Action> {
        let t = *ui.theme;
        ui.fill(r, t.bg);
        let (large, body, small) = (ui.fonts.large, ui.fonts.body, ui.fonts.small);
        let inner_w = r.w - 2 * PAD;
        let wide = r.w > r.h;
        // Landscape: picture on the left, text beside it.
        let pic_w = if wide { (r.w / 2 - PAD).min(560) } else { inner_w.min(640) };
        let pic_h = pic_w * 9 / 16;
        let text_x = if wide { r.x + PAD + pic_w + PAD } else { r.x + PAD };
        let text_w = if wide { r.right() - PAD - text_x } else { inner_w };
        let v = &d.video;
        let title = wrap_lines(&large, &v.title, text_w, 3);
        let mut meta: Vec<String> = Vec::new();
        let m = api::video_meta(v, now);
        if !m.is_empty() {
            meta.push(m);
        }
        let mut extra = Vec::new();
        if let Some(s) = v.duration_s {
            extra.push(api::format_duration(s));
        }
        if let Some(l) = v.likes {
            extra.push(format!("{} likes", api::format_count(l)));
        }
        if !extra.is_empty() {
            meta.push(extra.join(" · "));
        }
        let desc_w = if wide { text_w } else { inner_w };
        let mut desc: Vec<String> = Vec::new();
        for para in v.description.split('\n').take(60) {
            if para.trim().is_empty() {
                desc.push(String::new());
            } else {
                desc.extend(body.wrap(para, desc_w));
            }
        }
        desc.truncate(80);
        let info_h = title.len() as i32 * large.line_height + 8 + body.line_height + meta.len() as i32 * small.line_height + 16 + 60;
        let top_h = if wide { pic_h.max(info_h) } else { pic_h + PAD + info_h };
        let status_h = if d.loading || d.error.is_some() { body.line_height + 8 } else { 0 };
        let content_h = PAD + top_h + 16 + status_h + desc.len() as i32 * body.line_height + PAD;
        let off = ui.scroll(r, content_h, &mut d.scroll);
        let old = ui.canvas.set_clip(r.intersect(&ui.canvas.clip()));
        let top = r.y + PAD - off;
        let pic = Rect::new(r.x + PAD, top, pic_w, pic_h);
        draw_image(ui.canvas, pic, thumbs.get(&api::thumb_url(&v.id), 320, 180), t.surface_alt);
        let mut y = if wide { top } else { pic.bottom() + PAD };
        draw_lines(ui, text_x, &mut y, text_w, &title, &large, t.text);
        y += 8;
        ui.text(Rect::new(text_x, y, text_w, body.line_height), &ellipsize(&body, &v.channel, text_w), &body, t.accent, Align::Left);
        y += body.line_height;
        draw_lines(ui, text_x, &mut y, text_w, &meta, &small, t.text_dim);
        y += 16;
        let mut action = None;
        if ui.button_colored(Rect::new(text_x, y, 180, 56), "Play", RED, Color::WHITE) {
            action = Some(Action::Play(v.clone()));
        }
        let mut y = top + top_h + 16;
        if d.loading {
            ui.text(Rect::new(r.x + PAD, y, inner_w, body.line_height), "Loading details...", &body, t.text_dim, Align::Left);
            y += body.line_height + 8;
        } else if let Some(e) = &d.error {
            ui.text(Rect::new(r.x + PAD, y, inner_w, body.line_height), &ellipsize(&body, e, inner_w), &body, t.danger, Align::Left);
            y += body.line_height + 8;
        }
        let dx = if wide { text_x } else { r.x + PAD };
        for line in &desc {
            if y > r.bottom() {
                break;
            }
            if y + body.line_height >= r.y {
                ui.text(Rect::new(dx, y, desc_w, body.line_height), line, &body, t.text, Align::Left);
            }
            y += body.line_height;
        }
        ui.canvas.restore_clip(old);
        action
    }
}

fn list_action(act: Option<ListAction>, epoch: u32, items: &[Video]) -> Option<Action> {
    match act? {
        ListAction::Open(i) => items.get(i).cloned().map(Action::OpenVideo),
        ListAction::More => Some(Action::More(epoch)),
        ListAction::Reload => Some(Action::Reload(epoch)),
    }
}

impl App for YouTube {
    fn update(&mut self, ui: &mut Ui, area: Rect, sys: &mut dyn System) {
        if !self.started {
            self.start(sys);
        }
        self.now_ms = sys.uptime_ms();
        self.now = sys.now();
        self.redraw = false;
        self.pump_client(sys);
        self.ensure_loaded();

        let t = *ui.theme;
        ui.fill(area, t.bg);
        let mut action: Option<Action>;
        if let Some(screen) = self.stack.last_mut() {
            let (head, body) = area.split_top(HEADER_H);
            let title = match screen {
                Screen::Videos { title, .. } => title.clone(),
                Screen::Detail(_) => "Video".into(),
                Screen::Player(_) => "Player".into(),
            };
            let now = self.now;
            action = match screen {
                Screen::Videos { videos, .. } => {
                    let epoch = videos.list.epoch;
                    let thumbs = &mut self.thumbs;
                    let act = listing(ui, body, &mut videos.list, "This list is empty.", "Load more", |ui, rr, v| {
                        video_row(ui, rr, v, thumbs, now)
                    });
                    list_action(act, epoch, &videos.list.items)
                }
                Screen::Detail(d) => Self::detail(d, ui, body, &mut self.thumbs, now),
                Screen::Player(p) => {
                    p.update(ui, body, sys);
                    None
                }
            };
            // The header is drawn last so scrolled content never covers it.
            if header(ui, head, &title, true, false).0 {
                action = Some(Action::Back);
            }
        } else {
            // Follow the screen, not the area: the on-screen keyboard makes
            // the portrait area wider than tall.
            let rail = sys.orientation() == Orientation::Landscape;
            let (content, bar) = if rail {
                let (bar, content) = area.split_left(RAIL_W);
                (content, bar)
            } else {
                area.split_top(area.h - TAB_H)
            };
            action = match self.tab {
                Tab::Home => self.home(ui, content),
                Tab::Search => self.search_page(ui, content),
                Tab::Library => self.library(ui, content),
                Tab::Account => self.account(ui, content),
            };
            if let Some(tab) = tab_bar(ui, bar, self.tab, rail) {
                action = Some(Action::Tab(tab));
            }
        }
        if let Some(a) = action {
            self.apply(a, sys);
            self.ensure_loaded();
        }
        // Send what this frame asked for right away; answers that are
        // already there are shown in an immediate follow-up frame.
        self.redraw = self.pump_client(sys);
        self.redraw |= self.thumbs.pump(sys, ui.input.down);
        self.save_quota(sys, false);
    }

    fn tick_interval_ms(&self) -> Option<u32> {
        let player = match self.stack.last() {
            Some(Screen::Player(p)) => p.tick_interval_ms(),
            _ => None,
        };
        let working = self.client.busy()
            || self.thumbs.busy()
            || matches!(self.client.auth.state(), State::RequestingCode | State::WaitingForUser(_));
        let ours = if self.redraw {
            Some(0)
        } else if working {
            Some(50)
        } else {
            None
        };
        match (player, ours) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn on_close(&mut self, sys: &mut dyn System) {
        for screen in &mut self.stack {
            if let Screen::Player(p) = screen {
                p.close(sys);
            }
        }
        self.client.close(sys.net());
        self.thumbs.clear(sys);
        self.save_quota(sys, true);
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestSys;
    use super::*;
    use p4os_sdk::gui::font::builtin;
    use p4os_sdk::gui::ui::{Input, Theme, UiState};

    const TRENDING: &str = include_str!("fixtures/trending.json");
    const VIDEOS: &str = include_str!("fixtures/videos.json");

    /// Run one frame of the app on a 480x708 area (portrait, below the
    /// desktop's status and title bars).
    fn frame(app: &mut YouTube, sys: &mut TestSys) {
        let mut buf = alloc::vec![Color::BLACK; 480 * 708];
        let mut canvas = Canvas::new(&mut buf, 480, 708);
        let fonts = builtin::load();
        let theme = Theme::dark();
        let input = Input::default();
        let mut state = UiState::default();
        let mut ui = Ui { canvas: &mut canvas, input: &input, theme: &theme, fonts: &fonts, state: &mut state };
        app.update(&mut ui, Rect::new(0, 0, 480, 708), sys);
    }

    fn sys_with(pairs: &[(&str, &str)]) -> TestSys {
        let mut sys = TestSys::default();
        for (k, v) in pairs {
            sys.config.insert((*k).into(), (*v).into());
        }
        sys
    }

    #[test]
    fn signed_out_shows_trending_and_opens_videos() {
        let mut sys = sys_with(&[(config::YOUTUBE_API_KEY, "AIza-test")]);
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        assert_eq!(sys.net.pending_urls().len(), 1);
        assert!(sys.net.pending_urls()[0].contains("chart=mostPopular&regionCode=VN"));
        assert_eq!(sys.net.respond_matching("chart=mostPopular", 200, TRENDING), 1);
        frame(&mut app, &mut sys);
        assert_eq!(app.trending.list.items.len(), 8);
        // Thumbnails of the rows on screen are being fetched.
        frame(&mut app, &mut sys);
        let thumbs: Vec<String> = sys.net.pending_urls().into_iter().filter(|u| u.contains("i.ytimg.com")).collect();
        assert_eq!(thumbs.len(), thumbs::MAX_FETCHES);
        assert!(app.tick_interval_ms().is_some(), "frames keep coming while loading");

        let video = app.trending.list.items[0].clone();
        app.apply(Action::OpenVideo(video), &mut sys);
        frame(&mut app, &mut sys);
        assert_eq!(sys.net.respond_matching("&id=p4Os_Vid001&", 200, VIDEOS), 1);
        frame(&mut app, &mut sys);
        let Some(Screen::Detail(d)) = app.stack.last() else { panic!("detail page expected") };
        assert!(!d.loading);
        assert!(d.video.description.starts_with("Một buổi sáng"));
        let video = d.video.clone();
        app.apply(Action::Play(video), &mut sys);
        frame(&mut app, &mut sys);
        assert!(matches!(app.stack.last(), Some(Screen::Player(_))));
        app.apply(Action::Back, &mut sys);
        app.apply(Action::Back, &mut sys);
        assert!(app.stack.is_empty());
        assert_eq!(app.client.quota.used(), 2);
    }

    #[test]
    fn signs_in_stores_the_token_and_signs_out() {
        let mut sys = sys_with(&[(config::YOUTUBE_CLIENT_ID, "id.apps"), (config::YOUTUBE_CLIENT_SECRET, "secret")]);
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        assert!(sys.net.requests.is_empty(), "nothing to load without a key or account");
        app.apply(Action::Tab(Tab::Account), &mut sys);
        app.apply(Action::SignIn, &mut sys);
        sys.net.respond_matching("/device/code", 200, include_str!("fixtures/device-code.json"));
        frame(&mut app, &mut sys);
        assert!(app.device_qr.is_some());
        sys.uptime = 5_000;
        frame(&mut app, &mut sys);
        assert_eq!(sys.net.respond_matching("/token", 428, include_str!("fixtures/token-pending.json")), 1);
        frame(&mut app, &mut sys);
        sys.uptime = 10_000;
        frame(&mut app, &mut sys);
        let tokens = r#"{"access_token":"ya29.t","expires_in":3599,"refresh_token":"1//fixture-refresh","token_type":"Bearer"}"#;
        assert_eq!(sys.net.respond_matching("/token", 200, tokens), 1);
        frame(&mut app, &mut sys);
        assert_eq!(sys.config.get(config::YOUTUBE_REFRESH_TOKEN).map(String::as_str), Some("1//fixture-refresh"));
        assert_eq!(app.tab, Tab::Home);
        frame(&mut app, &mut sys);
        let subs = sys.net.requests.iter().rev().find(|r| r.url.contains("/subscriptions?")).expect("feed started");
        assert!(subs.headers.contains(&("Authorization".into(), "Bearer ya29.t".into())));

        app.apply(Action::SignOut, &mut sys);
        assert!(!sys.config.contains_key(config::YOUTUBE_REFRESH_TOKEN));
        assert!(sys.net.last_request().url.ends_with("/revoke"));
        assert!(!app.client.auth.is_signed_in());
        assert!(app.home_feed.list.items.is_empty());
    }

    #[test]
    fn builds_the_subscriptions_feed_and_saves_quota() {
        let mut sys = sys_with(&[
            (config::YOUTUBE_CLIENT_ID, "id.apps"),
            (config::YOUTUBE_CLIENT_SECRET, "secret"),
            (config::YOUTUBE_REFRESH_TOKEN, "1//stored"),
        ]);
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        sys.net.respond_matching("/token", 200, include_str!("fixtures/token.json"));
        frame(&mut app, &mut sys);
        let answers = [
            ("/subscriptions?", include_str!("fixtures/subscriptions.json")),
            ("/channels?", include_str!("fixtures/channels.json")),
            ("playlistId=UUp4osChannelAAAAAAAAAAA", include_str!("fixtures/uploads-1.json")),
            ("playlistId=UUp4osChannelCCCCCCCCCCC", include_str!("fixtures/uploads-2.json")),
            ("playlistId=UUp4osChannelGGGGGGGGGGG", include_str!("fixtures/uploads-3.json")),
            ("/videos?part=contentDetails,statistics", VIDEOS),
        ];
        for (pattern, body) in answers {
            assert_eq!(sys.net.respond_matching(pattern, 200, body), 1, "{pattern}");
            frame(&mut app, &mut sys);
        }
        let ids: Vec<&str> = app.home_feed.list.items.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["p4Os_Feed04", "p4Os_Feed01", "p4Os_Feed06", "p4Os_Feed02", "p4Os_Feed03", "p4Os_Feed05", "p4Os_Feed07"]);
        assert_eq!(app.home_feed.list.items[0].duration_s, Some(36 * 60 + 44));
        assert_eq!(app.client.quota.used(), 6);
        app.on_close(&mut sys);
        assert_eq!(sys.config.get(config::YOUTUBE_QUOTA).map(String::as_str), Some("2026-10-28:6"));
    }

    #[test]
    fn expired_session_deletes_the_token() {
        let mut sys = sys_with(&[
            (config::YOUTUBE_CLIENT_ID, "id.apps"),
            (config::YOUTUBE_CLIENT_SECRET, "secret"),
            (config::YOUTUBE_REFRESH_TOKEN, "1//old"),
        ]);
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        sys.net.respond_matching("/token", 400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#);
        frame(&mut app, &mut sys);
        assert!(!sys.config.contains_key(config::YOUTUBE_REFRESH_TOKEN));
        assert!(app.notice.as_deref().unwrap().contains("Sign in again"));
        assert!(!app.client.busy(), "the feed request waiting for a token is dropped");
        assert_eq!(sys.net.sent(), 1, "nothing is sent without a valid session");
        assert!(app.home_feed.list.items.is_empty() && !app.client.auth.is_signed_in());
    }

    #[test]
    fn channel_pages_load_under_an_open_video() {
        let mut sys = sys_with(&[
            (config::YOUTUBE_CLIENT_ID, "id.apps"),
            (config::YOUTUBE_CLIENT_SECRET, "secret"),
            (config::YOUTUBE_REFRESH_TOKEN, "1//stored"),
        ]);
        let mut app = YouTube::new();
        app.tab = Tab::Library;
        app.shelf = Shelf::Channels;
        frame(&mut app, &mut sys);
        sys.net.respond_matching("/token", 200, include_str!("fixtures/token.json"));
        frame(&mut app, &mut sys);
        sys.net.respond_matching("/subscriptions?", 200, include_str!("fixtures/subscriptions.json"));
        frame(&mut app, &mut sys);
        let channel = app.channels.items[1].clone();
        assert_eq!(channel.title, "Silicon Tinkerer");
        app.apply(Action::OpenCollection(channel), &mut sys);
        frame(&mut app, &mut sys);
        assert_eq!(sys.net.respond_matching("/channels?", 200, include_str!("fixtures/channels.json")), 1);
        frame(&mut app, &mut sys);
        // The user opens a video before the uploads arrive.
        let other = Video { id: "p4Os_Vid003".into(), ..Default::default() };
        app.apply(Action::OpenVideo(other), &mut sys);
        assert_eq!(sys.net.respond_matching("playlistId=UUp4osChannelCCCCCCCCCCC", 200, include_str!("fixtures/uploads-2.json")), 1);
        frame(&mut app, &mut sys);
        let Some(Screen::Videos { videos, title }) = app.stack.first() else { panic!() };
        assert_eq!(title, "Silicon Tinkerer");
        assert_eq!(videos.list.items.len(), 2);
        assert!(!videos.list.loading);
        assert_eq!(videos.source, Source::Playlist("UUp4osChannelCCCCCCCCCCC".into()));
    }

    #[test]
    fn without_a_key_nothing_is_requested() {
        let mut sys = TestSys::default();
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        frame(&mut app, &mut sys);
        assert!(sys.net.requests.is_empty());
        assert!(!app.client.can_browse());
        assert_eq!(app.tick_interval_ms(), None, "idle: no polling");
    }

    #[test]
    fn closing_cancels_requests_in_flight() {
        let mut sys = sys_with(&[(config::YOUTUBE_API_KEY, "AIza-test")]);
        let mut app = YouTube::new();
        frame(&mut app, &mut sys);
        sys.net.respond_matching("chart=mostPopular", 200, TRENDING);
        frame(&mut app, &mut sys);
        frame(&mut app, &mut sys);
        let video = app.trending.list.items[0].clone();
        app.apply(Action::OpenVideo(video), &mut sys);
        frame(&mut app, &mut sys);
        assert!(sys.net.unpolled() > 0, "detail and thumbnails are loading");
        app.on_close(&mut sys);
        assert!(sys.net.cancelled >= 3);
        assert_eq!(sys.net.unpolled(), 0, "nothing is left behind in the OS");
    }

    #[test]
    fn helpers_fit_text() {
        let fonts = builtin::load();
        let f = fonts.body;
        assert_eq!(ellipsize(&f, "short", 400), "short");
        let cut = ellipsize(&f, "a very long title that cannot possibly fit in the space", 120);
        assert!(cut.ends_with("...") && f.measure(&cut) <= 120);
        let lines = wrap_lines(&f, "one two three four five six seven eight nine ten eleven twelve", 100, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with("..."));
    }
}
