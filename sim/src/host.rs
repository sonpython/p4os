//! `System` implementation backed by the host: a directory stands in for
//! the SD card, HTTP and HTTPS go through std::net and p4os-net, audio is paced in real time and
//! discarded (the simulator has no sound output).

use p4os_net::http::{self, Url};
use p4os_net::tls::{self, TlsClient};
use p4os_sdk::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct HostSystem {
    start: Instant,
    fs: HostFs,
    audio: HostAudio,
    net: HostNet,
    brightness: f32,
    pub orientation: Orientation,
    /// Settings kept in memory for the session (the board uses flash).
    config: HashMap<String, String>,
}

impl HostSystem {
    pub fn new(sd_root: PathBuf) -> Self {
        Self {
            start: Instant::now(),
            fs: HostFs { root: sd_root },
            audio: HostAudio::default(),
            net: HostNet::default(),
            brightness: 0.8,
            orientation: Orientation::Portrait,
            config: HashMap::new(),
        }
    }
}

impl System for HostSystem {
    fn uptime_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn now(&self) -> Option<DateTime> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        Some(datetime_from_unix(secs))
    }

    fn set_time(&mut self, _t: DateTime) {}

    fn fs(&mut self) -> &mut dyn FileSystem {
        &mut self.fs
    }

    fn audio(&mut self) -> &mut dyn Audio {
        &mut self.audio
    }

    fn net(&mut self) -> &mut dyn Network {
        &mut self.net
    }

    fn stats(&self) -> SystemStats {
        SystemStats {
            uptime_ms: self.uptime_ms(),
            chip: "host simulator".into(),
            ..Default::default()
        }
    }

    fn decode_image(&mut self, data: &[u8], max_w: i32, max_h: i32) -> Option<Image> {
        p4os_media::image::decode(data, max_w, max_h)
    }

    fn brightness(&self) -> f32 {
        self.brightness
    }

    fn set_brightness(&mut self, value: f32) {
        self.brightness = value;
    }

    fn run_command(&mut self, line: &str) -> String {
        format!("(simulator) no kernel shell; got `{line}`\n")
    }

    fn launch(&mut self, _app_id: &str) {}

    fn orientation(&self) -> Orientation {
        self.orientation
    }

    fn set_orientation(&mut self, o: Orientation) {
        self.orientation = o;
    }

    fn wifi_connect(&mut self, ssid: &str, password: &str) {
        // The host is already online; just remember the network.
        self.config.insert(config::WIFI_SSID.into(), ssid.into());
        self.config.insert(config::WIFI_PASSWORD.into(), password.into());
    }

    fn config_get(&self, key: &str) -> Option<String> {
        if config::is_secret(key) {
            return None;
        }
        self.config.get(key).cloned()
    }

    fn config_set(&mut self, key: &str, value: &str) {
        self.config.insert(key.into(), value.into());
    }

    fn config_remove(&mut self, key: &str) {
        self.config.remove(key);
    }
}

impl HostSystem {
    /// Answer requests whose URL contains `pattern` with a canned response
    /// instead of going to the network (screenshots with fixture data).
    pub fn mock(&mut self, pattern: &str, status: u16, file: PathBuf) {
        self.net.mocks.push(Mock { pattern: pattern.into(), status, file });
    }
}

pub fn datetime_from_unix(secs: u64) -> DateTime {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as u16;
    DateTime {
        year,
        month,
        day,
        hour: (rem / 3600) as u8,
        minute: (rem / 60 % 60) as u8,
        second: (rem % 60) as u8,
    }
}

struct HostFs {
    root: PathBuf,
}

impl HostFs {
    fn resolve(&self, path: &str) -> Result<PathBuf, FsError> {
        let rest = path.strip_prefix("/sd").ok_or(FsError::NotFound)?;
        let rest = rest.trim_start_matches('/');
        if rest.split('/').any(|c| c == "..") {
            return Err(FsError::NotFound);
        }
        Ok(self.root.join(rest))
    }
}

fn io_err(e: std::io::Error) -> FsError {
    match e.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        _ => FsError::Io,
    }
}

