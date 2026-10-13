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
//! Nothing here reaches Sonos unless the person has signed in. Authentication
//! off needs no sign-in, though a person can opt in for cloud content browsing.
//! Long-running clients re-read the saved token so a later login or logout
//! takes effect without restarting the daemon.
//!
//! **The integration is the person's own.** Sonos issues OAuth credentials to a
//! registered *integration*, and a client secret cannot be kept secret inside an
//! open-source binary, so x2rock ships none: the key, secret and redirect URI are
//! read from `$XDG_CONFIG_HOME/x2rock/sonos-integration.json`, which the person
//! creates at developer.sonos.com. The redirect has to be a public HTTPS URL, so
//! rahga.github.io/x2rock/callback.html hands the browser on to x2rock listening
//! on `127.0.0.1` (see [`authorize_url`]), and shows the address for a paste when
//! the browser is on another machine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::credentials::now;
use crate::sonos::http;
use crate::store;

const AUTHORIZE: &str = "https://api.sonos.com/login/v3/oauth";
const TOKEN: &str = "https://api.sonos.com/login/v3/oauth/access";
const SCOPE: &str = "playback-control-all";
const TIMEOUT: Duration = Duration::from_secs(15);
/// How long the browser gets to send its request once connected, and how much
/// of one is read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST: usize = 16 * 1024;
/// Refreshed this long before it runs out, so no command is sent with a token
/// that expires on its way to the speaker.
const MARGIN: u64 = 600;
/// A failing token endpoint gets a pause before another command tries it.
const REFRESH_RETRY: Duration = Duration::from_secs(60);

/// The person's registered integration, from developer.sonos.com.
#[derive(Debug, Deserialize)]
pub struct Integration {
    pub key: String,
    pub secret: String,
    pub redirect_uri: String,
}

/// `$XDG_CONFIG_HOME/x2rock/sonos-integration.json`. Configuration the person
/// writes, not state x2rock learns, hence the config directory.
fn integration_path() -> Result<PathBuf> {
    store::config_path("sonos-integration.json")
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub obtained_at: u64,
}

impl Token {
    fn fresh(&self, now: u64) -> bool {
        now.saturating_add(MARGIN) < self.obtained_at.saturating_add(self.expires_in)
    }
}

fn token_path() -> Result<PathBuf> {
    store::path("sonos-token.json")
}

fn save(token: &Token) -> Result<()> {
    save_at(&token_path()?, token)
}

fn save_at(path: &Path, token: &Token) -> Result<()> {
    let _lock = store::Lock::exclusive(path)?;
    write_token(path, token)
}

/// Caller holds the token file's writer lock.
fn write_token(path: &Path, token: &Token) -> Result<()> {
    store::write_atomically(path, &serde_json::to_string(token)?, store::SECRET)
}

fn load() -> Option<Token> {
    load_at(&token_path().ok()?)
}

fn load_at(path: &Path) -> Option<Token> {
    let text = std::fs::read_to_string(path).ok()?;
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
    sign_out_at(&token_path()?)
}

