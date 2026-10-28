//! Reading an InnerTube `/player` response: playability, stream formats,
//! HLS manifest, storyboard spec, and picking the formats p4os can play.

use crate::error::{ErrorKind, PlayError};
use crate::json::{self, Value};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Format {
    pub itag: u32,
    pub url: String,
    /// e.g. `video/mp4; codecs="avc1.4D4015"`.
    pub mime: String,
    pub bitrate: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub content_length: u64,
    /// Inclusive byte ranges of the container header and the seek index.
    pub init_range: Option<(u64, u64)>,
    pub index_range: Option<(u64, u64)>,
    pub duration_ms: u64,
}

impl Format {
    pub fn is_h264_mp4(&self) -> bool {
        self.mime.starts_with("video/mp4") && self.mime.contains("avc1")
    }

    pub fn is_opus_webm(&self) -> bool {
        self.mime.starts_with("audio/webm") && self.mime.contains("opus")
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlayerInfo {
    pub video_id: String,
    pub title: String,
    pub author: String,
    pub length_s: u32,
    pub is_live: bool,
    /// Formats with a direct URL (no signature cipher).
    pub formats: Vec<Format>,
    pub hls_manifest_url: Option<String>,
    pub storyboard_spec: Option<String>,
    /// URLs stop working this long after the response.
    pub expires_in_s: u32,
}

/// What the `/player` call told us besides the formats.
pub enum Answer {
    Playable(PlayerInfo),
    /// Not playable; `visitor_data` is set when YouTube handed one out, so
    /// the request can be repeated with it.
    Refused { status: String, reason: String, visitor_data: Option<String> },
}

fn range(v: Option<Value>) -> Option<(u64, u64)> {
    let v = v?;
    let (s, e) = (v.u64_at(&["start"])?, v.u64_at(&["end"])?);
    (e >= s).then_some((s, e))
}

fn format(v: &Value) -> Option<Format> {
    // Formats behind a signature cipher need YouTube's JS player: unusable.
    let url = v.str_at(&["url"])?.into_owned();
    Some(Format {
        itag: v.u64_at(&["itag"])? as u32,
        url,
        mime: v.str_at(&["mimeType"]).map(|s| s.into_owned()).unwrap_or_default(),
        bitrate: v.u64_at(&["bitrate"]).unwrap_or(0) as u32,
        width: v.u64_at(&["width"]).unwrap_or(0) as u32,
        height: v.u64_at(&["height"]).unwrap_or(0) as u32,
        fps: v.u64_at(&["fps"]).unwrap_or(0) as u32,
        content_length: v.u64_at(&["contentLength"]).unwrap_or(0),
        init_range: range(v.get("initRange")),
        index_range: range(v.get("indexRange")),
        duration_ms: v.u64_at(&["approxDurationMs"]).unwrap_or(0),
    })
}

pub fn visitor_data(root: &Value) -> Option<String> {
    root.str_at(&["responseContext", "visitorData"]).map(|s| s.into_owned()).filter(|s| !s.is_empty())
}

pub fn parse_player(video_id: &str, text: &str) -> Result<Answer, PlayError> {
    let root = json::parse(text).map_err(|e| PlayError::new(ErrorKind::BadResponse, e))?;
    let status = root.str_at(&["playabilityStatus", "status"]).map(|s| s.into_owned()).unwrap_or_default();
    if status != "OK" {
        let reason = root
            .str_at(&["playabilityStatus", "reason"])
            .map(|s| s.into_owned())
            .unwrap_or_else(|| status.clone());
        return Ok(Answer::Refused { status, reason, visitor_data: visitor_data(&root) });
    }
    let sd = root.get("streamingData");
    let mut formats = Vec::new();
    for list in ["formats", "adaptiveFormats"] {
        if let Some(arr) = sd.and_then(|s| s.get(list)) {
            formats.extend(arr.items().filter_map(|f| format(&f)));
        }
    }
    let details = root.get("videoDetails");
    let text_of = |k: &str| details.and_then(|d| d.str_at(&[k])).map(|s| s.into_owned()).unwrap_or_default();
    Ok(Answer::Playable(PlayerInfo {
        video_id: video_id.to_string(),
        title: text_of("title"),
        author: text_of("author"),
        length_s: details.and_then(|d| d.u64_at(&["lengthSeconds"])).unwrap_or(0) as u32,
        is_live: details.and_then(|d| d.get("isLive")).and_then(|v| v.as_bool()).unwrap_or(false),
        formats,
        hls_manifest_url: sd.and_then(|s| s.str_at(&["hlsManifestUrl"])).map(|s| s.into_owned()),
        storyboard_spec: root.str_at(&["storyboards", "playerStoryboardSpecRenderer", "spec"]).map(|s| s.into_owned()),
        expires_in_s: sd.and_then(|s| s.u64_at(&["expiresInSeconds"])).unwrap_or(6 * 3600) as u32,
    }))
}

/// Video quality to aim for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    P144,
    P240,
}

impl Quality {
    pub fn height(self) -> u32 {
        match self {
            Quality::P144 => 144,
            Quality::P240 => 240,
        }
    }
}

/// Opus in WebM, preferring ~60 kbps (250) over ~46 (249) and ~130 (251).
pub fn pick_audio(info: &PlayerInfo) -> Option<&Format> {
    [250, 249, 251]
        .iter()
        .find_map(|&itag| info.formats.iter().find(|f| f.itag == itag && f.is_opus_webm() && f.index_range.is_some()))
        .or_else(|| info.formats.iter().filter(|f| f.is_opus_webm() && f.index_range.is_some()).min_by_key(|f| f.bitrate))
}

/// H.264 fMP4 at the wanted height, else the closest lower one, else the
/// lowest available.
pub fn pick_video(info: &PlayerInfo, q: Quality) -> Option<&Format> {
    let usable = || info.formats.iter().filter(|f| f.is_h264_mp4() && f.index_range.is_some() && f.init_range.is_some() && f.height > 0);
    usable()
        .filter(|f| f.height <= q.height())
        .max_by_key(|f| (f.height, u32::MAX - f.bitrate))
        .or_else(|| usable().min_by_key(|f| f.height))
}

/// One storyboard level: a grid of thumbnails per JPEG sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Storyboard {
    /// Sheet URL with `$M` standing for the sheet number.
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub count: u32,
    pub cols: u32,
    pub rows: u32,
}

