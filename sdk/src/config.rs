//! Persistent settings: key names, value conventions and the record format
//! the kernel stores in SPI flash.
//!
//! A record is one flash sector image:
//!
//! ```text
//! offset 0  magic  "P4C1"
//!        4  seq    u32 LE, incremented on every save
//!        8  len    u32 LE, payload length
//!       12  crc    u32 LE, CRC-32 (IEEE) over seq, len and the payload
//!       16  payload: repeated [key_len u8][key][value_len u8][value]
//! ```
//!
//! The store alternates between two sectors and the newest record with a
//! valid CRC wins, so an interrupted save leaves the previous one intact.
//! This module is pure so it can be unit-tested on the host.

use crate::Orientation;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

pub const WIFI_SSID: &str = "wifi.ssid";
pub const WIFI_PASSWORD: &str = "wifi.password";
/// "portrait" or "landscape".
pub const UI_ORIENTATION: &str = "ui.orientation";
/// Backlight level in whole percent, "5".."100".
pub const UI_BRIGHTNESS: &str = "ui.brightness";

/// Lock screen switch, "1" or "0". The lock is only active when a passcode
/// is stored as well.
pub const LOCK_ENABLED: &str = "lock.enabled";
/// Salted passcode hash `v1$<iterations>$<salt hex>$<hash hex>`, never the
/// passcode itself. Empty or absent: no passcode.
pub const LOCK_PIN: &str = "lock.pin";
/// Wrong passcode attempts since the last successful unlock.
pub const LOCK_FAILS: &str = "lock.fails";
/// Pending lockout: its end in Unix seconds, or "?" when the wall clock was
/// unknown. Empty or absent: no lockout.
pub const LOCK_UNTIL: &str = "lock.until";
/// Auto-lock: "screen-off" (default), "1m", "5m" or "never".
pub const LOCK_AUTO: &str = "lock.auto";

/// Keys whose values are write-only for apps and never printed.
pub fn is_secret(key: &str) -> bool {
    key == WIFI_PASSWORD
}

/// Keys printed as `[set]` in listings: the secrets, plus the passcode hash,
/// which the desktop must read back but which is never shown.
pub fn is_masked(key: &str) -> bool {
    is_secret(key) || key == LOCK_PIN
}

pub type Entries = BTreeMap<String, String>;

pub const MAGIC: [u8; 4] = *b"P4C1";
pub const HEADER_LEN: usize = 16;
pub const MAX_KEY_LEN: usize = 32;
pub const MAX_VALUE_LEN: usize = 255;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// Key empty, too long, or not printable ASCII without spaces.
    BadKey,
    ValueTooLong,
    /// The encoded record does not fit in the sector.
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub seq: u32,
    pub entries: Entries,
}

/// CRC-32 (IEEE 802.3, reflected, as used by zlib).
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &b in *part {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            }
        }
    }
    !crc
}

pub fn check_entry(key: &str, value: &str) -> Result<(), Error> {
    let key_ok = !key.is_empty() && key.len() <= MAX_KEY_LEN && key.bytes().all(|b| b.is_ascii_graphic());
    if !key_ok {
        return Err(Error::BadKey);
    }
    if value.len() > MAX_VALUE_LEN {
        return Err(Error::ValueTooLong);
    }
    Ok(())
}

/// Encode a record of at most `capacity` bytes. The result is padded with
/// 0xFF (the erased flash value) to a multiple of 4 bytes, as the flash
/// writer requires.
pub fn encode(seq: u32, entries: &Entries, capacity: usize) -> Result<Vec<u8>, Error> {
    let mut payload = Vec::new();
    for (k, v) in entries {
        check_entry(k, v)?;
        payload.push(k.len() as u8);
        payload.extend_from_slice(k.as_bytes());
        payload.push(v.len() as u8);
        payload.extend_from_slice(v.as_bytes());
    }
    let total = HEADER_LEN + payload.len();
    if total > capacity {
        return Err(Error::TooLarge);
    }
    let seq_b = seq.to_le_bytes();
    let len_b = (payload.len() as u32).to_le_bytes();
    let crc = crc32(&[&seq_b, &len_b, &payload]);
    let mut out = Vec::with_capacity(total.next_multiple_of(4));
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&seq_b);
    out.extend_from_slice(&len_b);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&payload);
    out.resize(total.next_multiple_of(4), 0xFF);
    Ok(out)
}

