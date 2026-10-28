//! A zero-copy JSON reader for API responses.
//!
//! Why hand-written instead of a crate: `serde-json-core` needs fixed-size
//! target structs and cannot skip the large unknown parts of YouTube
//! responses without declaring them, and `miniserde` builds a full DOM
//! (one allocation per value, several times the input size for a 200 KB
//! response). This reader allocates nothing: a [`Json`] is a slice of the
//! response text, lookups scan it on demand, and only strings the caller
//! asks for are decoded. [`parse`] validates the whole document once, so
//! later lookups never see malformed input. Nesting is limited to
//! [`MAX_DEPTH`] to bound stack use on the UI thread.

use alloc::borrow::Cow;
use alloc::string::String;

pub const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Syntax(usize),
    TooDeep,
}

/// One JSON value: a slice of the source text holding exactly that value.
#[derive(Clone, Copy, Debug)]
pub struct Json<'a> {
    text: &'a str,
}

/// Check `text` is a single well-formed JSON value and return it.
pub fn parse(text: &str) -> Result<Json<'_>, Error> {
    let b = text.as_bytes();
    let start = skip_ws(b, 0);
    let end = skip_value(b, start, 0)?;
    if skip_ws(b, end) != b.len() {
        return Err(Error::Syntax(end));
    }
    Ok(Json { text: &text[start..end] })
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

fn expect(b: &[u8], i: usize, c: u8) -> Result<usize, Error> {
    if b.get(i) == Some(&c) { Ok(i + 1) } else { Err(Error::Syntax(i)) }
}

/// Index just past the value starting at `i` (no leading whitespace).
fn skip_value(b: &[u8], i: usize, depth: usize) -> Result<usize, Error> {
    if depth >= MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    match b.get(i) {
        Some(b'{') => {
            let mut i = skip_ws(b, i + 1);
            if b.get(i) == Some(&b'}') {
                return Ok(i + 1);
            }
            loop {
                i = skip_string(b, i)?;
                i = expect(b, skip_ws(b, i), b':')?;
                i = skip_value(b, skip_ws(b, i), depth + 1)?;
                i = skip_ws(b, i);
                match b.get(i) {
                    Some(b',') => i = skip_ws(b, i + 1),
                    Some(b'}') => return Ok(i + 1),
                    _ => return Err(Error::Syntax(i)),
                }
            }
        }
        Some(b'[') => {
            let mut i = skip_ws(b, i + 1);
            if b.get(i) == Some(&b']') {
                return Ok(i + 1);
            }
            loop {
                i = skip_ws(b, skip_value(b, i, depth + 1)?);
                match b.get(i) {
                    Some(b',') => i = skip_ws(b, i + 1),
                    Some(b']') => return Ok(i + 1),
                    _ => return Err(Error::Syntax(i)),
                }
            }
        }
        Some(b'"') => skip_string(b, i),
        Some(b't') => literal(b, i, b"true"),
        Some(b'f') => literal(b, i, b"false"),
        Some(b'n') => literal(b, i, b"null"),
        Some(b'-' | b'0'..=b'9') => skip_number(b, i),
        _ => Err(Error::Syntax(i)),
    }
}

fn literal(b: &[u8], i: usize, word: &[u8]) -> Result<usize, Error> {
    if b[i..].starts_with(word) { Ok(i + word.len()) } else { Err(Error::Syntax(i)) }
}

fn digits(b: &[u8], mut i: usize) -> Result<usize, Error> {
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start { Err(Error::Syntax(i)) } else { Ok(i) }
}

fn skip_number(b: &[u8], mut i: usize) -> Result<usize, Error> {
    if b[i] == b'-' {
        i += 1;
    }
    i = digits(b, i)?;
    if b.get(i) == Some(&b'.') {
        i = digits(b, i + 1)?;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        i = digits(b, i)?;
    }
    Ok(i)
}

