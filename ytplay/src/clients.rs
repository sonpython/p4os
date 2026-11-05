//! The InnerTube client table: which app YouTube should think is asking.
//!
//! YouTube changes which clients get direct stream URLs every few months,
//! so the table is data, not code. It is read from
//! [`CLIENTS_PATH`] on the SD card when present, otherwise the built-in
//! [`DEFAULT_CLIENTS`] is used. Clients are tried in order.

use crate::json::{self, Kind};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub const CLIENTS_PATH: &str = "/sd/p4os/youtube-clients.json";

/// Built-in table (yt-dlp 2026.10.26 `INNERTUBE_CLIENTS["visionos"]`, the
/// only client that returned direct, unthrottled URLs without a JS engine
/// or PO token when this was written).
pub const DEFAULT_CLIENTS: &str = r#"{
  "clients": [
    {
      "name": "VISIONOS",
      "id": 101,
      "version": "1.02",
      "userAgent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
      "host": "www.youtube.com",
      "context": {
        "deviceMake": "Apple",
        "deviceModel": "RealityDevice17,1",
        "osName": "visionOS",
        "osVersion": "26.5.23O471"
      }
    }
  ]
}"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Client {
    /// `clientName` in the request context, e.g. "VISIONOS".
    pub name: String,
    /// Numeric client id for the `X-YouTube-Client-Name` header.
    pub id: u32,
    pub version: String,
    pub user_agent: String,
    pub host: String,
    /// Extra `context.client` members, as raw JSON `"key": value` pairs.
    pub context: Vec<(String, String)>,
}

fn token_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| (0x20..0x7F).contains(&b))
}

/// Parse a client table. Every entry is validated so a typo in the SD file
/// gives a clear message instead of a confusing failure later.
pub fn parse(text: &str) -> Result<Vec<Client>, String> {
    let root = json::parse(text)?;
    let list = root.get("clients").filter(|v| v.kind() == Kind::Array).ok_or("missing \"clients\" array")?;
    let mut out = Vec::new();
    for (i, c) in list.items().enumerate() {
        let field = |k: &str| -> Result<String, String> {
            let v = c.str_at(&[k]).ok_or_else(|| format!("client {}: missing string \"{k}\"", i + 1))?;
            if !token_ok(&v) {
                return Err(format!("client {}: bad \"{k}\"", i + 1));
            }
            Ok(v.into_owned())
        };
        let id = c.u64_at(&["id"]).filter(|&n| n > 0 && n < 10_000).ok_or_else(|| format!("client {}: missing number \"id\"", i + 1))?;
        let host = match c.get("host") {
            None => "www.youtube.com".to_string(),
            Some(_) => field("host")?,
        };
        if !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
            return Err(format!("client {}: bad \"host\"", i + 1));
        }
        let context = c
            .get("context")
            .map(|ctx| ctx.members().map(|(k, v)| (k.into_owned(), v.raw().to_string())).collect())
            .unwrap_or_default();
        out.push(Client {
            name: field("name")?,
            id: id as u32,
            version: field("version")?,
            user_agent: field("userAgent")?,
            host,
            context,
        });
    }
    if out.is_empty() {
        return Err("the \"clients\" list is empty".into());
    }
    Ok(out)
}

pub fn defaults() -> Vec<Client> {
    parse(DEFAULT_CLIENTS).expect("built-in client table is valid")
}

impl Client {
    pub fn player_url(&self) -> String {
        format!("https://{}/youtubei/v1/player?prettyPrint=false", self.host)
    }

    /// JSON body of a `/player` request.
    pub fn player_body(&self, video_id: &str, visitor_data: Option<&str>) -> String {
        let mut client = format!("\"clientName\":{},\"clientVersion\":{}", json::quote(&self.name), json::quote(&self.version));
        let has = |k: &str| self.context.iter().any(|(ck, _)| ck == k);
        for (k, v) in &self.context {
            client.push_str(&format!(",{}:{}", json::quote(k), v));
        }
        if !has("hl") {
            client.push_str(",\"hl\":\"en\"");
        }
        if !has("userAgent") {
            client.push_str(&format!(",\"userAgent\":{}", json::quote(&self.user_agent)));
        }
        if let Some(vd) = visitor_data {
            client.push_str(&format!(",\"visitorData\":{}", json::quote(vd)));
        }
        format!(
            "{{\"context\":{{\"client\":{{{client}}}}},\"videoId\":{},\"contentCheckOk\":true,\"racyCheckOk\":true}}",
            json::quote(video_id)
        )
    }

    /// Headers for a `/player` request (besides Content-Type).
    pub fn headers(&self, visitor_data: Option<&str>) -> Vec<(String, String)> {
        let mut h = alloc::vec![
            ("User-Agent".to_string(), self.user_agent.clone()),
            ("X-YouTube-Client-Name".to_string(), self.id.to_string()),
            ("X-YouTube-Client-Version".to_string(), self.version.clone()),
            ("Origin".to_string(), "https://www.youtube.com".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        if let Some(vd) = visitor_data {
            h.push(("X-Goog-Visitor-Id".to_string(), vd.to_string()));
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_parses() {
        let c = defaults();
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].name.as_str(), c[0].id, c[0].version.as_str()), ("VISIONOS", 101, "1.02"));
        assert_eq!(c[0].context.len(), 4);
    }

    #[test]
    fn builds_a_valid_player_body() {
        let c = &defaults()[0];
        let body = c.player_body("dQw4w9WgXcQ", Some("Cgt4\"x"));
        let v = json::parse(&body).unwrap();
        assert_eq!(v.str_at(&["videoId"]).as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(v.str_at(&["context", "client", "clientName"]).as_deref(), Some("VISIONOS"));
        assert_eq!(v.str_at(&["context", "client", "deviceModel"]).as_deref(), Some("RealityDevice17,1"));
        assert_eq!(v.str_at(&["context", "client", "visitorData"]).as_deref(), Some("Cgt4\"x"));
        assert_eq!(v.str_at(&["context", "client", "hl"]).as_deref(), Some("en"));
        let h = c.headers(Some("vd"));
        assert!(h.iter().any(|(k, v)| k == "X-YouTube-Client-Name" && v == "101"));
        assert!(h.iter().any(|(k, v)| k == "X-Goog-Visitor-Id" && v == "vd"));
        assert!(!c.headers(None).iter().any(|(k, _)| k == "X-Goog-Visitor-Id"));
    }

    #[test]
    fn rejects_bad_tables_with_a_reason() {
        assert!(parse("{}").unwrap_err().contains("clients"));
        assert!(parse(r#"{"clients": []}"#).unwrap_err().contains("empty"));
        let missing = r#"{"clients": [{"name": "X", "id": 1, "version": "1"}]}"#;
        assert!(parse(missing).unwrap_err().contains("userAgent"));
        let bad_host = r#"{"clients": [{"name": "X", "id": 1, "version": "1", "userAgent": "u", "host": "a/b"}]}"#;
        assert!(parse(bad_host).unwrap_err().contains("host"));
        let two = r#"{"clients": [{"name": "A", "id": 1, "version": "1", "userAgent": "u"},
                                 {"name": "B", "id": 2, "version": "2", "userAgent": "v", "context": {"osName": "x"}}]}"#;
        let c = parse(two).unwrap();
        assert_eq!(c[1].context, alloc::vec![("osName".to_string(), "\"x\"".to_string())]);
        assert_eq!(c[0].host, "www.youtube.com");
    }
}