impl Storyboard {
    /// Pick the most detailed level of a `storyboards` spec.
    pub fn parse(spec: &str) -> Option<Storyboard> {
        let mut parts = spec.split('|');
        let base = parts.next()?;
        if !base.starts_with("https://") {
            return None;
        }
        let mut best = None;
        for (level, args) in parts.enumerate() {
            let a: Vec<&str> = args.split('#').collect();
            if a.len() != 8 {
                continue;
            }
            let n = |i: usize| a[i].parse::<u32>().ok().filter(|&v| v > 0);
            let (Some(width), Some(height), Some(count), Some(cols), Some(rows)) = (n(0), n(1), n(2), n(3), n(4)) else {
                continue;
            };
            let url = alloc::format!("{}&sigh={}", base.replace("$L", &level.to_string()).replace("$N", a[6]), a[7]);
            let sb = Storyboard { url, width, height, count, cols, rows };
            if best.as_ref().is_none_or(|b: &Storyboard| sb.width > b.width) {
                best = Some(sb);
            }
        }
        best
    }

    /// Sheet number, column and row of the thumbnail shown at `t_ms`.
    pub fn locate(&self, t_ms: u64, duration_ms: u64) -> (u32, u32, u32) {
        let per_sheet = self.cols * self.rows;
        let k = if duration_ms == 0 { 0 } else { (t_ms * self.count as u64 / duration_ms) as u32 };
        let k = k.min(self.count - 1);
        (k / per_sheet, (k % per_sheet) % self.cols, (k % per_sheet) / self.cols)
    }

