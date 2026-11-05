//! WebM (Matroska/EBML) demuxing for YouTube's Opus audio streams: the
//! header (`Info`, `Tracks` with the OpusHead), the `Cues` seek index, and
//! `Cluster`s of `SimpleBlock`s turned into Opus packets.

use crate::mp4::Segment;
use crate::slice::Slice;
use alloc::sync::Arc;
use core::ops::Range;
use alloc::string::String;
use alloc::vec::Vec;

const ID_EBML: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMECODE_SCALE: u32 = 0x2A_D7B1;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CUE_POINT: u32 = 0xBB;
const ID_CUE_TIME: u32 = 0xB3;
const ID_CUE_TRACK_POSITIONS: u32 = 0xB7;
const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMECODE: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;

/// Element ID (with its length marker, as IDs are written) and its length.
fn read_id(d: &[u8], p: usize) -> Option<(u32, usize)> {
    let first = *d.get(p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut v = 0u32;
    for i in 0..len {
        v = (v << 8) | *d.get(p + i)? as u32;
    }
    Some((v, len))
}

/// Variable-length size: value (None when "unknown") and encoded length.
fn read_size(d: &[u8], p: usize) -> Option<(Option<u64>, usize)> {
    let first = *d.get(p)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mut v = (first as u64) & (0xFF >> len);
    let mut all_ones = v == (0xFF >> len) as u64;
    for i in 1..len {
        let b = *d.get(p + i)?;
        all_ones &= b == 0xFF;
        v = (v << 8) | b as u64;
    }
    Some(((!all_ones).then_some(v), len))
}

struct Element {
    id: u32,
    /// Offset of the body within the buffer.
    body: usize,
    /// Body length; for an unknown-size element, up to the end of the buffer.
    len: usize,
    known: bool,
}

fn element(d: &[u8], p: usize) -> Option<Element> {
    let (id, il) = read_id(d, p)?;
    let (size, sl) = read_size(d, p + il)?;
    let body = p + il + sl;
    let avail = d.len().saturating_sub(body);
    Some(match size {
        Some(n) => Element { id, body, len: (n as usize).min(avail), known: n as usize <= avail },
        None => Element { id, body, len: avail, known: false },
    })
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0, |v, &x| (v << 8) | x as u64)
}

