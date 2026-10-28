//! TCP/IP over the Wi-Fi station: smoltcp on top of the ESP-Hosted STA
//! interface, DHCP, DNS, SNTP (sets the wall clock) and a small HTTP/1.0
//! client with TLS for https:// that backs the `Network` service apps use.
//!
//! Everything runs in the `net` thread as non-blocking state machines that
//! are advanced on every poll; TLS handshakes and record decryption happen
//! inline in that thread (see `p4os_net::tls`).

use crate::sync::Mutex;
use crate::{kprintln, rtc, sched, time, wifi};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use p4os_net::http::{self, Url};
use p4os_net::sntp;
use p4os_net::tls::{self, ClientConfig, TlsClient};
use p4os_sdk::{FetchId, FetchState};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

const MTU: usize = 1514;
const FETCH_TIMEOUT_MS: u32 = 30_000;
/// HTTPS requests made right after DHCP wait up to this long for SNTP to
/// set the clock, which certificate validation needs.
const CLOCK_WAIT_MS: u32 = 15_000;

/// Tried in turn until one answers; a single pool server may not reply.
const SNTP_SERVERS: [&str; 3] = ["pool.ntp.org", "time.cloudflare.com", "time.google.com"];
const SNTP_LOCAL_PORT: u16 = 50_123;
const SNTP_TIMEOUT_MS: u32 = 3_000;
const SNTP_RETRY_MS: u32 = 1_000;
const SNTP_RESYNC_MS: u32 = 6 * 3600 * 1000;

static STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static IP: Mutex<Option<Ipv4Address>> = Mutex::new(None);
static REQUESTS: Mutex<VecDeque<(FetchId, String, http::Request)>> = Mutex::new(VecDeque::new());
static RESULTS: Mutex<BTreeMap<FetchId, FetchState>> = Mutex::new(BTreeMap::new());

struct WifiDevice;

struct RxToken(Vec<u8>);
struct TxToken;

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        if let Err(e) = wifi::send_sta(&buf) {
            kprintln!("net: tx failed: {:?}", e);
        }
        r
    }
}

impl Device for WifiDevice {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _ts: Instant) -> Option<(RxToken, TxToken)> {
        wifi::STA_RX.lock().pop_front().map(|f| (RxToken(f), TxToken))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<TxToken> {
        Some(TxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }
}

fn now() -> Instant {
    Instant::from_millis(time::ticks() as i64)
}

fn elapsed_ms(since: u32) -> u32 {
    time::ticks().wrapping_sub(since)
}

/// Start the network thread once Wi-Fi is associated.
pub fn start(mac: [u8; 6]) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // usize is 32-bit: the low 4 MAC bytes go in the thread argument and
    // the last 2 via a static set before the thread starts.
    MAC_HI.store(u16::from_le_bytes([mac[4], mac[5]]) as u32, Ordering::Relaxed);
    let lo = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
    // TLS handshakes (certificate parsing, RSA/ECDSA verification) need
    // more stack than plain TCP.
    sched::spawn("net", 2, 48 * 1024, run, lo as usize);
}

static MAC_HI: AtomicU32 = AtomicU32::new(0);

pub fn ip() -> Option<Ipv4Address> {
    *IP.lock()
}

pub fn fetch(url: &str) -> FetchId {
    send(url, http::Request::get())
}

/// Queue a request with any method, headers and body.
pub fn send(url: &str, request: http::Request) -> FetchId {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    RESULTS.lock().insert(id, FetchState::Pending);
    REQUESTS.lock().push_back((id, url.to_string(), request));
    id
}

/// Abandon a request: a queued one is never sent, a running one has its
/// connection closed, and no result is kept for it.
pub fn cancel(id: FetchId) {
    RESULTS.lock().remove(&id);
    REQUESTS.lock().retain(|(r, _, _)| *r != id);
}

/// State of a fetch. A finished result is handed out once and then
/// released, so response bodies do not accumulate in memory.
pub fn poll_fetch(id: FetchId) -> FetchState {
    let mut results = RESULTS.lock();
    match results.get(&id) {
        Some(FetchState::Pending) => FetchState::Pending,
        Some(_) => results.remove(&id).unwrap_or(FetchState::Pending),
        None => FetchState::Failed("unknown request".into()),
    }
}

