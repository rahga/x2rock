pub mod api;
pub mod http;
pub mod local;
pub mod plex;
pub mod proto;
pub mod smapi;
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
