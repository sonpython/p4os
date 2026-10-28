//! Google sign-in with the OAuth 2.0 device authorization grant ("TVs and
//! Limited Input devices"): the board shows a short code, the user enters
//! it on a phone at the verification URL, and the board polls the token
//! endpoint until the user approves.
//!
//! [`Auth`] owns its in-flight request and is driven by [`Auth::tick`]; it
//! never logs. Tokens and the client secret only leave this module inside
//! request bodies or the Authorization header, and `Debug` output redacts
//! them.

use super::api::percent_encode;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use p4os_sdk::{FetchId, FetchState, HttpRequest, Network};

use super::json;

pub const DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
pub const SCOPE: &str = "https://www.googleapis.com/auth/youtube.readonly";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Default polling interval when the server does not send one (RFC 8628).
const DEFAULT_INTERVAL_MS: u64 = 5_000;
/// Added to the interval on every `slow_down`.
const SLOW_DOWN_STEP_MS: u64 = 5_000;
/// Treat access tokens as expired this long before they really do.
const EXPIRY_MARGIN_MS: u64 = 60_000;
/// Wait before retrying a refresh that failed for network reasons.
const REFRESH_RETRY_MS: u64 = 15_000;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("client_id", &self.client_id).field("client_secret", &"[redacted]").finish()
    }
}

/// What the user needs to see while signing in.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_url: String,
    pub expires_at_ms: u64,
    device_code: String,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_url", &self.verification_url)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    SignedOut,
    /// Asking Google for a device code.
    RequestingCode,
    /// Showing the code; polling the token endpoint.
    WaitingForUser(DeviceCode),
    SignedIn,
}

/// Something the app should react to.
#[derive(Clone, PartialEq, Eq)]
pub enum Event {
    /// A device code arrived; show it.
    CodeReady,
    /// Sign-in finished (or Google issued a new refresh token): store it.
    SignedIn { refresh_token: String },
    /// The user declined on the phone.
    Denied,
    /// The code expired before the user approved.
    Expired,
    /// The stored refresh token no longer works (revoked, or expired as
    /// tokens of unpublished "Testing" apps do after 7 days): delete it.
    SessionEnded,
    /// A recoverable or configuration problem to show to the user.
    Failed(String),
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::CodeReady => f.write_str("CodeReady"),
            Event::SignedIn { .. } => f.write_str("SignedIn { refresh_token: [redacted] }"),
            Event::Denied => f.write_str("Denied"),
            Event::Expired => f.write_str("Expired"),
            Event::SessionEnded => f.write_str("SessionEnded"),
            Event::Failed(m) => write!(f, "Failed({m:?})"),
        }
    }
}

/// Whether an API call can carry a bearer token now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token<'a> {
    Ready(&'a str),
    /// Signed in; a fresh access token is on its way.
    Pending,
    SignedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    DeviceCode,
    Poll,
    Refresh,
}

pub struct Auth {
    creds: Option<Credentials>,
    state: State,
    refresh_token: Option<String>,
    /// Access token and the uptime at which to stop using it.
    access: Option<(String, u64)>,
    inflight: Option<(FetchId, Op)>,
    /// Requests whose answers no longer matter but must still be collected
    /// so the OS can release them (cancelled polls, revocations).
    orphans: Vec<FetchId>,
    interval_ms: u64,
    next_poll_ms: u64,
    want_access: bool,
    refresh_not_before: u64,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("state", &self.state)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("has_access_token", &self.access.is_some())
            .finish_non_exhaustive()
    }
}

/// The body of a finished request, or why there is none.
pub fn take_response(state: FetchState) -> Option<Result<(u16, Vec<u8>), String>> {
    match state {
        FetchState::Pending => None,
        FetchState::Done { status, body, .. } => Some(Ok((status, body))),
        FetchState::Failed(e) => Some(Err(e)),
    }
}

/// `error` and `error_description` of an OAuth error response.
fn oauth_error(body: &[u8]) -> (String, String) {
    let text = core::str::from_utf8(body).unwrap_or("");
    let Ok(doc) = json::parse(text) else { return (String::new(), String::new()) };
    let field = |k: &str| doc.get(k).and_then(|v| v.string()).unwrap_or_default();
    (field("error"), field("error_description"))
}

