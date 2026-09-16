//! A directory for one test, gone when the test ends.
//!
//! Three tests write real files, because the file handling is the thing under
//! test: the bookmarks listing, the desktop entry and icon placement, and the
//! `PATH` walk behind `invoked_path`. Each used to clean up with a call at the
//! end of the test - which is exactly the line a failed assertion skips, so one
//! red test left a directory behind for the next run to inherit, under a name
//! keyed by nothing but the process id.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory under the system temp directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Make one, named for `what` so anything a crash leaves behind says which
    /// test made it. Unique per call as well as per process, because cargo runs
    /// tests as threads of a single process.
    pub fn new(what: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("x2rock-{what}-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temp directory for the test");
        Self(dir)
    }

    /// The directory itself, to build paths under.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort on purpose: a test that has already failed should report
        // that, not a second panic from its own cleanup.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
