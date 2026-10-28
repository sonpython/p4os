//! YouTube Data API v3: request paths, response parsing and display
//! formatting. Pure functions, tested against hand-made fixtures.
//!
//! Every call asks only for the fields the app shows (`fields=`), which
//! keeps responses to a few kilobytes instead of 50-200 KB.

use super::json::{self, Json};
use super::quota::{LIST_COST, SEARCH_COST};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use p4os_sdk::DateTime;

pub const API_BASE: &str = "https://www.googleapis.com/youtube/v3";
/// Rows per page for lists.
pub const PAGE_SIZE: u32 = 20;

const VIDEO_FIELDS: &str =
    "items(id,snippet(title,channelTitle,channelId,publishedAt),contentDetails/duration,statistics/viewCount),nextPageToken";
const DETAIL_FIELDS: &str =
    "items(id,snippet(title,channelTitle,channelId,publishedAt,description),contentDetails/duration,statistics(viewCount,likeCount))";
const STATS_FIELDS: &str = "items(id,contentDetails/duration,statistics/viewCount)";
const SEARCH_FIELDS: &str = "items(id/videoId,snippet(title,channelTitle,channelId,publishedAt)),nextPageToken";
const ITEM_FIELDS: &str = "items(snippet(title,videoOwnerChannelTitle,videoOwnerChannelId,publishedAt),contentDetails(videoId,videoPublishedAt)),nextPageToken";
const PLAYLIST_FIELDS: &str = "items(id,snippet(title,channelTitle,thumbnails/medium/url),contentDetails/itemCount),nextPageToken";
const SUBSCRIPTION_FIELDS: &str = "items(snippet(title,resourceId/channelId,thumbnails/medium/url)),nextPageToken";
const UPLOADS_FIELDS: &str = "items(id,contentDetails/relatedPlaylists/uploads)";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    /// RFC 3339, e.g. "2026-10-27T14:00:05Z".
    pub published: String,
    /// `Some(0)` for live streams.
    pub duration_s: Option<u32>,
    pub views: Option<u64>,
    pub likes: Option<u64>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionKind {
    Playlist,
    Channel,
}

/// A playlist or a subscribed channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collection {
    pub kind: CollectionKind,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumb: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// One API request, relative to [`API_BASE`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub path: String,
    pub cost: u32,
    /// Needs a signed-in user (`mine=true`, `myRating`).
    pub needs_user: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    /// First `errors[].reason`, e.g. "quotaExceeded".
    pub reason: String,
    pub message: String,
}

impl ApiError {
    pub fn is_quota(&self) -> bool {
        matches!(self.reason.as_str(), "quotaExceeded" | "dailyLimitExceeded" | "rateLimitExceeded")
    }

    pub fn describe(&self) -> String {
        match self.reason.as_str() {
            "keyInvalid" => "The API key is not valid. Check youtube.api_key.".into(),
            "accessNotConfigured" | "SERVICE_DISABLED" => {
                "The YouTube Data API v3 is not enabled for this Cloud project.".into()
            }
            _ if self.message.is_empty() => format!("YouTube answered HTTP {}.", self.status),
            _ => format!("YouTube: {} (HTTP {})", self.message, self.status),
        }
    }
}

/// Percent-encode for a query string or form body (RFC 3986 unreserved
/// characters stay as they are).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// IDs and page tokens from the API only use these characters; anything
/// else is dropped rather than put into a URL.
pub fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn thumb_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/mqdefault.jpg")
}

pub fn watch_url(video_id: &str) -> String {
    format!("https://youtu.be/{video_id}")
}

fn page_param(page: Option<&str>) -> String {
    page.filter(|p| is_id(p)).map_or(String::new(), |p| format!("&pageToken={p}"))
}

fn list(path: String, needs_user: bool) -> Call {
    Call { path, cost: LIST_COST, needs_user }
}

pub fn trending(region: &str, page: Option<&str>) -> Call {
    let region = if region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic()) { region } else { "VN" };
    list(
        format!(
            "videos?part=snippet,contentDetails,statistics&chart=mostPopular&regionCode={}&maxResults={PAGE_SIZE}&fields={VIDEO_FIELDS}{}",
            region.to_ascii_uppercase(),
            page_param(page)
        ),
        false,
    )
}

