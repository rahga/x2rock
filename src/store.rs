//! The state directory, and the rules every file in it follows.
//!
//! Four files live under `$XDG_STATE_HOME/x2rock/` - the player list, the
//! service catalogue, the bookmarks and the credentials - and each carried its
//! own copy of the same two steps: find the directory, and write by renaming a
//! temporary over the real file. This is the one copy.
//!
//! Each write owns a newly created temporary file. A process id alone is not
//! enough: overlapping tasks in the same daemon otherwise remove or rename
//! each other's scratch file. Atomic replacement protects readers from partial
//! files; read/modify/write transactions still need [`Lock`] as well.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;

/// Ordinary state, readable like any file of the user's.
pub const PLAIN: u32 = 0o644;
/// A secret: the owner alone.
pub const SECRET: u32 = 0o600;

/// x2rock's own directories under each XDG base.
fn dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "x2rock").ok_or_else(|| anyhow!("no home directory"))
}

/// `$XDG_STATE_HOME/x2rock/<file>`.
pub fn path(file: &str) -> Result<PathBuf> {
    let dirs = dirs()?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join(file))
}

/// `$XDG_CONFIG_HOME/x2rock/<file>`: configuration the person writes, not
/// state x2rock learns.
pub fn config_path(file: &str) -> Result<PathBuf> {
    Ok(dirs()?.config_dir().join(file))
}

/// `$XDG_CACHE_HOME/x2rock/<sub>`: regenerable files, named for what fills them.
pub fn cache_dir(sub: &str) -> Result<PathBuf> {
    Ok(dirs()?.cache_dir().join(sub))
}

/// Create a cache directory owner-only, tightening one that already exists:
/// what is cached here came off the network on the owner's behalf - covers,
/// spoken clips - and is nobody else's to read.
pub fn owner_only_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