fn form(pairs: &[(&str, &str)]) -> String {
    let parts: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={}", percent_encode(v))).collect();
    parts.join("&")
}

fn json_post(url: &str, body: &str) -> HttpRequest {
    HttpRequest::post_form(url, body).header("Accept", "application/json")
}

impl Auth {
    pub fn new(creds: Option<Credentials>, refresh_token: Option<String>) -> Self {
        let refresh_token = refresh_token.filter(|t| !t.is_empty());
        let state = if refresh_token.is_some() && creds.is_some() { State::SignedIn } else { State::SignedOut };
        Self {
            creds,
            state,
            refresh_token,
            access: None,
            inflight: None,
            orphans: Vec::new(),
            interval_ms: DEFAULT_INTERVAL_MS,
            next_poll_ms: 0,
            want_access: false,
            refresh_not_before: 0,
        }
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn has_credentials(&self) -> bool {
        self.creds.is_some()
    }

    pub fn is_signed_in(&self) -> bool {
        self.state == State::SignedIn
    }

    /// Current polling interval (grows with `slow_down`).
    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    /// Ask Google for a device code.
    pub fn begin_sign_in(&mut self, net: &mut dyn Network) -> Result<(), String> {
        let creds = self.creds.as_ref().ok_or("No OAuth client configured: set youtube.client_id and youtube.client_secret.")?;
        let body = form(&[("client_id", &creds.client_id), ("scope", SCOPE)]);
        self.drop_inflight();
        let id = net.send(json_post(DEVICE_CODE_URL, &body));
        self.inflight = Some((id, Op::DeviceCode));
        self.interval_ms = DEFAULT_INTERVAL_MS;
        self.state = State::RequestingCode;
        Ok(())
    }

    /// Abandon a sign-in in progress.
    pub fn cancel(&mut self) {
        if matches!(self.state, State::RequestingCode | State::WaitingForUser(_)) {
            self.drop_inflight();
            self.state = State::SignedOut;
        }
    }

    /// Forget all tokens and ask Google to revoke the grant. The caller
    /// deletes the stored refresh token.
    pub fn sign_out(&mut self, net: &mut dyn Network) {
        self.drop_inflight();
        if let Some(token) = self.refresh_token.take() {
            let id = net.send(json_post(REVOKE_URL, &form(&[("token", &token)])));
            self.orphans.push(id);
        }
        self.access = None;
        self.want_access = false;
        self.state = State::SignedOut;
    }

    /// A bearer token for an API call, starting a refresh when needed.
    pub fn token(&mut self, now_ms: u64) -> Token<'_> {
        if self.state != State::SignedIn {
            return Token::SignedOut;
        }
        match &self.access {
            Some((t, until)) if now_ms < *until => Token::Ready(t),
            _ => {
                self.want_access = true;
                Token::Pending
            }
        }
    }

    /// The API rejected the access token (HTTP 401); get a new one.
    pub fn invalidate_access(&mut self) {
        self.access = None;
    }

    fn drop_inflight(&mut self) {
        if let Some((id, _)) = self.inflight.take() {
            self.orphans.push(id);
        }
    }

    /// Advance the flow: collect answers and send whatever is due.
    pub fn tick(&mut self, net: &mut dyn Network, now_ms: u64) -> Option<Event> {
        self.orphans.retain(|id| matches!(net.poll(*id), FetchState::Pending));
        if let Some((id, op)) = self.inflight {
            let result = take_response(net.poll(id))?;
            self.inflight = None;
            let event = match op {
                Op::DeviceCode => self.on_device_code(result, now_ms),
                Op::Poll => self.on_poll(result, now_ms),
                Op::Refresh => self.on_refresh(result, now_ms),
            };
            if event.is_some() {
                return event;
            }
        }
        self.send_due(net, now_ms)
    }