fn sign_out_at(path: &Path) -> Result<bool> {
    let _lock = store::Lock::exclusive(path)?;
    match std::fs::remove_file(path) {
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
    use tokio::io::AsyncWriteExt;
    loop {
        let (mut socket, _) = listener.accept().await.context("waiting for the browser")?;
        // Bounded, so a browser's speculative connection that never sends a
        // request is answered 404 and waited past rather than waited on.
        let text = http::read_message(&mut socket, REQUEST_TIMEOUT, MAX_REQUEST)
            .await
            .unwrap_or_default();
        let target = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if !target.starts_with("/callback") {
            let _ = socket.write_all(http::NOT_FOUND).await;
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
            super::xml_escape(&line)
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
    let basic = format!(
        "Basic {}",
        super::base64::encode(format!("{}:{}", integration.key, integration.secret).as_bytes())
    );
    let (status, body) = http::post(
        TOKEN,
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
    Ok(token)
}

/// Coalesce this process's refreshes. The token itself is always read from
/// disk, since login, logout and other clients' refreshes replace it there.
#[derive(Default)]
struct RefreshState {
    failed: Option<(Token, tokio::time::Instant)>,
}

static REFRESH: Mutex<RefreshState> = Mutex::const_new(RefreshState { failed: None });

/// The bearer to present, when a sign-in is held - refreshed first if it is
/// about to run out. `None` when nobody has signed in.
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
    bearer_from(&token_path().ok()?, &REFRESH, |held| async move {
        refresh(&held).await
    })
    .await
}

async fn bearer_from<F, Fut>(
    path: &Path,
    refreshes: &Mutex<RefreshState>,
    refresh: F,
) -> Option<String>
where
    F: FnOnce(Token) -> Fut,
    Fut: Future<Output = Result<Token>>,
{
    let held = load_at(path)?;
    if held.fresh(now()) {
        return Some(held.access_token);
    }
    let mut state = refreshes.lock().await;
    // Another caller may have refreshed, signed out or signed in while this
    // one waited. Re-read before deciding to contact the token endpoint.
    let held = load_at(path)?;
    if held.fresh(now())
        || state
            .failed
            .as_ref()
            .is_some_and(|(token, at)| token == &held && at.elapsed() < REFRESH_RETRY)
    {
        return Some(held.access_token);
    }
    let current = match refresh(held.clone()).await {
        Ok(token) => {
            state.failed = None;
            save_refresh_at(path, &held, &token).unwrap_or_else(|_| load_at(path))
        }
        Err(_) => {
            state.failed = Some((held, tokio::time::Instant::now()));
            load_at(path)
        }
    };
    current.map(|token| token.access_token)
}

/// Network work happens without a file lock. Commit only if the sign-in it
/// refreshed still exists unchanged; a concurrent logout or replacement wins.
fn save_refresh_at(path: &Path, held: &Token, refreshed: &Token) -> Result<Option<Token>> {
    let _lock = store::Lock::exclusive(path)?;
    let current = load_at(path);
    if current.as_ref() != Some(held) {
        return Ok(current);
    }
    write_token(path, refreshed)?;
    Ok(Some(refreshed.clone()))
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
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_token(access: &str) -> Token {
        Token {
            access_token: access.into(),
            refresh_token: "dummy-refresh".into(),
            expires_in: 86400,
            obtained_at: now(),
        }
    }

    async fn no_refresh(_: Token) -> Result<Token> {
        panic!("a fresh or missing token should not contact Sonos")
    }

    #[tokio::test]
    async fn a_running_client_observes_login_logout_and_replacement() {
        let dir = crate::testdir::TempDir::new("login-lifecycle");
        let path = dir.path().join("token.json");
        let cache = Mutex::new(RefreshState::default());
        assert_eq!(bearer_from(&path, &cache, no_refresh).await, None);
        save_at(&path, &fresh_token("first")).unwrap();
        assert_eq!(
            bearer_from(&path, &cache, no_refresh).await.as_deref(),
            Some("first")
        );
        save_at(&path, &fresh_token("replacement")).unwrap();
        assert_eq!(
            bearer_from(&path, &cache, no_refresh).await.as_deref(),
            Some("replacement")
        );
        sign_out_at(&path).unwrap();
        assert_eq!(bearer_from(&path, &cache, no_refresh).await, None);
    }

    #[tokio::test]
    async fn a_failed_refresh_does_not_keep_using_a_removed_sign_in() {
        let dir = crate::testdir::TempDir::new("login-failed-refresh");
        let path = dir.path().join("token.json");
        let mut expired = fresh_token("expired");
        expired.expires_in = 0;
        save_at(&path, &expired).unwrap();
        let cache = Mutex::new(RefreshState::default());
        let token = bearer_from(&path, &cache, |_| async {
            sign_out_at(&path).unwrap();
            bail!("simulated refresh failure")
        })
        .await;
        assert_eq!(token, None);
    }

    #[tokio::test]
    async fn a_refresh_cannot_restore_a_logged_out_or_replaced_sign_in() {
        let dir = crate::testdir::TempDir::new("login-refresh-race");
        let path = dir.path().join("token.json");
        let mut expired = fresh_token("old");
        expired.expires_in = 0;
        let refreshes = Mutex::new(RefreshState::default());
        for replacement in [None, Some(fresh_token("new-login"))] {
            save_at(&path, &expired).unwrap();
            let answer = bearer_from(&path, &refreshes, |_| async {
                match &replacement {
                    Some(token) => save_at(&path, token).unwrap(),
                    None => {
                        sign_out_at(&path).unwrap();
                    }
                }
                Ok(fresh_token("late-refresh"))
            })
            .await;
            assert_eq!(answer, replacement.as_ref().map(|t| t.access_token.clone()));
            assert_eq!(load_at(&path), replacement);
        }
    }

    #[test]
    fn the_first_committed_refresh_wins_over_another_processs_stale_reply() {
        let dir = crate::testdir::TempDir::new("login-refresh-commit");
        let path = dir.path().join("token.json");
        let held = fresh_token("old");
        save_at(&path, &held).unwrap();
        let first = fresh_token("first-refresh");
        save_refresh_at(&path, &held, &first).unwrap();
        let late = fresh_token("late-refresh");
        assert_eq!(
            save_refresh_at(&path, &held, &late).unwrap(),
            Some(first.clone())
        );
        assert_eq!(load_at(&path), Some(first));
    }

    #[tokio::test]
    async fn simultaneous_commands_share_one_refresh() {
        let dir = crate::testdir::TempDir::new("login-shared-refresh");
        let path = dir.path().join("token.json");
        let mut expired = fresh_token("old");
        expired.expires_in = 0;
        save_at(&path, &expired).unwrap();
        let refreshes = Mutex::new(RefreshState::default());
        let calls = std::cell::Cell::new(0);
        let request = |_| async {
            calls.set(calls.get() + 1);
            tokio::task::yield_now().await;
            Ok(fresh_token("refreshed"))
        };
        let (first, second) = tokio::join!(
            bearer_from(&path, &refreshes, request),
            bearer_from(&path, &refreshes, request),
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(first.as_deref(), Some("refreshed"));
        assert_eq!(first, second);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_refresh_has_a_retry_delay_but_does_not_block_a_new_login() {
        let dir = crate::testdir::TempDir::new("login-refresh-retry");
        let path = dir.path().join("token.json");
        let mut expired = fresh_token("old");
        expired.expires_in = 0;
        save_at(&path, &expired).unwrap();
        let refreshes = Mutex::new(RefreshState::default());
        let calls = std::cell::Cell::new(0);
        let fail = |_| async {
            calls.set(calls.get() + 1);
            tokio::task::yield_now().await;
            bail!("simulated endpoint failure")
        };
        let (first, second) = tokio::join!(
            bearer_from(&path, &refreshes, fail),
            bearer_from(&path, &refreshes, fail),
        );
        assert_eq!(first.as_deref(), Some("old"));
        assert_eq!(first, second);
        assert_eq!(
            calls.get(),
            1,
            "waiting callers should not each retry a failed refresh"
        );
        tokio::time::advance(REFRESH_RETRY).await;
        bearer_from(&path, &refreshes, fail).await;
        assert_eq!(calls.get(), 2, "failure should not disable refresh forever");

        expired.access_token = "new-login".into();
        save_at(&path, &expired).unwrap();
        let answer = bearer_from(&path, &refreshes, |_| async {
            Ok(fresh_token("new-refresh"))
        })
        .await;
        assert_eq!(answer.as_deref(), Some("new-refresh"));
    }

    #[test]
    fn refresh_replies_can_omit_the_refresh_token() {
        let token: Token =
            serde_json::from_str(r#"{"access_token":"new","expires_in":86400}"#).unwrap();
        assert!(
            token.refresh_token.is_empty(),
            "refresh retains the previous token in this case"
        );
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
