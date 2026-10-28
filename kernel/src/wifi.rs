//! Wi-Fi station control over ESP-Hosted RPC, and the frame layer shared
//! with the network stack.
//!
//! Frames: 12-byte header (if_type/if_num, flags, len, offset, checksum,
//! seq, throttle, packet type) followed by the payload. RPC messages travel
//! on the serial interface as TLVs [0x01 len "RPCRsp"][0x02 len protobuf];
//! Ethernet frames for the station travel on interface 1.

use crate::proto::{self, Reader, Value, Writer};
use crate::sync::Mutex;
use crate::{c6, kprintln, systimer};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const HEADER: usize = 12;
pub const IF_STA: u8 = 1;
const IF_SERIAL: u8 = 3;
const IF_PRIV: u8 = 5;

const RPC_REQ: i64 = 1;
const RPC_EVENT: i64 = 3;
const REQ_GET_MAC: u32 = 257;
const REQ_SET_WIFI_MODE: u32 = 260;
const REQ_WIFI_INIT: u32 = 278;
const REQ_WIFI_START: u32 = 280;
const REQ_WIFI_CONNECT: u32 = 282;
const REQ_WIFI_SET_CONFIG: u32 = 284;
const RESP_OFFSET: u32 = 256;
const EVENT_ESP_INIT: u32 = 769;
const EVENT_WIFI_NO_ARGS: u32 = 773;
const EVENT_STA_CONNECTED: u32 = 775;
const EVENT_STA_DISCONNECTED: u32 = 776;
const WIFI_MODE_STA: i64 = 1;
const WIFI_IF_STA: i64 = 0;
const WIFI_AUTH_WPA2_PSK: i64 = 3;

static SEQ: AtomicU32 = AtomicU32::new(0);
static UID: AtomicU32 = AtomicU32::new(1);
static CONNECTED: AtomicBool = AtomicBool::new(false);
/// Ethernet frames received for the station, consumed by the IP stack.
pub static STA_RX: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
/// RPC responses not yet claimed by a caller: (msg_id, payload).
static RESPONSES: Mutex<VecDeque<(u32, Vec<u8>)>> = Mutex::new(VecDeque::new());

pub fn is_connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}

fn frame(if_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = alloc::vec![0u8; HEADER + payload.len()];
    f[0] = if_type & 0x0F;
    f[2..4].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    f[4..6].copy_from_slice(&(HEADER as u16).to_le_bytes());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) as u16;
    f[8..10].copy_from_slice(&seq.to_le_bytes());
    f[HEADER..].copy_from_slice(payload);
    let sum: u16 = f.iter().fold(0u16, |s, &b| s.wrapping_add(b as u16));
    f[6..8].copy_from_slice(&sum.to_le_bytes());
    f
}

pub fn send_sta(eth: &[u8]) -> Result<(), c6::Error> {
    c6::send(&frame(IF_STA, eth))
}

fn tlv(tag: u8, value: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
}

/// Pull everything the slave has queued and route each frame.
pub fn poll() -> Result<(), c6::Error> {
    let data = c6::receive()?;
    let mut pos = 0;
    while pos + HEADER <= data.len() {
        let h = &data[pos..];
        let if_type = h[0] & 0x0F;
        let len = u16::from_le_bytes([h[2], h[3]]) as usize;
        let off = u16::from_le_bytes([h[4], h[5]]) as usize;
        if off < HEADER || pos + off + len > data.len() {
            break;
        }
        let payload = &data[pos + off..pos + off + len];
        match if_type {
            IF_STA => STA_RX.lock().push_back(payload.to_vec()),
            IF_SERIAL => handle_serial(payload),
            IF_PRIV => {}
            _ => {}
        }
        pos += off + len;
    }
    Ok(())
}

fn handle_serial(payload: &[u8]) {
    // TLV 0x01 endpoint name, 0x02 protobuf.
    let mut i = 0;
    let mut msg = None;
    while i + 3 <= payload.len() {
        let tag = payload[i];
        let len = u16::from_le_bytes([payload[i + 1], payload[i + 2]]) as usize;
        let v = payload.get(i + 3..i + 3 + len).unwrap_or(&[]);
        if tag == 0x02 {
            msg = Some(v);
        }
        i += 3 + len;
    }
    let Some(msg) = msg else { return };
    let msg_type = proto::int(Reader::find(msg, 1));
    let msg_id = proto::int(Reader::find(msg, 2)) as u32;
    let body = match Reader::find(msg, msg_id) {
        Some(Value::Bytes(b)) => b.to_vec(),
        _ => Vec::new(),
    };
    if msg_type == RPC_EVENT {
        handle_event(msg_id, &body);
    } else {
        RESPONSES.lock().push_back((msg_id, body));
    }
}

fn handle_event(id: u32, body: &[u8]) {
    match id {
        EVENT_ESP_INIT => kprintln!("wifi: co-processor ready"),
        EVENT_STA_CONNECTED => {
            CONNECTED.store(true, Ordering::Relaxed);
            let ssid = match Reader::find(body, 2).and_then(|c| match c {
                Value::Bytes(b) => Reader::find(b, 1),
                _ => None,
            }) {
                Some(Value::Bytes(s)) => String::from_utf8_lossy(s).trim_end_matches([' ', '\0']).into(),
                _ => String::new(),
            };
            kprintln!("wifi: connected to '{}'", ssid);
        }
        EVENT_STA_DISCONNECTED => {
            CONNECTED.store(false, Ordering::Relaxed);
            let reason = match Reader::find(body, 2) {
                Some(Value::Bytes(b)) => proto::int(Reader::find(b, 4)),
                _ => 0,
            };
            kprintln!("wifi: disconnected (reason {})", reason);
        }
        EVENT_WIFI_NO_ARGS => kprintln!("wifi: event {}", proto::int(Reader::find(body, 2))),
        other => kprintln!("wifi: event {:#x}", other),
    }
}

