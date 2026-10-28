//! Fragmented MP4 (DASH) demuxing for YouTube's H.264 streams: the init
//! segment (`moov`: timescale, `avcC`), the segment index (`sidx`) and media
//! fragments (`moof` + `mdat`) turned into Annex-B access units.

use alloc::string::String;
use alloc::vec::Vec;

/// Box header: type, header length, total length (bounded by `data`).
fn read_box(data: &[u8], pos: usize) -> Option<([u8; 4], usize, usize)> {
    let h = data.get(pos..pos + 8)?;
    let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
    let kind = [h[4], h[5], h[6], h[7]];
    let (hdr, size) = match size32 {
        0 => (8, (data.len() - pos) as u64),
        1 => (16, u64::from_be_bytes(data.get(pos + 8..pos + 16)?.try_into().ok()?)),
        n => (8, n),
    };
    if size < hdr as u64 {
        return None;
    }
    Some((kind, hdr, size as usize))
}

/// Children of a container box as (type, body).
fn children(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut pos = 0;
    core::iter::from_fn(move || {
        let (kind, hdr, size) = read_box(data, pos)?;
        let body = data.get(pos + hdr..pos + size)?;
        pos += size;
        Some((kind, body))
    })
}

fn find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn be64(d: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(d.get(at..at + 8)?.try_into().ok()?))
}

fn be16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VideoInit {
    pub timescale: u32,
    pub width: u16,
    pub height: u16,
    /// Bytes in each NAL length prefix (1, 2 or 4).
    pub nal_length_size: usize,
    /// SPS and PPS NAL units as one Annex-B blob, prepended to sync samples.
    pub parameter_sets: Vec<u8>,
    /// `trex` defaults for fragments that do not override them.
    pub default_duration: u32,
    pub default_size: u32,
    pub default_flags: u32,
}

const START_CODE: [u8; 4] = [0, 0, 0, 1];

fn parse_avcc(avcc: &[u8], init: &mut VideoInit) -> Result<(), String> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Err("bad avcC".into());
    }
    init.nal_length_size = (avcc[4] & 3) as usize + 1;
    if init.nal_length_size == 3 {
        return Err("avcC: 3-byte NAL lengths".into());
    }
    let mut pos = 5;
    for count_mask in [0x1F, 0xFF] {
        let count = *avcc.get(pos).ok_or("avcC truncated")? & count_mask;
        pos += 1;
        for _ in 0..count {
            let len = be16(avcc, pos).ok_or("avcC truncated")? as usize;
            let nal = avcc.get(pos + 2..pos + 2 + len).ok_or("avcC truncated")?;
            init.parameter_sets.extend_from_slice(&START_CODE);
            init.parameter_sets.extend_from_slice(nal);
            pos += 2 + len;
        }
    }
    if init.parameter_sets.is_empty() {
        return Err("avcC has no SPS/PPS".into());
    }
    Ok(())
}

/// Parse the init segment (`ftyp` + `moov`) of a single-track H.264 file.
pub fn parse_init(data: &[u8]) -> Result<VideoInit, String> {
    let moov = find(data, b"moov").ok_or("no moov box")?;
    let trak = find(moov, b"trak").ok_or("no trak box")?;
    let mdia = find(trak, b"mdia").ok_or("no mdia box")?;
    let mdhd = find(mdia, b"mdhd").ok_or("no mdhd box")?;
    let timescale = if mdhd.first() == Some(&1) { be32(mdhd, 20) } else { be32(mdhd, 12) }.ok_or("mdhd truncated")?;
    if timescale == 0 {
        return Err("zero timescale".into());
    }
    let stsd = find(mdia, b"minf").and_then(|m| find(m, b"stbl")).and_then(|s| find(s, b"stsd")).ok_or("no stsd box")?;
    // stsd: version/flags, entry count, then sample entries.
    let (kind, hdr, size) = read_box(stsd, 8).ok_or("empty stsd")?;
    if &kind != b"avc1" && &kind != b"avc3" {
        return Err(alloc::format!("not H.264 ({})", String::from_utf8_lossy(&kind)));
    }
    let entry = stsd.get(8 + hdr..8 + size).ok_or("stsd truncated")?;
    // VisualSampleEntry: 78 bytes of fixed fields before the child boxes.
    let (width, height) = (be16(entry, 24).unwrap_or(0), be16(entry, 26).unwrap_or(0));
    let avcc = find(entry.get(78..).ok_or("avc1 truncated")?, b"avcC").ok_or("no avcC box")?;
    let mut init = VideoInit { timescale, width, height, ..VideoInit::default() };
    parse_avcc(avcc, &mut init)?;
    if let Some(trex) = find(moov, b"mvex").and_then(|m| find(m, b"trex")) {
        init.default_duration = be32(trex, 12).unwrap_or(0);
        init.default_size = be32(trex, 16).unwrap_or(0);
        init.default_flags = be32(trex, 20).unwrap_or(0);
    }
    Ok(init)
}

