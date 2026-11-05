//! A zero-copy JSON reader. The document is validated once by [`parse`];
//! values are then views into the original text, so a 200 KB player
//! response is navigated without building a tree on the heap. Strings are
//! unescaped only when read.

use alloc::borrow::Cow;
use alloc::string::String;

/// Nesting deeper than this is rejected, bounding recursion on the stack.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

/// One JSON value: the exact text of the value inside the document.
#[derive(Clone, Copy, Debug)]
pub struct Value<'a> {
    text: &'a str,
}

/// Validate `text` as a single JSON document and return its root value.
pub fn parse(text: &str) -> Result<Value<'_>, String> {
    let b = text.as_bytes();
    let start = skip_ws(b, 0);
    let end = skip_value(b, start, 0).map_err(|at| alloc::format!("invalid JSON at byte {at}"))?;
    if skip_ws(b, end) != b.len() {
        return Err(alloc::format!("invalid JSON: trailing data at byte {end}"));
    }
    Ok(Value { text: &text[start..end] })
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// End of the value starting at `i`, or the offset of the first error.
fn skip_value(b: &[u8], i: usize, depth: usize) -> Result<usize, usize> {
    if depth > MAX_DEPTH {
        return Err(i);
    }
    match b.get(i) {
        Some(b'"') => skip_string(b, i),
        Some(b'{') => {
            let mut j = skip_ws(b, i + 1);
            if b.get(j) == Some(&b'}') {
                return Ok(j + 1);
            }
            loop {
                if b.get(j) != Some(&b'"') {
                    return Err(j);
                }
                j = skip_ws(b, skip_string(b, j)?);
                if b.get(j) != Some(&b':') {
                    return Err(j);
                }
                j = skip_ws(b, skip_value(b, skip_ws(b, j + 1), depth + 1)?);
                match b.get(j) {
                    Some(b',') => j = skip_ws(b, j + 1),
                    Some(b'}') => return Ok(j + 1),
                    _ => return Err(j),
                }
            }
        }
        Some(b'[') => {
            let mut j = skip_ws(b, i + 1);
            if b.get(j) == Some(&b']') {
                return Ok(j + 1);
            }
            loop {
                j = skip_ws(b, skip_value(b, j, depth + 1)?);
                match b.get(j) {
                    Some(b',') => j = skip_ws(b, j + 1),
                    Some(b']') => return Ok(j + 1),
                    _ => return Err(j),
                }
            }
        }
        Some(b't') if b[i..].starts_with(b"true") => Ok(i + 4),
        Some(b'f') if b[i..].starts_with(b"false") => Ok(i + 5),
        Some(b'n') if b[i..].starts_with(b"null") => Ok(i + 4),
        Some(b'-' | b'0'..=b'9') => {
            let mut j = i + 1;
            while j < b.len() && matches!(b[j], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                j += 1;
            }
            Ok(j)
        }
        _ => Err(i),
    }
}

fn skip_string(b: &[u8], i: usize) -> Result<usize, usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'"' => return Ok(j + 1),
            b'\\' => j += 2,
            0..=0x1F => return Err(j),
            _ => j += 1,
        }
    }
    Err(b.len())
}

fn hex4(b: &[u8]) -> Option<u32> {
    let s = core::str::from_utf8(b.get(..4)?).ok()?;
    u32::from_str_radix(s, 16).ok()
}

/// Decode the body of a JSON string (without the quotes).
fn unescape(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let b = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            // Copy the run up to the next escape as one UTF-8 slice.
            let next = raw[i..].find('\\').map_or(b.len(), |n| i + n);
            out.push_str(&raw[i..next]);
            i = next;
            continue;
        }
        let esc = b.get(i + 1).copied().unwrap_or(b'\\');
        i += 2;
        match esc {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'u' => {
                let mut cp = hex4(&b[i.min(b.len())..]).unwrap_or(0xFFFD);
                i += 4;
                if (0xD800..0xDC00).contains(&cp) && b.get(i) == Some(&b'\\') && b.get(i + 1) == Some(&b'u') {
                    if let Some(lo) = hex4(&b[(i + 2).min(b.len())..]).filter(|lo| (0xDC00..0xE000).contains(lo)) {
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        i += 6;
                    }
                }
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            other => out.push(other as char),
        }
    }
    Cow::Owned(out)
}