    pub fn sheet_url(&self, sheet: u32) -> String {
        self.url.replace("$M", &sheet.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{
      "responseContext": {"visitorData": "CgtVD"},
      "playabilityStatus": {"status": "OK"},
      "streamingData": {
        "expiresInSeconds": "21540",
        "adaptiveFormats": [
          {"itag": 133, "url": "https://r.googlevideo.com/videoplayback?itag=133", "mimeType": "video/mp4; codecs=\"avc1.4D4015\"",
           "bitrate": 236224, "width": 426, "height": 240, "initRange": {"start": "0", "end": "739"},
           "indexRange": {"start": "740", "end": "1227"}, "contentLength": "4310122", "fps": 25, "approxDurationMs": "213040"},
          {"itag": 160, "url": "https://r.googlevideo.com/videoplayback?itag=160", "mimeType": "video/mp4; codecs=\"avc1.4D400C\"",
           "bitrate": 109973, "width": 256, "height": 144, "initRange": {"start": "0", "end": "738"},
           "indexRange": {"start": "739", "end": "1226"}, "contentLength": "2058142", "fps": 25},
          {"itag": 278, "url": "https://r/x", "mimeType": "video/webm; codecs=\"vp9\"", "height": 144,
           "initRange": {"start": "0", "end": "1"}, "indexRange": {"start": "2", "end": "3"}},
          {"itag": 251, "signatureCipher": "s=abc", "mimeType": "audio/webm; codecs=\"opus\""},
          {"itag": 249, "url": "https://r.googlevideo.com/videoplayback?itag=249", "mimeType": "audio/webm; codecs=\"opus\"",
           "bitrate": 49496, "initRange": {"start": "0", "end": "258"}, "indexRange": {"start": "259", "end": "628"}, "contentLength": "1231355"}
        ],
        "hlsManifestUrl": "https://manifest.googlevideo.com/api/manifest/hls_variant/x"
      },
      "videoDetails": {"title": "T é", "author": "A", "lengthSeconds": "213", "isLive": false},
      "storyboards": {"playerStoryboardSpecRenderer": {"spec": "https://i.ytimg.com/sb/ID/storyboard3_L$L/$N.jpg?sqp=x|48#27#100#10#10#0#default#rs$A|80#45#108#10#10#2000#M$M#rs$B|160#90#108#5#5#2000#M$M#rs$C"}}
    }"#;

    fn info() -> PlayerInfo {
        match parse_player("ID", RESPONSE).unwrap() {
            Answer::Playable(i) => i,
            Answer::Refused { .. } => panic!("refused"),
        }
    }

    #[test]
    fn parses_formats_and_details() {
        let i = info();
        assert_eq!(i.title, "T é");
        assert_eq!(i.length_s, 213);
        assert_eq!(i.expires_in_s, 21540);
        // The ciphered format is dropped.
        assert_eq!(i.formats.iter().map(|f| f.itag).collect::<Vec<_>>(), [133, 160, 278, 249]);
        let f = &i.formats[0];
        assert_eq!((f.width, f.height, f.fps, f.content_length), (426, 240, 25, 4_310_122));
        assert_eq!((f.init_range, f.index_range), (Some((0, 739)), Some((740, 1227))));
        assert!(i.hls_manifest_url.is_some());
    }

    #[test]
    fn picks_formats() {
        let i = info();
        assert_eq!(pick_video(&i, Quality::P240).map(|f| f.itag), Some(133));
        assert_eq!(pick_video(&i, Quality::P144).map(|f| f.itag), Some(160));
        assert_eq!(pick_audio(&i).map(|f| f.itag), Some(249));
        let mut only_240 = i.clone();
        only_240.formats.retain(|f| f.itag != 160);
        assert_eq!(pick_video(&only_240, Quality::P144).map(|f| f.itag), Some(133));
    }

    #[test]
    fn reports_refusals_with_visitor_data() {
        let r = r#"{"responseContext": {"visitorData": "Cgt"}, "playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm you're not a bot"}}"#;
        match parse_player("ID", r).unwrap() {
            Answer::Refused { status, reason, visitor_data } => {
                assert_eq!(status, "LOGIN_REQUIRED");
                assert!(reason.contains("bot"));
                assert_eq!(visitor_data.as_deref(), Some("Cgt"));
            }
            Answer::Playable(_) => panic!("playable"),
        }
        assert!(parse_player("ID", "<html>").is_err());
    }

    #[test]
    fn parses_storyboards() {
        let sb = Storyboard::parse(info().storyboard_spec.as_deref().unwrap()).unwrap();
        assert_eq!((sb.width, sb.height, sb.count, sb.cols, sb.rows), (160, 90, 108, 5, 5));
        assert_eq!(sb.sheet_url(3), "https://i.ytimg.com/sb/ID/storyboard3_L2/M3.jpg?sqp=x&sigh=rs$C");
        assert_eq!(sb.locate(0, 213_000), (0, 0, 0));
        // Thumbnail 27 of 108: sheet 1 (25 per sheet), third in its first row.
        assert_eq!(sb.locate(27 * 213_000 / 108 + 1, 213_000), (1, 2, 0));
        assert_eq!(sb.locate(999_999, 213_000), (4, 2, 1));
    }
}