pub fn search(query: &str, page: Option<&str>) -> Call {
    Call {
        path: format!(
            "search?part=snippet&type=video&maxResults={PAGE_SIZE}&q={}&fields={SEARCH_FIELDS}{}",
            percent_encode(query.trim()),
            page_param(page)
        ),
        cost: SEARCH_COST,
        needs_user: false,
    }
}

pub fn liked(page: Option<&str>) -> Call {
    list(
        format!(
            "videos?part=snippet,contentDetails,statistics&myRating=like&maxResults={PAGE_SIZE}&fields={VIDEO_FIELDS}{}",
            page_param(page)
        ),
        true,
    )
}

/// Duration and view count for up to 50 videos.
pub fn stats(ids: &[&str]) -> Call {
    let ids: Vec<&str> = ids.iter().copied().filter(|i| is_id(i)).take(50).collect();
    list(format!("videos?part=contentDetails,statistics&id={}&fields={STATS_FIELDS}", ids.join(",")), false)
}

pub fn detail(id: &str) -> Call {
    let id = if is_id(id) { id } else { "" };
    list(format!("videos?part=snippet,contentDetails,statistics&id={id}&fields={DETAIL_FIELDS}"), false)
}

pub fn my_playlists(page: Option<&str>) -> Call {
    list(
        format!(
            "playlists?part=snippet,contentDetails&mine=true&maxResults={PAGE_SIZE}&fields={PLAYLIST_FIELDS}{}",
            page_param(page)
        ),
        true,
    )
}

pub fn subscriptions(max: u32, page: Option<&str>) -> Call {
    list(
        format!(
            "subscriptions?part=snippet&mine=true&order=relevance&maxResults={}&fields={SUBSCRIPTION_FIELDS}{}",
            max.min(50),
            page_param(page)
        ),
        true,
    )
}

/// Uploads playlist IDs for up to 50 channels.
pub fn uploads(channel_ids: &[&str]) -> Call {
    let ids: Vec<&str> = channel_ids.iter().copied().filter(|i| is_id(i)).take(50).collect();
    list(format!("channels?part=contentDetails&id={}&fields={UPLOADS_FIELDS}", ids.join(",")), false)
}

pub fn playlist_items(playlist_id: &str, max: u32, page: Option<&str>) -> Call {
    let id = if is_id(playlist_id) { playlist_id } else { "" };
    list(
        format!(
            "playlistItems?part=snippet,contentDetails&playlistId={id}&maxResults={}&fields={ITEM_FIELDS}{}",
            max.min(50),
            page_param(page)
        ),
        false,
    )
}

// ---------------------------------------------------------------------------
// Parsing

fn doc(body: &[u8]) -> Result<Json<'_>, String> {
    let text = core::str::from_utf8(body).map_err(|_| String::from("YouTube sent text that is not UTF-8."))?;
    json::parse(text).map_err(|_| String::from("YouTube sent malformed JSON."))
}

fn s(v: Option<Json>) -> String {
    v.and_then(|v| v.string()).unwrap_or_default()
}

fn next_token(d: &Json) -> Option<String> {
    d.get("nextPageToken").and_then(|v| v.string()).filter(|t| is_id(t))
}