/// Wall clock for certificate validity checks.
fn tls_clock() -> Option<u64> {
    rtc::unix_now().and_then(|s| u64::try_from(s).ok())
}

enum Stage {
    Resolve(dns::QueryHandle),
    Connect(SocketHandle),
    /// Plain HTTP: raw response bytes so far.
    Plain(SocketHandle, Vec<u8>),
    /// HTTPS: the TLS session and the decrypted response so far.
    Tls(SocketHandle, Box<TlsClient>, Vec<u8>),
}

impl Stage {
    fn socket(&self) -> Option<SocketHandle> {
        match self {
            Stage::Resolve(_) => None,
            Stage::Connect(h) | Stage::Plain(h, _) | Stage::Tls(h, _, _) => Some(*h),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Stage::Resolve(_) => "resolving the host name",
            Stage::Connect(_) => "connecting",
            Stage::Plain(..) => "waiting for the response",
            Stage::Tls(_, c, _) if c.is_handshaking() => "in the TLS handshake",
            Stage::Tls(..) => "waiting for the response",
        }
    }
}

struct Job {
    id: FetchId,
    target: Url,
    request: http::Request,
    redirects: u32,
    stage: Stage,
    started: u32,
    /// When the current TLS handshake began, for the log.
    tls_started: u32,
}

/// Shared state the jobs need from the net thread.
struct Net {
    iface: Interface,
    sockets: SocketSet<'static>,
    dns: SocketHandle,
    next_port: u16,
    tls_config: Option<Arc<ClientConfig>>,
}

fn run(mac_lo: usize) {
    let lo = (mac_lo as u32).to_le_bytes();
    let hi = (MAC_HI.load(Ordering::Relaxed) as u16).to_le_bytes();
    let mac = EthernetAddress([lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]]);
    let mut dev = WifiDevice;
    let iface = Interface::new(Config::new(HardwareAddress::Ethernet(mac)), &mut dev, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let dns = sockets.add(dns::Socket::new(&[], Vec::new()));
    let mut net = Net { iface, sockets, dns, next_port: 49152, tls_config: None };
    let mut sntp = Sntp::new(&mut net.sockets);
    let mut jobs: Vec<Job> = Vec::new();
    // Tick at which the current DHCP lease was obtained.
    let mut configured_at = 0;
    let watch = crate::wdt::watch("net", 20_000);

    loop {
        watch.checkin();
        if let Err(e) = wifi::poll() {
            kprintln!("net: poll failed: {:?}", e);
        }
        net.iface.poll(now(), &mut dev, &mut net.sockets);

        // Copy the lease out first: the event borrows the DHCP socket.
        let lease = net.sockets.get_mut::<dhcpv4::Socket>(dhcp).poll().map(|event| match event {
            dhcpv4::Event::Configured(cfg) => Some((cfg.address, cfg.router, cfg.dns_servers.iter().copied().collect::<Vec<_>>())),
            dhcpv4::Event::Deconfigured => None,
        });
        match lease {
            Some(Some((address, router, dns_servers))) => {
                net.iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(address));
                });
                if let Some(gw) = router {
                    let _ = net.iface.routes_mut().add_default_ipv4_route(gw);
                }
                let servers: Vec<IpAddress> = dns_servers.iter().map(|s| IpAddress::Ipv4(*s)).collect();
                net.sockets.get_mut::<dns::Socket>(net.dns).update_servers(&servers);
                *IP.lock() = Some(address.address());
                configured_at = time::ticks();
                sntp.restart();
                kprintln!("net: DHCP address {} gateway {:?} dns {:?}", address, router, servers);
            }
            Some(None) => {
                *IP.lock() = None;
                kprintln!("net: DHCP lease lost");
            }
            None => {}
        }

        if IP.lock().is_some() {
            sntp.poll(&mut net);
            // HTTPS requests wait briefly for network time after DHCP so
            // the first page after boot does not fail certificate checks.
            let hold_https = rtc::unix_now().is_none() && elapsed_ms(configured_at) < CLOCK_WAIT_MS;
            loop {
                let Some((id, url, request)) = REQUESTS.lock().pop_front() else { break };
                let target = match request.validate().and_then(|()| Url::parse(&url)) {
                    Ok(t) => t,
                    Err(e) => {
                        finish(id, FetchState::Failed(e));
                        continue;
                    }
                };
                if target.https && hold_https {
                    REQUESTS.lock().push_front((id, url, request));
                    break;
                }
                match net.resolve(&target) {
                    Ok(stage) => {
                        jobs.push(Job { id, target, request, redirects: 0, stage, started: time::ticks(), tls_started: 0 })
                    }
                    Err(e) => finish(id, FetchState::Failed(e)),
                }
            }
        }
        // Drop cancelled jobs (their result slot is gone) and close their sockets.
        jobs.retain(|job| {
            let live = RESULTS.lock().contains_key(&job.id);
            if !live {
                if let Some(h) = job.stage.socket() {
                    net.sockets.remove(h);
                }
            }
            live
        });
        jobs.retain_mut(|job| net.advance(job));
        sched::sleep_ms(if jobs.is_empty() { 10 } else { 2 });
    }
}

