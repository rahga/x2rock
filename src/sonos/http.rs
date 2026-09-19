//! The one HTTP client, deliberately minimal, over TCP or TLS.
//!
//! Lifted out of `upnp.rs` when SMAPI arrived. UPnP talks plain HTTP to a player
//! on the LAN; SMAPI talks HTTPS to a music service on the internet. The framing
//! is the same HTTP/1.1 in both directions, and there is little enough of it that
//! one small client beats a dependency.
//!
//! It reads to end of stream, so it needs `Connection: close` and handles
//! `Transfer-Encoding: chunked`, which is what both ends actually send.
//!
//! **Timeouts are the caller's, not this module's.** A player on the same switch
//! and a service in another country do not deserve the same patience, and search
//! is invoked from a bar widget where an unbounded wait is the failure mode that
//! matters. See "Rule: talking to a service never enters the daemon" in
//! docs/architecture.md.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Sent on every request that does not carry its own.
///
/// Not politeness. Deezer's SMAPI endpoint answers `getDeviceLinkCode` with an
/// empty HTTP 200 when the request has no `User-Agent`, which arrives here as
/// "answered HTTP 200 with an empty body" and made the service look dead - it
/// was recorded for a fortnight as Deezer being broken. Any non-empty value
/// satisfies it; verified 2026-09-17 against `api.deezer.com/sonos`, where even
/// `a` gets the real reply. A caller that sends its own `User-Agent` keeps it.
const AGENT: &str = concat!("x2rock/", env!("CARGO_PKG_VERSION"));

/// Where a request is going, and how to reach it.
///
/// `Lan` carries an address because players are found by scanning and their
/// names mean nothing to DNS. `Web` carries a hostname because TLS validates
/// against it, and because a service's endpoint arrives as a URL.
pub enum Endpoint {
    Lan { ip: IpAddr, port: u16 },
    Web { host: String, port: u16 },
}

impl Endpoint {
    fn authority(&self) -> String {
        match self {
            Self::Lan { ip, port } => format!("{ip}:{port}"),
            Self::Web { host, port } => format!("{host}:{port}"),
        }
    }

    /// The `Host` header. Port 443 is elided because some services answer a
    /// literal `host:443` with a redirect to themselves.
    fn host_header(&self) -> String {
        match self {
            Self::Lan { ip, port } => format!("{ip}:{port}"),
            Self::Web { host, port } if *port == 443 => host.clone(),
            Self::Web { host, port } => format!("{host}:{port}"),
        }
    }
}

/// Split `https://host[:port]/path` into an endpoint and a path.
///
/// Only the two schemes a Sonos deployment produces are accepted; anything else
/// is a service descriptor we have misread, and guessing at it would turn that
/// into a confusing network error later.
pub fn parse_url(url: &str) -> Result<(Endpoint, String, bool)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("not a URL: {url}"))?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        other => bail!("unsupported scheme {other:?} in {url}"),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // A bare IPv6 authority would need brackets; no Sonos service uses one, and
    // accepting it silently would mis-split on the colons.
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h, p.parse().context("port")?)
        }
        _ => (authority, if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        bail!("no host in {url}");
    }
    Ok((
        Endpoint::Web {
            host: host.to_string(),
            port,
        },
        path.to_string(),
        tls,
    ))
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            // Real certificate validation here, unlike the player socket in
            // `local.rs`: a music service is a public host with a real name and
            // a real chain, and this one leaves the LAN.
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            // The shared provider, named rather than taken from the process
            // default; see `sonos::crypto_provider` for why.
            let provider = super::crypto_provider();
            Arc::new(
                rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .expect("the default provider supports the default protocol versions")
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// One HTTP/1.1 POST, returning `(status, body)`.
///
/// `timeout` bounds the whole exchange, connect included, so a caller always
/// gets an answer or an error within it.
pub async fn post(
    endpoint: &Endpoint,
    tls: bool,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
    timeout: Duration,
) -> Result<(u16, String)> {
    tokio::time::timeout(
        timeout,
        exchange(endpoint, tls, "POST", path, headers, Some(body)),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "timed out after {timeout:?} talking to {}",
            endpoint.authority()
        )
    })?
}

/// One HTTP/1.1 GET. Service manifests and presentation maps are plain
/// documents on a CDN, fetched with no credential of any kind.
pub async fn get(url: &str, timeout: Duration) -> Result<(u16, String)> {
    get_with(url, timeout, &[]).await
}

/// A GET that carries headers. Only the radio directory needs this so far, to
/// send the `User-Agent` its operators ask third-party clients for; Sonos's own
/// CDN wants nothing.
pub async fn get_with(
    url: &str,
    timeout: Duration,
    headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let (endpoint, path, tls) = parse_url(url)?;
    tokio::time::timeout(
        timeout,
        exchange(&endpoint, tls, "GET", &path, headers, None),
    )
    .await
    .map_err(|_| anyhow!("timed out after {timeout:?} fetching {url}"))?
}

