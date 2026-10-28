//! A small, forgiving HTML tokenizer and character-reference decoder.
//!
//! It does not build a tree: the layout engine consumes the flat token
//! stream and keeps just enough state (open lists, links, headings) to
//! render. Malformed markup never fails; anything that does not look like
//! a tag is treated as text.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// Text with character references already decoded.
    Text(String),
    /// Tag and attribute names are lower-cased; values are decoded.
    Start { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    End(String),
}

impl Token {
    /// Value of attribute `key` on a start tag.
    pub fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
        attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Elements whose content is not markup. Script and style content is
/// dropped; title and textarea content is kept as text.
const RAW_TEXT: [&str; 6] = ["script", "style", "title", "textarea", "xmp", "noembed"];

pub fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    let flush = |out: &mut Vec<Token>, from: usize, to: usize| {
        if from < to {
            out.push(Token::Text(decode_entities(&src[from..to])));
        }
    };
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        let next = b.get(i + 1).copied().unwrap_or(0);
        if b[i..].starts_with(b"<!--") {
            flush(&mut out, text_start, i);
            i = find(b, i + 4, b"-->").map_or(b.len(), |e| e + 3);
            text_start = i;
        } else if next == b'!' || next == b'?' {
            // <!DOCTYPE ...>, <![CDATA[...]]>, <?xml ...?>: skip.
            flush(&mut out, text_start, i);
            i = find(b, i + 2, b">").map_or(b.len(), |e| e + 1);
            text_start = i;
        } else if next == b'/' && b.get(i + 2).is_some_and(u8::is_ascii_alphabetic) {
            flush(&mut out, text_start, i);
            let (name, after) = read_name(src, i + 2);
            i = find(b, after, b">").map_or(b.len(), |e| e + 1);
            text_start = i;
            out.push(Token::End(name));
        } else if next.is_ascii_alphabetic() {
            flush(&mut out, text_start, i);
            let (name, attrs, self_closing, after) = read_tag(src, i + 1);
            i = after;
            text_start = i;
            let raw = RAW_TEXT.contains(&name.as_str()) && !self_closing;
            out.push(Token::Start { name: name.clone(), attrs, self_closing });
            if raw {
                // Everything up to the matching close tag is content.
                let close = find_ci(b, i, &[b"</", name.as_bytes()].concat()).unwrap_or(b.len());
                if matches!(name.as_str(), "title" | "textarea") && close > i {
                    out.push(Token::Text(decode_entities(&src[i..close])));
                }
                i = find(b, close, b">").map_or(b.len(), |e| e + 1);
                text_start = i;
                out.push(Token::End(name));
            }
        } else {
            // A lone '<' (e.g. "a < b") is just text.
            i += 1;
        }
    }
    flush(&mut out, text_start, b.len());
    out
}

fn find(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn find_ci(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle)).map(|p| p + from)
}

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b':' || c == b'_'
}

fn read_name(src: &str, start: usize) -> (String, usize) {
    let b = src.as_bytes();
    let mut j = start;
    while j < b.len() && is_name_byte(b[j]) {
        j += 1;
    }
    (src[start..j].to_ascii_lowercase(), j)
}

/// Parse a start tag beginning at its name. Returns (name, attributes,
/// self-closing, index after '>').
fn read_tag(src: &str, start: usize) -> (String, Vec<(String, String)>, bool, usize) {
    let b = src.as_bytes();
    let (name, mut j) = read_name(src, start);
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() {
            break;
        }
        match b[j] {
            b'>' => {
                j += 1;
                break;
            }
            b'/' => {
                self_closing = b.get(j + 1) == Some(&b'>');
                j += 1;
                continue;
            }
            _ => {}
        }
        let key_start = j;
        while j < b.len() && !b[j].is_ascii_whitespace() && !matches!(b[j], b'=' | b'>' | b'/') {
            j += 1;
        }
        if j == key_start {
            // Stray '=' or similar: skip it.
            j += 1;
            continue;
        }
        let key = src[key_start..j].to_ascii_lowercase();
        let mut k = j;
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        let mut value = String::new();
        if b.get(k) == Some(&b'=') {
            k += 1;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            match b.get(k) {
                Some(&q) if q == b'"' || q == b'\'' => {
                    let end = find(b, k + 1, &[q]).unwrap_or(b.len());
                    value = decode_entities(&src[k + 1..end]);
                    j = (end + 1).min(b.len());
                }
                _ => {
                    let vs = k;
                    while k < b.len() && !b[k].is_ascii_whitespace() && b[k] != b'>' {
                        k += 1;
                    }
                    value = decode_entities(&src[vs..k]);
                    j = k;
                }
            }
        }
        attrs.push((key, value));
    }
    (name, attrs, self_closing, j)
}

