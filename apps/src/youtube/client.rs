//! Sends API calls with the right credentials and quota accounting.
//!
//! Signed in, calls carry the OAuth access token; signed out, public calls
//! use the optional API key. Calls queue while an access token is being
//! refreshed, at most [`MAX_INFLIGHT`] run at once, a 401 triggers one
//! refresh and retry, and `quotaExceeded` stops further calls for the day.
//! Each call carries a caller-defined tag that comes back with its result.

use super::api::{self, Call, API_BASE};
use super::oauth::{self, Auth, Event, Token};
use super::quota::Quota;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use p4os_sdk::{DateTime, FetchId, HttpRequest, Network};

pub const MAX_INFLIGHT: usize = 3;

struct Req<T> {
    call: Call,
    tag: T,
    /// Already retried after a 401.
    retried: bool,
    /// Sent with the user's access token.
    bearer: bool,
}

pub type Done<T> = (T, Result<Vec<u8>, String>);

pub struct Client<T> {
    pub auth: Auth,
    pub quota: Quota,
    api_key: Option<String>,
    queue: VecDeque<Req<T>>,
    inflight: Vec<(FetchId, Req<T>)>,
}

impl<T> Client<T> {
    pub fn new(auth: Auth, api_key: Option<String>, quota: Quota) -> Self {
        Self { auth, quota, api_key: api_key.filter(|k| !k.is_empty()), queue: VecDeque::new(), inflight: Vec::new() }
    }