    fn send_due(&mut self, net: &mut dyn Network, now_ms: u64) -> Option<Event> {
        if self.inflight.is_some() {
            return None;
        }
        let creds = self.creds.as_ref()?;
        match &self.state {
            State::WaitingForUser(code) => {
                if now_ms >= code.expires_at_ms {
                    self.state = State::SignedOut;
                    return Some(Event::Expired);
                }
                if now_ms >= self.next_poll_ms {
                    let body = form(&[
                        ("client_id", &creds.client_id),
                        ("client_secret", &creds.client_secret),
                        ("device_code", &code.device_code),
                        ("grant_type", DEVICE_GRANT),
                    ]);
                    self.inflight = Some((net.send(json_post(TOKEN_URL, &body)), Op::Poll));
                }
            }
            State::SignedIn if self.want_access && now_ms >= self.refresh_not_before => {
                let token = self.refresh_token.as_deref()?;
                let body = form(&[
                    ("client_id", &creds.client_id),
                    ("client_secret", &creds.client_secret),
                    ("refresh_token", token),
                    ("grant_type", "refresh_token"),
                ]);
                self.inflight = Some((net.send(json_post(TOKEN_URL, &body)), Op::Refresh));
            }
            _ => {}
        }
        None
    }

    fn on_device_code(&mut self, result: Result<(u16, Vec<u8>), String>, now_ms: u64) -> Option<Event> {
        if self.state != State::RequestingCode {
            return None;
        }
        self.state = State::SignedOut;
        let (status, body) = match result {
            Ok(r) => r,
            Err(e) => return Some(Event::Failed(format!("Could not reach Google: {e}"))),
        };
        let text = core::str::from_utf8(&body).unwrap_or("");
        let doc = json::parse(text).ok().filter(|_| status == 200);
        let Some(doc) = doc else {
            let (error, description) = oauth_error(&body);
            return Some(Event::Failed(describe_client_error(status, &error, &description)));
        };
        let field = |k: &str| doc.get(k).and_then(|v| v.string());
        let (Some(device_code), Some(user_code)) = (field("device_code"), field("user_code")) else {
            return Some(Event::Failed("Google sent an incomplete device code.".into()));
        };
        let verification_url = field("verification_url")
            .or_else(|| field("verification_uri"))
            .unwrap_or_else(|| "https://www.google.com/device".into());
        let expires_in = doc.get("expires_in").and_then(|v| v.u64()).unwrap_or(1800);
        self.interval_ms = doc.get("interval").and_then(|v| v.u64()).map_or(DEFAULT_INTERVAL_MS, |s| s.max(1) * 1000);
        self.next_poll_ms = now_ms + self.interval_ms;
        self.state = State::WaitingForUser(DeviceCode {
            user_code,
            verification_url,
            expires_at_ms: now_ms + expires_in * 1000,
            device_code,
        });
        Some(Event::CodeReady)
    }

    /// Store a token response; returns the refresh token it carried.
    fn accept_tokens(&mut self, body: &[u8], now_ms: u64) -> Result<Option<String>, ()> {
        let text = core::str::from_utf8(body).map_err(|_| ())?;
        let doc = json::parse(text).map_err(|_| ())?;
        let access = doc.get("access_token").and_then(|v| v.string()).ok_or(())?;
        let expires_ms = doc.get("expires_in").and_then(|v| v.u64()).unwrap_or(3600) * 1000;
        self.access = Some((access, now_ms + expires_ms.saturating_sub(EXPIRY_MARGIN_MS)));
        self.want_access = false;
        Ok(doc.get("refresh_token").and_then(|v| v.string()))
    }

    fn on_poll(&mut self, result: Result<(u16, Vec<u8>), String>, now_ms: u64) -> Option<Event> {
        if !matches!(self.state, State::WaitingForUser(_)) {
            return None;
        }
        let (status, body) = match result {
            Ok(r) => r,
            // Network trouble: keep the code and try again next interval.
            Err(_) => {
                self.next_poll_ms = now_ms + self.interval_ms;
                return None;
            }
        };
        if status == 200 {
            return match self.accept_tokens(&body, now_ms) {
                Ok(Some(refresh_token)) => {
                    self.refresh_token = Some(refresh_token.clone());
                    self.state = State::SignedIn;
                    Some(Event::SignedIn { refresh_token })
                }
                _ => {
                    self.access = None;
                    self.state = State::SignedOut;
                    Some(Event::Failed("Google approved the sign-in but sent no refresh token.".into()))
                }
            };
        }
        let (error, description) = oauth_error(&body);
        match error.as_str() {
            "authorization_pending" => {
                self.next_poll_ms = now_ms + self.interval_ms;
                None
            }
            "slow_down" => {
                self.interval_ms += SLOW_DOWN_STEP_MS;
                self.next_poll_ms = now_ms + self.interval_ms;
                None
            }
            "access_denied" => {
                self.state = State::SignedOut;
                Some(Event::Denied)
            }
            "expired_token" => {
                self.state = State::SignedOut;
                Some(Event::Expired)
            }
            _ if status >= 500 => {
                self.next_poll_ms = now_ms + self.interval_ms;
                None
            }
            _ => {
                self.state = State::SignedOut;
                Some(Event::Failed(describe_client_error(status, &error, &description)))
            }
        }
    }