/// Store the outcome of a request, unless it was cancelled meanwhile.
fn finish(id: FetchId, state: FetchState) {
    if let Some(slot) = RESULTS.lock().get_mut(&id) {
        *slot = state;
    }
}

/// What one step of a job produced.
enum Step {
    Continue,
    Failed(String),
    /// The complete HTTP response (headers and body).
    Response(Vec<u8>),
}

impl Net {
    fn resolve(&mut self, target: &Url) -> Result<Stage, String> {
        if let Ok(ip) = target.host.parse::<Ipv4Address>() {
            return self.connect(IpAddress::Ipv4(ip), target.port);
        }
        self.sockets
            .get_mut::<dns::Socket>(self.dns)
            .start_query(self.iface.context(), &target.host, DnsQueryType::A)
            .map(Stage::Resolve)
            .map_err(|e| format!("DNS: {:?}", e))
    }

    fn connect(&mut self, ip: IpAddress, port: u16) -> Result<Stage, String> {
        let mut sock = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 16 * 1024]), tcp::SocketBuffer::new(vec![0; 4096]));
        self.next_port = if self.next_port >= 65000 { 49152 } else { self.next_port + 1 };
        sock.connect(self.iface.context(), IpEndpoint::new(ip, port), self.next_port)
            .map_err(|e| format!("connect: {:?}", e))?;
        Ok(Stage::Connect(self.sockets.add(sock)))
    }

    fn tls_config(&mut self) -> Result<Arc<ClientConfig>, String> {
        if self.tls_config.is_none() {
            self.tls_config = Some(tls::client_config(tls_clock)?);
        }
        Ok(self.tls_config.clone().expect("set above"))
    }

    /// Drive one fetch forward; false when it is finished.
    fn advance(&mut self, job: &mut Job) -> bool {
        let step = if elapsed_ms(job.started) > FETCH_TIMEOUT_MS {
            Step::Failed(format!("timed out {} ({})", job.stage.name(), job.target.host))
        } else {
            self.step(job)
        };
        match step {
            Step::Continue => true,
            Step::Failed(e) => {
                if let Some(h) = job.stage.socket() {
                    self.sockets.remove(h);
                }
                finish(job.id, FetchState::Failed(e));
                false
            }
            Step::Response(raw) => {
                if let Some(h) = job.stage.socket() {
                    self.sockets.remove(h);
                }
                self.complete(job, &raw)
            }
        }
    }

    /// A response arrived: follow a redirect or report it.
    fn complete(&mut self, job: &mut Job, raw: &[u8]) -> bool {
        let response = match http::parse_response(raw) {
            Ok(r) => r,
            Err(e) => {
                finish(job.id, FetchState::Failed(e));
                return false;
            }
        };
        if response.is_redirect() && job.redirects < http::MAX_REDIRECTS {
            let next = job.target.resolve(response.location.as_deref().unwrap_or("/"));
            match Url::parse(&next).and_then(|t| Ok((self.resolve(&t)?, t))) {
                Ok((stage, target)) => {
                    job.request.follow_redirect(response.status, &job.target, &target);
                    job.target = target;
                    job.stage = stage;
                    job.redirects += 1;
                    return true;
                }
                Err(e) => {
                    finish(job.id, FetchState::Failed(e));
                    return false;
                }
            }
        }
        finish(
            job.id,
            FetchState::Done { status: response.status, content_type: response.content_type, body: response.body },
        );
        false
    }

    fn step(&mut self, job: &mut Job) -> Step {
        match &mut job.stage {
            Stage::Resolve(q) => match self.sockets.get_mut::<dns::Socket>(self.dns).get_query_result(*q) {
                Ok(addrs) => match addrs.first() {
                    Some(ip) => match self.connect(*ip, job.target.port) {
                        Ok(s) => {
                            job.stage = s;
                            Step::Continue
                        }
                        Err(e) => Step::Failed(e),
                    },
                    None => Step::Failed(format!("no address for {}", job.target.host)),
                },
                Err(dns::GetQueryResultError::Pending) => Step::Continue,
                Err(e) => Step::Failed(format!("cannot resolve {}: {:?}", job.target.host, e)),
            },
            Stage::Connect(h) => {
                let h = *h;
                let sock = self.sockets.get_mut::<tcp::Socket>(h);
                if sock.may_send() {
                    if !job.target.https {
                        let _ = sock.send_slice(&job.request.encode(&job.target));
                        job.stage = Stage::Plain(h, Vec::new());
                        return Step::Continue;
                    }
                    let client = self
                        .tls_config()
                        .and_then(|cfg| TlsClient::new(cfg, &job.target.host, job.request.encode(&job.target)));
                    match client {
                        Ok(c) => {
                            job.tls_started = time::ticks();
                            job.stage = Stage::Tls(h, Box::new(c), Vec::new());
                            Step::Continue
                        }
                        Err(e) => Step::Failed(e),
                    }
                } else if !sock.is_open() {
                    Step::Failed(format!("connection to {} refused", job.target.host))
                } else {
                    Step::Continue
                }
            }
            Stage::Plain(h, raw) => {
                let sock = self.sockets.get_mut::<tcp::Socket>(*h);
                while sock.can_recv() {
                    let _ = sock.recv(|d| {
                        raw.extend_from_slice(d);
                        (d.len(), ())
                    });
                }
                if !sock.may_recv() || !sock.is_open() {
                    Step::Response(core::mem::take(raw))
                } else {
                    Step::Continue
                }
            }
            Stage::Tls(h, client, plain) => {
                let sock = self.sockets.get_mut::<tcp::Socket>(*h);
                while sock.can_recv() {
                    let _ = sock.recv(|d| {
                        client.push_received(d);
                        (d.len(), ())
                    });
                }
                let was_handshaking = client.is_handshaking();
                if let Err(e) = client.process() {
                    return Step::Failed(e);
                }
                if was_handshaking && !client.is_handshaking() {
                    kprintln!(
                        "net: TLS with {} established in {} ms ({})",
                        job.target.host,
                        elapsed_ms(job.tls_started),
                        client.session_info()
                    );
                }
                plain.extend(client.take_plaintext());
                let out = client.pending_output();
                if !out.is_empty() && sock.can_send() {
                    match sock.send_slice(out) {
                        Ok(n) => client.consume_output(n),
                        Err(e) => return Step::Failed(format!("send: {:?}", e)),
                    }
                }
                // close_notify, or TCP closed (many servers skip close_notify
                // after "Connection: close"); all received bytes are processed.
                if client.peer_closed() || !sock.may_recv() || !sock.is_open() {
                    if client.is_handshaking() {
                        return Step::Failed(format!("{} closed the connection during the TLS handshake", job.target.host));
                    }
                    return Step::Response(core::mem::take(plain));
                }
                Step::Continue
            }
        }
    }
}

