//! Passcode entry buffer and the stored passcode hash.
//!
//! The passcode is never stored: `lock.pin` holds a versioned record
//! `v1$<iterations>$<salt hex>$<hash hex>` where the hash is
//! PBKDF2-HMAC-SHA256 over the digits with a random 16-byte salt.
//! Verification recomputes the hash and compares in constant time.

use alloc::format;
use alloc::string::String;
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub const MIN_LEN: usize = 4;
pub const MAX_LEN: usize = 8;
pub const SALT_LEN: usize = 16;
pub const HASH_LEN: usize = 32;
/// PBKDF2 rounds for new passcodes. Measured on the board (360 MHz RV32,
/// release build): see `lock bench` on the serial shell.
pub const ITERATIONS: u32 = 10_000;
/// Stored records outside this range are rejected: fewer rounds would be a
/// downgrade, far more would hang the UI on every attempt.
const ITERATIONS_RANGE: core::ops::RangeInclusive<u32> = 10_000..=1_000_000;
const VERSION: &str = "v1";

/// Digits typed so far. Fixed storage that is zeroed when cleared or
/// dropped, so passcodes do not linger in freed heap memory.
#[derive(Default, Clone)]
pub struct PinEntry {
    digits: [u8; MAX_LEN],
    len: usize,
}

impl PinEntry {
    /// Append an ASCII digit; ignored when full or not a digit.
    pub fn push(&mut self, digit: u8) {
        if digit.is_ascii_digit() && self.len < MAX_LEN {
            self.digits[self.len] = digit;
            self.len += 1;
        }
    }

    pub fn pop(&mut self) {
        if self.len > 0 {
            self.len -= 1;
            self.digits[self.len] = 0;
        }
    }

    pub fn clear(&mut self) {
        // Volatile so the wipe is not optimised away as a dead store.
        for d in self.digits.iter_mut() {
            unsafe { core::ptr::write_volatile(d, 0) };
        }
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.digits[..self.len]
    }

    /// A passcode is 4 to 8 digits.
    pub fn is_valid(&self) -> bool {
        (MIN_LEN..=MAX_LEN).contains(&self.len)
    }

    /// Constant-time equality, for comparing the two entries when a new
    /// passcode is confirmed.
    pub fn matches(&self, other: &PinEntry) -> bool {
        self.len == other.len && bool::from(self.as_bytes().ct_eq(other.as_bytes()))
    }
}

impl Drop for PinEntry {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PinHash {
    iterations: u32,
    salt: [u8; SALT_LEN],
    hash: [u8; HASH_LEN],
}

fn derive(pin: &[u8], salt: &[u8], iterations: u32) -> [u8; HASH_LEN] {
    let mut out = [0u8; HASH_LEN];
    pbkdf2::pbkdf2_hmac::<Sha256>(pin, salt, iterations, &mut out);
    out
}

impl PinHash {
    pub fn new(pin: &PinEntry, salt: [u8; SALT_LEN], iterations: u32) -> Self {
        Self { iterations, salt, hash: derive(pin.as_bytes(), &salt, iterations) }
    }

    pub fn verify(&self, pin: &PinEntry) -> bool {
        let candidate = derive(pin.as_bytes(), &self.salt, self.iterations);
        bool::from(candidate.ct_eq(&self.hash))
    }

    pub fn encode(&self) -> String {
        format!("{VERSION}${}${}${}", self.iterations, hex(&self.salt), hex(&self.hash))
    }

    /// Parse a stored record; `None` for an unknown version or bad data.
    pub fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split('$');
        if parts.next()? != VERSION {
            return None;
        }
        let iterations: u32 = parts.next()?.parse().ok()?;
        if !ITERATIONS_RANGE.contains(&iterations) {
            return None;
        }
        let salt = unhex::<SALT_LEN>(parts.next()?)?;
        let hash = unhex::<HASH_LEN>(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self { iterations, salt, hash })
    }
}

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != N * 2 {
        return None;
    }
    let nibble = |c: u8| (c as char).to_digit(16).map(|v| v as u8);
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = nibble(s[2 * i])? << 4 | nibble(s[2 * i + 1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn entry(digits: &str) -> PinEntry {
        let mut e = PinEntry::default();
        for b in digits.bytes() {
            e.push(b);
        }
        e
    }

    #[test]
    fn entry_accepts_only_digits_up_to_max() {
        let mut e = entry("12a3456789");
        assert_eq!(e.as_bytes(), b"12345678");
        e.pop();
        assert_eq!(e.as_bytes(), b"1234567");
        e.clear();
        assert!(e.is_empty());
        assert_eq!(e.digits, [0; MAX_LEN]);
    }

    #[test]
    fn length_rule() {
        assert!(!entry("123").is_valid());
        assert!(entry("1234").is_valid());
        assert!(entry("12345678").is_valid());
    }

    #[test]
    fn matches_compares_length_and_digits() {
        assert!(entry("1234").matches(&entry("1234")));
        assert!(!entry("1234").matches(&entry("12345")));
        assert!(!entry("1234").matches(&entry("1235")));
    }

    #[test]
    fn known_vector() {
        // RFC 7914 section 11 / common PBKDF2-HMAC-SHA256 vector, 1 round.
        let out = derive(b"passwd", b"salt", 1);
        assert_eq!(hex(&out), "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc");
    }

    #[test]
    fn verify_right_and_wrong() {
        let h = PinHash::new(&entry("2468"), [7; SALT_LEN], ITERATIONS);
        assert!(h.verify(&entry("2468")));
        assert!(!h.verify(&entry("2469")));
        assert!(!h.verify(&entry("24680")));
        assert!(!h.verify(&entry("")));
    }

    #[test]
    fn salt_changes_the_hash() {
        let a = PinHash::new(&entry("2468"), [1; SALT_LEN], ITERATIONS);
        let b = PinHash::new(&entry("2468"), [2; SALT_LEN], ITERATIONS);
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn record_round_trips_and_hides_the_pin() {
        let h = PinHash::new(&entry("13579"), [0xAB; SALT_LEN], ITERATIONS);
        let s = h.encode();
        assert!(s.starts_with("v1$10000$abab"));
        assert!(!s.contains("13579"));
        assert!(s.len() <= p4os_sdk::config::MAX_VALUE_LEN);
        let back = PinHash::decode(&s).unwrap();
        assert_eq!(back, h);
        assert!(back.verify(&entry("13579")));
    }

    #[test]
    fn versioning_and_bad_records() {
        let good = PinHash::new(&entry("1234"), [3; SALT_LEN], ITERATIONS).encode();
        assert!(PinHash::decode(&good.replacen("v1", "v2", 1)).is_none());
        assert!(PinHash::decode(&good.replacen("$10000$", "$100$", 1)).is_none());
        assert!(PinHash::decode(&good.replacen("$10000$", "$99999999$", 1)).is_none());
        assert!(PinHash::decode(&format!("{good}$extra")).is_none());
        assert!(PinHash::decode(&good[..good.len() - 2]).is_none());
        let bad_hex = format!("v1$10000${}${}", "zz".repeat(SALT_LEN), "00".repeat(HASH_LEN));
        assert!(PinHash::decode(&bad_hex).is_none());
        assert!(PinHash::decode("").is_none());
        assert!(PinHash::decode("1234").is_none());
    }
}
