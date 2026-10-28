//! The playback engine. It runs inside the GUI thread's `update` calls and
//! never blocks: it looks the video up through InnerTube, fetches the
//! streams in paced HTTP Range requests, demuxes them, feeds Opus packets to
//! the audio thread and H.264 access units to the decoder thread, and picks
//! the video frame to show from the audio clock.

use crate::audio::{self, AudioClock, PcmSource, RATE};
use crate::clients::{self, Client};
use crate::error::{ErrorKind, PlayError};
use crate::innertube::{self, Answer, Format, PlayerInfo, Storyboard};
use crate::mp4::{self, Sample, Segment, VideoInit};
use crate::remote::{self, Mode};
use crate::spsc::{self, Consumer, Producer};
use crate::video::{DecoderThread, POOL, VideoFrame, VideoIn, VideoShared};
use crate::webm::{self, AudioInit, Packet};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use p4os_sdk::{FetchId, FetchState, FsError, HttpRequest, Image, PlaybackState, System};

/// Fetch more once less than this much is buffered ahead of the clock.
const VIDEO_AHEAD_MS: u64 = 20_000;
const AUDIO_AHEAD_MS: u64 = 30_000;
/// Upper bound of one Range request. Bigger ranges mean fewer TLS
/// handshakes; smaller ones keep bursts of decryption (and memory) short.
const CHUNK_BYTES: u64 = 384 * 1024;
/// Packets / samples in flight to the audio and decoder threads.
const AUDIO_QUEUE: usize = 512;
const VIDEO_QUEUE: usize = 96;
/// Start buffering below this much queued audio; resume above the other.
const LOW_AUDIO_MS: u64 = 100;
const RESUME_AUDIO_MS: u64 = 1500;
const RESUME_VIDEO_SAMPLES: usize = 25;
const OPUS_PACKET_MS: u64 = 20;
/// A request taking longer than this is abandoned and retried.
const FETCH_TIMEOUT_MS: u64 = 45_000;
const MAX_RETRIES: u32 = 4;
/// Refresh the stream URLs this long before YouTube says they expire.
const EXPIRY_MARGIN_S: u64 = 300;
/// A second refusal this soon after a refresh is final.
const REFRESH_GRACE_MS: u64 = 60_000;
const DECODER_STACK: usize = 64 * 1024;
/// Stack for decoding a storyboard JPEG (zune-jpeg).
const SHEET_STACK: usize = 96 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Asking InnerTube for the stream URLs.
    LookingUp,
    /// Fetching container headers and indexes.
    Opening,
    Playing,
    Ended,
    Failed(PlayError),
}

/// How the current video is being presented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    /// H.264 video with Opus audio (or silent, if there is no audio stream).
    Video { height: u32, with_audio: bool },
    /// Opus audio with storyboard thumbnails when YouTube provides them.
    Audio { storyboard: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Audio,
    Video,
}

enum FetchKind {
    Header,
    Segments { end: usize },
}

struct Inflight {
    id: FetchId,
    kind: FetchKind,
    generation: u32,
    started_ms: u64,
    range: (u64, u64),
}

enum Demux {
    Audio(Option<AudioInit>),
    Video(Option<VideoInit>),
}

struct Stream {
    format: Format,
    demux: Demux,
    index: Vec<Segment>,
    /// Next segment to fetch.
    next: usize,
    inflight: Option<Inflight>,
    retries: u32,
    retry_at_ms: u64,
    bytes: u64,
}

impl Stream {
    fn new(format: Format, demux: Demux) -> Self {
        Stream { format, demux, index: Vec::new(), next: 0, inflight: None, retries: 0, retry_at_ms: 0, bytes: 0 }
    }

    fn ready(&self) -> bool {
        !self.index.is_empty()
    }

    fn fetched_all(&self) -> bool {
        self.ready() && self.next >= self.index.len()
    }

    /// Media time up to which data has been fetched.
    fn buffered_to_ms(&self, duration_ms: u64) -> u64 {
        self.index.get(self.next).map_or(duration_ms, |s| s.start_ms)
    }

    /// Index of the segment containing `t_ms`.
    fn segment_at(&self, t_ms: u64) -> usize {
        self.index.iter().rposition(|s| s.start_ms <= t_ms).unwrap_or(0)
    }
}

struct AudioOut {
    tx: Producer<Packet>,
    pending: VecDeque<Packet>,
    clock: Arc<AudioClock>,
    /// Media time where the source started.
    base_ms: u64,
    paused: bool,
    finished: bool,
}

struct VideoOut {
    tx: Producer<VideoIn>,
    pending: VecDeque<Sample>,
    frames: Consumer<VideoFrame>,
    free: Producer<VideoFrame>,
    shared: Arc<VideoShared>,
    ready: VecDeque<VideoFrame>,
    current: Option<VideoFrame>,
    current_drawn: bool,
}

impl VideoOut {
    fn recycle(&mut self, f: VideoFrame) {
        // The pool is sized so this cannot fail; if it ever did the buffer
        // is simply freed and the decoder allocates another.
        let _ = self.free.push(f);
    }
}

/// Storyboard sheets for the audio-only mode.
struct Sheets {
    board: Storyboard,
    sheet: Option<(u32, Image)>,
    fetch: Option<(u32, FetchId)>,
    /// A sheet being decoded on a worker thread.
    decoding: Option<(u32, Consumer<Option<Image>>)>,
    failed: bool,
}

/// Counters for the stats line.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub shown: u32,
    /// Frames decoded and converted but replaced before they were drawn.
    pub dropped_display: u32,
    pub fps: f32,
    pub net_kbps: f32,
    /// Time spent drawing the player screen (ms, whole-ms resolution) and
    /// the number of draws, for the average.
    pub draw_ms: u64,
    pub draws: u64,
}