/// Named references we understand. Names without a trailing ';' are only
/// accepted for the first few (the legacy ones browsers also accept).
const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("nbsp", "\u{a0}"),
    ("copy", "©"),
    ("reg", "®"),
    ("apos", "'"),
    ("trade", "™"),
    ("hellip", "…"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("sbquo", "‚"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("bdquo", "„"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("bull", "•"),
    ("middot", "·"),
    ("deg", "°"),
    ("times", "×"),
    ("divide", "÷"),
    ("plusmn", "±"),
    ("minus", "−"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("sect", "§"),
    ("para", "¶"),
    ("shy", ""),
    ("zwj", ""),
    ("zwnj", ""),
    ("ensp", " "),
    ("emsp", " "),
    ("thinsp", " "),
    ("iexcl", "¡"),
    ("iquest", "¿"),
    ("frac12", "½"),
    ("frac14", "¼"),
    ("frac34", "¾"),
    ("sup2", "²"),
    ("sup3", "³"),
    ("micro", "µ"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
    ("aacute", "á"),
    ("agrave", "à"),
    ("acirc", "â"),
    ("auml", "ä"),
    ("aring", "å"),
    ("atilde", "ã"),
    ("ccedil", "ç"),
    ("eacute", "é"),
    ("egrave", "è"),
    ("ecirc", "ê"),
    ("euml", "ë"),
    ("iacute", "í"),
    ("igrave", "ì"),
    ("icirc", "î"),
    ("iuml", "ï"),
    ("ntilde", "ñ"),
    ("oacute", "ó"),
    ("ograve", "ò"),
    ("ocirc", "ô"),
    ("ouml", "ö"),
    ("otilde", "õ"),
    ("oslash", "ø"),
    ("uacute", "ú"),
    ("ugrave", "ù"),
    ("ucirc", "û"),
    ("uuml", "ü"),
    ("yacute", "ý"),
    ("szlig", "ß"),
    ("Auml", "Ä"),
    ("Ouml", "Ö"),
    ("Uuml", "Ü"),
    ("Eacute", "É"),
    ("Aring", "Å"),
    ("Oslash", "Ø"),
    ("AElig", "Æ"),
    ("aelig", "æ"),
];
const LEGACY: usize = 5;

/// Windows-1252 code points that numeric references in 0x80..=0x9F mean
/// in practice (the HTML spec remaps them the same way).
fn cp1252(n: u32) -> Option<char> {
    Some(match n {
        0x80 => '€',
        0x85 => '…',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x99 => '™',
        _ => return None,
    })
}

/// Decode `&name;`, `&#NNN;` and `&#xHH;` references. Unknown or
/// malformed references are left as they are.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if b[i] != b'&' {
            i += 1;
            continue;
        }
        if let Some((text, len)) = reference_at(s, i) {
            out.push_str(&s[copied..i]);
            out.push_str(&text);
            i += len;
            copied = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// Decode the reference starting at `s[i] == '&'`; returns the text and the
/// number of bytes consumed.
fn reference_at(s: &str, i: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    let mut j = i + 1;
    if b.get(j) == Some(&b'#') {
        j += 1;
        let hex = matches!(b.get(j), Some(b'x' | b'X'));
        if hex {
            j += 1;
        }
        let digits_start = j;
        while j < b.len() && (if hex { b[j].is_ascii_hexdigit() } else { b[j].is_ascii_digit() }) {
            j += 1;
        }
        if j == digits_start || j - digits_start > 8 {
            return None;
        }
        let n = u32::from_str_radix(&s[digits_start..j], if hex { 16 } else { 10 }).ok()?;
        if b.get(j) == Some(&b';') {
            j += 1;
        }
        let ch = cp1252(n).or_else(|| char::from_u32(n).filter(|&c| c != '\0')).unwrap_or('\u{fffd}');
        let mut t = String::new();
        t.push(ch);
        return Some((t, j - i));
    }
    while j < b.len() && j - i <= 32 && b[j].is_ascii_alphanumeric() {
        j += 1;
    }
    let name = &s[i + 1..j];
    let pos = ENTITIES.iter().position(|(n, _)| *n == name)?;
    if b.get(j) == Some(&b';') {
        Some((ENTITIES[pos].1.to_string(), j + 1 - i))
    } else if pos < LEGACY {
        Some((ENTITIES[pos].1.to_string(), j - i))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn start(name: &str, attrs: &[(&str, &str)]) -> Token {
        Token::Start {
            name: name.into(),
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            self_closing: false,
        }
    }

    fn text(s: &str) -> Token {
        Token::Text(s.into())
    }

    #[test]
    fn tags_text_and_attributes() {
        let t = tokenize("<P CLASS=x>Hi <a href='/a?b=1&amp;c=2' data-x = \"y z\">link</A></p>");
        assert_eq!(
            t,
            vec![
                start("p", &[("class", "x")]),
                text("Hi "),
                start("a", &[("href", "/a?b=1&c=2"), ("data-x", "y z")]),
                text("link"),
                Token::End("a".into()),
                Token::End("p".into()),
            ]
        );
    }

    #[test]
    fn self_closing_and_boolean_attributes() {
        let t = tokenize("<br/><input disabled type=text /><img alt=\"a > b\">");
        assert_eq!(t[0], Token::Start { name: "br".into(), attrs: vec![], self_closing: true });
        assert_eq!(
            t[1],
            Token::Start {
                name: "input".into(),
                attrs: vec![("disabled".into(), "".into()), ("type".into(), "text".into())],
                self_closing: true,
            }
        );
        assert_eq!(t[2], start("img", &[("alt", "a > b")]));
    }

    #[test]
    fn comments_doctype_and_raw_text() {
        let t = tokenize(
            "<!DOCTYPE html><!-- <p>hidden</p> --><head><title>A &amp; B</title>\
             <script>if (a<b) { x = '</p>'; }</SCRIPT><style>p{}</style></head>x",
        );
        assert_eq!(
            t,
            vec![
                start("head", &[]),
                start("title", &[]),
                text("A & B"),
                Token::End("title".into()),
                start("script", &[]),
                Token::End("script".into()),
                start("style", &[]),
                Token::End("style".into()),
                Token::End("head".into()),
                text("x"),
            ]
        );
    }

    #[test]
    fn tolerates_garbage() {
        assert_eq!(tokenize("a < b > c"), vec![text("a < b > c")]);
        assert_eq!(tokenize("x<!-- never closed"), vec![text("x")]);
        let t = tokenize("<a href=\"unterminated>text");
        assert!(matches!(&t[0], Token::Start { name, .. } if name == "a"));
        assert_eq!(tokenize("</>"), vec![text("</>")]);
    }

    #[test]
    fn entities() {
        assert_eq!(decode_entities("&amp;&lt;&gt;&quot;&apos;"), "&<>\"'");
        assert_eq!(decode_entities("&#65;&#x42;&#X63;"), "ABc");
        assert_eq!(decode_entities("a&nbsp;b"), "a\u{a0}b");
        assert_eq!(decode_entities("&copy; 2024 &mdash; &eacute;"), "© 2024 — é");
        // Legacy names work without ';', others do not.
        assert_eq!(decode_entities("&amp &lt3 &copy x"), "& &lt3 &copy x");
        assert_eq!(decode_entities("&ampx"), "&ampx");
        // Unknown and malformed references stay literal.
        assert_eq!(decode_entities("&bogus; & &#; &#xZZ;"), "&bogus; & &#; &#xZZ;");
        // Out-of-range / NUL become U+FFFD; cp1252 range is remapped.
        assert_eq!(decode_entities("&#0;&#x110000;&#150;"), "\u{fffd}\u{fffd}–");
        assert_eq!(decode_entities("&#128512;"), "😀");
    }
}
