//! URLs, HTTP/1.0 requests (GET, or any method with headers and a body),
//! responses and redirects.

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
        Request::get().encode(self)
    }

    /// Value of the Host header.
    fn host_header(&self) -> String {
        if self.default_port() { self.host.clone() } else { format!("{}:{}", self.host, self.port) }
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

/// Everything about a request except its URL: method, extra headers and
/// body. Headers the client always sends (Host, User-Agent,
/// Accept-Encoding, Connection, Content-Length) must not be repeated here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Headers the client writes itself.
const RESERVED_HEADERS: [&str; 5] = ["host", "user-agent", "accept-encoding", "connection", "content-length"];

/// Largest body accepted, so a request always fits the socket send buffer.
pub const MAX_BODY: usize = 2048;

impl Request {
    pub fn get() -> Self {
        Self { method: "GET".into(), headers: Vec::new(), body: Vec::new() }
    }

    /// Reject anything that could break the request framing: methods that
    /// are not uppercase tokens, header names or values containing control
    /// characters (CR/LF injection), reserved headers and oversized bodies.
    pub fn validate(&self) -> Result<(), String> {
        if self.method.is_empty() || !self.method.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(format!("bad HTTP method {:?}", self.method));
        }
        for (name, value) in &self.headers {
            let name_ok = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !name_ok || RESERVED_HEADERS.iter().any(|r| name.eq_ignore_ascii_case(r)) {
                return Err(format!("bad or reserved HTTP header {name:?}"));
            }
            if value.bytes().any(|b| b.is_ascii_control()) {
                return Err(format!("control character in HTTP header {name:?}"));
            }
        }
        if self.body.len() > MAX_BODY {
            return Err(format!("request body larger than {MAX_BODY} bytes"));
        }
        Ok(())
    }

    /// The request bytes for `url`. Call [`Request::validate`] first.
    pub fn encode(&self, url: &Url) -> Vec<u8> {
        let mut head = format!("{} {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: p4os/0.1\r\n", self.method, url.path, url.host_header());
        if !self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("accept")) {
            head.push_str("Accept: text/html,*/*\r\n");
        }
        for (k, v) in &self.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("Accept-Encoding: identity\r\nConnection: close\r\n");
        if !self.body.is_empty() || self.method != "GET" {
            head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }

    /// Adjust the request for following a redirect with `status` from
    /// `from` to `to`: 307 and 308 repeat it, the others turn it into a
    /// body-less GET. Credentials never follow a redirect to another origin.
    pub fn follow_redirect(&mut self, status: u16, from: &Url, to: &Url) {
        if from.origin() != to.origin() {
            self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
        }
        if !matches!(status, 307 | 308) {
            self.method = "GET".into();
            self.body.clear();
            self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-type"));
        }
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
    fn plain_get_is_unchanged() {
        assert_eq!(
            String::from_utf8(url("https://h.com/x?q").request()).unwrap(),
            "GET /x?q HTTP/1.0\r\nHost: h.com\r\nUser-Agent: p4os/0.1\r\nAccept: text/html,*/*\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn builds_post_with_headers_and_body() {
        let req = Request {
            method: "POST".into(),
            headers: alloc::vec![
                ("Content-Type".into(), "application/x-www-form-urlencoded".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: b"a=1&b=2".to_vec(),
        };
        assert!(req.validate().is_ok());
        let raw = String::from_utf8(req.encode(&url("https://h.com/token"))).unwrap();
        assert!(raw.starts_with("POST /token HTTP/1.0\r\nHost: h.com\r\n"));
        assert!(raw.contains("Content-Type: application/x-www-form-urlencoded\r\n"));
        assert!(raw.contains("Accept: application/json\r\n"));
        assert!(!raw.contains("text/html"));
        assert!(raw.ends_with("Content-Length: 7\r\n\r\na=1&b=2"));
        // A body-less POST still says how long its body is.
        let empty = Request { method: "POST".into(), headers: Vec::new(), body: Vec::new() };
        assert!(String::from_utf8(empty.encode(&url("http://h.com/"))).unwrap().contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn rejects_unsafe_requests() {
        let with = |k: &str, v: &str| Request { method: "GET".into(), headers: alloc::vec![(k.into(), v.into())], body: Vec::new() };
        assert!(with("Authorization", "Bearer abc").validate().is_ok());
        assert!(with("X-Evil", "a\r\nHost: other").validate().is_err());
        assert!(with("Bad Name", "v").validate().is_err());
        assert!(with("host", "other").validate().is_err());
        assert!(with("Content-Length", "1").validate().is_err());
        let mut r = Request::get();
        r.method = "get".into();
        assert!(r.validate().is_err());
        r.method = "POST\r\n".into();
        assert!(r.validate().is_err());
        let big = Request { method: "POST".into(), headers: Vec::new(), body: alloc::vec![0; MAX_BODY + 1] };
        assert!(big.validate().is_err());
    }

    #[test]
    fn redirects_downgrade_posts() {
        let post = Request {
            method: "POST".into(),
            headers: alloc::vec![("Content-Type".into(), "text/plain".into()), ("Authorization".into(), "x".into())],
            body: b"x".to_vec(),
        };
        let (a, b) = (url("https://h.com/a"), url("https://other.com/b"));
        let mut r = post.clone();
        r.follow_redirect(307, &a, &url("https://h.com/c"));
        assert_eq!(r, post);
        r.follow_redirect(303, &a, &a);
        assert_eq!((r.method.as_str(), r.body.len(), r.headers.len()), ("GET", 0, 1));
        r.follow_redirect(302, &a, &b);
        assert!(r.headers.is_empty(), "authorization must not leak to another origin");
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
