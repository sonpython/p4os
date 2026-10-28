//! URLs, HTTP/1.0 GET requests, responses and redirects.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Redirects followed before a fetch gives up and returns the 3xx itself.
pub const MAX_REDIRECTS: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with '/'; never contains a fragment.
    pub path: String,
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let url = url.trim();
        let (https, rest) = if let Some(r) = strip_prefix_ci(url, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(url, "http://") {
            (false, r)
        } else {
            return Err(format!("unsupported address {url:?}: only http:// and https:// URLs can be fetched"));
        };
        let rest = rest.split('#').next().unwrap_or("");
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(split);
        let path = match tail.chars().next() {
            None => "/".to_string(),
            Some('?') => format!("/{tail}"),
            Some(_) => tail.to_string(),
        };
        // Drop "user:pass@"; credentials in URLs are not supported.
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        let default_port = if https { 443 } else { 80 };
        let (host, port) = match hostport.split_once(':') {
            Some((h, "")) => (h, default_port),
            Some((h, p)) => (h, p.parse().map_err(|_| format!("bad port in {url:?}"))?),
            None => (hostport, default_port),
        };
        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_') {
            return Err(format!("bad host name in {url:?}"));
        }
        Ok(Url { https, host: host.to_ascii_lowercase(), port, path })
    }

    fn default_port(&self) -> bool {
        self.port == if self.https { 443 } else { 80 }
    }

    /// "scheme://host[:port]".
    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        if self.default_port() {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }

    /// The full URL in canonical form.
    pub fn to_url_string(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }

    /// The bytes of an HTTP/1.0 GET for this URL.
    pub fn request(&self) -> Vec<u8> {
        let host = if self.default_port() { self.host.clone() } else { format!("{}:{}", self.host, self.port) };
        format!(
            "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: p4os/0.1\r\nAccept: text/html,*/*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
            self.path, host
        )
        .into_bytes()
    }

    /// Resolve a `Location` header value against this URL.
    pub fn resolve(&self, location: &str) -> String {
        let loc = location.trim();
        if strip_prefix_ci(loc, "http://").is_some() || strip_prefix_ci(loc, "https://").is_some() {
            return loc.to_string();
        }
        if let Some(rest) = loc.strip_prefix("//") {
            return format!("{}://{rest}", if self.https { "https" } else { "http" });
        }
        if loc.starts_with('/') {
            return format!("{}{loc}", self.origin());
        }
        let path_only = self.path.split('?').next().unwrap_or("/");
        let dir = &path_only[..path_only.rfind('/').map_or(0, |i| i + 1)];
        if loc.starts_with('?') {
            return format!("{}{path_only}{loc}", self.origin());
        }
        format!("{}{dir}{loc}", self.origin())
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308) && self.location.is_some()
    }
}

/// Parse a complete HTTP/1.x response (headers and body read to the end).
pub fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed HTTP response (no header end)")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    if !status_line.starts_with("HTTP/") {
        return Err("malformed HTTP response (no status line)".into());
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("malformed HTTP response (bad status code)")?;
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    Ok(Response {
        status,
        location: header("location"),
        content_type: header("content-type").unwrap_or_default(),
        body: raw[split + 4..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_schemes_ports_and_paths() {
        let u = url("https://Example.COM");
        assert_eq!((u.https, u.host.as_str(), u.port, u.path.as_str()), (true, "example.com", 443, "/"));
        let u = url("http://h.com:8080/a/b?q=1#frag");
        assert_eq!((u.https, u.port, u.path.as_str()), (false, 8080, "/a/b?q=1"));
        assert_eq!(url("HTTP://h.com?x=1").path, "/?x=1");
        assert_eq!(url("https://user:pw@h.com/").host, "h.com");
        assert_eq!(url("https://h.com:/x").port, 443);
        assert!(Url::parse("ftp://h.com/").is_err());
        assert!(Url::parse("https:///x").is_err());
        assert!(Url::parse("http://h.com:99999/").is_err());
    }

    #[test]
    fn builds_requests_with_host_header() {
        let req = String::from_utf8(url("https://h.com/x").request()).unwrap();
        assert!(req.starts_with("GET /x HTTP/1.0\r\nHost: h.com\r\n"));
        let req = String::from_utf8(url("http://h.com:81/").request()).unwrap();
        assert!(req.contains("Host: h.com:81\r\n"));
        assert!(req.ends_with("\r\n\r\n"));
    }

    #[test]
    fn resolves_redirects() {
        let base = url("http://h.com/a/b.html?q");
        assert_eq!(base.resolve("https://s.com/"), "https://s.com/");
        assert_eq!(base.resolve("//other.org/p"), "http://other.org/p");
        assert_eq!(base.resolve("/x"), "http://h.com/x");
        assert_eq!(base.resolve("c.html"), "http://h.com/a/c.html");
        assert_eq!(base.resolve("?p=2"), "http://h.com/a/b.html?p=2");
        assert_eq!(url("https://h.com:8443/").resolve("/y"), "https://h.com:8443/y");
        assert_eq!(url("https://h.com/").resolve("//cdn.h.com/"), "https://cdn.h.com/");
    }

    #[test]
    fn parses_responses() {
        let r = parse_response(b"HTTP/1.1 301 Moved\r\nLocation: https://h.com/\r\nContent-Type: text/html\r\n\r\nbody").unwrap();
        assert_eq!(r.status, 301);
        assert!(r.is_redirect());
        assert_eq!(r.location.as_deref(), Some("https://h.com/"));
        assert_eq!(r.content_type, "text/html");
        assert_eq!(r.body, b"body");
        let r = parse_response(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        assert!(!r.is_redirect());
        assert!(r.body.is_empty());
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"SSH-2.0\r\n\r\n").is_err());
    }
}
