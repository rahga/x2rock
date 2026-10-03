//! Hand a clip to a speaker: serve one file, once, from this machine.
//!
//! A Sonos player *fetches* an audio clip; nothing pushes bytes to it. So a
//! clip that exists only here - speech a service just made - has to be put
//! behind a URL for as long as it takes the player to come and get it. This is
//! that URL: a listener bound for one request, answering one path, to one
//! address, and gone.
//!
//! **This is x2rock's second inbound connection, and it shares the first's
//! port** - [`crate::netid::INBOUND_PORT`], which says why. The Control API is
//! outbound on :1443 and everything else this tool does is outbound too, by
//! design; see "The firewall problem" in docs/architecture.md.
//!
//! **What was measured (2026-10-02).** A Sonos One SL asked for a clip at
//! `http://<this machine>:8765/clip.mp3` accepted the request in 0.37s and
//! fetched the file - one plain `GET`, no `HEAD`, no `Range` - within a second
//! once the firewall let it through, and never at all before. Omarchy's
//! default-deny `ufw` was the only thing between the two.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::hint::{Code, Hint};
use crate::netid::local_ip_toward;

/// After the clip has been fetched, how much longer to stay up for a second
/// request. The player made exactly one in testing; this is for the day it
/// makes two.
const GRACE: Duration = Duration::from_millis(1500);
/// How long a connected peer gets to finish sending its request line.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST: usize = 8 * 1024;

/// What happened: how long the player took to come for the clip, and how often
/// it asked.
#[derive(Debug)]
pub struct Served {
    pub waited: Duration,
    pub requests: u32,
}

/// Serve `bytes` at `http://<this machine>:<port>/<name>` to `speaker`, calling
/// `trigger` with that URL once the listener is up (that is where
/// `loadAudioClip` goes), and return once the player has fetched it - or fail
/// with `clip_not_fetched` after `wait` if it never comes.
///
/// `port` 0 asks the OS for an ephemeral one, which suits a host with nothing
/// to open and is what the tests use.
pub async fn serve_once<F, Fut>(
    bytes: &[u8],
    mime: &str,
    name: &str,
    speaker: IpAddr,
    port: u16,
    wait: Duration,
    trigger: F,
) -> Result<Served>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
        .await
        .with_context(|| {
            if port == 0 {
                "opening a local port to serve the clip from".to_string()
            } else {
                format!(
                    "opening port {port} to serve the clip from - another x2rock may be using \
                     it (a `say`, or `link --from-household`); wait a moment, or pass --port"
                )
            }
        })?;
    let port = listener.local_addr()?.port();
    let local = local_ip_toward(speaker)?;
    let url = format!("http://{local}:{port}/{name}");
    let path = format!("/{name}");

    // The clock starts with the request: the player fetches the clip *while*
    // answering `loadAudioClip`, so by the time that call returns the GET is
    // usually already queued on the listener.
    let started = Instant::now();
    trigger(url.clone()).await?;

    let mut deadline = started + wait;
    let mut requests = 0u32;
    let mut waited: Option<Duration> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok((stream, peer))) = tokio::time::timeout(remaining, listener.accept()).await
        else {
            break;
        };
        // Only the player this clip is for. Anyone else on the LAN who finds
        // the port open gets the connection closed, not the audio.
        if peer.ip() != speaker {
            continue;
        }
        let answered = tokio::time::timeout(REQUEST_TIMEOUT, answer(stream, &path, mime, bytes))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false);
        if answered {
            requests += 1;
            let now = Instant::now();
            waited.get_or_insert(now - started);
            deadline = deadline.min(now + GRACE);
        }
    }

    let Some(waited) = waited else {
        return Err(not_fetched(&url, speaker, port, wait));
    };
    Ok(Served { waited, requests })
}

/// One HTTP exchange on an accepted connection. `Ok(true)` when the clip itself
/// was sent - a `GET` of the path; a `HEAD` or a wrong path is answered and is
/// `Ok(false)`.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<bool> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > MAX_REQUEST {
            break;
        }
    }
    let head = String::from_utf8_lossy(&raw);
    let sent = match parse_request(&head) {
        Some((method, asked)) if asked == path && matches!(method, "GET" | "HEAD") => {
            stream
                .write_all(ok_head(mime, bytes.len()).as_bytes())
                .await?;
            if method == "GET" {
                stream.write_all(bytes).await?;
                true
            } else {
                false
            }
        }
        _ => {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            false
        }
    };
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(sent)
}

fn ok_head(mime: &str, len: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n"
    )
}

/// The method and the path (query stripped) off a request line, or `None` for
/// anything that is not one.
fn parse_request(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let path = target.split(['?', '#']).next().unwrap_or(target);
    Some((method, path))
}