/// Decode the few HTML entities `search.list` puts in titles.
pub fn unescape_html(text: &str) -> String {
    if !text.contains('&') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                _ => {
                    let num = name.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn video_from(item: &Json, id: String) -> Option<Video> {
    if !is_id(&id) {
        return None;
    }
    let snippet = item.get("snippet");
    let sn = |k: &str| s(snippet.and_then(|v| v.get(k)));
    let stat = |k: &str| item.path(&["statistics", k]).and_then(|v| v.u64());
    Some(Video {
        id,
        title: sn("title"),
        channel: sn("channelTitle"),
        channel_id: sn("channelId"),
        published: sn("publishedAt"),
        duration_s: item.path(&["contentDetails", "duration"]).and_then(|v| v.str()).and_then(|d| parse_duration(&d)),
        views: stat("viewCount"),
        likes: stat("likeCount"),
        description: sn("description"),
    })
}

/// `videos.list` (trending, liked, details).
pub fn parse_videos(body: &[u8]) -> Result<Page<Video>, String> {
    let d = doc(body)?;
    let items = d.get("items").map_or(Vec::new(), |a| a.items().filter_map(|it| video_from(&it, s(it.get("id")))).collect());
    Ok(Page { items, next: next_token(&d) })
}

/// `search.list`: the ID is nested and titles are HTML-escaped.
pub fn parse_search(body: &[u8]) -> Result<Page<Video>, String> {
    let d = doc(body)?;
    let items = d.get("items").map_or(Vec::new(), |a| {
        a.items()
            .filter_map(|it| {
                let mut v = video_from(&it, s(it.path(&["id", "videoId"])))?;
                v.title = unescape_html(&v.title);
                v.channel = unescape_html(&v.channel);
                Some(v)
            })
            .collect()
    });
    Ok(Page { items, next: next_token(&d) })
}

/// `playlistItems.list`. Deleted and private videos (no owner channel)
/// are skipped.
pub fn parse_playlist_items(body: &[u8]) -> Result<Page<Video>, String> {
    let d = doc(body)?;
    let items = d.get("items").map_or(Vec::new(), |a| {
        a.items()
            .filter_map(|it| {
                let sn = |k: &str| s(it.path(&["snippet", k]));
                let channel = sn("videoOwnerChannelTitle");
                if channel.is_empty() {
                    return None;
                }
                let id = s(it.path(&["contentDetails", "videoId"]));
                let published = Some(s(it.path(&["contentDetails", "videoPublishedAt"])))
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| sn("publishedAt"));
                is_id(&id).then(|| Video {
                    id,
                    title: sn("title"),
                    channel,
                    channel_id: sn("videoOwnerChannelId"),
                    published,
                    ..Default::default()
                })
            })
            .collect()
    });
    Ok(Page { items, next: next_token(&d) })
}

pub fn parse_playlists(body: &[u8]) -> Result<Page<Collection>, String> {
    let d = doc(body)?;
    let items = d.get("items").map_or(Vec::new(), |a| {
        a.items()
            .filter_map(|it| {
                let id = s(it.get("id"));
                let count = it.path(&["contentDetails", "itemCount"]).and_then(|v| v.u64()).unwrap_or(0);
                is_id(&id).then(|| Collection {
                    kind: CollectionKind::Playlist,
                    id,
                    title: s(it.path(&["snippet", "title"])),
                    subtitle: format!("{count} video{}", if count == 1 { "" } else { "s" }),
                    thumb: it.path(&["snippet", "thumbnails", "medium", "url"]).and_then(|v| v.string()),
                })
            })
            .collect()
    });
    Ok(Page { items, next: next_token(&d) })
}

pub fn parse_subscriptions(body: &[u8]) -> Result<Page<Collection>, String> {
    let d = doc(body)?;
    let items = d.get("items").map_or(Vec::new(), |a| {
        a.items()
            .filter_map(|it| {
                let id = s(it.path(&["snippet", "resourceId", "channelId"]));
                is_id(&id).then(|| Collection {
                    kind: CollectionKind::Channel,
                    id,
                    title: s(it.path(&["snippet", "title"])),
                    subtitle: "Channel".into(),
                    thumb: it.path(&["snippet", "thumbnails", "medium", "url"]).and_then(|v| v.string()),
                })
            })
            .collect()
    });
    Ok(Page { items, next: next_token(&d) })
}

/// `channels.list`: (channel ID, uploads playlist ID) pairs.
pub fn parse_uploads(body: &[u8]) -> Result<Vec<(String, String)>, String> {
    let d = doc(body)?;
    Ok(d.get("items").map_or(Vec::new(), |a| {
        a.items()
            .filter_map(|it| {
                let pair = (s(it.get("id")), s(it.path(&["contentDetails", "relatedPlaylists", "uploads"])));
                (is_id(&pair.0) && is_id(&pair.1)).then_some(pair)
            })
            .collect()
    }))
}

/// The error object of a failed API call.
pub fn parse_error(status: u16, body: &[u8]) -> ApiError {
    let parsed = doc(body).ok().and_then(|d| d.get("error"));
    let reason = parsed
        .and_then(|e| e.get("errors"))
        .and_then(|a| a.items().next())
        .and_then(|e| e.get("reason"))
        .or_else(|| parsed.and_then(|e| e.get("status")))
        .and_then(|v| v.string())
        .unwrap_or_default();
    let message = s(parsed.and_then(|e| e.get("message")));
    ApiError { status, reason, message }
}

/// Copy duration and view counts from `stats` into matching videos.
pub fn merge_stats(videos: &mut [Video], stats: &[Video]) {
    for v in videos.iter_mut() {
        if let Some(s) = stats.iter().find(|s| s.id == v.id) {
            v.duration_s = s.duration_s.or(v.duration_s);
            v.views = s.views.or(v.views);
        }
    }
}

/// Newest first, without duplicates, at most `n`.
pub fn newest(mut videos: Vec<Video>, n: usize) -> Vec<Video> {
    // RFC 3339 UTC timestamps in the same format sort as strings.
    videos.sort_by(|a, b| b.published.cmp(&a.published));
    let mut seen: Vec<String> = Vec::new();
    videos.retain(|v| {
        if seen.contains(&v.id) {
            return false;
        }
        seen.push(v.id.clone());
        true
    });
    videos.truncate(n);
    videos
}

// ---------------------------------------------------------------------------
// Formatting

/// ISO 8601 duration ("PT1H2M3S", "P1DT2H") in seconds.
pub fn parse_duration(iso: &str) -> Option<u32> {
    let rest = iso.strip_prefix('P')?;
    let mut total: u32 = 0;
    let mut num: u32 = 0;
    let mut have_num = false;
    let mut in_time = false;
    for c in rest.chars() {
        match c {
            '0'..='9' => {
                num = num.checked_mul(10)?.checked_add(c as u32 - '0' as u32)?;
                have_num = true;
            }
            'T' if !in_time && !have_num => in_time = true,
            'W' | 'D' | 'H' | 'M' | 'S' if have_num => {
                let unit = match (c, in_time) {
                    ('W', false) => 604_800,
                    ('D', false) => 86_400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
                total = total.checked_add(num.checked_mul(unit)?)?;
                num = 0;
                have_num = false;
            }
            _ => return None,
        }
    }
    (!have_num).then_some(total)
}

pub fn format_duration(secs: u32) -> String {
    if secs == 0 {
        return "LIVE".into();
    }
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// 950, 1.2K, 12K, 3.4M, 1.1B.
pub fn format_count(n: u64) -> String {
    let (div, suffix) = match n {
        0..=999 => return n.to_string(),
        1_000..=999_999 => (1_000, "K"),
        1_000_000..=999_999_999 => (1_000_000, "M"),
        _ => (1_000_000_000, "B"),
    };
    let tenths = n * 10 / div;
    if tenths < 100 && tenths % 10 != 0 {
        format!("{}.{}{suffix}", tenths / 10, tenths % 10)
    } else {
        format!("{}{suffix}", n / div)
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since 1970 for "YYYY-MM-DDTHH:MM:SS..." (UTC).
pub fn parse_timestamp(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    let num = |r: core::ops::Range<usize>| -> Option<i64> { ts.get(r)?.parse().ok() };
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let days = days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?);
    Some(days * 86_400 + num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?)
}

fn unix(t: DateTime) -> i64 {
    days_from_civil(t.year as i64, t.month as i64, t.day as i64) * 86_400
        + t.hour as i64 * 3600
        + t.minute as i64 * 60
        + t.second as i64
}

/// "3 days ago", or the date when the clock is unknown.
pub fn format_age(published: &str, now: Option<DateTime>) -> String {
    let Some(then) = parse_timestamp(published) else { return String::new() };
    let Some(now) = now else { return published.get(..10).unwrap_or("").into() };
    let secs = (unix(now) - then).max(0);
    let units: [(i64, &str); 6] =
        [(365 * 86_400, "year"), (30 * 86_400, "month"), (7 * 86_400, "week"), (86_400, "day"), (3600, "hour"), (60, "minute")];
    for (size, name) in units {
        if secs >= size {
            let n = secs / size;
            return format!("{n} {name}{} ago", if n == 1 { "" } else { "s" });
        }
    }
    "just now".into()
}

/// "1.2M views · 3 days ago".
pub fn video_meta(v: &Video, now: Option<DateTime>) -> String {
    let age = format_age(&v.published, now);
    match v.views {
        Some(n) if age.is_empty() => format!("{} views", format_count(n)),
        Some(n) => format!("{} views · {age}", format_count(n)),
        None => age,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRENDING: &str = include_str!("fixtures/trending.json");
    const SEARCH: &str = include_str!("fixtures/search.json");
    const VIDEOS: &str = include_str!("fixtures/videos.json");
    const SUBSCRIPTIONS: &str = include_str!("fixtures/subscriptions.json");
    const CHANNELS: &str = include_str!("fixtures/channels.json");
    const UPLOADS_1: &str = include_str!("fixtures/uploads-1.json");
    const PLAYLISTS: &str = include_str!("fixtures/playlists.json");
    const QUOTA_ERROR: &str = include_str!("fixtures/error-quota.json");

    #[test]
    fn parses_trending() {
        let page = parse_videos(TRENDING.as_bytes()).unwrap();
        assert_eq!(page.items.len(), 8);
        let v = &page.items[0];
        assert_eq!(v.id, "p4Os_Vid001");
        assert_eq!(v.title, "Hà Nội mùa thu: 4K walking tour quanh Hồ Gươm");
        assert_eq!(v.channel, "Phố Cổ Walks");
        assert_eq!(v.duration_s, Some(1_524));
        assert_eq!(v.views, Some(1_234_567));
        assert_eq!(page.next.as_deref(), Some("CBQQAA"));
        assert!(page.items.iter().any(|v| v.duration_s == Some(0)), "a live stream");
    }

    #[test]
    fn parses_search_with_entities() {
        let page = parse_search(SEARCH.as_bytes()).unwrap();
        assert_eq!(page.items.len(), 6);
        assert_eq!(page.items[0].id, "p4Os_Src001");
        assert_eq!(page.items[1].title, "Rust & embedded: \"no_std\" from scratch");
        assert_eq!(page.items[0].duration_s, None, "search results need a stats call");
        let ids: Vec<&str> = page.items.iter().map(|v| v.id.as_str()).collect();
        let stats_call = stats(&ids);
        assert!(stats_call.path.contains("id=p4Os_Src001,p4Os_Src002,"));
        let mut items = page.items;
        merge_stats(&mut items, &parse_videos(VIDEOS.as_bytes()).unwrap().items);
        assert_eq!(items[0].duration_s, Some(754));
        assert_eq!(items[0].views, Some(48_211));
    }

    #[test]
    fn parses_details() {
        let page = parse_videos(VIDEOS.as_bytes()).unwrap();
        let v = page.items.iter().find(|v| v.id == "p4Os_Src001").unwrap();
        assert!(v.description.starts_with("Building an ESP32-P4"));
        assert!(v.description.contains('\n'));
        assert_eq!(v.likes, Some(3_120));
        assert!(page.next.is_none());
    }

    #[test]
    fn parses_subscription_feed_parts() {
        let subs = parse_subscriptions(SUBSCRIPTIONS.as_bytes()).unwrap();
        assert_eq!(subs.items.len(), 3);
        assert_eq!(subs.items[0].kind, CollectionKind::Channel);
        assert_eq!(subs.items[0].id, "UCp4osChannelAAAAAAAAAAA");
        let uploads = parse_uploads(CHANNELS.as_bytes()).unwrap();
        assert_eq!(uploads.len(), 3);
        assert_eq!(uploads[0].1, "UUp4osChannelAAAAAAAAAAA");
        let items = parse_playlist_items(UPLOADS_1.as_bytes()).unwrap();
        assert_eq!(items.items.len(), 3, "the private video is skipped");
        assert_eq!(items.items[0].published, "2026-10-27T14:00:05Z");
        assert_eq!(items.items[0].channel, "Phố Cổ Walks");
    }

    #[test]
    fn parses_playlists() {
        let page = parse_playlists(PLAYLISTS.as_bytes()).unwrap();
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].subtitle, "12 videos");
        assert_eq!(page.items[2].subtitle, "1 video");
        assert!(page.items[0].thumb.as_deref().unwrap().starts_with("https://i.ytimg.com/"));
    }

    #[test]
    fn parses_errors() {
        let e = parse_error(403, QUOTA_ERROR.as_bytes());
        assert_eq!(e.reason, "quotaExceeded");
        assert!(e.is_quota());
        let e = parse_error(400, br#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","errors":[{"reason":"badRequest"}],"status":"INVALID_ARGUMENT"}}"#);
        assert_eq!(e.reason, "badRequest");
        assert!(e.describe().contains("API key not valid"));
        let e = parse_error(502, b"<html>bad gateway</html>");
        assert_eq!((e.reason.as_str(), e.describe().as_str()), ("", "YouTube answered HTTP 502."));
        assert!(parse_videos(b"{\"items\": [").is_err());
    }

    #[test]
    fn builds_calls() {
        let c = search("phở hà nội & more", None);
        assert_eq!(c.cost, 100);
        assert!(c.path.contains("q=ph%E1%BB%9F%20h%C3%A0%20n%E1%BB%99i%20%26%20more"));
        assert!(!c.needs_user);
        let c = trending("us", Some("CBQQAA"));
        assert_eq!(c.cost, 1);
        assert!(c.path.contains("regionCode=US") && c.path.ends_with("&pageToken=CBQQAA"));
        assert!(trending("../x", None).path.contains("regionCode=VN"));
        assert!(!trending("VN", Some("bad&token=1")).path.contains("pageToken"));
        assert!(liked(None).needs_user && my_playlists(None).needs_user && subscriptions(50, None).needs_user);
        assert!(subscriptions(80, None).path.contains("maxResults=50"));
        assert!(detail("x/../../evil").path.contains("&id=&"));
        assert_eq!(watch_url("abc"), "https://youtu.be/abc");
    }

    #[test]
    fn newest_merges_feeds() {
        let v = |id: &str, p: &str| Video { id: id.into(), published: p.into(), ..Default::default() };
        let out = newest(
            alloc::vec![v("a", "2026-10-01T00:00:00Z"), v("b", "2026-10-03T00:00:00Z"), v("a", "2026-10-01T00:00:00Z"), v("c", "2026-10-02T00:00:00Z")],
            2,
        );
        let ids: Vec<&str> = out.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT4M13S"), Some(253));
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723));
        assert_eq!(parse_duration("PT15S"), Some(15));
        assert_eq!(parse_duration("P1DT2H"), Some(93_600));
        assert_eq!(parse_duration("P0D"), Some(0));
        assert_eq!(parse_duration("PT"), Some(0));
        for bad in ["", "T1M", "PT1X", "PT1", "P1H", "PTT1S"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(format_duration(253), "4:13");
        assert_eq!(format_duration(3723), "1:02:03");
        assert_eq!(format_duration(0), "LIVE");
    }

    #[test]
    fn counts_and_ages() {
        assert_eq!(format_count(950), "950");
        assert_eq!(format_count(1_234), "1.2K");
        assert_eq!(format_count(12_345), "12K");
        assert_eq!(format_count(1_000_000), "1M");
        assert_eq!(format_count(3_456_789), "3.4M");
        assert_eq!(format_count(2_100_000_000), "2.1B");
        let now = Some(DateTime { year: 2026, month: 10, day: 28, hour: 14, minute: 0, second: 0 });
        assert_eq!(format_age("2026-10-28T13:59:30Z", now), "just now");
        assert_eq!(format_age("2026-10-28T11:00:00Z", now), "3 hours ago");
        assert_eq!(format_age("2026-10-27T14:00:00Z", now), "1 day ago");
        assert_eq!(format_age("2026-09-01T00:00:00.000Z", now), "1 month ago");
        assert_eq!(format_age("2020-02-29T00:00:00Z", now), "6 years ago");
        assert_eq!(format_age("2026-10-27T14:00:00Z", None), "2026-10-27");
        assert_eq!(format_age("garbage", now), "");
        let v = Video { views: Some(1_500), published: "2026-10-21T14:00:00Z".into(), ..Default::default() };
        assert_eq!(video_meta(&v, now), "1.5K views · 1 week ago");
    }

    #[test]
    fn html_entities() {
        assert_eq!(unescape_html("a &amp; b &quot;c&quot; &#39;d&#39; &#x41; &lt;"), "a & b \"c\" 'd' A <");
        assert_eq!(unescape_html("AT&T & co;"), "AT&T & co;");
        assert_eq!(unescape_html("&bogus;"), "&bogus;");
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(percent_encode("a b/c?d=e&f~_.-"), "a%20b%2Fc%3Fd%3De%26f~_.-");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
