//! LAN transport: a WebSocket straight to a player, no cloud and no OAuth.
//!
//! Players expose the Control API on `wss://<ip>:1443/websocket/api`. The only
//! credential is a well-known API key; the certificate is self-signed, so it is
//! deliberately not verified.
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

use anyhow::{Context, Result, anyhow, bail};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Notify, broadcast, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};

use super::proto::{ErrorBody, Event, Groups, Header};

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

/// Accepts any certificate. Players present a self-signed cert for their own IP,
/// so there is nothing to validate against; the transport is confined to the LAN.
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
        let provider = Arc::new(rustls::crypto::ring::default_provider());
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
    sink: tokio::sync::Mutex<SplitSink<Socket, Message>>,
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

#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
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
            .map_err(|_| anyhow!("timed out connecting to player at {ip}:{PORT}"))?
            .with_context(|| format!("connecting to player at {ip}:{PORT}"))?;
        let (sink, stream) = socket.split();

        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            ip,
            sink: tokio::sync::Mutex::new(sink),
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
            bail!("connection to player at {} was lost", self.inner.ip);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        command["cmdId"] = json!(id.to_string());
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);

        let payload = Value::Array(vec![command, options]).to_string();
        let sent = self
            .inner
            .sink
            .lock()
            .await
            .send(Message::Text(payload.into()))
            .await;
        if let Err(e) = sent {
            self.inner.pending.lock().unwrap().remove(id);
            bail!("sending to player at {}: {e}", self.inner.ip);
        }

        match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => bail!("connection to player at {} was lost", self.inner.ip),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(id);
                bail!(
                    "player at {} did not reply within {:?}",
                    self.inner.ip,
                    REPLY_TIMEOUT
                )
            }
        }
    }

    /// Send a command and return its body, turning a player-side failure into an `Err`.
    ///
    /// `command` carries the namespace, command name and target
    /// (`groupId` / `playerId` / `householdId`); `options` is the command's parameters.
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
        bail!(
            "{what} failed: {} ({})",
            err.error_code.as_deref().unwrap_or("unknown error"),
            err.reason.as_deref().unwrap_or("no reason given")
        )
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

    pub async fn groups(&self) -> Result<Groups> {
        let household = self.household_id().await?;
        let body = self
            .call(
                json!({
                    "namespace": "groups:1",
                    "command": "getGroups",
                    "householdId": household,
                }),
                json!({}),
            )
            .await?;
        Ok(serde_json::from_value(body)?)
    }
}

impl Inner {
    /// Route one incoming frame: a reply to whoever is waiting for it, anything
    /// else out as an event.
    fn dispatch(&self, text: &str) {
        let Ok(mut parts) = serde_json::from_str::<Vec<Value>>(text) else {
            return;
        };
        if parts.len() != 2 {
            return;
        }
        let body = parts.pop().expect("length checked");
        let Ok(header) = serde_json::from_value::<Header>(parts.pop().expect("length checked"))
        else {
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
            Some(Ok(Message::Ping(payload))) => {
                let _ = inner.sink.lock().await.send(Message::Pong(payload)).await;
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            Some(Ok(_)) => {}
        }
    }
    inner.mark_dead();
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
        if inner
            .sink
            .lock()
            .await
            .send(Message::Ping(Vec::new().into()))
            .await
            .is_err()
        {
            inner.shutdown.notify_one();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