/// Percent-encode one value for a URL: everything unreserved passes through,
/// everything else becomes `%xx` in lowercase hex.
///
/// Shared rather than copied - it was written for Plex's client identifier and
/// pin code, and a search term going to the radio directory needs exactly the
/// same treatment. A second copy would be a second chance to get it wrong.
pub fn urlencode(value: &str) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX_DIGITS[(b >> 4) as usize] as char);
                out.push(HEX_DIGITS[(b & 0xf) as usize] as char);
            }
        }
    }
    out
}

async fn exchange(
    endpoint: &Endpoint,
    tls: bool,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Result<(u16, String)> {
    let authority = endpoint.authority();
    let stream = TcpStream::connect(&authority)
        .await
        .with_context(|| format!("connecting to {authority}"))?;

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\n",
        endpoint.host_header()
    );
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
    {
        head.push_str(&format!("User-Agent: {AGENT}\r\n"));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    // A GET carries no Content-Length: some servers read one as a promise of a
    // body and wait for it.
    if let Some(body) = body {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let request = format!("{head}{}", body.unwrap_or(""));

    if tls {
        let Endpoint::Web { host, .. } = endpoint else {
            bail!("TLS needs a hostname to validate against");
        };
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .with_context(|| format!("not a valid TLS server name: {host}"))?;
        let stream = TlsConnector::from(tls_config())
            .connect(name, stream)
            .await
            .with_context(|| format!("TLS handshake with {host}"))?;
        round_trip(stream, &request, &authority).await
    } else {
        round_trip(stream, &request, &authority).await
    }
}

async fn round_trip<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    request: &str,
    authority: &str,
) -> Result<(u16, String)> {
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    // Read to end of stream, but tolerate an unclean one. Plenty of servers -
    // SomaFM among them - answer a `Connection: close` request by dropping the
    // socket without a TLS close_notify, which rustls reports as
    // `UnexpectedEof`. The response is already complete at that point, so
    // failing on it would turn a working service into an intermittent one.
    // A truncated body is still caught downstream, by the parse.
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut header_end: Option<usize> = None;
    let mut expected_len: Option<usize> = None;
    let mut is_chunked = false;

    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if header_end.is_none()
                    && let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    header_end = Some(split + 4);
                    let head = String::from_utf8_lossy(&raw[..split]);
                    for line in head.lines() {
                        let lower = line.to_ascii_lowercase();
                        if let Some(val) = lower.strip_prefix("content-length:") {
                            if let Ok(cl) = val.trim().parse::<usize>() {
                                expected_len = Some(split + 4 + cl);
                            }
                        } else if lower.starts_with("transfer-encoding:")
                            && lower.contains("chunked")
                        {
                            is_chunked = true;
                        }
                    }
                }
                if let Some(target) = expected_len {
                    if raw.len() >= target {
                        break;
                    }
                } else if is_chunked
                    && let Some(hend) = header_end
                    && raw[hend..].ends_with(b"0\r\n\r\n")
                {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(e) => return Err(e).with_context(|| format!("reading from {authority}")),
        }
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from {authority}"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];

    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("no HTTP status in response from {authority}"))?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    // Transfer-Encoding is unwrapped before Content-Encoding, which is the order
    // they were applied in.
    //
    // This client never asks for a compressed body - it sends no
    // `Accept-Encoding` at all, and HTTP/1.1 says that means any encoding is
    // acceptable rather than none. Deezer takes it at its word and gzips, and
    // ignores an explicit `Accept-Encoding: identity` too, so declining is not
    // on offer: the only way to read the reply is to be able to decompress it.
    let encoded = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("content-encoding:") && (l.contains("gzip") || l.contains("x-gzip"))
    });
    let body = if encoded {
        gunzip(&body).with_context(|| format!("gzip body from {authority}"))?
    } else {
        body
    };
    // Services answer UTF-8 and some of them lead with a BOM, which every XML
    // parser then refuses as content before the declaration.
    let text = String::from_utf8_lossy(&body).into_owned();
    Ok((status, text.trim_start_matches('\u{feff}').to_string()))
}

