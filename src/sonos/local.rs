//! LAN transport: a WebSocket straight to a player, no cloud and no OAuth.
//!
//! Players expose the Control API on `wss://<ip>:1443/websocket/api`. The only
//! credential is a well-known API key. The player cert is *not* self-signed - it
//! is a leaf-only chain, `CN=<MAC>` signed by "Sonos Device Authentication Root
//! CA" with the root not sent, SAN `sonos-<MAC>.local` and no IP (verified on
//! hardware by x2rocktv, 2026-09-07). It is deliberately not verified here anyway:
//! the transport never leaves the LAN, which already concedes MITM. See
//! [`AcceptAnyCert`].
//!
//! One reader task owns the receiving half. Replies are matched to callers by the
//! `cmdId` the player echoes back; everything else is an event and is fanned out
//! to whoever asked for [`Connection::events`]. A `Connection` is a cheap handle,
//! so the daemon's MPRIS objects and its event loop can share one socket.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::fmt;
use tokio::net::TcpStream;
use tokio::sync::{Notify, broadcast, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};

use super::proto::{ErrorBody, Event, Header};

pub const PORT: u16 = 1443;
const API_KEY: &str = "123e4567-e89b-12d3-a456-426655440000";
const SUBPROTOCOL: &str = "v1.api.smartspeaker.audio";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Players answer in well under a second on a LAN. Waiting longer only ever means
/// the connection is dead.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Keeps the connection alive through firewalls that expire idle TCP sessions.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Nothing at all from the player for this long means the socket is dead even if
/// writes still succeed - the classic frozen-across-a-suspend zombie.
const SILENCE_LIMIT: Duration = Duration::from_secs(90);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Reply = (Header, Value);

/// Accepts any certificate - but not because there is nothing to validate. The
/// player cert is a leaf-only chain, `CN=<MAC>` signed by "Sonos Device
/// Authentication Root CA", SAN `sonos-<MAC>.local` and no IP, so connecting by
/// that `.local` name (derivable from the RINCON id) with the Sonos root as a
/// trust anchor would validate normally. It is accepted blindly only because the
/// transport is confined to the LAN, which already concedes active MITM, so
/// proper validation would buy nothing. (Cert shape verified by x2rocktv on
/// hardware, 2026-09-07.)
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The TLS configuration is identical for every player, so it is built once.
fn tls_connector() -> Connector {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let provider = super::crypto_provider();
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("the default provider supports the default protocol versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
                .with_no_client_auth(),
        )
    });
    Connector::Rustls(config.clone())
}

struct Inner {
    ip: IpAddr,
    /// The write half. `None` once the read loop has exited: it takes and
    /// drops the sink on its way out, because the socket closes only when both
    /// halves are gone, and this one would otherwise live as long as the
    /// keepalive's `Arc` - up to a tick after the connection was closed.
    sink: tokio::sync::Mutex<Option<SplitSink<Socket, Message>>>,
    /// Replies waiting to be matched. Keyed by `cmdId`; the queue keeps arrival
    /// order for any reply that comes back without one.
    pending: Mutex<Pending>,
    events: broadcast::Sender<Arc<Event>>,
    next_id: AtomicU64,
    household_id: Mutex<Option<String>>,
    last_rx: Mutex<Instant>,
    alive: AtomicBool,
    shutdown: Notify,
}

#[derive(Default)]
struct Pending {
    by_id: HashMap<u64, oneshot::Sender<Reply>>,
    order: VecDeque<u64>,
}

impl Pending {
    fn insert(&mut self, id: u64, tx: oneshot::Sender<Reply>) {
        self.by_id.insert(id, tx);
        self.order.push_back(id);
    }

    fn take(&mut self, id: Option<u64>) -> Option<oneshot::Sender<Reply>> {
        let id = match id {
            // A cmdId nobody is waiting on is a stale or duplicate reply: drop it,
            // never hand it to some other caller.
            Some(id) => id,
            // No cmdId at all: assume replies arrive in the order commands were sent.
            None => loop {
                let oldest = self.order.pop_front()?;
                if self.by_id.contains_key(&oldest) {
                    break oldest;
                }
            },
        };
        self.order.retain(|&queued| queued != id);
        self.by_id.remove(&id)
    }

