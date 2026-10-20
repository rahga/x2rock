//! Cover art, fetched on a front end's behalf and kept in a bounded cache.
//!
//! The bar widget used to hand every art URL straight to Qt's `Image`, which
//! fetches whatever it is given, from anywhere, with no cap on the bytes - and
//! half of those URLs come from music services, not from the speakers. So the
//! widget now asks `x2rock art`, and shows only what this module wrote.
//!
//! **What may be fetched.** `https://` from anywhere, which is where services
//! keep their covers, and `http://` only from a private-network address on
//! port 1400: a Sonos player's own `/getaa`, which is how now-playing and the
//! queue arrive. Nothing else, and a redirect is held to the same rule before
//! it is followed. What comes back must be at most [`MAX_BYTES`], arrive within
//! [`TIMEOUT`], carry no `Content-Encoding` (which would undo the cap), and be a
//! JPEG or a PNG - or it is not kept, and the widget shows its placeholder.
//!
//! **What is sanitised.** A cover comes from a music service's CDN, which is the
//! internet, so the bytes are disarmed before they are cached and handed to the
//! shared shell's image loader. Only a JPEG or a PNG is kept, and each is rebuilt
//! to the pixels and nothing else: a PNG keeps only the chunks it needs to render
//! (`IHDR`, `PLTE`, `IDAT`, `IEND`, `tRNS`) and a JPEG drops every `APPn` and
//! comment segment, so no `zTXt`/`iTXt`/`iCCP`/`EXIF` metadata - a place to hide
//! a decompression bomb that `sourceSize` scaling would never bound - survives.
//! Both are refused above [`MAX_DIM`] on a side, so a tiny file cannot declare a
//! raster large enough to exhaust the shell before it is scaled down. GIF, WebP,
//! AVIF and BMP are not inspected and so not kept. See `sanitize`.
//!
//! **What is kept, and for how long.** `$XDG_CACHE_HOME/x2rock/art/`, owner
//! only, one file per URL named for its MD5 - a key, not a security property.
//! A cached file is served again with its modification time bumped, which is
//! what makes eviction least-recently-used: access times are not reliable
//! under `relatime` or `noatime`. [`prune`] drops anything untouched for
//! [`MAX_AGE`] and then the oldest until the directory is under [`MAX_TOTAL`],
//! at most once per [`PRUNE_EVERY`] - so no timer runs anywhere, and the whole
//! directory can be deleted at any time for nothing but the refetch.
//!
//! **Not the daemon's.** A cover on a service's CDN is the internet, and
//! "talking to a service never enters the daemon" (docs/architecture.md). The
//! widget runs this as a subprocess, as it does search.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use serde_json::json;

use crate::sonos::http::{self, header};
use crate::store;

/// The most one image may be, as it arrives. A 544px cover - what Sonos and
/// the services hand out - is 50-150 KB; this is well above any real one.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// How long one image may take, redirects included.
pub const TIMEOUT: Duration = Duration::from_secs(8);
/// The cache's size cap. Several hundred covers: more than a household's
/// favorites, queue and recent history together.
pub const MAX_TOTAL: u64 = 50 * 1024 * 1024;
/// Anything not shown for this long goes, whatever the total.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// How often a run of `x2rock art` may walk the directory to prune it.
pub const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const REDIRECTS: usize = 3;
/// Fetches in flight at once, so a search's worth of results does not open
/// forty connections.
const PARALLEL: usize = 6;
/// Its modification time is when the directory was last pruned.
const MARKER: &str = ".pruned";
/// The extensions this module writes, which is what `cached` probes for. Only
/// the two that are sanitised; a GIF, WebP, AVIF or BMP is refused, not cached.
const KINDS: [&str; 2] = ["jpg", "png"];
/// The longest side a cover may claim, in pixels. Real covers measured here run
/// 50-1500 across the services and the speakers; this sits just above a 4K
/// display's 2160px, so it never limits what could be shown (a cover is only
/// ever scaled *down* to a tile), while refusing the tiny file that declares a
/// giant raster. That caps the decode a hostile file can force to about 18 MB
/// (2170 x 2170 x 4), which `sourceSize` then scales to the tile - rather than
/// the raster only being bounded *after* it was already built. A genuine
/// higher-resolution original is refused and shown as the placeholder; without a
/// decoder here it can be checked but not downscaled.
const MAX_DIM: u32 = 2170;
/// Bumped when the sanitiser changes what it keeps. Folded into the cache key so
/// a file written by an older rule is never served: its key no longer matches,
/// it is simply not found, and `prune` drops it in time. The cache is disposable
/// (see the module docs), so this costs a refetch and nothing else.
const SANITIZE_VERSION: &str = "s1";