    /// Public content (search, trending) can be loaded.
    pub fn can_browse(&self) -> bool {
        self.auth.is_signed_in() || self.api_key.is_some()
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub fn request(&mut self, call: Call, tag: T) {
        self.queue.push_back(Req { call, tag, retried: false, bearer: false });
    }

    /// Calls queued or running.
    pub fn busy(&self) -> bool {
        !self.queue.is_empty() || !self.inflight.is_empty()
    }

    /// Collect finished calls and send queued ones. Returns the finished
    /// calls and any sign-in event.
    pub fn tick(&mut self, net: &mut dyn Network, now_ms: u64, now: Option<DateTime>) -> (Vec<Done<T>>, Option<Event>) {
        let mut done = Vec::new();
        let event = self.auth.tick(net, now_ms);
        if let Some(Event::Failed(msg)) = &event {
            // The token refresh failed: calls waiting for it fail too.
            if self.auth.is_signed_in() {
                done.extend(self.queue.drain(..).map(|r| (r.tag, Err(msg.clone()))));
            }
        }
        self.collect(net, now, &mut done);
        self.send(net, now_ms, now, &mut done);
        // Calls are waiting for an access token: start the refresh now
        // rather than on the next tick.
        let mut event = event;
        if !self.queue.is_empty() && self.auth.token(now_ms) == Token::Pending {
            event = event.or(self.auth.tick(net, now_ms));
        }
        (done, event)
    }

    fn collect(&mut self, net: &mut dyn Network, now: Option<DateTime>, done: &mut Vec<Done<T>>) {
        let mut i = 0;
        while i < self.inflight.len() {
            let Some(result) = oauth::take_response(net.poll(self.inflight[i].0)) else {
                i += 1;
                continue;
            };
            let (_, mut req) = self.inflight.swap_remove(i);
            match result {
                Ok((200, body)) => done.push((req.tag, Ok(body))),
                Ok((401, _)) if req.bearer && !req.retried => {
                    self.auth.invalidate_access();
                    req.retried = true;
                    self.queue.push_front(req);
                }
                Ok((status, body)) => {
                    let err = api::parse_error(status, &body);
                    let msg = if err.is_quota() {
                        self.quota.mark_exhausted(now);
                        super::quota::QuotaError::Exhausted.message()
                    } else {
                        err.describe()
                    };
                    done.push((req.tag, Err(msg)));
                }
                Err(e) => done.push((req.tag, Err(format!("Network error: {e}")))),
            }
        }
    }

    fn send(&mut self, net: &mut dyn Network, now_ms: u64, now: Option<DateTime>, done: &mut Vec<Done<T>>) {
        while self.inflight.len() < MAX_INFLIGHT {
            let Some(front) = self.queue.front() else { return };
            let (needs_user, cost) = (front.call.needs_user, front.call.cost);
            let mut url = format!("{API_BASE}/{}", front.call.path);
            let mut bearer = None;
            match self.auth.token(now_ms) {
                Token::Ready(t) => bearer = Some(format!("Bearer {t}")),
                // Wait for the refresh; order is kept.
                Token::Pending => return,
                Token::SignedOut if needs_user => {
                    let req = self.queue.pop_front().expect("front exists");
                    done.push((req.tag, Err("Sign in to see this.".into())));
                    continue;
                }
                Token::SignedOut => match &self.api_key {
                    Some(key) => url = format!("{url}&key={}", api::percent_encode(key)),
                    None => {
                        let req = self.queue.pop_front().expect("front exists");
                        done.push((req.tag, Err("Sign in, or set youtube.api_key to browse without an account.".into())));
                        continue;
                    }
                },
            }
            let mut req = self.queue.pop_front().expect("front exists");
            if let Err(e) = self.quota.charge(cost, now) {
                done.push((req.tag, Err(e.message())));
                continue;
            }
            let mut http = HttpRequest::get(&url).header("Accept", "application/json");
            req.bearer = bearer.is_some();
            if let Some(b) = bearer {
                http = http.header("Authorization", &b);
            }
            self.inflight.push((net.send(http), req));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::oauth::Credentials;
    use super::super::testing::MockNet;
    use super::*;

    const TRENDING: &str = include_str!("fixtures/trending.json");
    const QUOTA_ERROR: &str = include_str!("fixtures/error-quota.json");

    fn creds() -> Option<Credentials> {
        Some(Credentials { client_id: "id".into(), client_secret: "secret".into() })
    }

    fn signed_out(key: Option<&str>) -> Client<u32> {
        Client::new(Auth::new(creds(), None), key.map(String::from), Quota::default())
    }

    #[test]
    fn signed_out_uses_the_api_key() {
        let mut net = MockNet::default();
        let mut c = signed_out(Some("AIza-key/+"));
        c.request(api::trending("VN", None), 1);
        let (done, _) = c.tick(&mut net, 0, None);
        assert!(done.is_empty());
        let req = net.last_request();
        assert!(req.url.starts_with("https://www.googleapis.com/youtube/v3/videos?"));
        assert!(req.url.ends_with("&key=AIza-key%2F%2B"));
        assert!(!req.headers.iter().any(|(k, _)| k == "Authorization"));
        net.respond_last(200, TRENDING);
        let (done, _) = c.tick(&mut net, 10, None);
        assert_eq!(done.len(), 1);
        assert_eq!(api::parse_videos(done[0].1.as_ref().unwrap()).unwrap().items.len(), 8);
        assert_eq!(c.quota.used(), 1);
        assert!(!c.busy());
    }

    #[test]
    fn signed_out_without_key_or_for_private_calls_fails() {
        let mut net = MockNet::default();
        let mut c = signed_out(None);
        assert!(!c.can_browse());
        c.request(api::trending("VN", None), 1);
        let mut keyed = signed_out(Some("k"));
        keyed.request(api::liked(None), 2);
        let (a, _) = c.tick(&mut net, 0, None);
        let (b, _) = keyed.tick(&mut net, 0, None);
        assert!(a[0].1.as_ref().unwrap_err().contains("youtube.api_key"));
        assert!(b[0].1.as_ref().unwrap_err().contains("Sign in"));
        assert_eq!(net.sent(), 0);
        assert_eq!(keyed.quota.used(), 0);
    }

    #[test]
    fn signed_in_waits_for_a_token_and_uses_it() {
        let mut net = MockNet::default();
        let mut c: Client<u32> = Client::new(Auth::new(creds(), Some("1//r".into())), Some("k".into()), Quota::default());
        c.request(api::liked(None), 7);
        c.request(api::search("x", None), 8);
        c.tick(&mut net, 0, None);
        assert_eq!(net.sent(), 1, "only the token refresh");
        assert_eq!(net.last_request().url, oauth::TOKEN_URL);
        net.respond_last(200, r#"{"access_token":"ya29.a","expires_in":3600}"#);
        c.tick(&mut net, 5, None);
        assert_eq!(net.sent(), 3);
        for r in &net.requests[1..] {
            assert!(r.headers.contains(&("Authorization".into(), "Bearer ya29.a".into())));
            assert!(!r.url.contains("key="), "the API key is not sent when signed in");
        }
        assert_eq!(c.quota.used(), 101);
    }

    #[test]
    fn unauthorized_refreshes_once_and_retries() {
        let mut net = MockNet::default();
        let mut c: Client<u32> = Client::new(Auth::new(creds(), Some("1//r".into())), None, Quota::default());
        c.request(api::liked(None), 1);
        c.tick(&mut net, 0, None);
        net.respond_last(200, r#"{"access_token":"old","expires_in":3600}"#);
        c.tick(&mut net, 0, None);
        net.respond_last(401, r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#);
        c.tick(&mut net, 0, None);
        assert_eq!(net.last_request().url, oauth::TOKEN_URL, "a new token is requested");
        net.respond_last(200, r#"{"access_token":"new","expires_in":3600}"#);
        c.tick(&mut net, 0, None);
        assert!(net.last_request().headers.contains(&("Authorization".into(), "Bearer new".into())));
        // A second 401 is reported instead of looping.
        net.respond_last(401, r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#);
        let (done, _) = c.tick(&mut net, 0, None);
        assert_eq!(done.len(), 1);
        assert!(done[0].1.as_ref().unwrap_err().contains("Invalid Credentials"));
    }

    #[test]
    fn quota_exceeded_stops_calls() {
        let mut net = MockNet::default();
        let mut c = signed_out(Some("k"));
        c.request(api::trending("VN", None), 1);
        c.tick(&mut net, 0, None);
        net.respond_last(403, QUOTA_ERROR);
        let (done, _) = c.tick(&mut net, 0, None);
        assert!(done[0].1.as_ref().unwrap_err().contains("quota"));
        c.request(api::trending("VN", None), 2);
        let (done, _) = c.tick(&mut net, 0, None);
        assert!(done[0].1.as_ref().unwrap_err().contains("quota"));
        assert_eq!(net.sent(), 1, "nothing more is sent");
    }

    #[test]
    fn local_budget_refuses_expensive_calls() {
        let mut net = MockNet::default();
        let mut c: Client<u32> = Client::new(Auth::new(None, None), Some("k".into()), Quota::new(150));
        c.request(api::search("a", None), 1);
        c.request(api::search("b", None), 2);
        let (done, _) = c.tick(&mut net, 0, None);
        assert_eq!(net.sent(), 1);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, 2);
        assert!(done[0].1.as_ref().unwrap_err().contains("100"));
    }

    #[test]
    fn limits_concurrency() {
        let mut net = MockNet::default();
        let mut c = signed_out(Some("k"));
        for i in 0..5 {
            c.request(api::stats(&["a"]), i);
        }
        c.tick(&mut net, 0, None);
        assert_eq!(net.sent(), MAX_INFLIGHT);
        net.respond(1, 200, "{}");
        c.tick(&mut net, 0, None);
        assert_eq!(net.sent(), MAX_INFLIGHT + 1);
    }

    #[test]
    fn network_failures_are_reported() {
        let mut net = MockNet::default();
        let mut c = signed_out(Some("k"));
        c.request(api::trending("VN", None), 1);
        c.tick(&mut net, 0, None);
        net.fail_last("timed out connecting (www.googleapis.com)");
        let (done, _) = c.tick(&mut net, 0, None);
        assert_eq!(done[0].1.as_ref().unwrap_err(), "Network error: timed out connecting (www.googleapis.com)");
    }
}