pub struct Player {
    pub video_id: String,
    pub mode: Mode,
    pub phase: Phase,
    pub info: Option<PlayerInfo>,
    /// Non-fatal message for the user (fallbacks taken, config problems).
    pub notice: Option<String>,
    pub paused: bool,
    pub buffering: bool,
    pub stats: Stats,
    clients: Vec<Client>,
    client: usize,
    used_visitor: bool,
    lookup: Option<FetchId>,
    lookup_retries: u32,
    refreshing: bool,
    looked_up_at_ms: u64,
    audio: Option<Stream>,
    video: Option<Stream>,
    aout: Option<AudioOut>,
    vout: Option<VideoOut>,
    sheets: Option<Sheets>,
    generation: u32,
    /// Where playback (re)starts once every stream is open.
    start_at_ms: u64,
    /// Fetches whose result no longer matters, cancelled on the next tick.
    stale: Vec<FetchId>,
    /// Clock without audio: (media ms, uptime ms) while running.
    wall: Option<(u64, u64)>,
    /// Clock value while the wall clock is stopped.
    wall_pos: u64,
    /// (window start, frames shown, bytes fetched) for rates.
    window: (u64, u32, u64),
    last_stats_ms: u64,
    /// Clock at the last tick.
    last_clock_ms: u64,
    /// The screen is locked or off: play audio, skip pictures.
    pub background: bool,
}

impl Player {
    pub fn new(video_id: &str, mode: Mode, sys: &mut dyn System) -> Self {
        let now = sys.uptime_ms();
        let mut p = Player {
            video_id: video_id.to_string(),
            mode,
            phase: Phase::LookingUp,
            info: None,
            notice: None,
            paused: false,
            buffering: true,
            stats: Stats::default(),
            clients: Vec::new(),
            client: 0,
            used_visitor: false,
            lookup: None,
            lookup_retries: 0,
            refreshing: false,
            looked_up_at_ms: 0,
            audio: None,
            video: None,
            aout: None,
            vout: None,
            sheets: None,
            generation: 0,
            start_at_ms: 0,
            stale: Vec::new(),
            wall: None,
            wall_pos: 0,
            window: (now, 0, 0),
            last_stats_ms: 0,
            last_clock_ms: 0,
            background: false,
        };
        if !valid_video_id(video_id) {
            p.phase = Phase::Failed(PlayError::new(ErrorKind::Unplayable, format!("{video_id:?} is not a YouTube video id")));
            return p;
        }
        p.clients = load_clients(sys, &mut p.notice);
        p.start_lookup(sys);
        p
    }

    fn fail(&mut self, e: PlayError, sys: &mut dyn System) {
        if self.stop_audio() {
            sys.audio().stop();
        }
        self.phase = Phase::Failed(e);
    }