/// `$XDG_CACHE_HOME/x2rock/art`.
pub fn dir() -> Result<PathBuf> {
    store::cache_dir("art")
}

/// Refuse a URL this module will not fetch - see the module docs.
fn allowed(url: &str) -> Result<()> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("not a URL"))?;
    match scheme {
        "https" => Ok(()),
        "http" => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            let (host, port) = authority
                .rsplit_once(':')
                .ok_or_else(|| anyhow!("plain http only to a speaker on port 1400"))?;
            let ip: Ipv4Addr = host
                .parse()
                .map_err(|_| anyhow!("plain http only to a speaker's address"))?;
            if port != "1400" || !ip.is_private() {
                bail!("plain http only to a speaker on the local network, port 1400");
            }
            Ok(())
        }
        other => bail!("scheme {other:?} is not fetched"),
    }
}

/// The image kind, from its first bytes, or `None` for anything else.
fn kind(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0xff, 0xd8, 0xff, ..] => Some("jpg"),
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some("png"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("webp"),
        // An ISO-BMFF box, `ftyp`, branded AVIF - a still (`avif`) or a
        // sequence (`avis`). Some services' CDNs answer with it; x2rocktv met
        // it on station logos. Kept even where the viewer cannot decode it -
        // this Qt has no AVIF plugin - since refusing it leaves the art just
        // as missing, and one that can decode it then shows it.
        [
            _,
            _,
            _,
            _,
            b'f',
            b't',
            b'y',
            b'p',
            b'a',
            b'v',
            b'i',
            b'f' | b's',
            ..,
        ] => Some("avif"),
        // A bitmap: rare, but Qt reads it with no plugin at all.
        [b'B', b'M', ..] => Some("bmp"),
        _ => None,
    }
}

/// Disarm an image, keeping only the pixels: a JPEG or a PNG rebuilt to its
/// rendering parts, or an error for anything else.
///
/// The reconstructed file is what reaches the shared shell's image loader. Only
/// the two formats this understands are kept; a GIF, WebP, AVIF or BMP would
/// have to be decoded to be trusted, and this does not carry a decoder, so it is
/// refused and the widget shows its placeholder instead.
fn sanitize(bytes: &[u8], kind: &'static str) -> Result<Vec<u8>> {
    match kind {
        "png" => sanitize_png(bytes),
        "jpg" => sanitize_jpeg(bytes),
        other => bail!("{other} covers are not shown"),
    }
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// A length-0 `IEND` with its fixed CRC, to close a stream whose own was dropped.
const PNG_IEND: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82];