/// Iterate the child elements of a body.
fn each(d: &[u8], mut f: impl FnMut(&Element) -> bool) {
    let mut p = 0;
    while let Some(e) = element(d, p) {
        if !f(&e) {
            return;
        }
        p = e.body + e.len;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioInit {
    /// File offset of the Segment body: Cues positions are relative to it.
    pub segment_start: u64,
    /// Nanoseconds per timecode unit.
    pub timecode_scale: u64,
    pub track: u64,
    /// OpusHead.
    pub codec_private: Vec<u8>,
}

impl AudioInit {
    pub fn channels(&self) -> u8 {
        self.codec_private.get(9).copied().unwrap_or(2)
    }

    /// Samples (at 48 kHz) to drop at the very start of the stream.
    pub fn pre_skip(&self) -> u16 {
        self.codec_private.get(10..12).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn ticks_to_ms(&self, t: i64) -> i64 {
        t * self.timecode_scale as i64 / 1_000_000
    }
}

/// Parse the header range (EBML header up to and including `Tracks`).
pub fn parse_init(data: &[u8]) -> Result<AudioInit, String> {
    let ebml = element(data, 0).filter(|e| e.id == ID_EBML && e.known).ok_or("not a WebM file")?;
    let seg = element(data, ebml.body + ebml.len).filter(|e| e.id == ID_SEGMENT).ok_or("no Segment element")?;
    let mut init = AudioInit { segment_start: seg.body as u64, timecode_scale: 1_000_000, ..AudioInit::default() };
    let mut opus_track = None;
    let body = &data[seg.body..seg.body + seg.len];
    each(body, |e| {
        let b = &body[e.body..e.body + e.len];
        match e.id {
            ID_INFO => each(b, |c| {
                if c.id == ID_TIMECODE_SCALE {
                    init.timecode_scale = uint(&b[c.body..c.body + c.len]);
                }
                true
            }),
            ID_TRACKS => each(b, |t| {
                if t.id == ID_TRACK_ENTRY {
                    let tb = &b[t.body..t.body + t.len];
                    let (mut num, mut codec, mut private) = (0, Vec::new(), Vec::new());
                    each(tb, |c| {
                        let v = &tb[c.body..c.body + c.len];
                        match c.id {
                            ID_TRACK_NUMBER => num = uint(v),
                            ID_CODEC_ID => codec = v.to_vec(),
                            ID_CODEC_PRIVATE => private = v.to_vec(),
                            _ => {}
                        }
                        true
                    });
                    if codec == b"A_OPUS" && opus_track.is_none() {
                        opus_track = Some((num, private));
                    }
                }
                true
            }),
            _ => {}
        }
        // Stop at the first Cues or Cluster: the header is over.
        !matches!(e.id, ID_CUES | ID_CLUSTER)
    });
    let (track, private) = opus_track.ok_or("no Opus track")?;
    if !private.starts_with(b"OpusHead") || private.len() < 19 {
        return Err("bad OpusHead".into());
    }
    if init.timecode_scale == 0 {
        return Err("zero TimecodeScale".into());
    }
    init.track = track;
    init.codec_private = private;
    Ok(init)
}

/// Parse the `Cues` element (the index range) into cluster positions.
/// `content_length` closes the last cluster.
pub fn parse_cues(data: &[u8], init: &AudioInit, content_length: u64) -> Result<Vec<Segment>, String> {
    let cues = element(data, 0).filter(|e| e.id == ID_CUES).ok_or("index is not a Cues element")?;
    let body = &data[cues.body..cues.body + cues.len];
    let mut points: Vec<(u64, u64)> = Vec::new();
    each(body, |cp| {
        if cp.id == ID_CUE_POINT {
            let b = &body[cp.body..cp.body + cp.len];
            let (mut time, mut pos) = (None, None);
            each(b, |c| {
                let v = &b[c.body..c.body + c.len];
                match c.id {
                    ID_CUE_TIME => time = Some(uint(v)),
                    ID_CUE_TRACK_POSITIONS => each(v, |t| {
                        if t.id == ID_CUE_CLUSTER_POSITION {
                            pos = Some(uint(&v[t.body..t.body + t.len]));
                        }
                        true
                    }),
                    _ => {}
                }
                true
            });
            if let (Some(t), Some(p)) = (time, pos) {
                points.push((init.ticks_to_ms(t as i64) as u64, init.segment_start + p));
            }
        }
        true
    });
    points.sort_unstable_by_key(|p| p.1);
    points.dedup_by_key(|p| p.1);
    if points.is_empty() {
        return Err("empty Cues".into());
    }
    let mut out = Vec::with_capacity(points.len());
    for (i, &(start_ms, offset)) in points.iter().enumerate() {
        let end = points.get(i + 1).map_or(content_length, |n| n.1);
        if end <= offset {
            return Err("Cues point past the end of the file".into());
        }
        out.push(Segment { start_ms, offset, size: end - offset });
    }
    Ok(out)
}

/// An Opus packet with its presentation time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub pts_ms: i64,
    pub data: Slice,
}

/// Split a laced block payload into frames.
fn unlace(d: &[u8], lacing: u8) -> Option<Vec<&[u8]>> {
    if lacing == 0 {
        return Some(alloc::vec![d]);
    }
    let count = *d.first()? as usize + 1;
    let mut p = 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        // Xiph: each size as a run of 255s plus a remainder.
        1 => {
            for _ in 0..count - 1 {
                let mut s = 0;
                loop {
                    let b = *d.get(p)?;
                    p += 1;
                    s += b as usize;
                    if b != 255 {
                        break;
                    }
                }
                sizes.push(s);
            }
        }
        // Fixed size: equal frames.
        2 => {
            let each = (d.len() - 1) / count;
            sizes.resize(count - 1, each);
        }
        // EBML: first size as a vint, then signed differences.
        _ => {
            let (first, l) = read_size(d, p)?;
            p += l;
            let mut s = first? as i64;
            sizes.push(s as usize);
            for _ in 1..count - 1 {
                let (raw, l) = read_size(d, p)?;
                let bias = (1i64 << (7 * l - 1)) - 1;
                s += raw? as i64 - bias;
                if s < 0 {
                    return None;
                }
                sizes.push(s as usize);
                p += l;
            }
        }
    }
    let used: usize = sizes.iter().sum();
    let last = d.len().checked_sub(p + used)?;
    sizes.push(last);
    let mut out = Vec::with_capacity(count);
    for s in sizes {
        out.push(d.get(p..p + s)?);
        p += s;
    }
    Some(out)
}

/// Demux every Opus packet of `init.track` from whole clusters in `data`.
pub fn parse_clusters(chunk: &Arc<Vec<u8>>, init: &AudioInit) -> Result<Vec<Packet>, String> {
    let ranges = cluster_ranges(chunk, init)?;
    Ok(ranges.into_iter().map(|(pts_ms, r)| Packet { pts_ms, data: Slice::new(chunk, r) }).collect())
}

/// Packet times and byte ranges (within `data`) of whole clusters.
fn cluster_ranges(data: &[u8], init: &AudioInit) -> Result<Vec<(i64, Range<usize>)>, String> {
    let mut out = Vec::new();
    let mut p = 0;
    while let Some(c) = element(data, p) {
        if c.id == ID_CLUSTER {
            if !c.known {
                return Err("cluster cut short".into());
            }
            let body = &data[c.body..c.body + c.len];
            let mut cluster_tc = 0i64;
            let mut bad = None;
            each(body, |e| {
                let v = &body[e.body..e.body + e.len];
                let block = match e.id {
                    ID_TIMECODE => {
                        cluster_tc = uint(v) as i64;
                        None
                    }
                    ID_SIMPLE_BLOCK => Some(v),
                    ID_BLOCK_GROUP => {
                        let mut inner = None;
                        each(v, |b| {
                            if b.id == ID_BLOCK {
                                inner = Some(&v[b.body..b.body + b.len]);
                            }
                            true
                        });
                        inner
                    }
                    _ => None,
                };
                if let Some(blk) = block
                    && let Err(err) = push_block(data, blk, cluster_tc, init, &mut out)
                {
                    bad = Some(err);
                    return false;
                }
                true
            });
            if let Some(e) = bad {
                return Err(e);
            }
        }
        p = c.body + c.len;
    }
    Ok(out)
}

/// Demux a whole (or truncated) file without an index: header plus every
/// complete cluster. Used by `yt bench` and host tools.
pub fn demux_file(data: &[u8]) -> Result<(AudioInit, Vec<Packet>), String> {
    let init = parse_init(data)?;
    let all = Arc::new(data.to_vec());
    let mut packets = Vec::new();
    let mut p = init.segment_start as usize;
    while let Some(e) = element(data, p) {
        if !e.known {
            break;
        }
        if e.id == ID_CLUSTER {
            for (pts_ms, r) in cluster_ranges(&data[p..e.body + e.len], &init)? {
                packets.push(Packet { pts_ms, data: Slice::new(&all, r.start + p..r.end + p) });
            }
        }
        p = e.body + e.len;
    }
    Ok((init, packets))
}

fn push_block(base: &[u8], blk: &[u8], cluster_tc: i64, init: &AudioInit, out: &mut Vec<(i64, Range<usize>)>) -> Result<(), String> {
    let (track, tl) = read_size(blk, 0).ok_or("bad block")?;
    if track != Some(init.track) {
        return Ok(());
    }
    let hdr = blk.get(tl..tl + 3).ok_or("bad block")?;
    let rel = i16::from_be_bytes([hdr[0], hdr[1]]) as i64;
    let lacing = (hdr[2] >> 1) & 3;
    let frames = unlace(&blk[tl + 3..], lacing).ok_or("bad block lacing")?;
    let pts_ms = init.ticks_to_ms(cluster_tc + rel);
    // Laced frames share the block time; Opus frames are 20 ms in practice.
    for (i, f) in frames.into_iter().enumerate() {
        // `f` is a sub-slice of `base`: its offset is the pointer distance.
        let at = f.as_ptr() as usize - base.as_ptr() as usize;
        out.push((pts_ms + 20 * i as i64, at..at + f.len()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::vec;

    pub fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect();
        // 8-byte size vint, as some muxers write.
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    pub fn opus_head() -> Vec<u8> {
        let mut h = b"OpusHead".to_vec();
        h.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        h
    }

    /// Header, cues and two clusters of a tiny one-track Opus file.
    pub fn file() -> (Vec<u8>, usize, usize) {
        let head = el(ID_EBML, &el(0x4282, b"webm"));
        let info = el(ID_INFO, &el(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]));
        let entry = [el(ID_TRACK_NUMBER, &[1]), el(ID_CODEC_ID, b"A_OPUS"), el(ID_CODEC_PRIVATE, &opus_head())].concat();
        let tracks = el(ID_TRACKS, &el(ID_TRACK_ENTRY, &entry));
        let block = |rel: i16, payload: &[u8]| {
            let mut b = vec![0x81];
            b.extend_from_slice(&rel.to_be_bytes());
            b.push(0x80);
            b.extend_from_slice(payload);
            el(ID_SIMPLE_BLOCK, &b)
        };
        let cluster1 = el(ID_CLUSTER, &[el(ID_TIMECODE, &[0]), block(0, b"aa"), block(20, b"bb")].concat());
        // Xiph-laced block with two frames of 1 and 2 bytes.
        let laced = {
            let mut b = vec![0x81, 0, 0, 0x82, 1, 1];
            b.extend_from_slice(b"xyz");
            el(ID_SIMPLE_BLOCK, &b)
        };
        let cluster2 = el(ID_CLUSTER, &[el(ID_TIMECODE, &[0x03, 0xE8]), laced, block(40, b"cc")].concat());
        let header_len = head.len() + 12 + info.len() + tracks.len();
        let c1 = (header_len - head.len() - 12) as u64;
        let c2 = c1 + cluster1.len() as u64;
        let cue = |t: u64, pos: u64| {
            el(ID_CUE_POINT, &[el(ID_CUE_TIME, &t.to_be_bytes()), el(ID_CUE_TRACK_POSITIONS, &el(ID_CUE_CLUSTER_POSITION, &pos.to_be_bytes()))].concat())
        };
        let cues = el(ID_CUES, &[cue(0, c1), cue(1000, c2)].concat());
        let body = [info, tracks, cluster1, cluster2, cues].concat();
        let seg = el(ID_SEGMENT, &body);
        let file = [head, seg].concat();
        (file, header_len, c2 as usize)
    }

    #[test]
    fn demuxes_header_cues_and_clusters() {
        let (file, header_len, _) = file();
        let init = parse_init(&file[..header_len]).unwrap();
        assert_eq!((init.track, init.timecode_scale, init.channels(), init.pre_skip()), (1, 1_000_000, 2, 312));
        let cues_at = file.windows(4).rposition(|w| w == ID_CUES.to_be_bytes()).unwrap();
        let segs = parse_cues(&file[cues_at..], &init, cues_at as u64).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start_ms, segs[1].start_ms), (0, 1000));
        assert_eq!(segs[0].offset, header_len as u64);
        let all = &file[segs[0].offset as usize..(segs[1].offset + segs[1].size) as usize];
        let pk = parse_clusters(&Arc::new(all.to_vec()), &init).unwrap();
        let got: Vec<(i64, &[u8])> = pk.iter().map(|p| (p.pts_ms, &p.data[..])).collect();
        assert_eq!(got, vec![(0, &b"aa"[..]), (20, b"bb"), (1000, b"x"), (1020, b"yz"), (1040, b"cc")]);
        // A cluster that was cut short is an error, not silently dropped.
        assert!(parse_clusters(&Arc::new(all[..all.len() - 2].to_vec()), &init).is_err());
    }

    /// For a whole file: the end of the header (first Cues or Cluster) and
    /// the Cues element's byte range, as YouTube reports them in
    /// `initRange` / `indexRange`.
    pub fn layout(file: &[u8]) -> (usize, (usize, usize)) {
        let ebml = element(file, 0).unwrap();
        let seg = element(file, ebml.body + ebml.len).unwrap();
        let (mut header_end, mut cues) = (None, None);
        let mut p = seg.body;
        while let Some(e) = element(file, p) {
            if matches!(e.id, ID_CUES | ID_CLUSTER) && header_end.is_none() {
                header_end = Some(p);
            }
            if e.id == ID_CUES {
                cues = Some((p, e.body + e.len));
            }
            p = e.body + e.len;
        }
        (header_end.unwrap(), cues.unwrap())
    }

    #[test]
    fn demuxes_the_ffmpeg_fixture() {
        let file = crate::audio::tests::TONE;
        let (header_end, (cs, ce)) = layout(file);
        let init = parse_init(&file[..header_end]).unwrap();
        assert_eq!(init.channels(), 2);
        let segs = parse_cues(&file[cs..ce], &init, file.len() as u64).unwrap();
        let mut packets = Vec::new();
        for s in &segs {
            // Elements other than clusters (ffmpeg's trailing Cues) are skipped.
            let chunk = Arc::new(file[s.offset as usize..(s.offset + s.size) as usize].to_vec());
            packets.extend(parse_clusters(&chunk, &init).unwrap());
        }
        // 1 s of 20 ms packets.
        assert!((50..=52).contains(&packets.len()), "{}", packets.len());
        // Without the index: the same packets, and fewer (but no error)
        // from a truncated file.
        assert_eq!(demux_file(file).unwrap().1, packets);
        let half = demux_file(&file[..file.len() / 2]).unwrap().1;
        assert!(half.len() < packets.len() && half.iter().zip(&packets).all(|(a, b)| a == b));
        // 20 ms apart, give or take the 1 ms timecode rounding.
        assert!(packets.windows(2).all(|w| (19..=21).contains(&(w[1].pts_ms - w[0].pts_ms))));
    }

    #[test]
    fn unlaces_all_kinds() {
        assert_eq!(unlace(b"\x02abcdef", 2).unwrap(), [&b"ab"[..], b"cd", b"ef"]);
        // EBML lacing: 3 frames, first size 2, then +1 (diff vint 0xC0 = +1).
        assert_eq!(unlace(&[2, 0x82, 0xC0, b'a', b'b', b'c', b'd', b'e', b'f'], 3).unwrap(), [&b"ab"[..], b"cde", b"f"]);
        assert!(unlace(&[1, 200, b'a'], 1).is_none());
    }

    #[test]
    fn reads_vints() {
        assert_eq!(read_size(&[0x81], 0), Some((Some(1), 1)));
        assert_eq!(read_size(&[0x40, 0x02], 0), Some((Some(2), 2)));
        assert_eq!(read_size(&[0xFF], 0), Some((None, 1)));
        assert_eq!(read_size(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 0), Some((None, 8)));
        assert_eq!(read_id(&[0x1A, 0x45, 0xDF, 0xA3], 0), Some((ID_EBML, 4)));
    }
}