/// Unwrap one gzip member (RFC 1952) into the deflate stream inside it.
///
/// `miniz_oxide` decompresses raw deflate and zlib, not gzip, and the only
/// difference is the wrapper: a ten-byte header, up to four optional
/// variable-length fields, then deflate, then an eight-byte trailer. The
/// trailer needs no handling - inflate stops at the final block and ignores
/// what follows - so this is header arithmetic and nothing more. Pulling in a
/// second crate to do that much would be the larger change.
fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
    // Magic, then the compression method: deflate is the only one ever defined.
    let [0x1f, 0x8b, 0x08, flags, ..] = data else {
        bail!("not a gzip member");
    };
    let flags = *flags;
    let mut at = 10;
    // FEXTRA: a two-byte little-endian length, then that many bytes.
    if flags & 0b0000_0100 != 0 {
        let len = data
            .get(at..at + 2)
            .ok_or_else(|| anyhow!("truncated gzip extra field"))?;
        at += 2 + u16::from_le_bytes([len[0], len[1]]) as usize;
    }
    // FNAME and FCOMMENT: NUL-terminated strings, skipped including the NUL.
    for flag in [0b0000_1000, 0b0001_0000] {
        if flags & flag != 0 {
            let end = data
                .get(at..)
                .and_then(|rest| rest.iter().position(|b| *b == 0))
                .ok_or_else(|| anyhow!("unterminated gzip header string"))?;
            at += end + 1;
        }
    }
    // FHCRC: a two-byte CRC over the header, which is not checked here - a
    // corrupt header will fail the inflate that follows anyway.
    if flags & 0b0000_0010 != 0 {
        at += 2;
    }
    let deflate = data
        .get(at..)
        .ok_or_else(|| anyhow!("gzip header runs past the end of the body"))?;
    miniz_oxide::inflate::decompress_to_vec(deflate)
        .map_err(|e| anyhow!("would not decompress: {e:?}"))
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| anyhow!("truncated chunked body"))?;
        let size_text = std::str::from_utf8(&data[..line_end])?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_text, 16)
            .with_context(|| format!("bad chunk size {size_text:?}"))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        // `checked_add`, because `size` came off the wire: a hex size near
        // `usize::MAX` would wrap `size + 2` to something tiny, pass the length
        // check, and the slice below would panic - and this is the daemon,
        // which aborts on panic. A bad chunk header is a bad reply, not a crash.
        let Some(need) = size.checked_add(2) else {
            bail!("bad chunk size {size_text:?}");
        };
        if data.len() < need {
            bail!("truncated chunk");
        }
        out.extend_from_slice(&data[..size]);
        data = &data[need..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gunzip_reads_a_real_member_with_a_filename_in_its_header() {
        // Produced by gzip(1) with -N, so FNAME is set and the header is not
        // the bare ten bytes: the optional fields are the part worth pinning.
        const GZ: &[u8] = include_bytes!("testdata/hello.gz");
        assert_eq!(GZ[3] & 0b0000_1000, 0b0000_1000, "FNAME is set");
        assert_eq!(gunzip(GZ).unwrap(), b"<hello>SMAPI</hello>\n");
    }

    #[test]
    fn gunzip_refuses_what_is_not_a_member() {
        assert!(gunzip(b"<?xml version=\"1.0\"?>").is_err(), "plain XML");
        assert!(gunzip(&[0x1f, 0x8b]).is_err(), "truncated magic");
    }

    #[test]
    fn dechunk_reassembles_and_ignores_extensions() {
        // "in\r\n\r\nchunks." is 13 bytes = 0xD, and the middle chunk carries an extension.
        let body = b"4\r\nWiki\r\n6;ext=1\r\npedia \r\nD\r\nin\r\n\r\nchunks.\r\n0\r\n\r\n";
        assert_eq!(dechunk(body).unwrap(), b"Wikipedia in\r\n\r\nchunks.");
        assert!(dechunk(b"5\r\nabc").is_err(), "truncated");
    }

    #[test]
    fn urls_split_into_endpoint_path_and_scheme() {
        let (e, path, tls) = parse_url("https://legato.radiotime.com/Radio.asmx").unwrap();
        assert!(tls);
        assert_eq!(path, "/Radio.asmx");
        assert_eq!(e.authority(), "legato.radiotime.com:443");
        // 443 is elided from Host; anything else is kept.
        assert_eq!(e.host_header(), "legato.radiotime.com");

        let (e, path, tls) = parse_url("http://example.test:8080").unwrap();
        assert!(!tls);
        assert_eq!(path, "/", "a URL with no path still posts to root");
        assert_eq!(e.host_header(), "example.test:8080");

        assert!(parse_url("ftp://example.test/x").is_err(), "scheme");
        assert!(parse_url("example.test/x").is_err(), "no scheme");
        assert!(parse_url("https:///x").is_err(), "no host");
    }

    #[test]
    fn a_colon_that_is_not_a_port_is_left_alone() {
        // Only an all-digit suffix is a port; otherwise the whole authority is
        // the host and the scheme's default port applies.
        let (e, _, _) = parse_url("https://host.test:notaport/x").unwrap();
        assert_eq!(e.authority(), "host.test:notaport:443");
    }

    #[test]
    fn urlencode_escapes_special_characters() {
        assert_eq!(urlencode("foo bar"), "foo%20bar");
        assert_eq!(urlencode("a/b?c=d&e+f"), "a%2fb%3fc%3dd%26e%2bf");
        assert_eq!(urlencode("plain_text-1.0~"), "plain_text-1.0~");
    }
}