/// A PNG rebuilt from its critical chunks alone.
///
/// A PNG is a signature then a run of `length(4) + type(4) + data + crc(4)`
/// chunks. Everything that is not needed to draw the image is dropped - every
/// `tEXt`/`zTXt`/`iTXt` (a compressed-text bomb), `iCCP` (a compressed profile,
/// the same risk), and the rest - leaving `IHDR`, `PLTE`, `IDAT`, `IEND` and
/// `tRNS`. The kept chunks are copied whole, crc and all, so nothing is
/// recomputed. The dimensions in `IHDR` are checked against [`MAX_DIM`] first.
fn sanitize_png(bytes: &[u8]) -> Result<Vec<u8>> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        bail!("not a PNG");
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&PNG_SIGNATURE);
    let mut i = PNG_SIGNATURE.len();
    let (mut saw_ihdr, mut saw_iend) = (false, false);
    while i + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]) as usize;
        let ctype = &bytes[i + 4..i + 8];
        // The whole chunk: 4 length, 4 type, `len` data, 4 crc. `checked_add`
        // because `len` came off the wire and must not wrap the bounds check.
        let end = i
            .checked_add(12)
            .and_then(|b| b.checked_add(len))
            .ok_or_else(|| anyhow!("PNG chunk length overflow"))?;
        if end > bytes.len() {
            bail!("truncated PNG chunk");
        }
        if ctype == b"IHDR" {
            let data = &bytes[i + 8..i + 8 + len];
            if data.len() < 8 {
                bail!("short PNG IHDR");
            }
            let w = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let h = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                bail!("PNG is {w}x{h}, over the {MAX_DIM}px cap");
            }
            saw_ihdr = true;
        }
        if matches!(ctype, b"IHDR" | b"PLTE" | b"IDAT" | b"IEND" | b"tRNS") {
            out.extend_from_slice(&bytes[i..end]);
        }
        if ctype == b"IEND" {
            saw_iend = true;
            break;
        }
        i = end;
    }
    if !saw_ihdr {
        bail!("PNG has no IHDR");
    }
    if !saw_iend {
        out.extend_from_slice(&PNG_IEND);
    }
    Ok(out)
}

/// A JPEG rebuilt without its application or comment segments.
///
/// A JPEG is markers: each is `0xFF` then a type byte, and most carry a 2-byte
/// length and that much data. Every `APPn` (`EXIF`, `ICC`, a thumbnail) and
/// every `COM` comment is dropped, since that is where oversized metadata rides;
/// the quantisation and Huffman tables and the frame header are kept. The frame
/// header (`SOFn`) gives the dimensions, checked against [`MAX_DIM`]. At the
/// start of scan (`SOS`) the entropy-coded data begins, which has no length and
/// runs to the end, so it is copied whole from there.
fn sanitize_jpeg(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        bail!("not a JPEG");
    }
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&[0xff, 0xd8]); // SOI
    let mut i = 2;
    loop {
        if i >= bytes.len() || bytes[i] != 0xff {
            bail!("JPEG marker expected");
        }
        // A marker may be preceded by any number of 0xff fill bytes.
        let mut m = i + 1;
        while m < bytes.len() && bytes[m] == 0xff {
            m += 1;
        }
        let marker = *bytes
            .get(m)
            .ok_or_else(|| anyhow!("truncated JPEG marker"))?;
        match marker {
            // End of image.
            0xd9 => {
                out.extend_from_slice(&[0xff, 0xd9]);
                break;
            }
            // Start of scan: the marker and all entropy data to the end, whole.
            0xda => {
                out.extend_from_slice(&bytes[i..]);
                break;
            }
            // Standalone markers that carry no length.
            0x01 | 0xd0..=0xd7 => {
                out.extend_from_slice(&[0xff, marker]);
                i = m + 1;
            }
            // Everything else is a segment: 2-byte length (itself included),
            // then its data.
            _ => {
                let lenpos = m + 1;
                if lenpos + 2 > bytes.len() {
                    bail!("truncated JPEG segment");
                }
                let seg_len = u16::from_be_bytes([bytes[lenpos], bytes[lenpos + 1]]) as usize;
                if seg_len < 2 {
                    bail!("bad JPEG segment length");
                }
                let seg_end = lenpos + seg_len;
                if seg_end > bytes.len() {
                    bail!("JPEG segment runs past the end");
                }
                // A frame header: precision(1), height(2), width(2), ...
                let is_sof = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
                if is_sof {
                    let data = &bytes[lenpos + 2..seg_end];
                    if data.len() < 5 {
                        bail!("short JPEG frame header");
                    }
                    let h = u16::from_be_bytes([data[1], data[2]]) as u32;
                    let w = u16::from_be_bytes([data[3], data[4]]) as u32;
                    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM {
                        bail!("JPEG is {w}x{h}, over the {MAX_DIM}px cap");
                    }
                }
                // Drop APPn (0xe0..=0xef) and COM (0xfe); keep tables and header.
                if !matches!(marker, 0xe0..=0xef | 0xfe) {
                    out.extend_from_slice(&[0xff, marker]);
                    out.extend_from_slice(&bytes[lenpos..seg_end]);
                }
                i = seg_end;
            }
        }
    }
    Ok(out)
}