impl FileSystem for HostFs {
    fn list(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        if path == "/" {
            return Ok(vec![DirEntry { name: "sd".into(), is_dir: true, size: 0 }]);
        }
        let dir = self.resolve(path)?;
        let mut out: Vec<DirEntry> = std::fs::read_dir(dir)
            .map_err(io_err)?
            .filter_map(|e| e.ok())
            .map(|e| {
                let meta = e.metadata().ok();
                DirEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
                    size: meta.map_or(0, |m| m.len()),
                }
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    fn read(&mut self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.resolve(path)?).map_err(io_err)
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        std::fs::write(self.resolve(path)?, data).map_err(io_err)
    }

    fn create_dir(&mut self, path: &str) -> Result<(), FsError> {
        std::fs::create_dir(self.resolve(path)?).map_err(io_err)
    }

    fn remove(&mut self, path: &str) -> Result<(), FsError> {
        let p = self.resolve(path)?;
        if p.is_dir() { std::fs::remove_dir(p) } else { std::fs::remove_file(p) }.map_err(io_err)
    }

    fn open(&mut self, path: &str) -> Result<Box<dyn FileReader>, FsError> {
        let f = std::fs::File::open(self.resolve(path)?).map_err(io_err)?;
        let size = f.metadata().map_err(io_err)?.len();
        Ok(Box::new(HostFile { f, size }))
    }
}

struct HostFile {
    f: std::fs::File,
    size: u64,
}

impl FileReader for HostFile {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, FsError> {
        self.f.read(buf).map_err(io_err)
    }

    fn size(&self) -> u64 {
        self.size
    }
}

/// Plays sources in real time on a thread and throws the samples away.
#[derive(Default)]
struct HostAudio {
    shared: Arc<AudioShared>,
    volume: f32,
}

#[derive(Default)]
struct AudioShared {
    generation: AtomicU64,
    playing: AtomicBool,
    paused: AtomicBool,
    frames: AtomicU64,
}

impl Audio for HostAudio {
    fn play(&mut self, mut source: Box<dyn AudioSource>) {
        let sh = self.shared.clone();
        let my_gen = sh.generation.fetch_add(1, Ordering::SeqCst) + 1;
        sh.frames.store(0, Ordering::SeqCst);
        sh.paused.store(false, Ordering::SeqCst);
        sh.playing.store(true, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut buf = vec![0i16; 2048];
            let rate = source.sample_rate().max(1) as u64;
            while sh.generation.load(Ordering::SeqCst) == my_gen {
                if sh.paused.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let n = source.fill(&mut buf);
                if n == 0 {
                    break;
                }
                let frames = (n / 2) as u64;
                sh.frames.fetch_add(frames, Ordering::SeqCst);
                std::thread::sleep(Duration::from_micros(frames * 1_000_000 / rate));
            }
            if sh.generation.load(Ordering::SeqCst) == my_gen {
                sh.playing.store(false, Ordering::SeqCst);
            }
        });
    }

    fn pause(&mut self, paused: bool) {
        self.shared.paused.store(paused, Ordering::SeqCst);
    }

    fn stop(&mut self) {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        self.shared.playing.store(false, Ordering::SeqCst);
    }

    fn state(&self) -> PlaybackState {
        match (self.shared.playing.load(Ordering::SeqCst), self.shared.paused.load(Ordering::SeqCst)) {
            (false, _) => PlaybackState::Stopped,
            (true, true) => PlaybackState::Paused,
            (true, false) => PlaybackState::Playing,
        }
    }

    fn position_frames(&self) -> u64 {
        self.shared.frames.load(Ordering::SeqCst)
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
    }

    fn volume(&self) -> f32 {
        self.volume
    }
}

struct Mock {
    pattern: String,
    status: u16,
    file: PathBuf,
}

impl Mock {
    fn respond(&self) -> FetchState {
        let content_type = match self.file.extension().and_then(|e| e.to_str()) {
            Some("json") => "application/json; charset=UTF-8",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => "text/plain",
        };
        match std::fs::read(&self.file) {
            Ok(body) => FetchState::Done { status: self.status, content_type: content_type.into(), body },
            Err(e) => FetchState::Failed(format!("mock {}: {e}", self.file.display())),
        }
    }
}

