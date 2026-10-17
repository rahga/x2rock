pub mod api;
pub mod base64;
pub mod cloud;
pub mod http;
pub mod local;
pub mod login;
pub mod plex;
pub mod proto;
pub mod smapi;
pub mod stored;
pub mod upnp;

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

/// The host address out of a player-published URL, whatever the scheme.
///
/// Two documents hand these out - `getGroups` gives `wss://<ip>:1443/…` per
/// player and the topology gives `http://<ip>:1400/xml/…` - and both parsers
/// used to carry their own copy of this, which had already drifted (one split
/// on `:` alone, the other on `:` and `/`) and shared a bug: a bracketed IPv6
/// host splits at the colon *inside* the brackets. Sonos publishes IPv4 today,
/// so the v6 arm is untrodden - handled anyway, because the failure would be a
/// reachable player silently reported as having no address.
pub(crate) fn host_ip(url: &str) -> Option<IpAddr> {
    let rest = url.split_once("://")?.1;
    let host = match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => rest.split([':', '/']).next()?,
    };
    host.parse().ok()
}

/// One element's text, by local name - the lookup every reply parser does,
/// in SOAP replies from the players and from music services alike. Written out
/// longhand six times in one file before it had a name, then twice more.
pub(crate) fn element_text<'a>(doc: &'a roxmltree::Document, tag: &str) -> Option<&'a str> {
    doc.descendants()
        .find(|n| n.tag_name().name() == tag)
        .and_then(|n| n.text())
}

/// The text of `node`'s first direct child with this local name - the
/// per-row counterpart to [`element_text`], for a parser walking a list of
/// rows rather than reading one value out of a whole reply. Five closures
/// across `upnp` and `smapi` spelled this out before it had a name.
pub(crate) fn child_text<'a>(node: roxmltree::Node<'a, '_>, tag: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.tag_name().name() == tag)
        .and_then(|c| c.text())
}

/// The longest a service-supplied *display* string is kept: a title, a second
/// line, a description. A row needs none of it beyond this, and an unbounded
/// one does not stop at the row - it travels through `--json` into the bar
/// widget, a long-lived process that parses and holds whatever arrives. So the
/// field is bounded where it is read, once, rather than trusted to the widget.
/// Identifiers (ids, URIs, `r:resMD`) are never clipped: they must arrive whole
/// for the track to play.
pub(crate) const MAX_FIELD_CHARS: usize = 512;

/// The longest a service-supplied URL is kept. Over this it is dropped rather
/// than clipped - a truncated URL is a broken fetch, so no cover art is the
/// better answer than a request that cannot succeed.
pub(crate) const MAX_URL_BYTES: usize = 8 * 1024;

/// Clip a display string to [`MAX_FIELD_CHARS`], on a char boundary so the
/// result stays valid UTF-8. A string already within the bound is untouched.
pub(crate) fn clip(mut s: String) -> String {
    if let Some((at, _)) = s.char_indices().nth(MAX_FIELD_CHARS) {
        s.truncate(at);
    }
    s
}

/// [`clip`] over an optional second line.
pub(crate) fn clip_opt(s: Option<String>) -> Option<String> {
    s.map(clip)
}

/// Drop a URL past [`MAX_URL_BYTES`]; keep a sane one as it is.
pub(crate) fn clip_url(url: Option<String>) -> Option<String> {
    url.filter(|u| u.len() <= MAX_URL_BYTES)
}

/// Text made safe to sit inside an XML element or attribute value.
///
/// One function serves SOAP arguments and DIDL-Lite alike. Four copies of
/// this chain had grown up - two in `bookmarks`, one each in `upnp` and
/// `smapi` - before it was written once.
///
/// All five of XML's predefined entities. Nothing here writes a single-quoted
/// attribute, so `'` was safe left alone; escaping it as well means no future
/// one can be broken by a title like "Don't Stop".
pub fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The one rustls crypto backend in this binary, named in one place.
///
/// Both TLS paths build their config from this: the player websockets in
/// `local.rs` and the service client in `http.rs`. Naming the provider
/// explicitly instead of using `ClientConfig::builder()` is deliberate even
/// though only one backend is compiled in today: `builder()` panics at the
/// first handshake the day any dependency re-enables rustls's default
/// `aws_lc_rs` feature and a second provider links back in, while a named
/// provider keeps working.
///
/// The backend is also named in `Cargo.toml`, in the feature lists of `rustls`
/// and `tokio-rustls`; swapping it means changing those two lines and this
/// function together. Why `ring` and not the `aws-lc-rs` default is recorded
/// in docs/architecture.md ("`ring` over `aws-lc-rs`, deliberately").
pub(crate) fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{MAX_FIELD_CHARS, MAX_URL_BYTES, clip, clip_url, xml_escape};

    #[test]
    fn xml_escape_covers_the_five_reserved_characters() {
        assert_eq!(
            xml_escape(r#"a&b<c>"d"'e'"#),
            "a&amp;b&lt;c&gt;&quot;d&quot;&apos;e&apos;"
        );
    }

    #[test]
    fn clip_bounds_a_display_string_on_a_char_boundary() {
        let short = "a short title".to_string();
        assert_eq!(clip(short.clone()), short, "within the cap is untouched");

        // A multi-byte char sitting on the boundary must not be split.
        let long: String = "é".repeat(MAX_FIELD_CHARS + 50);
        let clipped = clip(long);
        assert_eq!(clipped.chars().count(), MAX_FIELD_CHARS);
        assert!(clipped.is_char_boundary(clipped.len()));
    }

    #[test]
    fn clip_url_drops_an_absurd_url_and_keeps_a_sane_one() {
        let sane = Some("https://cdn.example/art.jpg".to_string());
        assert_eq!(clip_url(sane.clone()), sane);
        assert_eq!(clip_url(Some("x".repeat(MAX_URL_BYTES + 1))), None);
        assert_eq!(clip_url(None), None);
    }
}
