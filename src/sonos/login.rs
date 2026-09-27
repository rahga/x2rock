//! Signing in to a Sonos account, for a household that has **Authentication** on.
//!
//! The one exception to the axiom, and scoped to exactly this (see "The axiom,
//! stated precisely" in docs/architecture.md): with Connection security's
//! Authentication switched on, the speakers refuse every command from a client
//! that cannot say who it is. An access token from the person's Sonos account is
//! what they accept - as `Authorization: Bearer` on the local REST API, and on
//! the WebSocket as an `authorization` field in each command's header, never in
//! the handshake (verified 2026-09-27). Control stays on the LAN; the token only
//! proves who is asking.
//!
//! Nothing here runs unless the person has signed in. A household with
//! Authentication off, which is the default, never sees a token: no file, no
//! header, no call to Sonos.
//!
//! **The integration is the person's own.** Sonos issues OAuth credentials to a
//! registered *integration*, and a client secret cannot be kept secret inside an
//! open-source binary, so x2rock ships none: the key, secret and redirect URI are
//! read from `$XDG_CONFIG_HOME/x2rock/sonos-integration.json`, which the person
//! creates at developer.sonos.com. The redirect has to be a public HTTPS URL, so
//! the code comes back by copy and paste: rahga.github.io/x2rock/callback.html
//! shows the address it was opened with and sends it nowhere.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::sonos::http;
use crate::store;

const AUTHORIZE: &str = "https://api.sonos.com/login/v3/oauth";
const TOKEN: &str = "https://api.sonos.com/login/v3/oauth/access";
const SCOPE: &str = "playback-control-all";
const TIMEOUT: Duration = Duration::from_secs(15);
/// Refreshed this long before it runs out, so no command is sent with a token
/// that expires on its way to the speaker.
const MARGIN: u64 = 600;

/// The person's registered integration, from developer.sonos.com.
#[derive(Debug, Deserialize)]
pub struct Integration {
    pub key: String,
    pub secret: String,
    pub redirect_uri: String,
}

/// `$XDG_CONFIG_HOME/x2rock/sonos-integration.json`. Configuration the person
/// writes, not state x2rock learns, hence the config directory.
pub fn integration_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    Ok(dirs.config_dir().join("sonos-integration.json"))
}