    fn remove(&mut self, id: u64) {
        self.by_id.remove(&id);
        self.order.retain(|&queued| queued != id);
    }
}

/// A command's place in [`Pending`], given up when the command's future ends
/// however it ends. Removing an entry a reply already took is a no-op.
struct Registered<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.inner.pending.lock().unwrap().remove(self.id);
    }
}

#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

/// A command the player refused: its `errorCode` and `reason`, as the reply
/// carried them. Kept typed rather than flattened into the sentence, so the
/// one caller that acts on a *particular* refusal - `play` on a source the
/// room cannot play, `ERROR_PLAYBACK_FAILED` - can tell it from a lost socket
/// without matching text. Displays exactly as the sentence always read.
#[derive(Debug)]
pub struct ApiError {
    /// `namespace command`, e.g. `playback:1 play`.
    pub what: String,
    pub code: Option<String>,
    pub reason: Option<String>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} failed: {} ({})",
            self.what,
            self.code.as_deref().unwrap_or("unknown error"),
            self.reason.as_deref().unwrap_or("no reason given")
        )
    }
}

impl std::error::Error for ApiError {}

/// The player could not be reached, or stopped answering: a socket that would
/// not open, one that closed under a command, a reply that never came. Typed
/// so whoever holds the connection can tell "this socket is no good" from a
/// refusal ([`ApiError`]) and from every other failure - the TUI drops and
/// rebuilds its session on this and keeps it on anything else. Displays as
/// the sentence each site always printed.
#[derive(Debug)]
pub struct Unreachable {
    message: String,
}

impl Unreachable {
    fn error(message: String) -> anyhow::Error {
        Self { message }.into()
    }

    pub fn of(e: &anyhow::Error) -> Option<&Unreachable> {
        e.downcast_ref()
    }
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Unreachable {}

impl ApiError {
    /// The refusal inside an error chain, if it is one. The Control API's
    /// counterpart to [`crate::sonos::upnp::Fault::of`], and asked for the same
    /// reason: **the player answering "no" and the player not answering at all
    /// must not be treated alike.** A caller holding a fallback wants the
    /// first; the second means the fallback will fail the same way.
    pub fn of(e: &anyhow::Error) -> Option<&ApiError> {
        e.downcast_ref()
    }
}

impl Connection {
    pub async fn open(ip: IpAddr) -> Result<Self> {
        let mut request = format!("wss://{ip}:{PORT}/websocket/api").into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("X-Sonos-Api-Key", HeaderValue::from_static(API_KEY));
        headers.insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(SUBPROTOCOL),
        );
        // No Origin header. Players answer 403 Forbidden if one is present, and
        // 400 Bad Request without the API key (both verified against a One SL).

