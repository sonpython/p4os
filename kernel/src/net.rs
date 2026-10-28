//! TCP/IP over the Wi-Fi station: smoltcp on top of the ESP-Hosted STA
//! interface, DHCP and DNS, and a small HTTP/1.0 client that backs the
//! `Network` service apps use.

use crate::sync::Mutex;
use crate::{kprintln, sched, time, wifi};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use p4os_sdk::{FetchId, FetchState};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, dns, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{DnsQueryType, EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

const MTU: usize = 1514;
const FETCH_TIMEOUT_MS: u32 = 20_000;
const MAX_REDIRECTS: u32 = 3;

static STARTED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static IP: Mutex<Option<Ipv4Address>> = Mutex::new(None);
static REQUESTS: Mutex<VecDeque<(FetchId, String)>> = Mutex::new(VecDeque::new());
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

/// Start the network thread once Wi-Fi is associated.
pub fn start(mac: [u8; 6]) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // usize is 32-bit: the low 4 MAC bytes go in the thread argument and
    // the last 2 via a static set before the thread starts.
    MAC_HI.store(u16::from_le_bytes([mac[4], mac[5]]) as u32, Ordering::Relaxed);
    let lo = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
    sched::spawn("net", 2, 32 * 1024, run, lo as usize);
}

static MAC_HI: AtomicU32 = AtomicU32::new(0);

pub fn ip() -> Option<Ipv4Address> {
    *IP.lock()
}

pub fn fetch(url: &str) -> FetchId {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    RESULTS.lock().insert(id, FetchState::Pending);
    REQUESTS.lock().push_back((id, url.to_string()));
    id
}

pub fn poll_fetch(id: FetchId) -> FetchState {
    RESULTS.lock().get(&id).cloned().unwrap_or(FetchState::Failed("unknown request".into()))
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Url, String> {
    if url.starts_with("https://") {
        return Err("HTTPS is not supported (no TLS yet); try http://".into());
    }
    let rest = url.strip_prefix("http://").ok_or("only http:// URLs are supported")?;
    let (hostport, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| "bad port")?),
        None => (hostport, 80),
    };
    Ok(Url { host: host.to_string(), port, path })
}

enum Stage {
    Resolve(dns::QueryHandle),
    Connect(SocketHandle),
    Receive(SocketHandle, Vec<u8>),
}

struct Job {
    id: FetchId,
    url: String,
    target: Url,
    redirects: u32,
    stage: Stage,
    started: u32,
}

fn run(mac_lo: usize) {
    let lo = (mac_lo as u32).to_le_bytes();
    let hi = (MAC_HI.load(Ordering::Relaxed) as u16).to_le_bytes();
    let mac = EthernetAddress([lo[0], lo[1], lo[2], lo[3], hi[0], hi[1]]);
    let mut dev = WifiDevice;
    let mut iface = Interface::new(Config::new(HardwareAddress::Ethernet(mac)), &mut dev, now());
    let mut sockets = SocketSet::new(Vec::new());
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let dns_handle = sockets.add(dns::Socket::new(&[], Vec::new()));
    let mut jobs: Vec<Job> = Vec::new();
    let mut next_port: u16 = 49152;

    loop {
        if let Err(e) = wifi::poll() {
            kprintln!("net: poll failed: {:?}", e);
        }
        iface.poll(now(), &mut dev, &mut sockets);

        // Copy the lease out first: the event borrows the DHCP socket.
        let lease = sockets.get_mut::<dhcpv4::Socket>(dhcp).poll().map(|event| match event {
            dhcpv4::Event::Configured(cfg) => Some((cfg.address, cfg.router, cfg.dns_servers.iter().copied().collect::<Vec<_>>())),
            dhcpv4::Event::Deconfigured => None,
        });
        match lease {
            Some(Some((address, router, dns_servers))) => {
                iface.update_ip_addrs(|a| {
                    a.clear();
                    let _ = a.push(IpCidr::Ipv4(address));
                });
                if let Some(gw) = router {
                    let _ = iface.routes_mut().add_default_ipv4_route(gw);
                }
                let servers: Vec<IpAddress> = dns_servers.iter().map(|s| IpAddress::Ipv4(*s)).collect();
                sockets.get_mut::<dns::Socket>(dns_handle).update_servers(&servers);
                *IP.lock() = Some(address.address());
                kprintln!("net: DHCP address {} gateway {:?} dns {:?}", address, router, servers);
            }
            Some(None) => {
                *IP.lock() = None;
                kprintln!("net: DHCP lease lost");
            }
            None => {}
        }

        // Start queued fetches once an address is configured.
        if IP.lock().is_some() {
            while let Some((id, url)) = REQUESTS.lock().pop_front() {
                match parse_url(&url) {
                    Ok(target) => {
                        let stage = resolve(&mut sockets, dns_handle, &mut iface, &target, &mut next_port);
                        match stage {
                            Ok(stage) => jobs.push(Job { id, url, target, redirects: 0, stage, started: time::ticks() }),
                            Err(e) => finish(id, FetchState::Failed(e)),
                        }
                    }
                    Err(e) => finish(id, FetchState::Failed(e)),
                }
            }
        }
        jobs.retain_mut(|job| advance(job, &mut sockets, dns_handle, &mut iface, &mut next_port));
        sched::sleep_ms(if jobs.is_empty() { 10 } else { 2 });
    }
}

fn finish(id: FetchId, state: FetchState) {
    RESULTS.lock().insert(id, state);
}