fn skip_string(b: &[u8], i: usize) -> Result<usize, Error> {
    let mut i = expect(b, i, b'"')?;
    loop {
        match b.get(i) {
            None => return Err(Error::Syntax(i)),
            Some(b'"') => return Ok(i + 1),
            Some(b'\\') => match b.get(i + 1) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                Some(b'u') if b.get(i + 2..i + 6).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) => i += 6,
                _ => return Err(Error::Syntax(i)),
            },
            Some(c) if *c < 0x20 => return Err(Error::Syntax(i)),
            Some(_) => i += 1,
        }
    }
}

fn hex4(b: &[u8]) -> u32 {
    b.iter().fold(0, |acc, &c| acc * 16 + (c as char).to_digit(16).unwrap_or(0))
}

impl<'a> Json<'a> {
    /// The raw JSON text of this value.
    pub fn raw(&self) -> &'a str {
        self.text
    }

    pub fn is_object(&self) -> bool {
        self.text.starts_with('{')
    }

    pub fn is_null(&self) -> bool {
        self.text == "null"
    }

    /// Member `key` of an object.
    pub fn get(&self, key: &str) -> Option<Json<'a>> {
        self.members().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Follow a chain of object keys.
    pub fn path(&self, keys: &[&str]) -> Option<Json<'a>> {
        keys.iter().try_fold(*self, |v, k| v.get(k))
    }

    /// Elements of an array (nothing for other values).
    pub fn items(&self) -> Items<'a> {
        let pos = if self.text.starts_with('[') { 1 } else { self.text.len() };
        Items { text: self.text, pos }
    }

    /// (key, value) pairs of an object; keys with escapes are decoded.
    pub fn members(&self) -> Members<'a> {
        let pos = if self.is_object() { 1 } else { self.text.len() };
        Members { text: self.text, pos }
    }

    /// The decoded value of a string.
    pub fn str(&self) -> Option<Cow<'a, str>> {
        let inner = self.text.strip_prefix('"')?.strip_suffix('"')?;
        if !inner.contains('\\') {
            return Some(Cow::Borrowed(inner));
        }
        let b = inner.as_bytes();
        let mut out = String::with_capacity(inner.len());
        let mut i = 0;
        while i < b.len() {
            let run = b[i..].iter().position(|&c| c == b'\\').map_or(b.len(), |p| i + p);
            out.push_str(&inner[i..run]);
            if run == b.len() {
                break;
            }
            i = run + 2;
            let c = match b[run + 1] {
                b'n' => '\n',
                b't' => '\t',
                b'r' => '\r',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'u' => {
                    let hi = hex4(&b[i..i + 4]);
                    i += 4;
                    let is_pair = (0xD800..0xDC00).contains(&hi) && b[i..].starts_with(b"\\u");
                    let lo = if is_pair { hex4(&b[i + 2..i + 6]) } else { 0 };
                    if is_pair && (0xDC00..0xE000).contains(&lo) {
                        i += 6;
                        char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)).unwrap_or('\u{FFFD}')
                    } else {
                        char::from_u32(hi).unwrap_or('\u{FFFD}')
                    }
                }
                other => other as char,
            };
            out.push(c);
        }
        Some(Cow::Owned(out))
    }

    pub fn string(&self) -> Option<String> {
        self.str().map(Cow::into_owned)
    }

    /// A non-negative integer, given as a number or as a decimal string
    /// (the YouTube API sends counts as strings).
    pub fn u64(&self) -> Option<u64> {
        let s = self.text.trim_matches('"');
        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }

    pub fn bool(&self) -> Option<bool> {
        match self.text {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }
}

pub struct Items<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Iterator for Items<'a> {
    type Item = Json<'a>;

    fn next(&mut self) -> Option<Json<'a>> {
        let b = self.text.as_bytes();
        let start = skip_ws(b, self.pos);
        if start >= b.len() || b[start] == b']' {
            self.pos = b.len();
            return None;
        }
        // The document was validated by `parse`, so this cannot fail.
        let end = skip_value(b, start, 0).ok()?;
        let next = skip_ws(b, end);
        self.pos = if b.get(next) == Some(&b',') { next + 1 } else { b.len() };
        Some(Json { text: &self.text[start..end] })
    }
}