/// Quote and escape `s` as a JSON string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl<'a> Value<'a> {
    pub fn kind(&self) -> Kind {
        match self.text.as_bytes().first() {
            Some(b'"') => Kind::String,
            Some(b'{') => Kind::Object,
            Some(b'[') => Kind::Array,
            Some(b't' | b'f') => Kind::Bool,
            Some(b'n') => Kind::Null,
            _ => Kind::Number,
        }
    }

    /// The raw JSON text of this value.
    pub fn raw(&self) -> &'a str {
        self.text
    }

    /// Members of an object as (unescaped key, value); empty otherwise.
    pub fn members(&self) -> Members<'a> {
        let pos = if self.kind() == Kind::Object { 1 } else { self.text.len() };
        Members { text: self.text, pos }
    }

    /// Elements of an array; empty otherwise.
    pub fn items(&self) -> Items<'a> {
        let pos = if self.kind() == Kind::Array { 1 } else { self.text.len() };
        Items { text: self.text, pos }
    }

    pub fn get(&self, key: &str) -> Option<Value<'a>> {
        self.members().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Follow a chain of object keys.
    pub fn path(&self, keys: &[&str]) -> Option<Value<'a>> {
        keys.iter().try_fold(*self, |v, k| v.get(k))
    }

    pub fn as_str(&self) -> Option<Cow<'a, str>> {
        (self.kind() == Kind::String).then(|| unescape(&self.text[1..self.text.len() - 1]))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.text {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// A number, or a string holding one (YouTube sends 64-bit sizes as
    /// strings).
    pub fn as_f64(&self) -> Option<f64> {
        match self.kind() {
            Kind::Number => self.text.parse().ok(),
            Kind::String => self.as_str()?.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self.kind() {
            Kind::Number => self.text.parse().ok(),
            Kind::String => self.as_str()?.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn str_at(&self, keys: &[&str]) -> Option<Cow<'a, str>> {
        self.path(keys)?.as_str()
    }

    pub fn u64_at(&self, keys: &[&str]) -> Option<u64> {
        self.path(keys)?.as_u64()
    }
}

pub struct Members<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Iterator for Members<'a> {
    type Item = (Cow<'a, str>, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.text.as_bytes();
        let k = skip_ws(b, self.pos);
        if b.get(k) != Some(&b'"') {
            self.pos = b.len();
            return None;
        }
        let kend = skip_string(b, k).ok()?;
        let colon = skip_ws(b, kend);
        let vstart = skip_ws(b, colon + 1);
        let vend = skip_value(b, vstart, 0).ok()?;
        let after = skip_ws(b, vend);
        self.pos = if b.get(after) == Some(&b',') { after + 1 } else { b.len() };
        Some((unescape(&self.text[k + 1..kend - 1]), Value { text: &self.text[vstart..vend] }))
    }
}

pub struct Items<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Iterator for Items<'a> {
    type Item = Value<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let b = self.text.as_bytes();
        let start = skip_ws(b, self.pos);
        if start >= b.len() || b[start] == b']' {
            self.pos = b.len();
            return None;
        }
        let end = skip_value(b, start, 0).ok()?;
        let after = skip_ws(b, end);
        self.pos = if b.get(after) == Some(&b',') { after + 1 } else { b.len() };
        Some(Value { text: &self.text[start..end] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigates_objects_and_arrays() {
        let doc = r#" { "a": {"b": [1, 2.5, "x"], "c": true}, "s": "q\"\\\né😀", "n": null, "len": "12345678901" } "#;
        let v = parse(doc).unwrap();
        assert_eq!(v.kind(), Kind::Object);
        let arr = v.path(&["a", "b"]).unwrap();
        let items: alloc::vec::Vec<_> = arr.items().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].as_f64(), Some(2.5));
        assert_eq!(items[2].as_str().as_deref(), Some("x"));
        assert_eq!(v.path(&["a", "c"]).and_then(|c| c.as_bool()), Some(true));
        assert_eq!(v.str_at(&["s"]).as_deref(), Some("q\"\\\né😀"));
        assert_eq!(v.get("n").map(|n| n.kind()), Some(Kind::Null));
        assert_eq!(v.u64_at(&["len"]), Some(12_345_678_901));
        assert!(v.get("missing").is_none());
        assert_eq!(v.members().count(), 4);
        assert_eq!(parse("[]").unwrap().items().count(), 0);
        assert_eq!(parse("{}").unwrap().members().count(), 0);
    }

    #[test]
    fn rejects_malformed_documents() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "{\"a\":1,}", "\"abc", "tru", "[1] x", "{\"a\":\"\x01\"}"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn quotes_strings() {
        assert_eq!(quote("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
        let q = quote("xé\"");
        assert_eq!(parse(&q).unwrap().as_str().as_deref(), Some("xé\""));
    }
}