    fn on_refresh(&mut self, result: Result<(u16, Vec<u8>), String>, now_ms: u64) -> Option<Event> {
        if self.state != State::SignedIn {
            return None;
        }
        let retry_later = |auth: &mut Self, msg: String| {
            auth.refresh_not_before = now_ms + REFRESH_RETRY_MS;
            Some(Event::Failed(msg))
        };
        let (status, body) = match result {
            Ok(r) => r,
            Err(e) => return retry_later(self, format!("Could not reach Google: {e}")),
        };
        if status == 200 {
            return match self.accept_tokens(&body, now_ms) {
                Ok(Some(rotated)) if Some(&rotated) != self.refresh_token.as_ref() => {
                    self.refresh_token = Some(rotated.clone());
                    Some(Event::SignedIn { refresh_token: rotated })
                }
                Ok(_) => None,
                Err(()) => retry_later(self, "Google sent an unreadable token response.".into()),
            };
        }
        let (error, description) = oauth_error(&body);
        if error == "invalid_grant" {
            self.refresh_token = None;
            self.access = None;
            self.want_access = false;
            self.state = State::SignedOut;
            return Some(Event::SessionEnded);
        }
        if status >= 500 || error.is_empty() {
            return retry_later(self, format!("Google token service error (HTTP {status})."));
        }
        // invalid_client and similar: the configuration is wrong.
        self.refresh_not_before = u64::MAX;
        Some(Event::Failed(describe_client_error(status, &error, &description)))
    }
}