pub struct Members<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Iterator for Members<'a> {
    type Item = (Cow<'a, str>, Json<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.text.as_bytes();
        let key_start = skip_ws(b, self.pos);
        if key_start >= b.len() || b[key_start] != b'"' {
            self.pos = b.len();
            return None;
        }
        let key_end = skip_string(b, key_start).ok()?;
        let colon = skip_ws(b, key_end);
        let start = skip_ws(b, colon + 1);
        let end = skip_value(b, start, 0).ok()?;
        let next = skip_ws(b, end);
        self.pos = if b.get(next) == Some(&b',') { next + 1 } else { b.len() };
        let key = Json { text: &self.text[key_start..key_end] }.str()?;
        Some((key, Json { text: &self.text[start..end] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn reads_nested_values() {
        let doc = parse(r#" {"a": {"b": [1, "two", {"c": true}], "n": null}, "s": "x", "e": {}} "#).unwrap();
        let arr: Vec<Json> = doc.path(&["a", "b"]).unwrap().items().collect();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0].u64(), Some(1));
        assert_eq!(arr[1].str().as_deref(), Some("two"));
        assert_eq!(arr[2].get("c").and_then(|v| v.bool()), Some(true));
        assert!(doc.path(&["a", "n"]).unwrap().is_null());
        assert_eq!(doc.get("s").unwrap().string().as_deref(), Some("x"));
        assert_eq!(doc.get("e").unwrap().members().count(), 0);
        assert!(doc.get("missing").is_none());
        assert_eq!(doc.get("s").unwrap().items().count(), 0, "a string has no items");
        assert_eq!(parse("[]").unwrap().items().count(), 0);
    }

    #[test]
    fn decodes_escapes_and_unicode() {
        let doc = parse(r#"{"t": "a\"b\\c\/d\n\u00e9\u1EA1 \ud83d\ude00 \ud800x", "k\u0065y": 1, "vi": "Tiếng Việt"}"#).unwrap();
        assert_eq!(doc.get("t").unwrap().str().unwrap(), "a\"b\\c/d\néạ 😀 \u{FFFD}x");
        assert_eq!(doc.get("key").unwrap().u64(), Some(1));
        assert_eq!(doc.get("vi").unwrap().str().unwrap(), "Tiếng Việt");
    }

    #[test]
    fn numbers_and_numeric_strings() {
        let doc = parse(r#"{"a": "12345", "b": 7, "c": -1, "d": 1.5e3, "e": "12x"}"#).unwrap();
        assert_eq!(doc.get("a").unwrap().u64(), Some(12345));
        assert_eq!(doc.get("b").unwrap().u64(), Some(7));
        assert_eq!(doc.get("c").unwrap().u64(), None);
        assert_eq!(doc.get("d").unwrap().u64(), None);
        assert_eq!(doc.get("e").unwrap().u64(), None);
    }

    #[test]
    fn rejects_malformed_documents() {
        for bad in [
            "", "{", "[1,]", "{\"a\" 1}", "{\"a\":1,}", "\"open", "tru", "01x", "1.", "-", "[1] 2", "{a:1}", "\"\\x\"",
            "\"\\u12\"", "\"tab\there\"", "[1 2]",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        assert_eq!(parse(&deep).unwrap_err(), Error::TooDeep);
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn handles_large_documents() {
        let mut s = String::from("{\"items\":[");
        for i in 0..5000 {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&alloc::format!("{{\"id\":\"v{i}\",\"skip\":{{\"deep\":[1,2,3]}}}}"));
        }
        s.push_str("]}");
        assert!(s.len() > 150_000);
        let doc = parse(&s).unwrap();
        let ids: Vec<String> = doc.get("items").unwrap().items().filter_map(|v| v.get("id")?.string()).collect();
        assert_eq!(ids.len(), 5000);
        assert_eq!(ids[4999], "v4999");
    }
}