    /// Stop the audio decoder thread; true if one was running.
    fn stop_audio(&mut self) -> bool {
        match self.aout.take() {
            Some(a) => {
                a.clock.stop.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    pub fn presentation(&self) -> Option<Presentation> {
        if let Some(v) = &self.video {
            return Some(Presentation::Video { height: v.format.height, with_audio: self.audio.is_some() });
        }
        self.audio.as_ref().map(|_| Presentation::Audio { storyboard: self.sheets.as_ref().is_some_and(|s| !s.failed) })
    }

    pub fn duration_ms(&self) -> u64 {
        let from_formats = [&self.audio, &self.video].into_iter().flatten().map(|s| s.format.duration_ms).max().unwrap_or(0);
        if from_formats > 0 {
            return from_formats;
        }
        self.info.as_ref().map_or(0, |i| i.length_s as u64 * 1000)
    }

    // ---- lookup -----------------------------------------------------------

    fn start_lookup(&mut self, sys: &mut dyn System) {
        let Some(client) = self.clients.get(self.client) else { return };
        let vd = remote::visitor_data();
        self.used_visitor = vd.is_some();
        let body = client.player_body(&self.video_id, vd.as_deref()).into_bytes();
        let mut req = HttpRequest::post(&client.player_url(), "application/json", body);
        req.headers.extend(client.headers(vd.as_deref()));
        self.lookup = Some(sys.net().send(req));
    }

    fn poll_lookup(&mut self, sys: &mut dyn System) {
        let Some(id) = self.lookup else { return };
        let (status, body) = match sys.net().poll(id) {
            FetchState::Pending => return,
            FetchState::Failed(e) => {
                self.lookup = None;
                if self.lookup_retries < 2 {
                    self.lookup_retries += 1;
                    self.start_lookup(sys);
                } else {
                    self.lookup_failed(PlayError::new(ErrorKind::Network, e), sys);
                }
                return;
            }
            FetchState::Done { status, body, .. } => (status, body),
        };
        self.lookup = None;
        let client_name = self.clients[self.client].name.clone();
        if status != 200 {
            return self.next_client(PlayError::new(ErrorKind::Http(status), format!("player request ({client_name})")), sys);
        }
        let answer = innertube::parse_player(&self.video_id, &String::from_utf8_lossy(&body));
        drop(body);
        match answer {
            Err(e) => self.next_client(e, sys),
            Ok(Answer::Refused { status, reason, visitor_data }) => {
                if status == "LOGIN_REQUIRED" && !self.used_visitor && let Some(vd) = visitor_data {
                    // First contact ("Sign in to confirm you're not a bot"):
                    // repeat the request with the visitorData YouTube just
                    // handed out.
                    remote::set_visitor_data(vd);
                    return self.start_lookup(sys);
                }
                let kind = if reason.contains("bot") { ErrorKind::ClientBlocked } else { ErrorKind::Unplayable };
                self.next_client(PlayError::new(kind, format!("{client_name}: {status}: {reason}")), sys);
            }
            Ok(Answer::Playable(info)) => {
                if self.refreshing {
                    self.apply_refresh(info, sys);
                } else {
                    self.open(info, sys);
                }
            }
        }
    }

    fn next_client(&mut self, why: PlayError, sys: &mut dyn System) {
        self.client += 1;
        if self.client < self.clients.len() {
            self.start_lookup(sys);
        } else {
            self.lookup_failed(why, sys);
        }
    }

    fn lookup_failed(&mut self, why: PlayError, sys: &mut dyn System) {
        if self.refreshing {
            let e = PlayError::new(ErrorKind::Expired, format!("refreshing the stream links failed: {why}"));
            self.fail(e, sys);
        } else {
            self.fail(why, sys);
        }
    }

    fn refresh(&mut self, sys: &mut dyn System) {
        if self.refreshing || self.lookup.is_some() {
            return;
        }
        self.refreshing = true;
        self.client = 0;
        self.lookup_retries = 0;
        self.start_lookup(sys);
    }

    /// New URLs for the same streams (they expired or were refused).
    fn apply_refresh(&mut self, info: PlayerInfo, sys: &mut dyn System) {
        self.refreshing = false;
        self.looked_up_at_ms = sys.uptime_ms();
        let mut missing = None;
        for s in [&mut self.audio, &mut self.video].into_iter().flatten() {
            match info.formats.iter().find(|f| f.itag == s.format.itag) {
                Some(f) => s.format.url = f.url.clone(),
                None => missing = Some(s.format.itag),
            }
            s.retries = 0;
            s.retry_at_ms = 0;
        }
        if let Some(itag) = missing {
            return self.fail(PlayError::new(ErrorKind::Expired, format!("itag {itag} disappeared after a refresh")), sys);
        }
        self.info = Some(info);
    }

    fn open(&mut self, info: PlayerInfo, sys: &mut dyn System) {
        self.looked_up_at_ms = sys.uptime_ms();
        if info.is_live {
            return self.fail(PlayError::new(ErrorKind::Live, "live streams have no seekable stream files"), sys);
        }
        let audio = innertube::pick_audio(&info).cloned();
        let video = match self.mode {
            Mode::Video(q) => innertube::pick_video(&info, q).cloned(),
            Mode::Audio => None,
        };
        if audio.is_none() && video.is_none() {
            let have: Vec<String> = info.formats.iter().map(|f| f.itag.to_string()).collect();
            return self.fail(PlayError::new(ErrorKind::NoFormat, format!("itags offered: {}", have.join(" "))), sys);
        }
        if matches!(self.mode, Mode::Video(_)) && video.is_none() {
            self.notice = Some("No H.264 stream: playing audio only".into());
        }
        if audio.is_none() {
            self.notice = Some("No Opus stream: video without sound".into());
        }
        self.audio = audio.map(|f| Stream::new(f, Demux::Audio(None)));
        if let Some(f) = video {
            match self.start_decoder(sys) {
                Ok(()) => self.video = Some(Stream::new(f, Demux::Video(None))),
                Err(e) if self.audio.is_some() => self.notice = Some(format!("{}: playing audio only", e.detail)),
                Err(e) => return self.fail(e, sys),
            }
        }
        self.info = Some(info);
        self.setup_storyboard();
        self.phase = Phase::Opening;
    }

    fn setup_storyboard(&mut self) {
        self.sheets = None;
        if self.video.is_none()
            && let Some(spec) = self.info.as_ref().and_then(|i| i.storyboard_spec.as_deref())
        {
            self.sheets = Storyboard::parse(spec).map(|board| Sheets { board, sheet: None, fetch: None, decoding: None, failed: false });
        }
    }

    fn start_decoder(&mut self, sys: &mut dyn System) -> Result<(), PlayError> {
        let (tx, input) = spsc::channel(VIDEO_QUEUE);
        let (output, frames) = spsc::channel(POOL);
        let (free, free_rx) = spsc::channel(POOL);
        let shared = Arc::new(VideoShared::default());
        shared.input_done.store(u32::MAX, Ordering::Relaxed);
        shared.generation.store(self.generation, Ordering::Relaxed);
        let thread = DecoderThread { input, output, free: free_rx, shared: shared.clone() };
        // Not pinned: the GUI thread (higher priority) moves between the
        // cores, and a decoder pinned to one of them stalls whenever the GUI
        // lands there. Unpinned, it takes whichever core is free.
        sys.spawn_worker("ytvideo", None, DECODER_STACK, Box::new(move |w| thread.run(w)))
            .map_err(|e| PlayError::new(ErrorKind::System, format!("cannot start the video decoder: {e}")))?;
        self.vout = Some(VideoOut { tx, pending: VecDeque::new(), frames, free, shared, ready: VecDeque::new(), current: None, current_drawn: false });
        Ok(())
    }

    fn stop_decoder(&mut self) {
        if let Some(v) = self.vout.take() {
            v.shared.stop.store(true, Ordering::Release);
        }
    }

    // ---- the clock --------------------------------------------------------

    /// Media position in ms.
    pub fn clock_ms(&self, now_ms: u64) -> u64 {
        match &self.aout {
            Some(a) if !a.finished => a.base_ms + a.clock.played.load(Ordering::Relaxed) as u64 * 1000 / RATE as u64,
            _ => match self.wall {
                Some((media, at)) => media + now_ms.saturating_sub(at),
                None => self.wall_pos,
            },
        }
    }

    fn set_running(&mut self, running: bool, sys: &mut dyn System) {
        let now = sys.uptime_ms();
        if let Some(a) = &mut self.aout
            && !a.finished
        {
            if a.paused == running {
                a.paused = !running;
                sys.audio().pause(!running);
            }
            return;
        }
        match (running, self.wall) {
            (true, None) => self.wall = Some((self.wall_pos, now)),
            (false, Some(_)) => {
                self.wall_pos = self.clock_ms(now);
                self.wall = None;
            }
            _ => {}
        }
    }

    // ---- controls ---------------------------------------------------------

    pub fn toggle_pause(&mut self, sys: &mut dyn System) {
        match self.phase {
            Phase::Playing => {
                self.paused = !self.paused;
                let run = !self.paused && !self.buffering;
                self.set_running(run, sys);
            }
            Phase::Ended => self.seek_to(0, sys),
            _ => {}
        }
    }

    pub fn seek_by(&mut self, delta_ms: i64, sys: &mut dyn System) {
        let now = self.clock_ms(sys.uptime_ms()) as i64;
        self.seek_to((now + delta_ms).max(0) as u64, sys);
    }

    pub fn seek_to(&mut self, target_ms: u64, sys: &mut dyn System) {
        if !matches!(self.phase, Phase::Playing | Phase::Ended) {
            return;
        }
        let dur = self.duration_ms();
        let target = if dur > 1000 { target_ms.min(dur - 1000) } else { 0 };
        self.begin(target, sys);
    }

    /// Switch between video qualities and audio-only, keeping the position.
    pub fn set_mode(&mut self, mode: Mode, sys: &mut dyn System) {
        if mode == self.mode || !matches!(self.phase, Phase::Playing | Phase::Ended | Phase::Opening) {
            self.mode = mode;
            return;
        }
        self.mode = mode;
        let pos = self.clock_ms(sys.uptime_ms());
        let Some(info) = self.info.clone() else { return };
        match mode {
            Mode::Audio => {
                if let Some(v) = self.video.take() {
                    self.forget_inflight(v);
                }
                self.stop_decoder();
                self.setup_storyboard();
                if self.audio.is_none() {
                    self.notice = Some("No Opus stream for audio-only mode".into());
                }
            }
            Mode::Video(q) => {
                let Some(f) = innertube::pick_video(&info, q).cloned() else {
                    self.notice = Some("No H.264 stream for this quality".into());
                    return;
                };
                if self.video.as_ref().is_some_and(|v| v.format.itag == f.itag) {
                    return;
                }
                if let Some(v) = self.video.take() {
                    self.forget_inflight(v);
                }
                if self.vout.is_none()
                    && let Err(e) = self.start_decoder(sys)
                {
                    self.notice = Some(e.detail);
                    return;
                }
                self.video = Some(Stream::new(f, Demux::Video(None)));
                self.sheets = None;
                self.start_at_ms = pos;
                self.phase = Phase::Opening;
            }
        }
    }

    fn forget_inflight(&mut self, mut s: Stream) {
        if let Some(i) = s.inflight.take() {
            self.stale.push(i.id);
        }
    }

    /// Start (or restart) playback at `target`: drop everything queued,
    /// snap to the video keyframe before it and restart the audio there.
    fn begin(&mut self, target: u64, sys: &mut dyn System) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let mut t0 = target;
        if let Some(v) = &mut self.video {
            let i = v.segment_at(target);
            t0 = v.index[i].start_ms;
            v.next = i;
            if let Some(f) = v.inflight.take() {
                self.stale.push(f.id);
            }
        }
        if let Some(vo) = &mut self.vout {
            vo.pending.clear();
            vo.shared.generation.store(generation, Ordering::Release);
            vo.shared.input_done.store(u32::MAX, Ordering::Release);
            vo.shared.clock_ms.store(t0 as i32, Ordering::Relaxed);
            while let Some(f) = vo.ready.pop_front() {
                vo.recycle(f);
            }
        }
        self.stop_audio();
        if let Some(a) = &mut self.audio {
            a.next = a.segment_at(t0);
            if let Some(f) = a.inflight.take() {
                self.stale.push(f.id);
            }
            if let Demux::Audio(Some(init)) = &a.demux {
                let (tx, packets) = spsc::channel(AUDIO_QUEUE);
                let (pcm, pcm_rx) = spsc::channel(audio::PCM_QUEUE);
                let clock = Arc::new(AudioClock::default());
                let thread = audio::DecoderThread { packets, pcm, channels: init.channels(), pre_skip: init.pre_skip(), start_ms: t0 as i64, clock: clock.clone() };
                if let Err(e) = sys.spawn_worker("ytaudio", None, audio::DECODER_STACK, Box::new(move |w| thread.run(w))) {
                    return self.fail(PlayError::new(ErrorKind::System, format!("cannot start the audio decoder: {e}")), sys);
                }
                sys.audio().play(Box::new(PcmSource::new(pcm_rx, clock.clone())));
                sys.audio().pause(true);
                self.aout = Some(AudioOut { tx, pending: VecDeque::new(), clock, base_ms: t0, paused: true, finished: false });
            }
        }
        self.wall = None;
        self.wall_pos = t0;
        self.buffering = true;
        self.phase = Phase::Playing;
    }

    // ---- fetching ---------------------------------------------------------

    fn stream_mut(&mut self, w: Which) -> &mut Option<Stream> {
        match w {
            Which::Audio => &mut self.audio,
            Which::Video => &mut self.video,
        }
    }

    fn service(&mut self, w: Which, sys: &mut dyn System) {
        let Some(mut s) = self.stream_mut(w).take() else { return };
        let r = self.service_stream(w, &mut s, sys);
        *self.stream_mut(w) = Some(s);
        if let Err(e) = r {
            self.fail(e, sys);
        }
    }

    fn service_stream(&mut self, w: Which, s: &mut Stream, sys: &mut dyn System) -> Result<(), PlayError> {
        let now = sys.uptime_ms();
        if let Some(inf) = &s.inflight {
            let state = sys.net().poll(inf.id);
            let inf = match state {
                FetchState::Pending if now.saturating_sub(inf.started_ms) > FETCH_TIMEOUT_MS => {
                    let inf = s.inflight.take().expect("checked above");
                    self.stale.push(inf.id);
                    return retry(s, now, PlayError::new(ErrorKind::Network, "stream request timed out"));
                }
                FetchState::Pending => return Ok(()),
                FetchState::Failed(e) => {
                    s.inflight = None;
                    return retry(s, now, PlayError::new(ErrorKind::Network, e));
                }
                FetchState::Done { status, body, .. } => {
                    let inf = s.inflight.take().expect("checked above");
                    (inf, status, body)
                }
            };
            let (inf, status, body) = inf;
            if matches!(status, 403 | 410) {
                // Expired or revoked links: refresh once, then give up.
                if now.saturating_sub(self.looked_up_at_ms) < REFRESH_GRACE_MS {
                    return Err(PlayError::new(ErrorKind::Http(status), "YouTube refused the stream even with fresh links"));
                }
                s.retry_at_ms = now;
                self.refresh(sys);
                return Ok(());
            }
            let data = range_body(status, body, inf.range, s.format.content_length);
            let data = match data {
                Ok(d) => d,
                Err(e) if matches!(e.kind, ErrorKind::Http(500..=599)) => return retry(s, now, e),
                Err(e) => return Err(e),
            };
            s.retries = 0;
            s.bytes += data.len() as u64;
            self.window.2 += data.len() as u64;
            match inf.kind {
                FetchKind::Header => self.parse_header(s, &data)?,
                FetchKind::Segments { end } if inf.generation == self.generation => {
                    self.demux(w, s, data)?;
                    s.next = end;
                }
                FetchKind::Segments { .. } => {}
            }
            return Ok(());
        }
        if self.refreshing || now < s.retry_at_ms {
            return Ok(());
        }
        if !s.ready() {
            let (a, b) = match (s.format.init_range, s.format.index_range) {
                (Some(i), Some(x)) => (i.0.min(x.0), i.1.max(x.1)),
                _ => return Err(PlayError::new(ErrorKind::NoFormat, "stream without index")),
            };
            let id = sys.net().send(HttpRequest::get(&s.format.url).range(a, b));
            s.inflight = Some(Inflight { id, kind: FetchKind::Header, generation: self.generation, started_ms: now, range: (a, b) });
            return Ok(());
        }
        if self.phase != Phase::Playing || s.fetched_all() {
            return Ok(());
        }
        let ahead = if w == Which::Audio { AUDIO_AHEAD_MS } else { VIDEO_AHEAD_MS };
        let clock = self.clock_ms(now);
        let dur = self.duration_ms();
        if s.buffered_to_ms(dur) > clock + ahead || !self.has_room(w) {
            return Ok(());
        }
        // Whole segments up to CHUNK_BYTES (at least one).
        let first = s.next;
        let mut end = first + 1;
        let mut bytes = s.index[first].size;
        while end < s.index.len() && bytes + s.index[end].size <= CHUNK_BYTES {
            bytes += s.index[end].size;
            end += 1;
        }
        let a = s.index[first].offset;
        let b = a + bytes - 1;
        let id = sys.net().send(HttpRequest::get(&s.format.url).range(a, b));
        s.inflight = Some(Inflight { id, kind: FetchKind::Segments { end }, generation: self.generation, started_ms: now, range: (a, b) });
        Ok(())
    }

    /// Room to take another chunk: the demuxed data waiting for the decoder
    /// threads stays bounded.
    fn has_room(&self, w: Which) -> bool {
        match w {
            Which::Audio => self.aout.as_ref().is_some_and(|a| a.pending.len() < AUDIO_QUEUE * 2),
            Which::Video => self.vout.as_ref().is_some_and(|v| v.pending.len() < VIDEO_QUEUE * 4),
        }
    }

    fn parse_header(&mut self, s: &mut Stream, data: &[u8]) -> Result<(), PlayError> {
        let bad = |e: String| PlayError::new(ErrorKind::BadResponse, e);
        let (Some(init), Some(index)) = (s.format.init_range, s.format.index_range) else {
            return Err(bad("stream without index".into()));
        };
        let base = init.0.min(index.0);
        let slice = |r: (u64, u64)| data.get((r.0 - base) as usize..=(r.1 - base) as usize).ok_or_else(|| bad("short header".into()));
        match &mut s.demux {
            Demux::Audio(slot) => {
                let a = webm::parse_init(slice(init)?).map_err(bad)?;
                s.index = webm::parse_cues(slice(index)?, &a, s.format.content_length).map_err(bad)?;
                *slot = Some(a);
            }
            Demux::Video(slot) => {
                let v = mp4::parse_init(slice(init)?).map_err(bad)?;
                s.index = mp4::parse_sidx(slice(index)?, index.0).map_err(bad)?;
                *slot = Some(v);
            }
        }
        Ok(())
    }

    fn demux(&mut self, w: Which, s: &Stream, data: Vec<u8>) -> Result<(), PlayError> {
        let bad = |e: String| PlayError::new(ErrorKind::BadResponse, e);
        match (&s.demux, w) {
            (Demux::Audio(Some(init)), Which::Audio) => {
                let packets = webm::parse_clusters(&Arc::new(data), init).map_err(bad)?;
                if let Some(a) = &mut self.aout {
                    a.pending.extend(packets);
                }
            }
            (Demux::Video(Some(init)), Which::Video) => {
                let samples = mp4::parse_fragments(data, init).map_err(bad)?;
                if let Some(v) = &mut self.vout {
                    v.pending.extend(samples);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Move demuxed data into the queues of the audio and decoder threads.
    fn feed(&mut self) {
        let generation = self.generation;
        if let Some(a) = &mut self.aout {
            while let Some(p) = a.pending.pop_front() {
                if let Err(p) = a.tx.push(p) {
                    a.pending.push_front(p);
                    break;
                }
            }
            if a.pending.is_empty() && self.audio.as_ref().is_some_and(Stream::fetched_all) {
                a.clock.input_done.store(true, Ordering::Release);
            }
        }
        if let Some(v) = &mut self.vout {
            while let Some(sample) = v.pending.pop_front() {
                if let Err(item) = v.tx.push(VideoIn { generation, sample }) {
                    v.pending.push_front(item.sample);
                    break;
                }
            }
            if v.pending.is_empty() && self.video.as_ref().is_some_and(Stream::fetched_all) {
                v.shared.input_done.store(generation, Ordering::Release);
            }
        }
    }

    // ---- per-frame work ---------------------------------------------------

    /// Advance everything; call on every GUI tick.
    pub fn tick(&mut self, sys: &mut dyn System) {
        self.step(sys);
        self.update_stats(sys);
    }

    fn step(&mut self, sys: &mut dyn System) {
        let now = sys.uptime_ms();
        for id in self.stale.drain(..) {
            sys.net().cancel(id);
        }
        self.poll_lookup(sys);
        if matches!(self.phase, Phase::Failed(_) | Phase::LookingUp) {
            return;
        }
        if !self.refreshing
            && let Some(info) = &self.info
            && now.saturating_sub(self.looked_up_at_ms) / 1000 + EXPIRY_MARGIN_S > info.expires_in_s as u64
        {
            self.refresh(sys);
        }
        self.service(Which::Audio, sys);
        self.service(Which::Video, sys);
        if matches!(self.phase, Phase::Failed(_)) {
            return;
        }
        if self.phase == Phase::Opening && [&self.audio, &self.video].into_iter().flatten().all(Stream::ready) {
            self.begin(self.start_at_ms, sys);
        }
        if self.phase != Phase::Playing && self.phase != Phase::Ended {
            return;
        }
        self.feed();
        if let Some(a) = &self.aout
            && a.clock.failed.load(Ordering::Acquire)
        {
            return self.fail(PlayError::new(ErrorKind::Decode, "the Opus decoder could not start"), sys);
        }
        if let Some(v) = &self.vout
            && v.shared.failed.load(Ordering::Acquire)
        {
            return self.fail(PlayError::new(ErrorKind::Decode, "the H.264 decoder gave up on this stream"), sys);
        }
        self.update_buffering(sys);
        let clock = self.clock_ms(sys.uptime_ms());
        if self.background {
            // Nothing is shown: let the decoder stall on a full frame pool
            // instead of decoding pictures nobody sees. It catches up at
            // the next keyframe once the screen is back.
            if let Some(v) = &self.vout {
                v.shared.clock_ms.store(clock as i32, Ordering::Relaxed);
            }
        } else {
            self.pick_frame(clock);
            self.update_storyboard(clock, sys);
        }
        self.check_end(clock, sys);
    }

    fn update_buffering(&mut self, sys: &mut dyn System) {
        let device_lost = sys.audio().state() == PlaybackState::Stopped;
        if let Some(a) = &mut self.aout
            && !a.finished
            && (a.clock.finished.load(Ordering::Acquire) || device_lost)
        {
            if device_lost && !a.clock.finished.load(Ordering::Acquire) {
                // The OS has no audio output (or another app took it):
                // keep the picture going on the wall clock.
                self.notice = Some("Audio output unavailable: playing without sound".into());
            }
            // Audio ran out first: let the wall clock carry on from there.
            let now = sys.uptime_ms();
            let pos = a.base_ms + a.clock.played.load(Ordering::Relaxed) as u64 * 1000 / RATE as u64;
            a.finished = true;
            self.wall_pos = pos;
            self.wall = (!self.paused).then_some((pos, now));
        }
        let audio_q = self.aout.as_ref().filter(|a| !a.finished).map(|a| (a.tx.len() + a.pending.len()) as u64 * OPUS_PACKET_MS);
        let audio_done = self.audio.as_ref().is_none_or(Stream::fetched_all) && self.aout.as_ref().is_none_or(|a| a.pending.is_empty());
        let video_done = self.video.as_ref().is_none_or(Stream::fetched_all) && self.vout.as_ref().is_none_or(|v| v.pending.is_empty());
        let video_q = self.vout.as_ref().map(|v| v.tx.len() + v.pending.len() + v.ready.len());
        let has_frame = self.vout.as_ref().is_none_or(|v| !v.ready.is_empty() || v.current.is_some());
        let starving = audio_q.is_some_and(|q| q < LOW_AUDIO_MS && !audio_done) || video_q.is_some_and(|q| q == 0 && !video_done);
        if !self.buffering && starving {
            self.buffering = true;
        } else if self.buffering {
            let audio_ok = audio_done || audio_q.is_none_or(|q| q >= RESUME_AUDIO_MS);
            let video_ok = video_done || (video_q.is_none_or(|q| q >= RESUME_VIDEO_SAMPLES) && has_frame);
            if audio_ok && video_ok {
                self.buffering = false;
            }
        }
        let run = !self.paused && !self.buffering && self.phase == Phase::Playing;
        self.set_running(run, sys);
    }

    fn pick_frame(&mut self, clock: u64) {
        self.last_clock_ms = clock;
        let generation = self.generation;
        let Some(v) = &mut self.vout else { return };
        v.shared.clock_ms.store(clock as i32, Ordering::Relaxed);
        while let Some(f) = v.frames.pop() {
            if f.generation == generation {
                v.ready.push_back(f);
            } else {
                v.recycle(f);
            }
        }
        while v.ready.front().is_some_and(|f| f.pts_ms <= clock as i64) {
            let f = v.ready.pop_front().expect("checked above");
            if let Some(old) = v.current.replace(f) {
                if !v.current_drawn {
                    self.stats.dropped_display += 1;
                }
                v.recycle(old);
            }
            v.current_drawn = false;
        }
    }

    /// Time until the next decoded picture is due, if one is ready.
    pub fn next_frame_in_ms(&self) -> Option<u64> {
        let pts = self.vout.as_ref()?.ready.front()?.pts_ms.max(0) as u64;
        Some(pts.saturating_sub(self.last_clock_ms))
    }

    /// The video frame to draw now; counts it as shown.
    pub fn frame(&mut self) -> Option<&VideoFrame> {
        let v = self.vout.as_mut()?;
        if v.current.is_some() && !v.current_drawn {
            v.current_drawn = true;
            self.stats.shown += 1;
            self.window.1 += 1;
        }
        v.current.as_ref()
    }

    fn update_storyboard(&mut self, clock: u64, sys: &mut dyn System) {
        let dur = self.duration_ms();
        let Some(sb) = &mut self.sheets else { return };
        if sb.failed {
            return;
        }
        if let Some((n, rx)) = &mut sb.decoding {
            if let Some(img) = rx.pop() {
                match img {
                    Some(img) => sb.sheet = Some((*n, img)),
                    None => sb.failed = true,
                }
                sb.decoding = None;
            }
            return;
        }
        if let Some((n, id)) = sb.fetch {
            match sys.net().poll(id) {
                FetchState::Pending => {}
                FetchState::Done { status: 200, body, .. } => {
                    sb.fetch = None;
                    // JPEG decoding needs more stack than the GUI thread has.
                    let (w, h) = ((sb.board.width * sb.board.cols) as i32, (sb.board.height * sb.board.rows) as i32);
                    let (mut tx, rx) = spsc::channel(1);
                    let job = move |_: &dyn p4os_sdk::Worker| {
                        let _ = tx.push(p4os_media::image::decode(&body, w, h));
                    };
                    match sys.spawn_worker("ytsheet", None, SHEET_STACK, Box::new(job)) {
                        Ok(()) => sb.decoding = Some((n, rx)),
                        Err(_) => sb.failed = true,
                    }
                }
                _ => {
                    sb.fetch = None;
                    sb.failed = true;
                }
            }
            return;
        }
        let (sheet, _, _) = sb.board.locate(clock, dur);
        if sb.sheet.as_ref().is_none_or(|(n, _)| *n != sheet) {
            let id = sys.net().fetch(&sb.board.sheet_url(sheet));
            sb.fetch = Some((sheet, id));
        }
    }

    /// The storyboard sheet and the thumbnail cell (x, y, w, h) for now.
    pub fn thumbnail(&self, now_ms: u64) -> Option<(&Image, (i32, i32, i32, i32))> {
        let sb = self.sheets.as_ref()?;
        let (sheet, col, row) = sb.board.locate(self.clock_ms(now_ms), self.duration_ms());
        let (n, img) = sb.sheet.as_ref()?;
        // While the right sheet loads, keep showing the previous one.
        let (col, row) = if *n == sheet { (col, row) } else { (sb.board.cols - 1, sb.board.rows - 1) };
        let (w, h) = (sb.board.width as i32, sb.board.height as i32);
        Some((img, (col as i32 * w, row as i32 * h, w, h)))
    }

    fn check_end(&mut self, clock: u64, sys: &mut dyn System) {
        if self.phase != Phase::Playing || self.buffering {
            return;
        }
        let audio_end = self.aout.as_ref().is_none_or(|a| a.finished) && self.audio.as_ref().is_none_or(Stream::fetched_all);
        let video_end = self.vout.as_ref().is_none_or(|v| {
            v.tx.is_empty() && v.pending.is_empty() && v.ready.is_empty() && self.video.as_ref().is_some_and(Stream::fetched_all)
        });
        let dur = self.duration_ms();
        if audio_end && video_end && (self.aout.is_some() || clock + 200 >= dur) {
            self.phase = Phase::Ended;
            self.set_running(false, sys);
        }
    }

    fn update_stats(&mut self, sys: &mut dyn System) {
        let now = sys.uptime_ms();
        let (start, shown, bytes) = self.window;
        let dt = now.saturating_sub(start);
        if dt >= 2000 {
            self.stats.fps = shown as f32 * 1000.0 / dt as f32;
            self.stats.net_kbps = bytes as f32 * 8.0 / dt as f32;
            self.window = (now, 0, 0);
        }
        if now.saturating_sub(self.last_stats_ms) >= 1000 {
            self.last_stats_ms = now;
            let st = sys.stats();
            let loads: Vec<String> = st.core_loads.iter().map(|l| format!("{:.0}%", l * 100.0)).collect();
            let line = format!(
                "{} | cpu {} | sram {} KiB psram {} KiB | {}",
                self.stats_line(now),
                loads.join("/"),
                st.heap_used / 1024,
                st.psram_used / 1024,
                st.chip
            );
            remote::publish_stats(line);
        }
    }

    /// One line describing the playback, for `yt stat` and the log.
    pub fn stats_line(&self, now_ms: u64) -> String {
        let pos = self.clock_ms(now_ms);
        let mut s = format!(
            "yt {} {:?} pos {}.{}/{} s",
            self.video_id,
            self.phase,
            pos / 1000,
            pos % 1000 / 100,
            self.duration_ms() / 1000
        );
        if let Some(p) = self.presentation() {
            s.push_str(&format!(" | {p:?}"));
        }
        if self.paused {
            s.push_str(" paused");
        }
        if self.buffering {
            s.push_str(" buffering");
        }
        if let Some(v) = &self.vout {
            let sh = &v.shared;
            let decoded = sh.decoded.load(Ordering::Relaxed).max(1);
            s.push_str(&format!(
                " | fps {:.1} shown {} decoded {} late {} skipped {} replaced {} | decode avg {} us max {} | yuv->rgb avg {} us | queue {}+{}",
                self.stats.fps,
                self.stats.shown,
                sh.decoded.load(Ordering::Relaxed),
                sh.dropped_late.load(Ordering::Relaxed),
                sh.skipped.load(Ordering::Relaxed),
                self.stats.dropped_display,
                sh.decode_us.load(Ordering::Relaxed) / decoded,
                sh.decode_max_us.load(Ordering::Relaxed),
                sh.convert_us.load(Ordering::Relaxed) / decoded,
                v.tx.len(),
                v.pending.len()
            ));
        }
        if let Some(a) = &self.aout {
            let c = &a.clock;
            s.push_str(&format!(
                " | audio queued {} ms opus avg {} us underruns {} errors {} sum {:08x}",
                (a.tx.len() + a.pending.len()) as u64 * OPUS_PACKET_MS,
                c.decode_us.load(Ordering::Relaxed) / c.decoded.load(Ordering::Relaxed).max(1),
                c.underruns.load(Ordering::Relaxed),
                c.decode_errors.load(Ordering::Relaxed),
                c.checksum.load(Ordering::Relaxed)
            ));
        }
        let dur = self.duration_ms();
        for (name, st) in [("a", &self.audio), ("v", &self.video)] {
            if let Some(st) = st {
                let fetching = match &st.inflight {
                    Some(i) => format!(" fetching {} s", now_ms.saturating_sub(i.started_ms) / 1000),
                    None if st.retries > 0 => format!(" retry {}", st.retries),
                    None => String::new(),
                };
                s.push_str(&format!(
                    " | {name} itag {} ahead {} s {} KiB{fetching}",
                    st.format.itag,
                    st.buffered_to_ms(dur).saturating_sub(pos) / 1000,
                    st.bytes / 1024
                ));
            }
        }
        s.push_str(&format!(" | net {:.0} kbit/s | draw avg {:.1} ms", self.stats.net_kbps, self.stats.draw_ms as f32 / self.stats.draws.max(1) as f32));
        s
    }

    /// Stop playback and release the audio device and decoder thread.
    pub fn close(&mut self, sys: &mut dyn System) {
        if self.stop_audio() {
            sys.audio().stop();
        }
        self.stop_decoder();
        let mut ids: Vec<FetchId> = self.stale.drain(..).collect();
        for s in [&mut self.audio, &mut self.video].into_iter().flatten() {
            ids.extend(s.inflight.take().map(|i| i.id));
        }
        ids.extend(self.lookup.take());
        ids.extend(self.sheets.as_mut().and_then(|s| s.fetch.take()).map(|(_, id)| id));
        for id in ids {
            sys.net().cancel(id);
        }
    }

    /// Whether the GUI should keep ticking quickly.
    pub fn is_active(&self) -> bool {
        !matches!(self.phase, Phase::Failed(_) | Phase::Ended) || self.lookup.is_some()
    }
}

/// Schedule a retry with backoff, or fail after too many.
fn retry(s: &mut Stream, now: u64, e: PlayError) -> Result<(), PlayError> {
    s.retries += 1;
    if s.retries > MAX_RETRIES {
        return Err(e);
    }
    s.retry_at_ms = now + 1000 * (1 << (s.retries - 1));
    Ok(())
}

/// The bytes of a Range answer: 206 with exactly the range, or a full 200
/// answer cut down to it.
fn range_body(status: u16, body: Vec<u8>, range: (u64, u64), content_length: u64) -> Result<Vec<u8>, PlayError> {
    let want = range.1 - range.0 + 1;
    match status {
        206 if body.len() as u64 == want => Ok(body),
        206 => Err(PlayError::new(ErrorKind::BadResponse, format!("got {} of {} bytes", body.len(), want))),
        200 if content_length > 0 && body.len() as u64 == content_length && range.1 < content_length => {
            Ok(body[range.0 as usize..=range.1 as usize].to_vec())
        }
        s => Err(PlayError::new(ErrorKind::Http(s), "stream request")),
    }
}

fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The client table from the SD card, else the built-in one.
fn load_clients(sys: &mut dyn System, notice: &mut Option<String>) -> Vec<Client> {
    match sys.fs().read(clients::CLIENTS_PATH) {
        Ok(bytes) => match core::str::from_utf8(&bytes).map_err(|_| "not UTF-8".to_string()).and_then(clients::parse) {
            Ok(c) => return c,
            Err(e) => *notice = Some(format!("{} invalid ({e}); using the built-in table", clients::CLIENTS_PATH)),
        },
        Err(FsError::NotFound | FsError::NoMedium) => {}
        Err(e) => *notice = Some(format!("cannot read {} ({e:?}); using the built-in table", clients::CLIENTS_PATH)),
    }
    clients::defaults()
}

/// Accept a bare id or any common YouTube URL form.
pub fn parse_video_id(s: &str) -> Option<String> {
    let s = s.trim();
    if valid_video_id(s) {
        return Some(s.to_string());
    }
    let after = |marker: &str| s.find(marker).map(|i| &s[i + marker.len()..]);
    let tail = after("v=").or_else(|| after("youtu.be/")).or_else(|| after("/shorts/")).or_else(|| after("/embed/"))?;
    let id: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    valid_video_id(&id).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_video_ids_and_urls() {
        for s in [
            "dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10",
            "https://youtu.be/dQw4w9WgXcQ?si=x",
            "youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
        ] {
            assert_eq!(parse_video_id(s).as_deref(), Some("dQw4w9WgXcQ"), "{s}");
        }
        assert_eq!(parse_video_id("short"), None);
        assert_eq!(parse_video_id("https://example.com/"), None);
    }

    #[test]
    fn accepts_only_exact_ranges() {
        assert_eq!(range_body(206, alloc::vec![1, 2, 3], (10, 12), 100).unwrap(), [1, 2, 3]);
        assert!(range_body(206, alloc::vec![1, 2], (10, 12), 100).is_err());
        let full: Vec<u8> = (0..20).collect();
        assert_eq!(range_body(200, full.clone(), (3, 5), 20).unwrap(), [3, 4, 5]);
        assert_eq!(range_body(200, full, (3, 5), 30).unwrap_err().kind, ErrorKind::Http(200));
        assert_eq!(range_body(403, Vec::new(), (0, 1), 2).unwrap_err().kind, ErrorKind::Http(403));
    }

    #[test]
    fn retries_with_backoff_then_gives_up() {
        let mut s = Stream::new(Format::default(), Demux::Audio(None));
        let e = PlayError::new(ErrorKind::Network, "x");
        for (i, wait) in [1000, 2000, 4000, 8000].iter().enumerate() {
            retry(&mut s, 100, e.clone()).unwrap();
            assert_eq!(s.retry_at_ms, 100 + wait, "retry {i}");
        }
        assert!(retry(&mut s, 100, e).is_err());
    }
}
