//! SNTPv4 (RFC 4330) client packets.

pub const PORT: u16 = 123;
pub const PACKET_LEN: usize = 48;

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: i64 = 2_208_988_800;
/// Anything earlier than 2025-01-01 is a broken reply, not the time.
const MIN_PLAUSIBLE_UNIX: i64 = 1_735_689_600;

/// A client request. The transmit timestamp carries `nonce`, which a
/// genuine reply echoes back as its originate timestamp.
pub fn request(nonce: &[u8; 8]) -> [u8; PACKET_LEN] {
    let mut p = [0u8; PACKET_LEN];
    p[0] = 0x23; // LI 0, version 4, mode 3 (client)
    p[40..48].copy_from_slice(nonce);
    p
}

/// Unix seconds (UTC) from a server reply to the request carrying `nonce`.
pub fn parse_reply(p: &[u8], nonce: &[u8; 8]) -> Option<i64> {
    // Mode 4 (server); stratum 0 is a "kiss-o'-death" refusal.
    if p.len() < PACKET_LEN || p[0] & 0x07 != 4 || p[1] == 0 || p[24..32] != nonce[..] {
        return None;
    }
    let ntp = u32::from_be_bytes([p[40], p[41], p[42], p[43]]) as i64;
    // NTP era 0 ends in 2036; small values belong to era 1.
    let ntp = if ntp < 0x8000_0000 { ntp + (1 << 32) } else { ntp };
    let unix = ntp - NTP_UNIX_OFFSET;
    (unix >= MIN_PLAUSIBLE_UNIX).then_some(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(nonce: &[u8; 8], ntp_secs: u32) -> [u8; PACKET_LEN] {
        let mut p = [0u8; PACKET_LEN];
        p[0] = 0x24; // version 4, mode 4
        p[1] = 2;
        p[24..32].copy_from_slice(nonce);
        p[40..44].copy_from_slice(&ntp_secs.to_be_bytes());
        p
    }

    #[test]
    fn request_layout() {
        let p = request(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(p[0], 0x23);
        assert_eq!(&p[40..48], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn parses_valid_reply() {
        let nonce = [9; 8];
        // 2026-10-28 00:00:00 UTC = 1_793_145_600 Unix.
        let ntp = (1_793_145_600i64 + NTP_UNIX_OFFSET) as u32;
        assert_eq!(parse_reply(&reply(&nonce, ntp), &nonce), Some(1_793_145_600));
    }

    #[test]
    fn handles_era_rollover() {
        let nonce = [9; 8];
        // 2036-02-07 06:28:16 UTC is NTP 2^32 (era 1, seconds 0); add 100 s.
        let unix = (1i64 << 32) - NTP_UNIX_OFFSET + 100;
        assert_eq!(parse_reply(&reply(&nonce, 100), &nonce), Some(unix));
    }

    #[test]
    fn rejects_bad_replies() {
        let nonce = [9; 8];
        let ntp = (1_793_145_600i64 + NTP_UNIX_OFFSET) as u32;
        assert_eq!(parse_reply(&reply(&[8; 8], ntp), &nonce), None, "wrong nonce");
        let mut kod = reply(&nonce, ntp);
        kod[1] = 0;
        assert_eq!(parse_reply(&kod, &nonce), None, "kiss-o'-death");
        let mut client = reply(&nonce, ntp);
        client[0] = 0x23;
        assert_eq!(parse_reply(&client, &nonce), None, "not a server reply");
        assert_eq!(parse_reply(&reply(&nonce, 3_000_000_000), &nonce), None, "before 2025");
        assert_eq!(parse_reply(&[0x24; 20], &nonce), None, "short");
    }
}