        let connect = connect_async_tls_with_config(request, None, false, Some(tls_connector()));
        let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| {
                Unreachable::error(format!("timed out connecting to player at {ip}:{PORT}"))
            })?
            .map_err(|e| Unreachable::error(format!("connecting to player at {ip}:{PORT}: {e}")))?;
        let (sink, stream) = socket.split();

        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            ip,
            sink: tokio::sync::Mutex::new(Some(sink)),
            pending: Mutex::new(Pending::default()),
            events,
            next_id: AtomicU64::new(1),
            household_id: Mutex::new(None),
            last_rx: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            shutdown: Notify::new(),
        });
        tokio::spawn(read_loop(inner.clone(), stream));
        tokio::spawn(keepalive(inner.clone()));
        Ok(Self { inner })
    }

    pub fn ip(&self) -> IpAddr {
        self.inner.ip
    }

    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Relaxed)
    }

    /// Events from every namespace subscribed on this connection, plus a final
    /// [`Event::LOST`] when the socket dies.
    pub fn events(&self) -> broadcast::Receiver<Arc<Event>> {
        self.inner.events.subscribe()
    }

    /// Close deliberately. The reader exits and anyone waiting on events sees `LOST`.
    /// Used to force a reconnect after a suspend, when the socket is a zombie
    /// that would otherwise never report itself dead.
    pub fn close(&self) {
        self.inner.shutdown.notify_one();
    }

    /// Raw exchange: send `[command, options]`, return `[header, body]` whatever
    /// the outcome. Most callers want [`Connection::call`].
    pub async fn command(&self, mut command: Value, options: Value) -> Result<Reply> {
        if !self.is_alive() {
            return Err(self.lost());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        command["cmdId"] = json!(id.to_string());
        // Signed in, for a household with Authentication on: the token rides in
        // each command's own header - the handshake's `Authorization` is ignored
        // - as `Bearer <token>`; a bare token is refused. Nobody signed in, no
        // field, which is every household with Authentication off.
        if let Some(bearer) = super::login::bearer().await {
            command["authorization"] = json!(format!("Bearer {bearer}"));
        }
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        // Unregistered however this ends - a reply, a failure, the deadline,
        // or the caller dropping this future (an outer timeout, a `try_join!`
        // whose sibling failed), which ran none of the explicit removals and
        // left the sender registered for as long as the socket lived.
        let _registered = Registered {
            inner: &self.inner,
            id,
        };

        // **One deadline over the whole exchange**: the wait for the shared
        // write half, the write, and the reply. It covered only the reply,
        // so a socket that stopped draining held this command - and every
        // command queued behind the write lock - for as long as it liked.
        let payload = Value::Array(vec![command, options]).to_string();
        let mut sent = false;
        let exchange = async {
            match self.inner.sink.lock().await.as_mut() {
                Some(sink) => sink
                    .send(Message::Text(payload.into()))
                    .await
                    .map_err(|e| e.to_string())?,
                None => return Err("socket closed".to_string()),
            }
            sent = true;
            Ok(rx.await)
        };
        match tokio::time::timeout(REPLY_TIMEOUT, exchange).await {
            Ok(Ok(Ok(reply))) => Ok(reply),
            Ok(Ok(Err(_))) => Err(self.lost()),
            Ok(Err(e)) => Err(Unreachable::error(format!(
                "sending to player at {}: {e}",
                self.inner.ip
            ))),
            // Sent, and no answer: the player's silence, as it always was.
            Err(_) if sent => Err(Unreachable::error(format!(
                "player at {} did not reply within {:?}",
                self.inner.ip, REPLY_TIMEOUT
            ))),
            // The write itself never finished. A frame may be half on the wire,
            // so nothing more can be sent on this socket: it is closed, and the
            // next command reconnects. Not retried here - what did reach the
            // player may already have acted.
            Err(_) => {
                self.inner.shutdown.notify_one();
                Err(Unreachable::error(format!(
                    "could not send to player at {} within {:?}",
                    self.inner.ip, REPLY_TIMEOUT
                )))
            }
        }
    }

    fn lost(&self) -> anyhow::Error {
        Unreachable::error(format!(
            "connection to player at {} was lost",
            self.inner.ip
        ))
    }

    /// Send a command and return its body, turning a player-side failure into an `Err`.
    ///
    /// `command` carries the namespace, command name and target
    /// (`groupId` / `playerId` / `householdId`); `options` is the command's parameters.
    /// A refusal is an [`ApiError`], so a caller that needs to know *which*
    /// refusal can downcast rather than read the sentence.
    pub async fn call(&self, command: Value, options: Value) -> Result<Value> {
        let what = format!(
            "{} {}",
            command["namespace"].as_str().unwrap_or("?"),
            command["command"].as_str().unwrap_or("?")
        );
        let (header, body) = self.command(command, options).await?;
        if header.success == Some(true) {
            return Ok(body);
        }
        let err: ErrorBody = serde_json::from_value(body).unwrap_or_default();
        Err(ApiError {
            what,
            code: err.error_code,
            reason: err.reason,
        }
        .into())
    }

    /// The household this player belongs to.
    ///
    /// There is no command for this, but every response header carries it, so an
    /// intentionally invalid command is the cheapest way to ask. It fails, by
    /// design, which is why this goes through `command` rather than `call`.
    pub async fn household_id(&self) -> Result<String> {
        if let Some(id) = self.inner.household_id.lock().unwrap().clone() {
            return Ok(id);
        }
        let (header, _) = self.command(json!({}), json!({})).await?;
        let id = header
            .household_id
            .ok_or_else(|| anyhow!("player did not report a household id"))?;
        *self.inner.household_id.lock().unwrap() = Some(id.clone());
        Ok(id)
    }
}