pub fn integration() -> Result<Integration> {
    let path = integration_path()?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        anyhow!(
            "no Sonos integration at {}. Signing in needs one of your own: create a Control \
             Integration and a key at developer.sonos.com, with the redirect URI \
             https://rahga.github.io/x2rock/callback.html, and save \
             {{\"key\": ..., \"secret\": ..., \"redirect_uri\": ...}} there, readable only by you",
            path.display()
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

/// What the token endpoint answers, plus when it was obtained.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub obtained_at: u64,
}

impl Token {
    fn fresh(&self, now: u64) -> bool {
        now + MARGIN < self.obtained_at + self.expires_in
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn token_path() -> Result<PathBuf> {
    store::path("sonos-token.json")
}

fn save(token: &Token) -> Result<()> {
    store::write_atomically(
        &token_path()?,
        &serde_json::to_string(token)?,
        store::SECRET,
    )
}

fn load() -> Option<Token> {
    let text = std::fs::read_to_string(token_path().ok()?).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `state` of a sign-in that has been started and not finished, so that a
/// second `x2rock login '<address>'` can finish it - the only way through where
/// nothing can be pasted into a prompt (a command run from an agent, a `!` line).
/// Mode 600: it is what makes a pasted code this machine's to redeem.
fn pending_path() -> Result<PathBuf> {
    store::path("sonos-login-pending")
}

pub fn remember_pending(state: &str) -> Result<()> {
    store::write_atomically(&pending_path()?, state, store::SECRET)
}

pub fn pending() -> Option<String> {
    let state = std::fs::read_to_string(pending_path().ok()?).ok()?;
    Some(state.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn forget_pending() {
    if let Ok(path) = pending_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a sign-in is held on this machine.
pub fn signed_in() -> bool {
    load().is_some()
}

/// Forget the token. Revoking the grant is done in the Sonos account itself.
pub fn sign_out() -> Result<bool> {
    let path = token_path()?;
    *CACHE.lock().unwrap() = Some(None);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// The page to send the person to, and the `state` it must come back with.
///
/// **`port` rides in the state**, as `<port>.<random>`, when x2rock is listening
/// for the browser on `127.0.0.1`. Sonos will only redirect to a public HTTPS
/// page, so the callback page reads the port back out and sends the browser on
/// to `http://127.0.0.1:<port>/callback` - the usual loopback sign-in, one hop
/// later. The state is echoed by Sonos untouched, which is what makes it the
/// one channel to the page that needs no configuration.
pub fn authorize_url(integration: &Integration, port: Option<u16>) -> Result<(String, String)> {
    let mut bytes = [0u8; 12];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .context("reading /dev/urandom for the sign-in state")?;
    let random: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let state = match port {
        Some(port) => format!("{port}.{random}"),
        None => random,
    };
    let url = format!(
        "{AUTHORIZE}?client_id={}&response_type=code&state={state}&scope={SCOPE}&redirect_uri={}",
        http::urlencode(&integration.key),
        http::urlencode(&integration.redirect_uri)
    );
    Ok((url, state))
}

/// The code out of what the person pasted - the whole callback address, or the
/// bare code - after checking the `state` it carries is the one sent.
pub fn code_from(pasted: &str, state: &str) -> Result<String> {
    let pasted = pasted.trim();
    let Some((_, query)) = pasted.split_once('?') else {
        // A bare code: nothing to check it against, and nothing to parse.
        if pasted.is_empty() || pasted.contains(char::is_whitespace) {
            bail!("that is not a sign-in address or code");
        }
        return Ok(pasted.to_string());
    };
    let param = |name: &str| {
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| v.to_string())
        })
    };
    if let Some(error) = param("error") {
        bail!("Sonos did not sign you in: {error}");
    }
    match param("state") {
        Some(back) if back == state => {}
        Some(_) => bail!("that address is from a different sign-in; run `x2rock login` again"),
        None => bail!("that address carries no state; paste the whole address the page shows"),
    }
    param("code").ok_or_else(|| anyhow!("that address carries no code"))
}

/// Wait for the browser to come back to `listener`, and finish the sign-in with
/// what it brings: the callback page forwards Sonos's redirect here. The tab
/// gets a page saying how it went, so it can be closed.
///
/// Anything but `/callback` - a favicon, a stray prefetch - is answered 404 and
/// waited past. The first request to `/callback` decides it: a code that fails
/// its `state` check, or that Sonos will not exchange, is the answer, not a
/// reason to keep listening for another.
pub async fn catch_redirect(
    listener: tokio::net::TcpListener,
    integration: &Integration,
    state: &str,
) -> Result<Token> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener.accept().await.context("waiting for the browser")?;
        let mut head = Vec::new();
        let mut chunk = [0u8; 2048];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 16 * 1024 {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
        let text = String::from_utf8_lossy(&head);
        let target = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if !target.starts_with("/callback") {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let outcome = match code_from(target, state) {
            Ok(code) => exchange(integration, &code).await,
            Err(e) => Err(e),
        };
        let (title, line) = match &outcome {
            Ok(_) => (
                "Signed in",
                "x2rock is signed in. You can close this tab.".to_string(),
            ),
            Err(e) => ("Not signed in", format!("x2rock could not sign in: {e:#}")),
        };
        let page = format!(
            "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <title>x2rock: {title}</title></head><body style=\"font:16px system-ui;\
             max-width:40em;margin:4em auto;padding:0 1em\"><h1>{title}</h1><p>{}</p>\
             </body></html>",
            line.replace('&', "&amp;").replace('<', "&lt;")
        );
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        return outcome;
    }
}

/// POST to the token endpoint with the integration's credentials.
async fn token_request(integration: &Integration, form: &str) -> Result<Token> {
    let (endpoint, path, tls) = http::parse_url(TOKEN)?;
    let basic = format!(
        "Basic {}",
        base64(format!("{}:{}", integration.key, integration.secret).as_bytes())
    );
    let (status, body) = http::post(
        &endpoint,
        tls,
        &path,
        &[
            ("Authorization", &basic),
            (
                "Content-Type",
                "application/x-www-form-urlencoded;charset=utf-8",
            ),
        ],
        form,
        TIMEOUT,
    )
    .await
    .context("reaching Sonos's sign-in service")?;
    if status != 200 {
        // The body names the error (`invalid_grant`, `invalid_client`) and holds
        // nothing secret: it is the refusal, not a token.
        bail!(
            "Sonos's sign-in service answered HTTP {status}: {}",
            body.trim()
        );
    }
    let mut token: Token = serde_json::from_str(&body).context("reading the token")?;
    token.obtained_at = now();
    Ok(token)
}

/// Trade the code for a token, and keep it.
pub async fn exchange(integration: &Integration, code: &str) -> Result<Token> {
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}",
        http::urlencode(code),
        http::urlencode(&integration.redirect_uri)
    );
    let token = token_request(integration, &form).await?;
    save(&token)?;
    *CACHE.lock().unwrap() = Some(Some(token.clone()));
    Ok(token)
}

/// The token in use, read once and then held - including the answer "none",
/// so a process that never signed in does not re-read a missing file on every
/// command it sends. The outer `None` is "not read yet".
static CACHE: Mutex<Option<Option<Token>>> = Mutex::new(None);

/// The bearer to present, when a sign-in is held - refreshed first if it is
/// about to run out. `None` when nobody has signed in, which is every household
/// with Authentication off.
///
/// A refresh that fails keeps the token it has: a speaker that refuses it says
/// so with `authentication_required`, which names the way back, and failing
/// here instead would stop even a household with Authentication off.
///
/// **This reaches Sonos's cloud from inside the daemon**, once a day, which the
/// rule "talking to a service never enters the daemon" otherwise forbids. The
/// exception is the axiom's own: a daemon that cannot refresh stops being able
/// to control a household with Authentication on a day after sign-in.
pub async fn bearer() -> Option<String> {
    let held = CACHE.lock().unwrap().get_or_insert_with(load).clone()?;
    if held.fresh(now()) {
        return Some(held.access_token);
    }
    match refresh(&held).await {
        Ok(token) => Some(token.access_token),
        Err(_) => Some(held.access_token),
    }
}

async fn refresh(held: &Token) -> Result<Token> {
    let integration = integration()?;
    let form = format!(
        "grant_type=refresh_token&refresh_token={}",
        http::urlencode(&held.refresh_token)
    );
    let mut token = token_request(&integration, &form).await?;
    // Sonos returns the same refresh token, but keep the old one if a reply
    // ever leaves it out.
    if token.refresh_token.is_empty() {
        token.refresh_token.clone_from(&held.refresh_token);
    }
    save(&token)?;
    *CACHE.lock().unwrap() = Some(Some(token.clone()));
    Ok(token)
}

/// Standard base64, for the one Basic credential this module sends.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"key:secret"), "a2V5OnNlY3JldA==");
    }

    #[test]
    fn the_code_comes_back_only_with_the_state_that_was_sent() {
        let url = "https://rahga.github.io/x2rock/callback.html?state=abc&code=1AcY";
        assert_eq!(code_from(url, "abc").unwrap(), "1AcY");
        assert!(code_from(url, "xyz").is_err(), "another sign-in's address");
        assert_eq!(
            code_from("  1AcY \n", "abc").unwrap(),
            "1AcY",
            "a bare code"
        );
        let refused = "https://x/callback.html?error=access_denied&state=abc";
        assert!(format!("{:#}", code_from(refused, "abc").unwrap_err()).contains("access_denied"));
        assert!(
            code_from("https://x/callback.html?code=1AcY", "abc").is_err(),
            "no state"
        );
    }

    /// The callback page reads the port back out of the state, so the format
    /// is a contract with a file that lives in another repository.
    #[test]
    fn the_listening_port_leads_the_state_and_survives_the_round_trip() {
        let integration = Integration {
            key: "k".into(),
            secret: "s".into(),
            redirect_uri: "https://example/callback.html".into(),
        };
        let (url, state) = authorize_url(&integration, Some(38431)).unwrap();
        let (port, random) = state.split_once('.').unwrap();
        assert_eq!(port, "38431");
        assert_eq!(random.len(), 24);
        assert!(url.contains(&format!("state={state}")));
        let (_, bare) = authorize_url(&integration, None).unwrap();
        assert!(!bare.contains('.'), "no listener, no port");
        let back = format!("/callback?state={state}&code=abc");
        assert_eq!(code_from(&back, &state).unwrap(), "abc");
    }

    #[test]
    fn a_token_is_refreshed_before_it_runs_out_not_after() {
        let token = Token {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 86400,
            obtained_at: 1_000,
        };
        assert!(token.fresh(1_000));
        assert!(token.fresh(1_000 + 86400 - MARGIN - 1));
        assert!(!token.fresh(1_000 + 86400 - MARGIN));
    }
}