/// Decode a sector image. Returns `None` for erased, torn or corrupt data.
pub fn decode(sector: &[u8]) -> Option<Record> {
    let header = sector.get(..HEADER_LEN)?;
    if header[..4] != MAGIC {
        return None;
    }
    let word = |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    let (seq, len, crc) = (word(4), word(8) as usize, word(12));
    let payload = sector.get(HEADER_LEN..HEADER_LEN.checked_add(len)?)?;
    if crc32(&[&header[4..8], &header[8..12], payload]) != crc {
        return None;
    }
    let mut entries = Entries::new();
    let mut rest = payload;
    while !rest.is_empty() {
        let (key, r) = take_field(rest)?;
        let (value, r) = take_field(r)?;
        let key = core::str::from_utf8(key).ok()?;
        let value = core::str::from_utf8(value).ok()?;
        check_entry(key, value).ok()?;
        entries.insert(key.into(), value.into());
        rest = r;
    }
    Some(Record { seq, entries })
}

fn take_field(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&len, rest) = data.split_first()?;
    let len = len as usize;
    Some((rest.get(..len)?, &rest[len..]))
}

/// True if sequence number `a` is newer than `b`, tolerating wrap-around.
pub fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Index of the newest valid record among the slots.
pub fn newest(records: &[Option<Record>]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, r) in records.iter().enumerate() {
        let Some(r) = r else { continue };
        match best {
            Some(b) if !is_newer(r.seq, records[b].as_ref().map_or(0, |x| x.seq)) => {}
            _ => best = Some(i),
        }
    }
    best
}

pub fn orientation_value(o: Orientation) -> &'static str {
    match o {
        Orientation::Portrait => "portrait",
        Orientation::Landscape => "landscape",
    }
}

pub fn parse_orientation(v: &str) -> Option<Orientation> {
    match v {
        "portrait" => Some(Orientation::Portrait),
        "landscape" => Some(Orientation::Landscape),
        _ => None,
    }
}

/// The dimmest stored level; fully dark would leave no way to find the
/// brightness slider again.
pub const MIN_BRIGHTNESS: f32 = 0.05;

pub fn brightness_value(level: f32) -> String {
    alloc::format!("{}", (level.clamp(MIN_BRIGHTNESS, 1.0) * 100.0 + 0.5) as u32)
}

pub fn parse_brightness(v: &str) -> Option<f32> {
    let pct: u32 = v.parse().ok()?;
    (1..=100).contains(&pct).then(|| (pct as f32 / 100.0).max(MIN_BRIGHTNESS))
}

