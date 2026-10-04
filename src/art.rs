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
//! [`TIMEOUT`], carry no `Content-Encoding` (which would undo the cap), and
//! start like a JPEG, PNG, GIF or WebP - or it is not kept.
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
use std::os::unix::fs::PermissionsExt;
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
const KINDS: [&str; 4] = ["jpg", "png", "gif", "webp"];

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
        _ => None,
    }
}

/// The cache file's name, without its extension.
fn key(url: &str) -> String {
    format!("{:x}", Md5::digest(url.as_bytes()))
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
                return Ok((body, kind));
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

/// Mark a file as just used, for [`prune`]'s least-recently-used order.
pub(crate) fn touch(path: &Path) {
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
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
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// The local file for one URL: the cached copy, or a fresh one.
async fn resolve(dir: &Path, url: &str) -> Result<PathBuf> {
    if let Some(path) = cached(dir, url) {
        touch(&path);
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
        assert_eq!(kind(b"<html><img src=x>"), None);
        assert_eq!(kind(b"<svg"), None);
        assert_eq!(kind(b""), None);
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