/// The player took the clip and never came for it. No `fix`: the remedy is a
/// firewall rule under `sudo`, which is a person's to run, so it is spelled out
/// in the message and handed back in `data.firewall_rule` instead.
fn not_fetched(url: &str, speaker: IpAddr, port: u16, wait: Duration) -> anyhow::Error {
    let subnet = match speaker {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(ip) => ip.to_string(),
    };
    let rule = format!(
        "sudo ufw allow from {subnet} to any port {port} proto tcp comment 'x2rock: clips and account events'"
    );
    Hint::new(
        format!(
            "the speaker accepted the clip but never fetched it from {url} within {}s. It has to \
             open a connection to this machine, and a firewall here is the usual reason it \
             cannot; on ufw: {rule}",
            wait.as_secs()
        ),
        Code::ClipNotFetched,
        None,
    )
    .with_data(json!({ "url": url, "port": port, "firewall_rule": rule }))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_yields_method_and_path_without_the_query() {
        assert_eq!(
            parse_request("GET /a.mp3?x=1 HTTP/1.1\r\nHost: h\r\n\r\n"),
            Some(("GET", "/a.mp3"))
        );
        assert_eq!(
            parse_request("HEAD /a.mp3 HTTP/1.0\r\n\r\n"),
            Some(("HEAD", "/a.mp3"))
        );
        assert_eq!(parse_request("nonsense\r\n\r\n"), None);
        assert_eq!(parse_request(""), None);
    }

    /// One request against `answer`, as a client would send it: whether the
    /// clip was counted as sent, and everything the client read back.
    async fn exchange(request: &[u8]) -> (bool, String) {
        let (mut client, server) = tokio::io::duplex(4096);
        let served =
            tokio::spawn(async move { answer(server, "/c.mp3", "audio/mpeg", b"ID3fake").await });
        client.write_all(request).await.unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).await.unwrap();
        (
            served.await.unwrap().unwrap(),
            String::from_utf8_lossy(&got).into_owned(),
        )
    }

    #[tokio::test]
    async fn a_get_of_the_path_sends_the_bytes_and_anything_else_does_not() {
        // GET: the whole clip, after a head that states its length.
        let (sent, text) = exchange(b"GET /c.mp3 HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(sent);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Length: 7\r\n"), "{text}");
        assert!(text.ends_with("ID3fake"), "{text}");

        // HEAD: the head alone, and it does not count as fetched.
        let (sent, text) = exchange(b"HEAD /c.mp3 HTTP/1.1\r\n\r\n").await;
        assert!(!sent);
        assert!(text.ends_with("\r\n\r\n"), "{text}");

        // The wrong path: a 404 and nothing else.
        let (sent, text) = exchange(b"GET /other HTTP/1.1\r\n\r\n").await;
        assert!(!sent);
        assert!(text.starts_with("HTTP/1.1 404"), "{text}");
    }

    /// The whole thing against a loopback "player": the trigger is handed a
    /// URL, a client fetches it, and the serve returns having counted one.
    #[tokio::test]
    async fn serves_the_clip_to_the_player_that_was_triggered() {
        let me: IpAddr = Ipv4Addr::LOCALHOST.into();
        let served = serve_once(
            b"ID3 some mp3",
            "audio/mpeg",
            "x.mp3",
            me,
            0,
            Duration::from_secs(5),
            |url| async move {
                assert!(url.starts_with("http://127.0.0.1:"));
                assert!(url.ends_with("/x.mp3"));
                let rest = url.strip_prefix("http://").unwrap();
                let (authority, file) = rest.split_once('/').unwrap();
                let path = format!("/{file}");
                let authority = authority.to_string();
                tokio::spawn(async move {
                    let mut s = tokio::net::TcpStream::connect(authority).await.unwrap();
                    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                        .await
                        .unwrap();
                    let mut got = Vec::new();
                    s.read_to_end(&mut got).await.unwrap();
                    assert!(got.ends_with(b"ID3 some mp3"));
                });
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(served.requests, 1);
        // Measured to the fetch itself, not to the end of the grace after it.
        assert!(served.waited < GRACE, "{:?}", served.waited);
    }

    #[tokio::test]
    async fn a_player_that_never_comes_is_clip_not_fetched_with_the_rule_in_data() {
        let err = serve_once(
            &[1, 2, 3],
            "audio/mpeg",
            "x.mp3",
            IpAddr::V4(Ipv4Addr::new(192, 168, 77, 94)),
            0,
            Duration::from_millis(50),
            |_url| async { Ok(()) },
        )
        .await
        .unwrap_err();
        let (code, fix) = crate::hint::of(&err);
        assert_eq!(code, Code::ClipNotFetched);
        assert!(fix.is_none());
        let data = crate::hint::error_json(&err);
        let rule = data["firewall_rule"].as_str().unwrap();
        assert!(rule.contains("192.168.77.0/24"), "{rule}");
        assert!(rule.contains("proto tcp"), "{rule}");
    }
}