/// Mark a cached file as just used, by its modification time: access times
/// are not reliable under `relatime` or `noatime`, and a cache pruned
/// least-recently-used needs to know.
pub fn touch(path: &Path) {
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// `<file>.<ext>` beside `path`.
fn sibling(path: &Path, ext: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{ext}"))
}

/// One write's scratch file, removed on any exit that did not rename it away.
struct Scratch {
    path: PathBuf,
    file: fs::File,
}

impl Scratch {
    fn create(path: &Path, mode: u32) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let tmp = sibling(path, &format!("{}.{serial}.tmp", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&tmp)
            {
                Ok(file) => return Ok(Self { path: tmp, file }),
                // A crash followed by PID reuse can leave this name occupied.
                // Never truncate or remove a file this write did not create.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", tmp.display())),
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// The file's text, or `None` for a file that does not exist yet. Every
/// "missing means empty" loader reads through this; what it makes of the text
/// - refuse a corrupt file, migrate it, tighten its mode - stays its own.
pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A JSON file parsed, a missing one as `T::default()`. A *corrupt* one is an
/// error, for the files that are the only copy of something: what to do
/// beyond parsing - a schema check, a migration - stays the caller's.
pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match read_optional(path)? {
        Some(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        None => Ok(T::default()),
    }
}

/// A JSON file parsed, or `None` when it is missing, unreadable or corrupt
/// alike - for a cache, which is rebuilt rather than failed over.
pub fn load_json_lenient<T: DeserializeOwned>(path: &Path) -> Option<T> {
    read_optional(path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// Write `text` to `path` atomically: a crash mid-write leaves the old file,
/// never a truncated one.
///
/// `mode` is applied when the scratch file is *created*, not after it is
/// written - a `chmod` afterwards leaves a window in which the contents sit at
/// the umask's mercy, which matters for the one file here that holds a secret.
/// `create_new` is what makes the mode stick: an existing file keeps its own
/// bits whatever `mode` asks. Occupied scratch names are skipped, never reused.
pub fn write_atomically(path: &Path, text: &str, mode: u32) -> Result<()> {
    write_bytes_atomically(path, text.as_bytes(), mode)
}

/// [`write_atomically`] for bytes that are not text: a cached clip.
pub fn write_bytes_atomically(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut scratch = Scratch::create(path, mode)?;
    scratch
        .file
        .write_all(bytes)
        .with_context(|| format!("writing {}", scratch.path.display()))?;
    fs::rename(&scratch.path, path).with_context(|| format!("writing {}", path.display()))
}

/// An exclusive lock over a file's writers, held while the guard lives.
///
/// Taken on a sibling `<file>.lock`, never the file itself: the atomic write
/// replaces the file's inode, so a lock on the old one would guard nothing the
/// moment a rename landed. Advisory, which binds exactly the writers that take
/// it - the daemon and the CLI, which is all of them. Blocks until free; the
/// holders keep it for the microseconds between a read and a rename.
pub struct Lock {
    /// Held for its `Drop`, which is the unlock.
    _file: fs::File,
}

impl Lock {
    pub fn exclusive(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let lock = sibling(path, "lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(SECRET)
            .open(&lock)
            .with_context(|| format!("opening {}", lock.display()))?;
        // `File::lock` is what moved the MSRV to 1.89: it is `flock(2)` from
        // std, and the alternative was a crate for one call.
        file.lock()
            .with_context(|| format!("locking {}", lock.display()))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn overlapping_writes_publish_only_complete_files() {
        let dir = TempDir::new("store-overlap");
        let path = dir.path().join("shared.json");
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..8)
                .map(|id| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let bytes = vec![b'a' + id; 256 * 1024];
                        barrier.wait();
                        for _ in 0..8 {
                            write_bytes_atomically(path, &bytes, SECRET)?;
                            let seen = fs::read(path)?;
                            anyhow::ensure!(
                                seen.len() == bytes.len(),
                                "a partial write was visible"
                            );
                            anyhow::ensure!(
                                seen.iter().all(|b| *b == seen[0]),
                                "mixed writers' bytes"
                            );
                        }
                        anyhow::Ok(())
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
        });
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_publish_cleans_up_its_scratch_file() {
        let dir = TempDir::new("store-publish-failed");
        let path = dir.path().join("occupied");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), "original").unwrap();
        assert!(write_atomically(&path, "new", SECRET).is_err());
        assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_leftover_scratch_file_does_not_loosen_the_mode() {
        // `mode` on open applies only to a file being created. A leftover from a
        // crashed writer at the umask's default used to keep its 0644 through
        // the truncate, take the secret, and be renamed into place - the window
        // the doc comment on `Credentials::save` promised did not exist.
        let dir = TempDir::new("store-mode");
        let path = dir.path().join("credentials.json");
        let leftover = sibling(&path, &format!("{}.tmp", std::process::id()));
        fs::write(&leftover, "stale").unwrap();
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).unwrap();

        write_atomically(&path, "{\"secret\":1}", SECRET).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"secret\":1}");
        assert_eq!(fs::read_to_string(leftover).unwrap(), "stale");
    }

    #[test]
    fn overlapping_scratch_files_have_separate_ownership() {
        let dir = TempDir::new("store-scratch");
        let path = dir.path().join("bookmarks.json");
        let mut first = Scratch::create(&path, SECRET).unwrap();
        first.file.write_all(b"first writer").unwrap();
        let second = Scratch::create(&path, SECRET).unwrap();
        assert_ne!(first.path, second.path);
        drop(second);
        assert_eq!(fs::read_to_string(&first.path).unwrap(), "first writer");
        drop(first);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_lock_lives_beside_the_file_and_outlasts_a_rename() {
        let dir = TempDir::new("store-lock");
        let path = dir.path().join("bookmarks.json");
        let guard = Lock::exclusive(&path).unwrap();
        assert!(dir.path().join("bookmarks.json.lock").exists());
        // The write replaces the file's inode; the lock file is untouched by it,
        // so a second writer waiting on the lock still waits on the same inode.
        let before = fs::metadata(dir.path().join("bookmarks.json.lock")).unwrap();
        assert_eq!(
            before.permissions().mode() & 0o777,
            SECRET,
            "lock file should have mode 0600"
        );
        write_atomically(&path, "[]", PLAIN).unwrap();
        let after = fs::metadata(dir.path().join("bookmarks.json.lock")).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after)
        );
        drop(guard);
    }
}
