//! Plex account linking, and why it does not go through SMAPI.
//!
//! Plex's SMAPI endpoint (`https://sonos.plex.tv/v2.2/soap`) implements the
//! content half of the protocol - `search`, `getMetadata`, `getMediaURI` - and
//! honours a **plain Plex account token** as the `loginToken/token` in the
//! credentials header (verified 2026-08-31: a search over it returned the track
//! the household was playing at that moment). Its *link* half is dead, though:
//! `getAppLink` answers `Server.ServiceUnknownError` and `getDeviceLinkCode`
//! answers `Client.AuthTokenExpired`, whatever the credentials header carries -
//! with or without a `loginToken`, a `deviceId`, or the extra WSDL fields.
//!
//! So the token comes from Plex directly, through the PIN flow Plex publishes
//! for any client (`plex.tv/api/v2/pins` - the same flow every third-party Plex
//! app uses): mint a pin, send the person to a browser page that claims it, and
//! poll the pin until it carries the account's token. The shape is exactly a
//! SMAPI device link - regUrl, code, poll - which is why `x2rock link Plex`
//! looks identical to any other link from the outside.
//!
//! This is the first service-specific auth path in x2rock. It earns the
//! exception by being the service's own published flow, not a scraped one, and
//! the token it mints is visible and revocable on the account's device list at
//! plex.tv.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use roxmltree::Document;

use super::http;

/// Plex's id in the Sonos service catalogue. Stable where the name is not,
/// which is the rule credentials are keyed by.
pub const SERVICE_ID: &str = "212";

/// plex.tv answers fast, but it is an internet service like any SMAPI one and
/// gets the same budget.
const TIMEOUT: Duration = Duration::from_secs(6);

/// A pin waiting to be claimed in a browser.
#[derive(Debug, Clone)]
pub struct Pin {
    id: u64,
    code: String,
    /// The client identifier the pin was minted under. The poll must present
    /// the same one, and the token ends up filed under it on the account's
    /// device list.
    client: String,
}

/// The identifier this machine links as. Stable so that re-linking replaces
/// the device entry on the Plex account instead of growing a new one each time.
fn client_identifier() -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_default();
    match host.is_empty() {
        true => "x2rock".to_string(),
        false => format!("x2rock-{host}"),
    }
}

/// Mint a pin. Returns the pin to poll and the URL to put in front of the
/// person - `app.plex.tv/auth` with the code embedded, so logging in is the
/// whole interaction and nothing has to be typed.
pub async fn pin() -> Result<(Pin, String)> {
    let client = client_identifier();
    // `strong=true` asks for a code that can ride in the auth URL; the weak
    // four-character kind is for typing into plex.tv/link, which would put a
    // transcription step in a flow that does not need one. The X-Plex fields
    // travel as query parameters, which plex.tv accepts everywhere it accepts
    // the headers.
    let path = format!(
        "/api/v2/pins?strong=true&X-Plex-Product=x2rock&X-Plex-Client-Identifier={}",
        http::urlencode(&client)
    );
    let (endpoint, _, tls) = http::parse_url("https://plex.tv/")?;
    let (status, body) = http::post(&endpoint, tls, &path, &[], "", TIMEOUT).await?;
    if status != 201 && status != 200 {
        bail!("plex.tv refused to mint a link pin: HTTP {status}");
    }
    let (id, code, _) = parse_pin(&body)?;
    let url = format!(
        "https://app.plex.tv/auth#?clientID={}&code={}&context%5Bdevice%5D%5Bproduct%5D=x2rock",
        http::urlencode(&client),
        http::urlencode(&code)
    );
    Ok((Pin { id, code, client }, url))
}

/// Ask whether the pin has been claimed. `Ok(None)` is *pending*, exactly like
/// `smapi::device_auth_token`: the reply is well-formed and simply carries no
/// token until the person finishes in the browser.
pub async fn poll(pin: &Pin) -> Result<Option<String>> {
    let url = format!(
        "https://plex.tv/api/v2/pins/{}?code={}&X-Plex-Client-Identifier={}",
        pin.id,
        http::urlencode(&pin.code),
        http::urlencode(&pin.client)
    );
    let (status, body) = http::get(&url, TIMEOUT).await?;
    // An expired or foreign pin is a 404; anything else unexpected is worth
    // showing rather than polling through.
    if status == 404 {
        bail!("the Plex link pin expired before it was claimed");
    }
    if status != 200 {
        bail!("plex.tv answered HTTP {status} while waiting for the link");
    }
    let (_, _, token) = parse_pin(&body)?;
    Ok(token)
}