#[derive(Default)]
struct HostNet {
    next: FetchId,
    fetches: HashMap<FetchId, Arc<Mutex<FetchState>>>,
    /// Checked in order; the last matching mock wins so later script
    /// lines can override earlier ones.
    mocks: Vec<Mock>,
}

impl Network for HostNet {
    fn status(&self) -> String {
        "host network".into()
    }

    fn is_up(&self) -> bool {
        true
    }

    fn fetch(&mut self, url: &str) -> FetchId {
        self.send(HttpRequest::get(url))
    }

    fn send(&mut self, req: HttpRequest) -> FetchId {
        self.next += 1;
        let slot = Arc::new(Mutex::new(FetchState::Pending));
        self.fetches.insert(self.next, slot.clone());
        if let Some(mock) = self.mocks.iter().rev().find(|m| req.url.contains(&m.pattern)) {
            *slot.lock().unwrap() = mock.respond();
            return self.next;
        }
        std::thread::spawn(move || {
            let request = http::Request { method: req.method, headers: req.headers, body: req.body };
            let result = http_request(&req.url, request);
            *slot.lock().unwrap() = result;
        });
        self.next
    }

    fn poll(&mut self, id: FetchId) -> FetchState {
        self.fetches.get(&id).map_or(FetchState::Failed("unknown request".into()), |s| s.lock().unwrap().clone())
    }
}

/// An HTTP/1.0 request over http:// or https://, following redirects.
fn http_request(url: &str, mut request: http::Request) -> FetchState {
    if let Err(e) = request.validate() {
        return FetchState::Failed(e);
    }
    let mut url = url.to_string();
    let mut redirects = 0;
    loop {
        let response = Url::parse(&url).and_then(|target| {
            let bytes = request.encode(&target);
            let raw = if target.https { tls_exchange(&target, bytes)? } else { plain_exchange(&target, &bytes)? };
            Ok((target, http::parse_response(&raw)?))
        });
        match response {
            Ok((target, r)) if r.is_redirect() && redirects < http::MAX_REDIRECTS => {
                url = target.resolve(r.location.as_deref().unwrap_or("/"));
                if let Ok(next) = Url::parse(&url) {
                    request.follow_redirect(r.status, &target, &next);
                }
                redirects += 1;
            }
            Ok((_, r)) => return FetchState::Done { status: r.status, content_type: r.content_type, body: r.body },
            Err(e) => return FetchState::Failed(e),
        }
    }
}

fn connect(target: &Url) -> Result<TcpStream, String> {
    let addr = format!("{}:{}", target.host, target.port);
    let stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    Ok(stream)
}

/// Send the request and read the raw response until the server closes.
fn plain_exchange(target: &Url, request: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = connect(target)?;
    stream.write_all(request).map_err(|e| format!("send: {e}"))?;
    let mut raw = Vec::new();
    match stream.read_to_end(&mut raw) {
        Err(e) if raw.is_empty() => Err(format!("read: {e}")),
        _ => Ok(raw),
    }
}

/// The same exchange through TLS, verified against the Mozilla roots.
fn tls_exchange(target: &Url, request: Vec<u8>) -> Result<Vec<u8>, String> {
    static CONFIG: OnceLock<Result<Arc<tls::ClientConfig>, String>> = OnceLock::new();
    let config = CONFIG
        .get_or_init(|| tls::client_config(|| SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())))
        .clone()?;
    let mut stream = connect(target)?;
    let mut client = TlsClient::new(config, &target.host, request)?;
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let out = client.pending_output();
        if !out.is_empty() {
            stream.write_all(out).map_err(|e| format!("send: {e}"))?;
            let n = out.len();
            client.consume_output(n);
        }
        if client.peer_closed() {
            break;
        }
        let n = stream.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            // Many servers close TCP without close_notify after Connection: close.
            if client.is_handshaking() {
                return Err("connection closed during the TLS handshake".into());
            }
            break;
        }
        client.push_received(&buf[..n]);
        client.process()?;
        raw.extend(client.take_plaintext());
    }
    raw.extend(client.take_plaintext());
    Ok(raw)
}
