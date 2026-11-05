//! Player errors with stable short codes, shown to the user and printed by
//! `yt` shell commands so failures can be told apart at a glance.

use alloc::string::String;
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No network link, DNS/TCP/TLS failure or timeout.
    Network,
    /// An HTTP status other than the expected one.
    Http(u16),
    /// YouTube refused the video (private, removed, region, age, made for kids).
    Unplayable,
    /// YouTube asked to sign in / prove we are not a bot even with visitorData:
    /// the client in the table is probably blocked.
    ClientBlocked,
    /// No stream in a format p4os can decode (H.264 fMP4 or Opus WebM).
    NoFormat,
    /// Live streams are not supported.
    Live,
    /// The stream URLs expired or were revoked and refreshing did not help.
    Expired,
    /// The player response or a container could not be parsed.
    BadResponse,
    /// The decoder rejected the stream.
    Decode,
    /// The client table on the SD card is invalid.
    Config,
    /// The OS could not start the decoder thread or audio.
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl PlayError {
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self { kind, detail: detail.into() }
    }

    /// Short stable code, e.g. "YT-HTTP403".
    pub fn code(&self) -> String {
        match self.kind {
            ErrorKind::Network => "YT-NET".into(),
            ErrorKind::Http(s) => alloc::format!("YT-HTTP{s}"),
            ErrorKind::Unplayable => "YT-UNPLAYABLE".into(),
            ErrorKind::ClientBlocked => "YT-CLIENT".into(),
            ErrorKind::NoFormat => "YT-NOFORMAT".into(),
            ErrorKind::Live => "YT-LIVE".into(),
            ErrorKind::Expired => "YT-EXPIRED".into(),
            ErrorKind::BadResponse => "YT-PARSE".into(),
            ErrorKind::Decode => "YT-DECODE".into(),
            ErrorKind::Config => "YT-CONFIG".into(),
            ErrorKind::System => "YT-SYSTEM".into(),
        }
    }

    /// One-line explanation for the player screen.
    pub fn headline(&self) -> &'static str {
        match self.kind {
            ErrorKind::Network => "Network error",
            ErrorKind::Http(403) => "YouTube refused the stream",
            ErrorKind::Http(_) => "Unexpected reply from YouTube",
            ErrorKind::Unplayable => "This video cannot be played",
            ErrorKind::ClientBlocked => "YouTube blocked the player client: update youtube-clients.json",
            ErrorKind::NoFormat => "No playable stream format",
            ErrorKind::Live => "Live streams are not supported",
            ErrorKind::Expired => "Stream links expired",
            ErrorKind::BadResponse => "Could not read YouTube's reply",
            ErrorKind::Decode => "Could not decode the stream",
            ErrorKind::Config => "youtube-clients.json is invalid",
            ErrorKind::System => "System error",
        }
    }
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.code(), self.headline(), self.detail)
    }
}
