//! TLS 1.2/1.3 client on rustls's unbuffered API with the pure-Rust
//! RustCrypto provider. Server certificates are verified against the
//! Mozilla root store (webpki-roots) using a caller-supplied wall clock.
//!
//! [`TlsClient`] never touches a socket: the transport feeds it received
//! ciphertext, calls [`TlsClient::process`], sends whatever
//! [`TlsClient::pending_output`] holds and collects decrypted bytes with
//! [`TlsClient::take_plaintext`]. That keeps it usable from a non-blocking
//! poll loop.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::time::Duration;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{CertificateError, Error, RootCertStore};

pub use rustls::ClientConfig;

/// Returns the current time as Unix seconds (UTC), or `None` when unknown.
pub type Clock = fn() -> Option<u64>;

#[derive(Debug)]
struct WallClock(Clock);

impl TimeProvider for WallClock {
    fn current_time(&self) -> Option<UnixTime> {
        (self.0)().map(|s| UnixTime::since_unix_epoch(Duration::from_secs(s)))
    }
}

/// Client configuration: TLS 1.3 and 1.2, all RustCrypto suites, Mozilla
/// roots. Build it once and share it between connections.
pub fn client_config(clock: Clock) -> Result<Arc<ClientConfig>, String> {
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = ClientConfig::builder_with_details(Arc::new(rustls_rustcrypto::provider()), Arc::new(WallClock(clock)))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS setup: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Turn a rustls error into a message for the user.
pub fn describe(e: &Error) -> String {
    match e {
        Error::FailedToGetCurrentTime => {
            "clock not set, so the certificate dates cannot be checked (network time unavailable; set the time in Settings)".into()
        }
        Error::InvalidCertificate(c) => match c {
            CertificateError::UnknownIssuer => "certificate not trusted (unknown issuer)".into(),
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                "certificate has expired (or the clock is wrong)".into()
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                "certificate not valid yet (or the clock is wrong)".into()
            }
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                "certificate does not match the host name".into()
            }
            other => format!("invalid certificate: {other:?}"),
        },
        Error::AlertReceived(a) => format!("server rejected the TLS handshake ({a:?})"),
        other => format!("TLS: {other}"),
    }
}

pub struct TlsClient {
    conn: UnbufferedClientConnection,
    /// Ciphertext received from the server and not yet consumed.
    incoming: Vec<u8>,
    /// Ciphertext waiting to be sent to the server, in order.
    outgoing: Vec<u8>,
    /// Application data to send once the handshake allows it.
    request: Option<Vec<u8>>,
    plaintext: Vec<u8>,
    peer_closed: bool,
}

impl TlsClient {
    /// Start a handshake with `host` (also used for SNI and certificate
    /// name checks); `request` is sent as soon as the handshake completes.
    pub fn new(config: Arc<ClientConfig>, host: &str, request: Vec<u8>) -> Result<Self, String> {
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("invalid TLS server name {host:?}"))?;
        let conn = UnbufferedClientConnection::new(config, name).map_err(|e| describe(&e))?;
        let mut client = TlsClient {
            conn,
            incoming: Vec::new(),
            outgoing: Vec::new(),
            request: Some(request),
            plaintext: Vec::new(),
            peer_closed: false,
        };
        // Queue the ClientHello.
        client.process()?;
        Ok(client)
    }

    /// Append ciphertext received from the transport.
    pub fn push_received(&mut self, data: &[u8]) {
        self.incoming.extend_from_slice(data);
    }

    /// Ciphertext the transport should send next.
    pub fn pending_output(&self) -> &[u8] {
        &self.outgoing
    }

    /// The transport sent the first `n` bytes of [`Self::pending_output`].
    pub fn consume_output(&mut self, n: usize) {
        self.outgoing.drain(..n.min(self.outgoing.len()));
    }

    /// Decrypted application data received so far (drained).
    pub fn take_plaintext(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.plaintext)
    }

    pub fn is_handshaking(&self) -> bool {
        self.conn.is_handshaking()
    }

    /// The server sent close_notify: everything it will send has arrived.
    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// Negotiated protocol and cipher suite, e.g. "TLSv1_3 TLS13_AES_128_GCM_SHA256".
    pub fn session_info(&self) -> String {
        match (self.conn.protocol_version(), self.conn.negotiated_cipher_suite()) {
            (Some(v), Some(s)) => format!("{:?} {:?}", v, s.suite()),
            _ => "handshake incomplete".into(),
        }
    }

    /// Run the TLS state machine over everything received so far, queueing
    /// handshake records and the request for sending and collecting
    /// plaintext. Returns once more input from the server is needed.
    pub fn process(&mut self) -> Result<(), String> {
        loop {
            let UnbufferedStatus { mut discard, state } = self.conn.process_tls_records(&mut self.incoming);
            let state = state.map_err(|e| describe(&e))?;
            let mut wait = false;
            match state {
                ConnectionState::ReadTraffic(mut traffic) => {
                    while let Some(record) = traffic.next_record() {
                        let record = record.map_err(|e| describe(&e))?;
                        discard += record.discard;
                        self.plaintext.extend_from_slice(record.payload);
                    }
                }
                ConnectionState::EncodeTlsData(mut data) => {
                    append_output(&mut self.outgoing, |buf| match data.encode(buf) {
                        Ok(n) => Ok(Ok(n)),
                        Err(EncodeError::InsufficientSize(s)) => Ok(Err(s.required_size)),
                        Err(e) => Err(format!("TLS encode: {e:?}")),
                    })?;
                }
                // Encoded records stay queued in `outgoing` (in order) until
                // the transport takes them, so they count as transmitted.
                ConnectionState::TransmitTlsData(data) => data.done(),
                ConnectionState::WriteTraffic(mut traffic) => match self.request.take() {
                    Some(req) => append_output(&mut self.outgoing, |buf| match traffic.encrypt(&req, buf) {
                        Ok(n) => Ok(Ok(n)),
                        Err(EncryptError::InsufficientSize(s)) => Ok(Err(s.required_size)),
                        Err(e) => Err(format!("TLS encrypt: {e:?}")),
                    })?,
                    None => wait = true,
                },
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    self.peer_closed = true;
                    wait = true;
                }
                // BlockedHandshake, and server-only states.
                _ => wait = true,
            }
            self.incoming.drain(..discard);
            if wait {
                return Ok(());
            }
        }
    }
}

/// Let `write` fill the tail of `out`; it returns Ok(Err(size)) to ask for
/// a larger buffer.
fn append_output(
    out: &mut Vec<u8>,
    mut write: impl FnMut(&mut [u8]) -> Result<Result<usize, usize>, String>,
) -> Result<(), String> {
    let start = out.len();
    let mut room = 2048;
    loop {
        out.resize(start + room, 0);
        match write(&mut out[start..]) {
            Ok(Ok(n)) => {
                out.truncate(start + n);
                return Ok(());
            }
            Ok(Err(required)) if required > room => room = required,
            Ok(Err(_)) => {
                out.truncate(start);
                return Err("TLS: output buffer sizing failed".into());
            }
            Err(e) => {
                out.truncate(start);
                return Err(e);
            }
        }
    }
}