enum SntpState {
    Idle { next: u32 },
    Resolving { query: dns::QueryHandle, since: u32 },
    Waiting { since: u32, nonce: [u8; 8] },
}

/// Minimal SNTPv4 client (RFC 4330): one query after each DHCP lease and
/// every few hours, retried on failure with the next server. Sets `rtc` (UTC).
struct Sntp {
    udp: SocketHandle,
    state: SntpState,
    /// Index into `SNTP_SERVERS` of the server being asked.
    server: usize,
}

impl Sntp {
    fn new(sockets: &mut SocketSet<'static>) -> Self {
        let rx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 512]);
        let tx = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 2], vec![0; 128]);
        let mut sock = udp::Socket::new(rx, tx);
        if let Err(e) = sock.bind(SNTP_LOCAL_PORT) {
            kprintln!("net: SNTP bind failed: {:?}", e);
        }
        Sntp { udp: sockets.add(sock), state: SntpState::Idle { next: time::ticks() }, server: 0 }
    }

    /// Query again as soon as possible (new lease).
    fn restart(&mut self) {
        self.state = SntpState::Idle { next: time::ticks() };
    }

    fn retry_later(&mut self, why: &str) {
        kprintln!("net: SNTP {}: {}; trying the next server", SNTP_SERVERS[self.server], why);
        self.server = (self.server + 1) % SNTP_SERVERS.len();
        self.state = SntpState::Idle { next: time::ticks().wrapping_add(SNTP_RETRY_MS) };
    }

    fn poll(&mut self, net: &mut Net) {
        match self.state {
            SntpState::Idle { next } => {
                // Wrapping-safe "now >= next".
                if (time::ticks().wrapping_sub(next) as i32) < 0 {
                    return;
                }
                match net.sockets.get_mut::<dns::Socket>(net.dns).start_query(net.iface.context(), SNTP_SERVERS[self.server], DnsQueryType::A) {
                    Ok(query) => self.state = SntpState::Resolving { query, since: time::ticks() },
                    Err(e) => self.retry_later(&format!("DNS query failed ({:?})", e)),
                }
            }
            SntpState::Resolving { query, since } => {
                let dns = net.sockets.get_mut::<dns::Socket>(net.dns);
                match dns.get_query_result(query) {
                    Ok(addrs) => match addrs.first() {
                        Some(ip) => self.send(net, *ip),
                        None => self.retry_later("server has no address"),
                    },
                    Err(dns::GetQueryResultError::Pending) if elapsed_ms(since) < SNTP_TIMEOUT_MS => {}
                    Err(dns::GetQueryResultError::Pending) => {
                        dns.cancel_query(query);
                        self.retry_later("DNS timed out");
                    }
                    Err(e) => self.retry_later(&format!("DNS failed ({:?})", e)),
                }
            }
            SntpState::Waiting { since, nonce } => {
                let sock = net.sockets.get_mut::<udp::Socket>(self.udp);
                while let Ok((data, meta)) = sock.recv() {
                    if meta.endpoint.port != sntp::PORT {
                        continue;
                    }
                    if let Some(unix) = sntp::parse_reply(data, &nonce) {
                        rtc::set_unix(unix);
                        if let Some(t) = rtc::now() {
                            kprintln!(
                                "net: clock set by SNTP ({}): {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                                SNTP_SERVERS[self.server], t.year, t.month, t.day, t.hour, t.minute, t.second
                            );
                        }
                        self.state = SntpState::Idle { next: time::ticks().wrapping_add(SNTP_RESYNC_MS) };
                        return;
                    }
                }
                if elapsed_ms(since) > SNTP_TIMEOUT_MS {
                    self.retry_later("got no reply");
                }
            }
        }
    }

    fn send(&mut self, net: &mut Net, server: IpAddress) {
        let mut nonce = [0u8; 8];
        crate::rng::fill(&mut nonce);
        let sock = net.sockets.get_mut::<udp::Socket>(self.udp);
        match sock.send_slice(&sntp::request(&nonce), IpEndpoint::new(server, sntp::PORT)) {
            Ok(()) => self.state = SntpState::Waiting { since: time::ticks(), nonce },
            Err(e) => self.retry_later(&format!("send failed ({:?})", e)),
        }
    }
}

/// The `Network` service handed to apps.
#[derive(Default)]
pub struct WifiNet;

impl p4os_sdk::Network for WifiNet {
    fn status(&self) -> String {
        match (wifi::is_connected(), ip()) {
            (true, Some(ip)) => format!("Wi-Fi {}", ip),
            (true, None) => "Wi-Fi (DHCP...)".into(),
            _ => crate::wifi_auto::describe().unwrap_or_else(|| "offline".into()),
        }
    }

    fn is_up(&self) -> bool {
        ip().is_some()
    }

    fn fetch(&mut self, url: &str) -> FetchId {
        fetch(url)
    }

    fn send(&mut self, req: p4os_sdk::HttpRequest) -> FetchId {
        send(&req.url, http::Request { method: req.method, headers: req.headers, body: req.body })
    }

    fn poll(&mut self, id: FetchId) -> FetchState {
        poll_fetch(id)
    }

    fn cancel(&mut self, id: FetchId) {
        cancel(id)
    }
}