impl Inner {
    /// Route one incoming frame: a reply to whoever is waiting for it, anything
    /// else out as an event.
    fn dispatch(&self, text: &str) {
        let Ok((header, body)) = serde_json::from_str::<(Header, Value)>(text) else {
            return;
        };

        // Replies carry `success`; events never do (verified).
        if header.success.is_some() {
            let id = header.cmd_id.as_deref().and_then(|s| s.parse().ok());
            if let Some(tx) = self.pending.lock().unwrap().take(id) {
                let _ = tx.send((header, body));
            }
        } else {
            let _ = self.events.send(Arc::new(Event::new(header, body)));
        }
    }

    fn mark_dead(&self) {
        if self.alive.swap(false, Ordering::Relaxed) {
            // Dropping the senders fails every in-flight command promptly.
            self.pending.lock().unwrap().by_id.clear();
            let _ = self.events.send(Arc::new(Event::lost()));
        }
    }
}

async fn read_loop(inner: Arc<Inner>, mut stream: SplitStream<Socket>) {
    loop {
        let message = tokio::select! {
            message = stream.next() => message,
            _ = inner.shutdown.notified() => break,
        };
        *inner.last_rx.lock().unwrap() = Instant::now();
        match message {
            Some(Ok(Message::Text(text))) => inner.dispatch(&text),
            // The library queues pongs but only flushes them on our next write,
            // which on a quiet daemon could be never. Answer explicitly.
            // Bounded like every write: a socket that will not drain must not
            // stop this loop from noticing it is shut down.
            Some(Ok(Message::Ping(payload))) => {
                let pong = async {
                    if let Some(sink) = inner.sink.lock().await.as_mut() {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                };
                if tokio::time::timeout(REPLY_TIMEOUT, pong).await.is_err() {
                    break;
                }
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            Some(Ok(_)) => {}
        }
    }
    inner.mark_dead();
    // The read half went with this loop; the write half lives in `inner`,
    // which the keepalive keeps until its next tick, and a socket closes only
    // when both halves are dropped. Taken out and dropped here - a Close
    // frame first, for a player that is still listening - so the TCP
    // connection goes down when the socket is closed or found dead, not up to
    // thirty seconds later. A session's `close` used to leave its sockets
    // `ESTAB` for that long.
    // Bounded: on a socket that stopped draining, both the lock (a write
    // holding it) and the Close frame could wait for ever, and the sink is
    // what has to go for the connection to close.
    let _ = tokio::time::timeout(REPLY_TIMEOUT, async {
        if let Some(mut sink) = inner.sink.lock().await.take() {
            let _ = sink.close().await;
        }
    })
    .await;
}

async fn keepalive(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(PING_INTERVAL);
    tick.tick().await; // the first tick fires immediately
    loop {
        tick.tick().await;
        if !inner.alive.load(Ordering::Relaxed) {
            return;
        }
        if inner.last_rx.lock().unwrap().elapsed() > SILENCE_LIMIT {
            // A zombie: the reader is blocked on a socket that will never speak
            // again, so wake it up and let callers reconnect.
            inner.shutdown.notify_one();
            return;
        }
        // Bounded, so a ping stuck behind a socket that will not drain comes
        // back to the silence check rather than waiting with it.
        let ping = async {
            match inner.sink.lock().await.as_mut() {
                Some(sink) => sink.send(Message::Ping(Vec::new().into())).await.is_ok(),
                None => false,
            }
        };
        let pinged = tokio::time::timeout(REPLY_TIMEOUT, ping)
            .await
            .unwrap_or(false);
        if !pinged {
            inner.shutdown.notify_one();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection over a loopback socket whose far end is handed back and
    /// never read or answered: no TLS, no handshake, no read loop - only the
    /// command path, which is what these tests are about.
    async fn unanswered() -> (Connection, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ours = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (theirs, _) = listener.accept().await.unwrap();
        let socket = WebSocketStream::from_raw_socket(
            MaybeTlsStream::Plain(ours),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let (sink, _stream) = socket.split();
        let (events, _) = broadcast::channel(16);
        let inner = Arc::new(Inner {
            ip: "127.0.0.1".parse().unwrap(),
            sink: tokio::sync::Mutex::new(Some(sink)),
            pending: Mutex::new(Pending::default()),
            events,
            next_id: AtomicU64::new(1),
            household_id: Mutex::new(None),
            last_rx: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            shutdown: Notify::new(),
        });
        (Connection { inner }, theirs)
    }

    /// A caller that gives up - an outer timeout, a `try_join!` whose sibling
    /// failed - takes its registration with it.
    #[tokio::test]
    async fn a_cancelled_command_leaves_nothing_registered() {
        let (connection, _theirs) = unanswered().await;
        let caller = connection.clone();
        let task = tokio::spawn(async move { caller.command(json!({}), json!({})).await });
        while connection.inner.pending.lock().unwrap().by_id.is_empty() {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        let pending = connection.inner.pending.lock().unwrap();
        assert!(pending.by_id.is_empty() && pending.order.is_empty());
    }

    /// A socket that will not drain fails the command blocked writing to it,
    /// and the one queued behind the write lock, within the deadline - rather
    /// than holding both for as long as it stays stuck.
    #[tokio::test(start_paused = true)]
    async fn a_write_that_cannot_drain_is_bounded_too() {
        let (connection, _theirs) = unanswered().await;
        // Past what loopback buffers hold, so the write really blocks.
        let big = json!({ "padding": "x".repeat(16 * 1024 * 1024) });
        let (first, second) = tokio::join!(
            tokio::time::timeout(REPLY_TIMEOUT * 2, connection.command(json!({}), big)),
            tokio::time::timeout(REPLY_TIMEOUT * 2, connection.command(json!({}), json!({}))),
        );
        let first = first.expect("the blocked write outlived its deadline");
        let second = second.expect("the command behind it outlived its deadline");
        assert!(first.is_err() && second.is_err());
        assert!(Unreachable::of(&first.unwrap_err()).is_some());
        assert!(connection.inner.pending.lock().unwrap().by_id.is_empty());
    }

    /// `enqueue_and_play` decides whether to delete a queue row and fall back to
    /// streaming on whether this downcast finds anything, so the mechanism is
    /// pinned rather than assumed - including under a `context` layer, since
    /// adding one is the kind of tidy-up that would silently turn every player
    /// refusal into "could not reach the player".
    #[test]
    fn a_refusal_is_recognisable_through_the_error_chain() {
        let refusal: anyhow::Error = ApiError {
            what: "playback:1 play".to_string(),
            code: Some("ERROR_PLAYBACK_FAILED".to_string()),
            reason: None,
        }
        .into();
        assert_eq!(
            ApiError::of(&refusal).and_then(|e| e.code.as_deref()),
            Some("ERROR_PLAYBACK_FAILED")
        );

        let wrapped = refusal.context("while starting the room");
        assert!(
            ApiError::of(&wrapped).is_some(),
            "context must not hide the refusal"
        );

        // A lost socket is the case that must *not* look like a refusal.
        let lost = anyhow::anyhow!("timed out after 8s reading from 192.168.1.2:1400");
        assert!(ApiError::of(&lost).is_none());
    }

    fn pending_with(ids: &[u64]) -> (Pending, Vec<oneshot::Receiver<Reply>>) {
        let mut pending = Pending::default();
        let mut receivers = Vec::new();
        for &id in ids {
            let (tx, rx) = oneshot::channel();
            pending.insert(id, tx);
            receivers.push(rx);
        }
        (pending, receivers)
    }

    #[test]
    fn replies_match_by_id_regardless_of_order() {
        let (mut pending, _rx) = pending_with(&[1, 2, 3]);
        assert!(pending.take(Some(3)).is_some());
        assert!(pending.take(Some(1)).is_some());
        assert!(pending.take(Some(3)).is_none(), "already taken");
        assert_eq!(pending.by_id.len(), 1);
    }

    #[test]
    fn replies_without_an_id_fall_back_to_arrival_order() {
        let (mut pending, _rx) = pending_with(&[7, 8, 9]);
        pending.remove(7); // timed out before its reply came
        assert!(pending.take(None).is_some(), "oldest still-waiting is 8");
        assert!(pending.by_id.contains_key(&9));
        assert!(!pending.by_id.contains_key(&8));
    }
}