/// One addressable piece of a stream: a DASH segment or a WebM cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: u64,
    pub offset: u64,
    pub size: u64,
}

/// Parse a `sidx` box located at absolute file offset `at`.
pub fn parse_sidx(data: &[u8], at: u64) -> Result<Vec<Segment>, String> {
    let (kind, hdr, size) = read_box(data, 0).ok_or("no box")?;
    if &kind != b"sidx" {
        return Err("index is not a sidx box".into());
    }
    let b = data.get(hdr..size).ok_or("sidx truncated")?;
    let v1 = b.first() == Some(&1);
    let timescale = be32(b, 8).filter(|&t| t > 0).ok_or("sidx timescale")? as u64;
    let (mut t, first_offset, mut p) = if v1 {
        (be64(b, 12).ok_or("sidx")?, be64(b, 20).ok_or("sidx")?, 28)
    } else {
        (be32(b, 12).ok_or("sidx")? as u64, be32(b, 16).ok_or("sidx")? as u64, 20)
    };
    let count = be16(b, p + 2).ok_or("sidx")? as usize;
    p += 4;
    // Offsets count from the first byte after the sidx box.
    let mut offset = at + size as u64 + first_offset;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let r = be32(b, p).ok_or("sidx truncated")?;
        let dur = be32(b, p + 4).ok_or("sidx truncated")? as u64;
        if r & 0x8000_0000 != 0 {
            return Err("hierarchical sidx not supported".into());
        }
        let len = (r & 0x7FFF_FFFF) as u64;
        out.push(Segment { start_ms: t * 1000 / timescale, offset, size: len });
        offset += len;
        t += dur;
        p += 12;
    }
    if out.is_empty() {
        return Err("empty sidx".into());
    }
    Ok(out)
}

/// A video access unit, already in Annex-B form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub pts_ms: i64,
    pub is_sync: bool,
    pub data: Vec<u8>,
}

/// Convert length-prefixed NAL units to Annex-B.
fn to_annex_b(avcc: &[u8], nls: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let mut p = 0;
    while p < avcc.len() {
        let len = match nls {
            1 => *avcc.get(p).ok_or("nal")? as usize,
            2 => be16(avcc, p).ok_or("nal")? as usize,
            _ => be32(avcc, p).ok_or("nal")? as usize,
        };
        let nal = avcc.get(p + nls..p + nls + len).ok_or("NAL runs past the sample")?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
        p += nls + len;
    }
    Ok(())
}

/// Demux every `moof` + `mdat` pair in `data` (whole fragments, as fetched
/// by segment ranges). Sync samples get the SPS/PPS prepended so decoding
/// can start at any of them.
pub fn parse_fragments(data: &[u8], init: &VideoInit) -> Result<Vec<Sample>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((kind, hdr, size)) = read_box(data, pos) {
        if pos + size > data.len() {
            return Err("fragment cut short".into());
        }
        if &kind == b"moof" {
            parse_moof(data, pos, &data[pos + hdr..pos + size], init, &mut out)?;
        }
        pos += size;
    }
    Ok(out)
}

