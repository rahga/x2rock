//! The state directory, and the rules every file in it follows.
//!
//! Four files live under `$XDG_STATE_HOME/x2rock/` - the player list, the
//! service catalogue, the bookmarks and the credentials - and each carried its
//! own copy of the same two steps: find the directory, and write by renaming a
//! temporary over the real file. This is the one copy.
//!
//! The temporary is named for the writing *process*, not just the file. The
//! daemon and the CLI both write these files, and a shared `x.json.tmp` had
//! them opening the same inode with `O_TRUNC` when they overlapped: a file
//! with both writers' bytes in it, which one of them then renamed into place,
//! and which `load` thereafter refused for good. A rename is atomic; two
//! writers sharing one scratch file was the part that was not.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Ordinary state, readable like any file of the user's.
pub const PLAIN: u32 = 0o644;
/// A secret: the owner alone.
pub const SECRET: u32 = 0o600;

/// `$XDG_STATE_HOME/x2rock/<file>`.
pub fn path(file: &str) -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "x2rock")
        .ok_or_else(|| anyhow!("no home directory"))?;
    let dir = dirs
        .state_dir()
        .ok_or_else(|| anyhow!("no XDG state directory on this platform"))?;
    Ok(dir.join(file))
}

/// `<file>.<ext>` beside `path`.
fn sibling(path: &Path, ext: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{ext}"))
}

/// `<file>.<pid>.tmp` beside `path`: this process's scratch file, and no other's.
fn scratch(path: &Path) -> PathBuf {
    sibling(path, &format!("{}.tmp", std::process::id()))
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

/// Write `text` to `path` atomically: a crash mid-write leaves the old file,
/// never a truncated one.
///
/// `mode` is applied when the scratch file is *created*, not after it is
/// written - a `chmod` afterwards leaves a window in which the contents sit at
/// the umask's mercy, which matters for the one file here that holds a secret.
/// `create_new` is what makes the mode stick: an existing file keeps its own
/// bits whatever `mode` asks, so a leftover is removed first. It can only be
/// this process's own name reused after a crash; nobody is mid-write in it.
pub fn write_atomically(path: &Path, text: &str, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = scratch(path);
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    if let Err(e) = file.write_all(text.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
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
    use std::os::unix::fs::PermissionsExt;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("x2rock-store-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_leftover_scratch_file_does_not_loosen_the_mode() {
        // `mode` on open applies only to a file being created. A leftover from a
        // crashed writer at the umask's default used to keep its 0644 through
        // the truncate, take the secret, and be renamed into place - the window
        // the doc comment on `Credentials::save` promised did not exist.
        let dir = scratch_dir("mode");
        let path = dir.join("credentials.json");
        let leftover = scratch(&path);
        fs::write(&leftover, "stale").unwrap();
        fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).unwrap();

        write_atomically(&path, "{\"secret\":1}", SECRET).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:04o}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"secret\":1}");
        assert!(!leftover.exists(), "the scratch file was renamed away");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_scratch_name_is_per_process() {
        let path = Path::new("/state/x2rock/bookmarks.json");
        let tmp = scratch(path);
        assert_eq!(tmp.parent(), path.parent());
        assert_eq!(
            tmp.file_name().unwrap().to_str().unwrap(),
            format!("bookmarks.json.{}.tmp", std::process::id())
        );
    }

    #[test]
    fn the_lock_lives_beside_the_file_and_outlasts_a_rename() {
        let dir = scratch_dir("lock");
        let path = dir.join("bookmarks.json");
        let guard = Lock::exclusive(&path).unwrap();
        assert!(dir.join("bookmarks.json.lock").exists());
        // The write replaces the file's inode; the lock file is untouched by it,
        // so a second writer waiting on the lock still waits on the same inode.
        let before = fs::metadata(dir.join("bookmarks.json.lock")).unwrap();
        write_atomically(&path, "[]", PLAIN).unwrap();
        let after = fs::metadata(dir.join("bookmarks.json.lock")).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after)
        );
        drop(guard);
        fs::remove_dir_all(&dir).ok();
    }
}