/// The cache file's name, without its extension.
fn key(url: &str) -> String {
    // The sanitiser version is mixed in so a change to what is kept invalidates
    // every older file without touching the directory the widget matches on.
    let mut hasher = Md5::new();
    hasher.update(SANITIZE_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Where a redirect's `Location` points, made absolute against `from`.
fn follow(from: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_owned();
    }
    let origin_end = from
        .find("://")
        .and_then(|i| from[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(from.len());
    format!("{}{location}", &from[..origin_end])
}

/// Fetch one image, holding every hop to [`allowed`].
async fn fetch(url: &str) -> Result<(Vec<u8>, &'static str)> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut current = url.to_owned();
    for _ in 0..=REDIRECTS {
        allowed(&current)?;
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let (status, head, body) = http::get_bytes(&current, left, MAX_BYTES).await?;
        match status {
            200 => {
                if header(&head, "content-encoding").is_some_and(|v| v != "identity") {
                    bail!("a compressed body");
                }
                let kind = kind(&body).ok_or_else(|| anyhow!("not an image"))?;
                return Ok((sanitize(&body, kind)?, kind));
            }
            301 | 302 | 303 | 307 | 308 => {
                let to =
                    header(&head, "location").ok_or_else(|| anyhow!("a redirect to nowhere"))?;
                current = follow(&current, to);
            }
            other => bail!("HTTP {other}"),
        }
    }
    bail!("more than {REDIRECTS} redirects")
}

/// The cached file for `url` in `dir`, if there is one.
fn cached(dir: &Path, url: &str) -> Option<PathBuf> {
    let key = key(url);
    KINDS
        .iter()
        .map(|ext| dir.join(format!("{key}.{ext}")))
        .find(|p| p.is_file())
}

/// Write an image in by renaming a scratch file over it, so the widget - which
/// loads nothing but finished files from this directory - never sees half of one.
fn store(dir: &Path, url: &str, bytes: &[u8], ext: &str) -> Result<PathBuf> {
    let path = dir.join(format!("{}.{ext}", key(url)));
    store::write_bytes_atomically(&path, bytes, store::SECRET)?;
    Ok(path)
}

/// The cache directory, created owner-only.
fn open_dir() -> Result<PathBuf> {
    let dir = dir()?;
    store::owner_only_dir(&dir)?;
    Ok(dir)
}

/// The local file for one URL: the cached copy, or a fresh one.
async fn resolve(dir: &Path, url: &str) -> Result<PathBuf> {
    if let Some(path) = cached(dir, url) {
        // Bumped for `prune`'s least-recently-used order.
        store::touch(&path);
        return Ok(path);
    }
    let (bytes, ext) = fetch(url).await?;
    store(dir, url, &bytes, ext)
}

/// Bring the directory back inside [`MAX_AGE`] and [`MAX_TOTAL`] - unless it
/// was done less than [`PRUNE_EVERY`] ago, which is most calls, and then this
/// is one `stat`. Scratch files a crash left behind go once they are old.
pub fn prune(dir: &Path, now: SystemTime) -> Result<()> {
    let marker = dir.join(MARKER);
    let since = |t: SystemTime| now.duration_since(t).unwrap_or_default();
    if fs::metadata(&marker)
        .and_then(|m| m.modified())
        .is_ok_and(|t| since(t) < PRUNE_EVERY)
    {
        return Ok(());
    }
    let mut kept = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == MARKER {
            continue;
        }
        let meta = entry.metadata()?;
        let modified = meta.modified()?;
        let stale = if name.ends_with(".tmp") {
            since(modified) > PRUNE_EVERY
        } else {
            since(modified) > MAX_AGE
        };
        if stale {
            let _ = fs::remove_file(entry.path());
        } else if !name.ends_with(".tmp") {
            kept.push((modified, meta.len(), entry.path()));
        }
    }
    // Newest first: whatever pushes the running total over the cap, and
    // everything older, goes.
    kept.sort_by_key(|(modified, ..)| std::cmp::Reverse(*modified));
    let mut total = 0u64;
    for (_, len, path) in kept {
        total += len;
        if total > MAX_TOTAL {
            let _ = fs::remove_file(path);
        }
    }
    fs::write(&marker, b"")?;
    Ok(())
}

/// One `--json`/`--each` row.
fn row(url: &str, result: &Result<PathBuf>) -> serde_json::Value {
    match result {
        Ok(path) => json!({"url": url, "path": path}),
        Err(e) => json!({"url": url, "path": null, "error": format!("{e:#}")}),
    }
}

/// `x2rock art`: print a local file for each URL, in order - its path, or an
/// empty line (`null` under `--json`) for one that would not be fetched.
///
/// `each` prints one JSON row per URL **as it lands**, in whatever order
/// that is, rather than all of them in order at the end. The widget asks for a
/// list's worth at a time, and waiting for the slowest before showing any made
/// every cover as slow as the worst CDN in the batch.
pub async fn run(urls: &[String], clear: bool, json: bool, each: bool) -> Result<()> {
    if clear {
        let dir = dir()?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
        if urls.is_empty() {
            return Ok(());
        }
    }
    let dir = open_dir()?;
    // Pruned first, so this run's own files - just touched or just written -
    // are never what it evicts.
    let _ = prune(&dir, SystemTime::now());

    let mut unique: Vec<&str> = urls.iter().map(String::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    let mut landing = futures_util::stream::iter(unique)
        .map(|url| {
            let dir = &dir;
            async move { (url, resolve(dir, url).await) }
        })
        .buffer_unordered(PARALLEL);

    if each {
        let mut out = std::io::stdout().lock();
        while let Some((url, result)) = landing.next().await {
            writeln!(out, "{}", row(url, &result))?;
            // Line by line to a pipe, which Rust would otherwise hold until
            // the buffer fills - the delay this mode exists to remove.
            out.flush()?;
        }
        return Ok(());
    }
    let results: HashMap<&str, Result<PathBuf>> = landing.collect().await;

    if json {
        let rows: Vec<_> = urls
            .iter()
            .map(|url| row(url, &results[url.as_str()]))
            .collect();
        println!("{}", serde_json::Value::Array(rows));
    } else {
        for url in urls {
            match &results[url.as_str()] {
                Ok(path) => println!("{}", path.display()),
                Err(e) => {
                    eprintln!("x2rock: {url}: {e:#}");
                    println!();
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;

    #[test]
    fn only_https_and_a_speakers_own_port_are_fetched() {
        assert!(allowed("https://c.saavncdn.com/art.jpg").is_ok());
        assert!(allowed("http://192.168.77.94:1400/getaa?s=1&u=x").is_ok());
        assert!(allowed("http://10.0.0.5:1400/getaa").is_ok());

        assert!(allowed("http://example.com/art.jpg").is_err());
        assert!(
            allowed("http://8.8.8.8:1400/getaa").is_err(),
            "public address"
        );
        assert!(
            allowed("http://192.168.77.94:80/x").is_err(),
            "not port 1400"
        );
        assert!(allowed("http://192.168.77.94/x").is_err());
        assert!(allowed("file:///etc/passwd").is_err());
        assert!(allowed("data:image/png;base64,AAAA").is_err());
        assert!(allowed("ftp://x/y").is_err());
    }

    #[test]
    fn an_image_is_known_by_its_first_bytes() {
        assert_eq!(kind(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(kind(b"\x89PNG\r\n\x1a\n...."), Some("png"));
        assert_eq!(kind(b"GIF89a"), Some("gif"));
        assert_eq!(kind(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(kind(b"\0\0\0\x1cftypavif\0\0"), Some("avif"));
        assert_eq!(kind(b"\0\0\0\x20ftypavis\0\0"), Some("avif"));
        assert_eq!(kind(b"\0\0\0\x1cftypheic"), None, "HEIC is not AVIF");
        assert_eq!(kind(b"BM\x36\x00\x0c\x00"), Some("bmp"));
        assert_eq!(kind(b"<html><img src=x>"), None);
        assert_eq!(kind(b"<svg"), None);
        assert_eq!(kind(b""), None);
    }

    fn png_chunk(ctype: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(&(data.len() as u32).to_be_bytes());
        c.extend_from_slice(ctype);
        c.extend_from_slice(data);
        c.extend_from_slice(&[0, 0, 0, 0]); // crc: copied, never checked here
        c
    }

    fn png_ihdr(w: u32, h: u32) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit truecolour, no interlace
        png_chunk(b"IHDR", &d)
    }

    #[test]
    fn sanitize_png_keeps_the_pixels_and_drops_the_metadata() {
        let mut input = PNG_SIGNATURE.to_vec();
        input.extend(png_ihdr(1, 1));
        input.extend(png_chunk(b"zTXt", &vec![0u8; 4000])); // the bomb's hiding place
        input.extend(png_chunk(b"iCCP", &vec![0u8; 4000]));
        input.extend(png_chunk(b"IDAT", &[0x08, 0x1d, 0x01])); // a stub body
        input.extend(png_chunk(b"IEND", &[]));

        let out = sanitize_png(&input).unwrap();
        assert!(out.starts_with(&PNG_SIGNATURE));
        assert!(contains(&out, b"IHDR") && contains(&out, b"IDAT"));
        assert!(!contains(&out, b"zTXt"), "text chunk survived");
        assert!(!contains(&out, b"iCCP"), "profile chunk survived");
        assert!(out.ends_with(&PNG_IEND) || contains(&out, b"IEND"));
        assert!(out.len() < input.len());
    }

    #[test]
    fn sanitize_png_refuses_an_oversized_raster() {
        let mut input = PNG_SIGNATURE.to_vec();
        input.extend(png_ihdr(100_000, 1));
        input.extend(png_chunk(b"IEND", &[]));
        assert!(
            sanitize_png(&input).is_err(),
            "a 100000px side must be refused"
        );
    }

    fn jpeg_segment(marker: u8, data: &[u8]) -> Vec<u8> {
        let mut s = vec![0xff, marker];
        s.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(data);
        s
    }

    fn jpeg_sof0(w: u16, h: u16) -> Vec<u8> {
        // precision, height, width, one component (id, sampling, quant table)
        let mut d = vec![8];
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&[1, 0x11, 0]);
        jpeg_segment(0xc0, &d)
    }

    #[test]
    fn sanitize_jpeg_drops_app_and_comment_segments() {
        let mut input = vec![0xff, 0xd8]; // SOI
        input.extend(jpeg_segment(0xe1, b"Exif\0\0padding........")); // APP1
        input.extend(jpeg_segment(0xfe, b"a comment")); // COM
        input.extend(jpeg_segment(0xdb, &[0u8; 65])); // DQT, kept
        input.extend(jpeg_sof0(1, 1)); // kept, carries the dimensions
        input.extend(jpeg_segment(0xda, &[1, 1, 0, 0, 0, 0x3f, 0])); // SOS header
        input.extend_from_slice(&[0x12, 0x34, 0x56]); // entropy data
        input.extend_from_slice(&[0xff, 0xd9]); // EOI

        let out = sanitize_jpeg(&input).unwrap();
        assert!(out.starts_with(&[0xff, 0xd8]) && out.ends_with(&[0xff, 0xd9]));
        assert!(!contains(&out, b"Exif"), "APP1 survived");
        assert!(!contains(&out, b"a comment"), "COM survived");
        // The quantisation table, frame header and scan data are still there.
        assert!(contains(&out, &[0xff, 0xdb]) && contains(&out, &[0xff, 0xc0]));
        assert!(contains(&out, &[0xff, 0xda]) && contains(&out, &[0x12, 0x34, 0x56]));
        assert!(out.len() < input.len());
    }

    #[test]
    fn sanitize_jpeg_refuses_an_oversized_raster() {
        let mut input = vec![0xff, 0xd8];
        input.extend(jpeg_sof0(1, 5000)); // 5000 > MAX_DIM
        input.extend(jpeg_segment(0xda, &[1, 1, 0, 0, 0, 0x3f, 0]));
        input.extend_from_slice(&[0xff, 0xd9]);
        assert!(
            sanitize_jpeg(&input).is_err(),
            "a 5000px side must be refused"
        );
    }

    #[test]
    fn only_png_and_jpeg_are_kept() {
        assert!(sanitize(b"GIF89a....", "gif").is_err());
        assert!(sanitize(b"RIFF....WEBP", "webp").is_err());
        assert!(sanitize(b"BM......", "bmp").is_err());
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn a_relative_redirect_stays_on_its_origin() {
        assert_eq!(
            follow("https://a.example/x/y.jpg", "/z.jpg"),
            "https://a.example/z.jpg"
        );
        assert_eq!(
            follow("https://a.example/x", "http://b.example/"),
            "http://b.example/"
        );
    }

    /// Stale files go, then the least recently used until under the cap -
    /// and not again within the hour.
    #[test]
    fn prune_drops_the_stale_then_the_oldest_and_then_rests() {
        let tmp = TempDir::new("art-prune");
        let dir = tmp.path();
        let now = SystemTime::now();
        let aged = |name: &str, len: usize, age: Duration| {
            let path = dir.join(name);
            fs::write(&path, vec![0u8; len]).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(now - age).unwrap();
            path
        };
        let day = Duration::from_secs(24 * 3600);
        let half = (MAX_TOTAL / 2) as usize;
        let ancient = aged("a.jpg", 10, MAX_AGE + day);
        let oldest = aged("b.jpg", half, 3 * day);
        let middle = aged("c.jpg", half, 2 * day);
        let newest = aged("d.jpg", half - 10, day);
        let leftover = aged("e.1.tmp", 10, 2 * PRUNE_EVERY);

        prune(dir, now).unwrap();
        assert!(!ancient.exists(), "older than MAX_AGE");
        assert!(!leftover.exists(), "an abandoned scratch file");
        assert!(!oldest.exists(), "least recently used, over the cap");
        assert!(middle.exists() && newest.exists());

        // Within the hour, nothing is walked: a file that would now go stays.
        let later = aged("f.jpg", 10, MAX_AGE + day);
        prune(dir, now).unwrap();
        assert!(later.exists());
    }

    #[test]
    fn a_stored_image_is_found_again_and_leaves_no_scratch() {
        let tmp = TempDir::new("art-store");
        let url = "https://x.example/cover.png";
        assert!(cached(tmp.path(), url).is_none());
        let path = store(tmp.path(), url, b"\x89PNG\r\n\x1a\n", "png").unwrap();
        assert_eq!(cached(tmp.path(), url), Some(path.clone()));
        assert!(path.to_string_lossy().ends_with(".png"));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