fn resolve(
    sockets: &mut SocketSet,
    dns_handle: SocketHandle,
    iface: &mut Interface,
    target: &Url,
    next_port: &mut u16,
) -> Result<Stage, String> {
    if let Ok(ip) = target.host.parse::<Ipv4Address>() {
        return connect(sockets, iface, IpAddress::Ipv4(ip), target.port, next_port);
    }
    sockets
        .get_mut::<dns::Socket>(dns_handle)
        .start_query(iface.context(), &target.host, DnsQueryType::A)
        .map(Stage::Resolve)
        .map_err(|e| format!("DNS: {:?}", e))
}

fn connect(sockets: &mut SocketSet, iface: &mut Interface, ip: IpAddress, port: u16, next_port: &mut u16) -> Result<Stage, String> {
    let mut sock = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; 16 * 1024]), tcp::SocketBuffer::new(vec![0; 2048]));
    *next_port = if *next_port >= 65000 { 49152 } else { *next_port + 1 };
    sock.connect(iface.context(), IpEndpoint::new(ip, port), *next_port).map_err(|e| format!("connect: {:?}", e))?;
    Ok(Stage::Connect(sockets.add(sock)))
}

/// Drive one fetch forward; false when it is finished.
fn advance(job: &mut Job, sockets: &mut SocketSet, dns_handle: SocketHandle, iface: &mut Interface, next_port: &mut u16) -> bool {
    if time::ticks().wrapping_sub(job.started) > FETCH_TIMEOUT_MS {
        if let Stage::Connect(h) | Stage::Receive(h, _) = job.stage {
            sockets.remove(h);
        }
        finish(job.id, FetchState::Failed(format!("timed out fetching {}", job.url)));
        return false;
    }
    match &mut job.stage {
        Stage::Resolve(q) => match sockets.get_mut::<dns::Socket>(dns_handle).get_query_result(*q) {
            Ok(addrs) => match addrs.first() {
                Some(ip) => match connect(sockets, iface, *ip, job.target.port, next_port) {
                    Ok(s) => job.stage = s,
                    Err(e) => {
                        finish(job.id, FetchState::Failed(e));
                        return false;
                    }
                },
                None => {
                    finish(job.id, FetchState::Failed(format!("no address for {}", job.target.host)));
                    return false;
                }
            },
            Err(dns::GetQueryResultError::Pending) => {}
            Err(e) => {
                finish(job.id, FetchState::Failed(format!("cannot resolve {}: {:?}", job.target.host, e)));
                return false;
            }
        },
        Stage::Connect(h) => {
            let h = *h;
            let sock = sockets.get_mut::<tcp::Socket>(h);
            if sock.may_send() {
                let req = format!(
                    "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: p4os/0.1\r\nAccept: text/html,*/*\r\nConnection: close\r\n\r\n",
                    job.target.path, job.target.host
                );
                let _ = sock.send_slice(req.as_bytes());
                job.stage = Stage::Receive(h, Vec::new());
            } else if !sock.is_open() {
                sockets.remove(h);
                finish(job.id, FetchState::Failed(format!("connection to {} refused", job.target.host)));
                return false;
            }
        }
        Stage::Receive(h, body) => {
            let h = *h;
            let sock = sockets.get_mut::<tcp::Socket>(h);
            while sock.can_recv() {
                let _ = sock.recv(|d| {
                    body.extend_from_slice(d);
                    (d.len(), ())
                });
            }
            if !sock.may_recv() || !sock.is_open() {
                sock.close();
                let raw = core::mem::take(body);
                sockets.remove(h);
                return match parse_response(&raw) {
                    Ok((status, location, content_type, payload)) => {
                        if (300..400).contains(&status) && job.redirects < MAX_REDIRECTS {
                            if let Some(loc) = location {
                                let next = if loc.starts_with("http") {
                                    loc
                                } else {
                                    format!("http://{}{}", job.target.host, loc)
                                };
                                match parse_url(&next).and_then(|t| {
                                    let s = resolve(sockets, dns_handle, iface, &t, next_port)?;
                                    Ok((t, s))
                                }) {
                                    Ok((t, s)) => {
                                        job.url = next;
                                        job.target = t;
                                        job.stage = s;
                                        job.redirects += 1;
                                        return true;
                                    }
                                    Err(e) => {
                                        finish(job.id, FetchState::Failed(e));
                                        return false;
                                    }
                                }
                            }
                        }
                        finish(job.id, FetchState::Done { status, content_type, body: payload });
                        false
                    }
                    Err(e) => {
                        finish(job.id, FetchState::Failed(e));
                        false
                    }
                };
            }
        }
    }
    true
}

type Response = (u16, Option<String>, String, Vec<u8>);

fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let header = |name: &str| {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    Ok((status, header("location"), header("content-type").unwrap_or_default(), raw[split + 4..].to_vec()))
}

/// The `Network` service handed to apps.
#[derive(Default)]
pub struct WifiNet;

impl p4os_sdk::Network for WifiNet {
    fn status(&self) -> String {
        match (wifi::is_connected(), ip()) {
            (true, Some(ip)) => format!("Wi-Fi {}", ip),
            (true, None) => "Wi-Fi (DHCP...)".into(),
            _ => "offline".into(),
        }
    }

    fn is_up(&self) -> bool {
        ip().is_some()
    }

    fn fetch(&mut self, url: &str) -> FetchId {
        fetch(url)
    }

    fn poll(&mut self, id: FetchId) -> FetchState {
        poll_fetch(id)
    }
}