/// Validate Wi-Fi credentials before storing them: SSID 1-32 bytes;
/// password empty (open network) or a WPA2 passphrase of 8-63 characters.
pub fn check_wifi(ssid: &str, password: &str) -> Result<(), &'static str> {
    if ssid.is_empty() || ssid.len() > 32 {
        return Err("SSID must be 1-32 bytes");
    }
    if ssid.contains(' ') {
        return Err("SSID with spaces is not supported");
    }
    if !password.is_empty() && !(8..=63).contains(&password.len()) {
        return Err("password must be 8-63 characters");
    }
    if !password.bytes().all(|b| (0x20..0x7F).contains(&b)) {
        return Err("password must be printable ASCII");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &str)]) -> Entries {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    #[test]
    fn crc_matches_reference() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
    }

    #[test]
    fn round_trip() {
        let e = entries(&[(WIFI_SSID, "home"), (WIFI_PASSWORD, "secret pass"), (UI_BRIGHTNESS, "80")]);
        let mut sector = encode(7, &e, 4096).unwrap();
        assert_eq!(sector.len() % 4, 0);
        sector.resize(4096, 0xFF);
        assert_eq!(decode(&sector), Some(Record { seq: 7, entries: e }));
    }

    #[test]
    fn empty_store_round_trips() {
        let sector = encode(1, &Entries::new(), 4096).unwrap();
        assert_eq!(sector.len(), HEADER_LEN);
        assert_eq!(decode(&sector).unwrap().entries.len(), 0);
    }

    #[test]
    fn erased_and_corrupt_sectors_are_rejected() {
        assert_eq!(decode(&[0xFF; 4096]), None);
        assert_eq!(decode(&[0; 4096]), None);
        assert_eq!(decode(&[]), None);
        let good = encode(3, &entries(&[("a", "b")]), 4096).unwrap();
        for i in 4..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 0x01;
            assert_eq!(decode(&bad), None, "flipped byte {i}");
        }
        // A torn write: the tail never made it to flash.
        let mut torn = good.clone();
        let n = torn.len();
        torn[n - 2..].fill(0xFF);
        assert_eq!(decode(&torn), None);
        // Length pointing past the end of the buffer.
        let mut long = good;
        long[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&long), None);
    }

    #[test]
    fn invalid_entries_are_refused() {
        assert_eq!(check_entry("", "x"), Err(Error::BadKey));
        assert_eq!(check_entry("has space", "x"), Err(Error::BadKey));
        assert_eq!(check_entry(&"k".repeat(33), "x"), Err(Error::BadKey));
        assert_eq!(check_entry("k", &"v".repeat(256)), Err(Error::ValueTooLong));
        assert_eq!(encode(1, &entries(&[("k", &"v".repeat(256))]), 4096), Err(Error::ValueTooLong));
    }

    #[test]
    fn capacity_is_enforced() {
        let big: Entries = (0..40).map(|i| (alloc::format!("key{i}"), "v".repeat(100))).collect();
        assert_eq!(encode(1, &big, 4096), Err(Error::TooLarge));
    }

    #[test]
    fn newest_record_wins_across_wraparound() {
        let rec = |seq| Some(Record { seq, entries: Entries::new() });
        assert_eq!(newest(&[None, None]), None);
        assert_eq!(newest(&[rec(4), None]), Some(0));
        assert_eq!(newest(&[None, rec(4)]), Some(1));
        assert_eq!(newest(&[rec(4), rec(5)]), Some(1));
        assert_eq!(newest(&[rec(9), rec(8)]), Some(0));
        assert_eq!(newest(&[rec(u32::MAX), rec(0)]), Some(1));
        assert!(is_newer(0, u32::MAX));
        assert!(!is_newer(5, 5));
    }

    #[test]
    fn ui_values() {
        for o in [Orientation::Portrait, Orientation::Landscape] {
            assert_eq!(parse_orientation(orientation_value(o)), Some(o));
        }
        assert_eq!(parse_orientation("sideways"), None);
        assert_eq!(brightness_value(0.8), "80");
        assert_eq!(brightness_value(0.0), "5");
        assert_eq!(parse_brightness("80"), Some(0.8));
        assert_eq!(parse_brightness("1"), Some(MIN_BRIGHTNESS));
        assert_eq!(parse_brightness("0"), None);
        assert_eq!(parse_brightness("101"), None);
        assert_eq!(parse_brightness("x"), None);
    }

    #[test]
    fn masked_keys() {
        assert!(is_masked(WIFI_PASSWORD));
        assert!(is_masked(LOCK_PIN));
        assert!(!is_secret(LOCK_PIN), "the desktop must be able to read the hash");
        assert!(!is_masked(LOCK_FAILS));
        assert!(!is_masked(WIFI_SSID));
    }

    #[test]
    fn wifi_credentials() {
        assert!(check_wifi("home", "password1").is_ok());
        assert!(check_wifi("open-net", "").is_ok());
        assert!(check_wifi("", "password1").is_err());
        assert!(check_wifi(&"s".repeat(33), "password1").is_err());
        assert!(check_wifi("my net", "password1").is_err());
        assert!(check_wifi("home", "short").is_err());
        assert!(check_wifi("home", &"p".repeat(64)).is_err());
        assert!(check_wifi("home", "pässword1").is_err());
    }
}