/// Human-readable explanation for an OAuth error answer.
fn describe_client_error(status: u16, error: &str, description: &str) -> String {
    let hint = match error {
        "invalid_client" => "Check youtube.client_id and youtube.client_secret.",
        "unauthorized_client" => "The OAuth client must be of type \"TVs and Limited Input devices\".",
        "invalid_scope" => "Enable the YouTube Data API v3 for the Cloud project.",
        "org_internal" | "access_denied" => "Add your Google account as a test user of the app.",
        _ => "",
    };
    let mut msg = if error.is_empty() { format!("Sign-in failed (HTTP {status}).") } else { format!("Sign-in failed: {error}.") };
    if !description.is_empty() {
        msg.push(' ');
        msg.push_str(description);
    }
    if !hint.is_empty() {
        msg.push(' ');
        msg.push_str(hint);
    }
    msg.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::super::testing::MockNet;
    use super::*;

    fn creds() -> Option<Credentials> {
        Some(Credentials { client_id: "client-123.apps.googleusercontent.com".into(), client_secret: "s3cret".into() })
    }

    const CODE: &str = r#"{"device_code":"AH-1Ng-dev","user_code":"GQVQ-JKEC","verification_url":"https://www.google.com/device","expires_in":1800,"interval":5}"#;
    const TOKENS: &str = r#"{"access_token":"ya29.access-1","expires_in":3599,"refresh_token":"1//refresh-1","scope":"https://www.googleapis.com/auth/youtube.readonly","token_type":"Bearer"}"#;

    /// Sign-in started and the device code received at t=0.
    fn waiting(net: &mut MockNet) -> Auth {
        let mut auth = Auth::new(creds(), None);
        auth.begin_sign_in(net).unwrap();
        let req = net.last_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, DEVICE_CODE_URL);
        let body = String::from_utf8(req.body.clone()).unwrap();
        assert!(body.contains("client_id=client-123.apps.googleusercontent.com"));
        assert!(body.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fyoutube.readonly"));
        assert!(!body.contains("s3cret"), "the device code request needs no secret");
        net.respond_last(200, CODE);
        assert_eq!(auth.tick(net, 0), Some(Event::CodeReady));
        auth
    }

    #[test]
    fn device_code_is_shown() {
        let mut net = MockNet::default();
        let auth = waiting(&mut net);
        let State::WaitingForUser(code) = auth.state() else { panic!("{:?}", auth.state()) };
        assert_eq!(code.user_code, "GQVQ-JKEC");
        assert_eq!(code.verification_url, "https://www.google.com/device");
        assert_eq!(code.expires_at_ms, 1_800_000);
        assert!(!alloc::format!("{auth:?} {code:?}").contains("AH-1Ng-dev"), "debug output hides the device code");
    }

    #[test]
    fn polls_at_the_interval_until_approved() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        let sent = net.sent();
        assert_eq!(auth.tick(&mut net, 4_999), None);
        assert_eq!(net.sent(), sent, "no poll before the interval");
        assert_eq!(auth.tick(&mut net, 5_000), None);
        assert_eq!(net.sent(), sent + 1);
        let poll = net.last_request();
        assert_eq!(poll.url, TOKEN_URL);
        let body = String::from_utf8(poll.body.clone()).unwrap();
        assert!(body.contains("device_code=AH-1Ng-dev"));
        assert!(body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));

        // Pending: the next poll waits another interval.
        net.respond_last(428, r#"{"error":"authorization_pending","error_description":"Precondition Required"}"#);
        assert_eq!(auth.tick(&mut net, 5_100), None);
        assert_eq!(auth.tick(&mut net, 10_000), None);
        assert_eq!(net.sent(), sent + 1);
        assert_eq!(auth.tick(&mut net, 10_100), None);
        assert_eq!(net.sent(), sent + 2);

        net.respond_last(200, TOKENS);
        let event = auth.tick(&mut net, 10_200);
        assert_eq!(event, Some(Event::SignedIn { refresh_token: "1//refresh-1".into() }));
        assert!(!alloc::format!("{event:?}").contains("refresh-1"), "debug output hides tokens");
        assert!(auth.is_signed_in());
        assert_eq!(auth.token(20_000), Token::Ready("ya29.access-1"));
    }

    #[test]
    fn slow_down_increases_the_interval() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        auth.tick(&mut net, 5_000);
        net.respond_last(403, r#"{"error":"slow_down","error_description":"Forbidden"}"#);
        auth.tick(&mut net, 5_000);
        assert_eq!(auth.interval_ms(), 10_000);
        let sent = net.sent();
        auth.tick(&mut net, 14_999);
        assert_eq!(net.sent(), sent);
        auth.tick(&mut net, 15_000);
        assert_eq!(net.sent(), sent + 1);
        net.respond_last(403, r#"{"error":"slow_down"}"#);
        auth.tick(&mut net, 15_000);
        assert_eq!(auth.interval_ms(), 15_000);
    }

    #[test]
    fn expired_token_and_local_expiry() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        auth.tick(&mut net, 5_000);
        net.respond_last(400, r#"{"error":"expired_token","error_description":"Bad Request"}"#);
        assert_eq!(auth.tick(&mut net, 5_000), Some(Event::Expired));
        assert_eq!(*auth.state(), State::SignedOut);

        // The code runs out locally without a final answer from Google.
        let mut auth = waiting(&mut net);
        let sent = net.sent();
        assert_eq!(auth.tick(&mut net, 1_800_000), Some(Event::Expired));
        assert_eq!(net.sent(), sent, "no poll after expiry");
    }

    #[test]
    fn access_denied() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        auth.tick(&mut net, 5_000);
        net.respond_last(403, r#"{"error":"access_denied","error_description":"Forbidden"}"#);
        assert_eq!(auth.tick(&mut net, 5_000), Some(Event::Denied));
        assert_eq!(auth.token(5_000), Token::SignedOut);
    }

    #[test]
    fn network_errors_keep_polling() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        auth.tick(&mut net, 5_000);
        net.fail_last("timed out");
        assert_eq!(auth.tick(&mut net, 5_000), None);
        assert!(matches!(auth.state(), State::WaitingForUser(_)));
        let sent = net.sent();
        auth.tick(&mut net, 10_000);
        assert_eq!(net.sent(), sent + 1);
    }

    #[test]
    fn misconfigured_clients_are_explained() {
        let mut net = MockNet::default();
        let mut auth = Auth::new(creds(), None);
        auth.begin_sign_in(&mut net).unwrap();
        net.respond_last(401, r#"{"error":"invalid_client","error_description":"The OAuth client was not found."}"#);
        let Some(Event::Failed(msg)) = auth.tick(&mut net, 0) else { panic!() };
        assert!(msg.contains("invalid_client") && msg.contains("youtube.client_id"), "{msg}");
        assert_eq!(*auth.state(), State::SignedOut);

        let mut unconfigured = Auth::new(None, Some("1//x".into()));
        assert!(!unconfigured.is_signed_in(), "a token without a client cannot be used");
        assert!(unconfigured.begin_sign_in(&mut net).is_err());
    }

    #[test]
    fn refreshes_access_tokens() {
        let mut net = MockNet::default();
        let mut auth = Auth::new(creds(), Some("1//stored".into()));
        assert!(auth.is_signed_in());
        assert_eq!(auth.token(0), Token::Pending);
        assert_eq!(auth.tick(&mut net, 0), None);
        let req = net.last_request();
        assert_eq!(req.url, TOKEN_URL);
        let body = String::from_utf8(req.body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token") && body.contains("refresh_token=1%2F%2Fstored"));
        net.respond_last(200, r#"{"access_token":"ya29.fresh","expires_in":3600,"token_type":"Bearer"}"#);
        assert_eq!(auth.tick(&mut net, 0), None);
        assert_eq!(auth.token(1_000), Token::Ready("ya29.fresh"));
        // Used up to a minute before the real expiry, then refreshed again.
        assert_eq!(auth.token(3_540_000), Token::Pending);
        let sent = net.sent();
        auth.tick(&mut net, 3_540_000);
        assert_eq!(net.sent(), sent + 1);

        // A 401 from the API forces a refresh.
        net.respond_last(200, r#"{"access_token":"ya29.third","expires_in":3600}"#);
        auth.tick(&mut net, 3_540_000);
        auth.invalidate_access();
        assert_eq!(auth.token(3_541_000), Token::Pending);
    }

    #[test]
    fn revoked_refresh_token_ends_the_session() {
        let mut net = MockNet::default();
        let mut auth = Auth::new(creds(), Some("1//old".into()));
        auth.token(0);
        auth.tick(&mut net, 0);
        net.respond_last(400, r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#);
        assert_eq!(auth.tick(&mut net, 0), Some(Event::SessionEnded));
        assert_eq!(auth.token(0), Token::SignedOut);
    }

    #[test]
    fn refresh_failures_back_off() {
        let mut net = MockNet::default();
        let mut auth = Auth::new(creds(), Some("1//t".into()));
        auth.token(0);
        auth.tick(&mut net, 0);
        net.fail_last("DNS: failure");
        assert!(matches!(auth.tick(&mut net, 0), Some(Event::Failed(_))));
        let sent = net.sent();
        auth.token(1_000);
        auth.tick(&mut net, 1_000);
        assert_eq!(net.sent(), sent, "waits before retrying");
        auth.tick(&mut net, REFRESH_RETRY_MS);
        assert_eq!(net.sent(), sent + 1);
    }

    #[test]
    fn sign_out_revokes_and_forgets() {
        let mut net = MockNet::default();
        let mut auth = Auth::new(creds(), Some("1//t".into()));
        auth.sign_out(&mut net);
        let req = net.last_request();
        assert_eq!((req.method.as_str(), req.url.as_str()), ("POST", REVOKE_URL));
        assert_eq!(String::from_utf8(req.body.clone()).unwrap(), "token=1%2F%2Ft");
        assert_eq!(auth.token(0), Token::SignedOut);
        // The revocation answer is collected so the OS can free it.
        net.respond_last(200, "{}");
        auth.tick(&mut net, 0);
        assert_eq!(net.unpolled(), 0);
    }

    #[test]
    fn cancel_collects_the_abandoned_request() {
        let mut net = MockNet::default();
        let mut auth = waiting(&mut net);
        auth.tick(&mut net, 5_000);
        auth.cancel();
        assert_eq!(*auth.state(), State::SignedOut);
        net.respond_last(200, TOKENS);
        assert_eq!(auth.tick(&mut net, 6_000), None, "a late approval is ignored");
        assert!(!auth.is_signed_in());
        assert_eq!(net.unpolled(), 0);
    }

    #[test]
    fn credentials_debug_is_redacted() {
        let s = alloc::format!("{:?}", creds().unwrap());
        assert!(s.contains("client-123") && !s.contains("s3cret"));
    }
}