/// Read `(id, code, authToken)` out of a pin document. plex.tv answers XML by
/// default - one `<pin>` element with everything as attributes - and an empty
/// `authToken` means unclaimed, not missing.
fn parse_pin(body: &str) -> Result<(u64, String, Option<String>)> {
    let doc = Document::parse(body).context("parsing plex.tv pin response")?;
    let pin = doc
        .descendants()
        .find(|n| n.has_tag_name("pin"))
        .ok_or_else(|| anyhow!("plex.tv answered without a pin element"))?;
    let id = pin
        .attribute("id")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| anyhow!("plex.tv pin carries no id"))?;
    let code = pin
        .attribute("code")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("plex.tv pin carries no code"))?;
    let token = pin
        .attribute("authToken")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok((id, code, token))
}

/// The Plex token a player's own art URL carries, if any.
///
/// The household's Plex integration puts its token in every Plex art URL the
/// players hand out - `getMetadataStatus` image URLs end in
/// `...%26X-Plex-Token%3D<token>&width=300` - so any controller on the LAN can
/// read it unauthenticated, exactly as `keep` reads object ids. That token is
/// the household integration's own: it does everything the SMAPI endpoint
/// offers (root browse included, which a fresh account token cannot on a
/// server without Remote Access), and it dies whenever the owner relinks Plex
/// to Sonos. `x2rock link plex --from-player` stores it deliberately, with
/// that trade written down.
///
/// The token appears percent-encoded (`%3D`) inside the outer URL, but not
/// always, so both spellings of the `=` are accepted.
pub fn token_in(url: &str) -> Option<String> {
    let at = url.find("X-Plex-Token")? + "X-Plex-Token".len();
    let rest = &url[at..];
    let rest = rest
        .strip_prefix("=")
        .or_else(|| rest.strip_prefix("%3D"))
        .or_else(|| rest.strip_prefix("%3d"))?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    match token.is_empty() {
        true => None,
        false => Some(token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unclaimed_pin_parses_as_pending() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <pin id="12345" code="abcDEF123" product="x2rock" authToken=""/>"#;
        let (id, code, token) = parse_pin(body).unwrap();
        assert_eq!(id, 12345);
        assert_eq!(code, "abcDEF123");
        assert_eq!(token, None);
    }

    #[test]
    fn a_claimed_pin_carries_the_token() {
        let body = r#"<pin id="9" code="c" authToken="tok-123"/>"#;
        let (_, _, token) = parse_pin(body).unwrap();
        assert_eq!(token.as_deref(), Some("tok-123"));
    }

    #[test]
    fn a_reply_without_a_pin_is_an_error_naming_plex() {
        let err = parse_pin("<errors><error>nope</error></errors>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("without a pin"), "{err}");
    }

    #[test]
    fn the_token_is_read_out_of_a_real_art_url() {
        // The shape a player actually hands out: the token percent-encoded
        // inside the outer sonos.plex.tv wrapper, followed by an outer query
        // parameter of its own.
        let url = "https://sonos.plex.tv/img?height=1&url=https%3A%2F%2Fx.plex.direct%3A13502%2Flibrary%2Fmetadata%2F68162%2Fthumb%2F1%3FX-Plex-Client-Identifier%3Dsonos-abc%26X-Plex-Token%3Dz8mPgbR9RQEbBatkru1V&width=300";
        assert_eq!(token_in(url).as_deref(), Some("z8mPgbR9RQEbBatkru1V"));
        // A plain, un-encoded spelling works too, and absence is None.
        assert_eq!(
            token_in("http://x/y?X-Plex-Token=abc-123&z=1").as_deref(),
            Some("abc-123")
        );
        assert_eq!(token_in("http://x/y?X-Plex-Token="), None);
        assert_eq!(token_in("http://x/y?width=300"), None);
    }

    #[test]
    fn the_identifier_survives_url_encoding() {
        assert_eq!(http::urlencode("x2rock-my.host"), "x2rock-my.host");
        assert_eq!(http::urlencode("a b:c"), "a%20b%3ac");
    }
}