fn parse_moof(data: &[u8], moof_at: usize, moof: &[u8], init: &VideoInit, out: &mut Vec<Sample>) -> Result<(), String> {
    for (kind, traf) in children(moof) {
        if &kind != b"traf" {
            continue;
        }
        let tfhd = find(traf, b"tfhd").ok_or("no tfhd")?;
        let flags = be32(tfhd, 0).ok_or("tfhd")? & 0xFF_FFFF;
        let mut p = 8;
        let mut base = moof_at as u64;
        if flags & 0x01 != 0 {
            base = be64(tfhd, p).ok_or("tfhd")?;
            p += 8;
        }
        if flags & 0x02 != 0 {
            p += 4;
        }
        let mut def_dur = init.default_duration;
        let mut def_size = init.default_size;
        let mut def_flags = init.default_flags;
        for (bit, slot) in [(0x08, &mut def_dur), (0x10, &mut def_size), (0x20, &mut def_flags)] {
            if flags & bit != 0 {
                *slot = be32(tfhd, p).ok_or("tfhd")?;
                p += 4;
            }
        }
        let mut dts = match find(traf, b"tfdt") {
            Some(t) if t.first() == Some(&1) => be64(t, 4).ok_or("tfdt")?,
            Some(t) => be32(t, 4).ok_or("tfdt")? as u64,
            None => 0,
        };
        for (kind, trun) in children(traf) {
            if &kind != b"trun" {
                continue;
            }
            let vflags = be32(trun, 0).ok_or("trun")?;
            let (version, tflags) = (vflags >> 24, vflags & 0xFF_FFFF);
            let count = be32(trun, 4).ok_or("trun")? as usize;
            let mut p = 8;
            let mut data_pos = base;
            if tflags & 0x01 != 0 {
                data_pos = (base as i64 + be32(trun, p).ok_or("trun")? as i32 as i64) as u64;
                p += 4;
            }
            let mut first_flags = None;
            if tflags & 0x04 != 0 {
                first_flags = Some(be32(trun, p).ok_or("trun")?);
                p += 4;
            }
            for i in 0..count {
                let mut field = |bit: u32, default: u32| -> Result<u32, String> {
                    if tflags & bit != 0 {
                        let v = be32(trun, p).ok_or("trun truncated")?;
                        p += 4;
                        Ok(v)
                    } else {
                        Ok(default)
                    }
                };
                let dur = field(0x100, def_dur)?;
                let size = field(0x200, def_size)? as usize;
                let sflags = field(0x400, if i == 0 { first_flags.unwrap_or(def_flags) } else { def_flags })?;
                let cto = field(0x800, 0)?;
                let cto = if version == 0 { cto as i64 } else { cto as i32 as i64 };
                let start = data_pos as usize;
                let avcc = data.get(start..start + size).ok_or("sample outside the fetched data")?;
                let is_sync = sflags & 0x0001_0000 == 0;
                let mut au = Vec::with_capacity(size + if is_sync { init.parameter_sets.len() } else { 0 } + 16);
                if is_sync {
                    au.extend_from_slice(&init.parameter_sets);
                }
                to_annex_b(avcc, init.nal_length_size, &mut au)?;
                let pts = dts as i64 + cto;
                out.push(Sample { pts_ms: pts * 1000 / init.timescale as i64, is_sync, data: au });
                data_pos += size as u64;
                dts += dur as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec;

    pub fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    fn full(kind: &[u8; 4], vflags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = vflags.to_be_bytes().to_vec();
        b.extend_from_slice(body);
        bx(kind, &b)
    }

    const SPS: &[u8] = &[0x67, 0x4D, 0x40, 0x15, 0xAA];
    const PPS: &[u8] = &[0x68, 0xEE, 0x3C, 0x80];

    pub fn init_segment(timescale: u32) -> Vec<u8> {
        let mut avcc = vec![1, 0x4D, 0x40, 0x15, 0xFF, 0xE1];
        avcc.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
        avcc.extend_from_slice(SPS);
        avcc.push(1);
        avcc.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
        avcc.extend_from_slice(PPS);
        let mut entry = vec![0u8; 78];
        entry[24..26].copy_from_slice(&426u16.to_be_bytes());
        entry[26..28].copy_from_slice(&240u16.to_be_bytes());
        entry.extend(bx(b"avcC", &avcc));
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend(bx(b"avc1", &entry));
        let stsd = full(b"stsd", 0, &stsd_body);
        let mut mdhd_body = vec![0u8; 8];
        mdhd_body.extend_from_slice(&timescale.to_be_bytes());
        mdhd_body.extend_from_slice(&[0; 8]);
        let mdia = [full(b"mdhd", 0, &mdhd_body), bx(b"minf", &bx(b"stbl", &stsd))].concat();
        let mut trex = vec![0u8; 8];
        trex.extend_from_slice(&1000u32.to_be_bytes());
        trex.extend_from_slice(&0u32.to_be_bytes());
        trex.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        let moov = [bx(b"trak", &bx(b"mdia", &mdia)), bx(b"mvex", &full(b"trex", 0, &trex))].concat();
        [bx(b"ftyp", b"dash\0\0\0\0"), bx(b"moov", &moov)].concat()
    }

    /// One fragment with three samples (sync, then two non-sync with a
    /// composition offset), each a single 4-byte-length NAL.
    pub fn fragment(base_dts: u64) -> Vec<u8> {
        let nals: [&[u8]; 3] = [&[0x65, 1, 2, 3], &[0x41, 4], &[0x01, 5, 6]];
        let mdat_body: Vec<u8> = nals.iter().flat_map(|n| [(n.len() as u32).to_be_bytes().to_vec(), n.to_vec()].concat()).collect();
        let build = |data_offset: u32| {
            let tfhd = full(b"tfhd", 0x02_0000, &1u32.to_be_bytes());
            let mut tfdt_body = base_dts.to_be_bytes().to_vec();
            tfdt_body.truncate(8);
            let tfdt = full(b"tfdt", 0x0100_0000, &tfdt_body);
            // data offset, first-sample flags, then size + composition offset per sample.
            let mut trun_body = 3u32.to_be_bytes().to_vec();
            trun_body.extend_from_slice(&data_offset.to_be_bytes());
            trun_body.extend_from_slice(&0x0200_0000u32.to_be_bytes());
            for (n, cto) in nals.iter().zip([1000u32, 3000, 0]) {
                trun_body.extend_from_slice(&((n.len() + 4) as u32).to_be_bytes());
                trun_body.extend_from_slice(&cto.to_be_bytes());
            }
            let trun = full(b"trun", 0x0000_0A05, &trun_body);
            bx(b"moof", &bx(b"traf", &[tfhd, tfdt, trun].concat()))
        };
        let moof_len = build(0).len() as u32;
        [build(moof_len + 8), bx(b"mdat", &mdat_body)].concat()
    }

    #[test]
    fn parses_init_segment() {
        let init = parse_init(&init_segment(15360)).unwrap();
        assert_eq!((init.timescale, init.width, init.height, init.nal_length_size), (15360, 426, 240, 4));
        assert_eq!(init.parameter_sets, [&[0, 0, 0, 1][..], SPS, &[0, 0, 0, 1], PPS].concat());
        assert_eq!((init.default_duration, init.default_flags), (1000, 0x0001_0000));
        assert!(parse_init(b"garbage").is_err());
    }

    #[test]
    fn parses_fragments_into_annex_b_samples() {
        let init = parse_init(&init_segment(1000)).unwrap();
        let data = [fragment(5000), fragment(8000)].concat();
        let s = parse_fragments(&data, &init).unwrap();
        assert_eq!(s.len(), 6);
        assert_eq!(s.iter().map(|x| x.pts_ms).collect::<Vec<_>>(), [6000, 9000, 7000, 9000, 12000, 10000]);
        assert_eq!(s.iter().map(|x| x.is_sync).collect::<Vec<_>>(), [true, false, false, true, false, false]);
        assert_eq!(s[0].data, [&init.parameter_sets[..], &[0, 0, 0, 1, 0x65, 1, 2, 3]].concat());
        assert_eq!(s[1].data, [0, 0, 0, 1, 0x41, 4]);
        assert!(parse_fragments(&data[..data.len() - 3], &init).is_err());
    }

    #[test]
    fn parses_sidx() {
        let mut body = vec![0u8; 4];
        body.extend_from_slice(&1u32.to_be_bytes()); // reference id
        body.extend_from_slice(&90_000u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // earliest pts
        body.extend_from_slice(&10u32.to_be_bytes()); // first offset
        body.extend_from_slice(&[0, 0, 0, 2]);
        for (size, dur) in [(1000u32, 450_000u32), (2000, 180_000)] {
            body.extend_from_slice(&size.to_be_bytes());
            body.extend_from_slice(&dur.to_be_bytes());
            body.extend_from_slice(&0x9000_0000u32.to_be_bytes());
        }
        let sidx = bx(b"sidx", &body);
        let segs = parse_sidx(&sidx, 740).unwrap();
        let first = 740 + sidx.len() as u64 + 10;
        assert_eq!(segs, [
            Segment { start_ms: 0, offset: first, size: 1000 },
            Segment { start_ms: 5000, offset: first + 1000, size: 2000 },
        ]);
        assert!(parse_sidx(&bx(b"free", &[]), 0).is_err());
    }
}