/// Send an RPC request and wait for its response body.
fn rpc(id: u32, body: &Writer) -> Result<Vec<u8>, String> {
    let mut msg = Writer::new();
    msg.int(1, RPC_REQ).int(2, id as i64).int(3, UID.fetch_add(1, Ordering::Relaxed) as i64).message(id, body);
    let mut payload = Vec::new();
    tlv(0x01, b"RPCRsp", &mut payload);
    tlv(0x02, &msg.buf, &mut payload);
    c6::send(&frame(IF_SERIAL, &payload)).map_err(|e| alloc::format!("send: {:?}", e))?;
    let want = id + RESP_OFFSET;
    let end = systimer::now_ticks() + 5_000_000 * systimer::TICKS_PER_US;
    loop {
        {
            let mut q = RESPONSES.lock();
            if let Some(i) = q.iter().position(|(m, _)| *m == want) {
                return Ok(q.remove(i).unwrap().1);
            }
        }
        if systimer::now_ticks() > end {
            return Err(alloc::format!("no response to RPC {:#x}", id));
        }
        poll().map_err(|e| alloc::format!("poll: {:?}", e))?;
        crate::sched::sleep_ms(5);
    }
}

/// Response field 1 (or 2 for GetMac) is the esp_err_t result.
fn check(name: &str, resp: &[u8], field: u32) -> Result<(), String> {
    match proto::int(Reader::find(resp, field)) {
        0 => Ok(()),
        e => Err(alloc::format!("{} failed: esp_err {:#x}", name, e)),
    }
}

pub fn mac() -> Result<[u8; 6], String> {
    let mut req = Writer::new();
    req.int(1, WIFI_IF_STA);
    let resp = rpc(REQ_GET_MAC, &req)?;
    check("get_mac", &resp, 2)?;
    match Reader::find(&resp, 1) {
        Some(Value::Bytes(m)) if m.len() == 6 => Ok(m.try_into().unwrap()),
        _ => Err("bad MAC".into()),
    }
}

/// Initialise the C6 transport and Wi-Fi, then join `ssid`.
pub fn connect(ssid: &str, password: &str) -> Result<(), String> {
    // The co-processor is reset below, which drops any existing link.
    CONNECTED.store(false, Ordering::Relaxed);
    c6::init().map_err(|e| alloc::format!("c6 init: {:?}", e))?;
    for _ in 0..20 {
        poll().map_err(|e| alloc::format!("poll: {:?}", e))?;
        systimer::delay_us(50_000);
    }
    // WIFI_INIT_CONFIG_DEFAULT() values for the C6 (ESP-IDF 5.x).
    let mut cfg = Writer::new();
    for (f, v) in [(1, 10), (2, 32), (3, 1), (4, 0), (5, 32), (6, 0), (7, 0), (8, 1), (9, 1), (10, 0), (11, 1),
                   (12, 0), (13, 6), (14, 0), (15, 752), (16, 32), (17, 1), (19, 7), (20, 0x1F2F_3F4F)] {
        cfg.int(f, v);
    }
    let mut init = Writer::new();
    init.message(1, &cfg);
    check("wifi_init", &rpc(REQ_WIFI_INIT, &init)?, 1)?;

    let mut mode = Writer::new();
    mode.int(1, WIFI_MODE_STA);
    check("set_mode", &rpc(REQ_SET_WIFI_MODE, &mode)?, 1)?;

    // Every sub-message is sent explicitly: older slave firmware copies
    // threshold/pmf_cfg without checking for their presence.
    let mut threshold = Writer::new();
    threshold.int(1, -127).int(2, WIFI_AUTH_WPA2_PSK).int(3, 0);
    let mut pmf = Writer::new();
    pmf.int(1, 1).int(2, 0);
    let mut sta = Writer::new();
    sta.bytes(1, ssid.as_bytes())
        .bytes(2, password.as_bytes())
        .int(3, 0) // fast scan
        .int(4, 0)
        .bytes(5, &[0; 6])
        .int(6, 0)
        .int(7, 0)
        .int(8, 0)
        .message(9, &threshold)
        .message(10, &pmf)
        .int(11, 0)
        .int(12, 0)
        .int(13, 0)
        .int(14, 0)
        .bytes(15, &[])
        .int(16, 0);
    let mut wcfg = Writer::new();
    wcfg.message(2, &sta);
    let mut set = Writer::new();
    set.int(1, WIFI_IF_STA).message(2, &wcfg);
    check("set_config", &rpc(REQ_WIFI_SET_CONFIG, &set)?, 1)?;

    check("start", &rpc(REQ_WIFI_START, &Writer::new())?, 1)?;
    check("connect", &rpc(REQ_WIFI_CONNECT, &Writer::new())?, 1)?;

    let end = systimer::now_ticks() + 15_000_000 * systimer::TICKS_PER_US;
    while !is_connected() {
        if systimer::now_ticks() > end {
            return Err("timed out waiting for association".into());
        }
        poll().map_err(|e| alloc::format!("poll: {:?}", e))?;
        crate::sched::sleep_ms(20);
    }
    let m = mac()?;
    kprintln!("wifi: station MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    crate::net::start(m);
    Ok(())
}
