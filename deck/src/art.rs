//! Cover art: fetched through x2rock's own art cache (which keeps the TV app's
//! rules: https from anywhere, http only from a speaker's port 1400, a size cap,
//! JPEG and PNG only, sanitised), decoded and sized here, and coloured.

pub mod cache;

use anyhow::{Context, Result};
use cache::Size;
use image::imageops::FilterType;
use std::{future::Future, path::Path, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::palette::{self, Rgb};

/// A decoded cover, ready to become a `slint::Image` on the UI thread.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub colors: Option<(Rgb, Rgb)>,
    // Also bound completed pixels waiting on the UI event queue. Demo decodes
    // have no permit; a loaded cover releases it when the UI consumes/drops it.
    pub(crate) _permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug)]
pub struct LoadError {
    pub error: anyhow::Error,
    kind: x2rock::art::FailureKind,
}

impl LoadError {
    fn fetch(error: anyhow::Error) -> Self {
        let kind = x2rock::art::failure_kind(&error);
        Self { error, kind }
    }

    fn transient(error: anyhow::Error) -> Self {
        Self {
            error,
            kind: x2rock::art::FailureKind::Transient,
        }
    }

    pub(crate) fn permanent(error: anyhow::Error) -> Self {
        Self {
            error,
            kind: x2rock::art::FailureKind::Permanent,
        }
    }

    pub fn retry_after(&self) -> Duration {
        match self.kind {
            x2rock::art::FailureKind::Transient => cache::TRANSIENT_RETRY,
            x2rock::art::FailureKind::Permanent => cache::PERMANENT_RETRY,
        }
    }
}

/// The sample the colours are read from, as the TV app's 128px.
const SAMPLE: u32 = 128;

// Keep I/O concurrent while bounding simultaneous decodes and their
// resize/palette scratch space, including larger source rasters.
const PARALLEL: usize = 4;

/// Shared by an app's requests, covering both fetch and decode work.
#[derive(Clone)]
pub struct Loader {
    slots: Arc<Semaphore>,
}

impl Default for Loader {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(PARALLEL)),
        }
    }
}

impl Loader {
    pub async fn load(&self, url: String, size: Size) -> Result<Decoded, LoadError> {
        self.run(|permit| async move {
            let path = x2rock::art::local_file(&url)
                .await
                .map_err(LoadError::fetch)?;
            tokio::task::spawn_blocking(move || {
                // A blocking decode cannot be aborted. Keep its slot even if
                // the async caller is cancelled while waiting for it.
                decode_file(&path, size, Some(permit))
            })
            .await
            .context("decoding a cover")
            .map_err(LoadError::transient)?
        })
        .await
    }

    // The job owns its permit so it can transfer it into a blocking worker.
    // Tests use gated jobs to exercise saturation and cancellation without I/O.
    async fn run<T, F, Fut>(&self, work: F) -> T
    where
        F: FnOnce(OwnedSemaphorePermit) -> Fut,
        Fut: Future<Output = T>,
    {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("the art loader never closes its semaphore");
        work(permit).await
    }
}

fn decode_file(
    path: &Path,
    size: Size,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<Decoded, LoadError> {
    let bytes = std::fs::read(path).map_err(|error| LoadError::transient(error.into()))?;
    let mut decoded = decode(&bytes, size).map_err(LoadError::permanent)?;
    decoded._permit = permit;
    Ok(decoded)
}

pub fn decode(bytes: &[u8], size: Size) -> Result<Decoded> {
    let img = image::load_from_memory(bytes).context("reading a cover")?;
    let sample = img.resize(SAMPLE, SAMPLE, FilterType::Triangle).to_rgba8();
    let pixels: Vec<[u8; 4]> = sample.pixels().map(|p| p.0).collect();
    let colors = palette::art_colors(&pixels);
    let max_side = size.max_side();
    let sized = if img.width() > max_side || img.height() > max_side {
        img.resize(max_side, max_side, FilterType::Triangle)
    } else {
        img
    };
    let rgba = sized.to_rgba8();
    Ok(Decoded {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
        colors,
        _permit: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use std::task::Poll;
    use tokio::sync::{mpsc, oneshot};

    #[test]
    fn list_art_decodes_at_168px_while_full_art_keeps_512px() {
        let dir = x2rock::testdir::TempDir::new("deck-art-sizes");
        let path = dir.path().join("synthetic-cover.png");
        image::RgbaImage::new(512, 256).save(&path).unwrap();
        let small = decode_file(&path, Size::Thumbnail, None).expect("thumbnail decodes");
        let full = decode_file(&path, Size::Full, None).expect("full cover decodes");
        assert_eq!((small.width, small.height), (168, 84));
        assert_eq!(small.rgba.len(), 168 * 84 * 4);
        assert_eq!((full.width, full.height), (512, 256));
    }

    #[test]
    fn malformed_images_wait_longer_than_file_and_transport_failures() {
        let dir = x2rock::testdir::TempDir::new("deck-art-retry");
        let path = dir.path().join("synthetic-cover.png");
        let missing = decode_file(&path, Size::Full, None)
            .err()
            .expect("missing file fails");
        assert_eq!(missing.retry_after(), Duration::from_secs(30));
        std::fs::write(&path, b"synthetic invalid image").unwrap();
        let malformed = decode_file(&path, Size::Full, None)
            .err()
            .expect("malformed image fails");
        assert_eq!(malformed.retry_after(), Duration::from_secs(1800));
        let timeout = LoadError::fetch(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        assert_eq!(timeout.retry_after(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn completed_pixels_keep_their_slots_until_the_ui_consumes_them() {
        let dir = x2rock::testdir::TempDir::new("deck-art-handoff");
        let path = dir.path().join("synthetic-cover.png");
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        let loader = Loader::default();
        let mut waiting_for_ui = Vec::new();
        for _ in 0..4 {
            let decoded = loader
                .run(|permit| async { decode_file(&path, Size::Full, Some(permit)) })
                .await
                .expect("synthetic cover decodes");
            waiting_for_ui.push(decoded);
        }
        let mut next = Box::pin(loader.run(|_permit| async { "next load" }));
        assert!(
            poll!(next.as_mut()).is_pending(),
            "completed pixels waiting for UI must still occupy slots"
        );
        drop(waiting_for_ui.pop());
        assert_eq!(next.await, "next load");
    }

    #[tokio::test]
    async fn four_loads_run_and_cancelled_waiters_do_not_block_the_rest() {
        let loader = Loader::default();
        let (started, mut starts) = mpsc::unbounded_channel();
        let mut releases = Vec::new();
        let mut jobs = Vec::new();
        for id in 0..6 {
            let (release, gate) = oneshot::channel();
            releases.push(Some(release));
            let started = started.clone();
            let loader = loader.clone();
            jobs.push(Box::pin(async move {
                loader
                    .run(|permit| async move {
                        let _permit = permit;
                        started.send(id).unwrap();
                        gate.await.unwrap();
                        id
                    })
                    .await
            }));
        }
        // Poll every job: the last two really attempted to acquire a slot.
        for job in &mut jobs {
            assert!(poll!(job.as_mut()).is_pending());
        }
        for id in 0..4 {
            assert_eq!(starts.try_recv().unwrap(), id);
        }
        assert!(starts.try_recv().is_err(), "only four jobs may start");

        // Cancel the first waiter, then free a slot for the one behind it.
        drop(jobs.remove(4));
        releases[0].take().unwrap().send(()).unwrap();
        assert_eq!(poll!(jobs[0].as_mut()), Poll::Ready(0));
        assert!(poll!(jobs[4].as_mut()).is_pending());
        assert_eq!(starts.try_recv().unwrap(), 5);
        assert!(starts.try_recv().is_err());

        for id in [1, 2, 3, 5] {
            releases[id].take().unwrap().send(()).unwrap();
        }
        for (job, id) in jobs.into_iter().skip(1).zip([1, 2, 3, 5]) {
            assert_eq!(job.await, id);
        }
    }

    #[tokio::test]
    async fn cancelled_caller_keeps_its_slot_until_the_blocking_worker_finishes() {
        let loader = Loader::default();
        // Hold three slots with pending async jobs.
        let mut held = Vec::new();
        for _ in 0..3 {
            let loader = loader.clone();
            let mut job = Box::pin(async move {
                loader
                    .run(|permit| async move {
                        let _permit = permit;
                        std::future::pending::<()>().await;
                    })
                    .await;
            });
            assert!(poll!(job.as_mut()).is_pending());
            held.push(job);
        }
        let (started, running) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (finished, done) = oneshot::channel();
        let mut caller = Box::pin(loader.run(|permit| async move {
            tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                gate.recv().unwrap();
                drop(permit);
                finished.send(()).unwrap();
            })
            .await
            .unwrap();
        }));
        assert!(poll!(caller.as_mut()).is_pending());
        running.await.unwrap();
        drop(caller);

        let mut next = Box::pin(loader.run(|_permit| async { "next load" }));
        let before_release = poll!(next.as_mut());
        // Release the worker before asserting, so even a failed assertion
        // cannot strand the runtime waiting for a blocking thread to finish.
        release.send(()).unwrap();
        done.await.unwrap();
        assert!(
            before_release.is_pending(),
            "cancelled decode still occupies its slot"
        );
        assert_eq!(next.await, "next load");
    }

    #[tokio::test]
    async fn failed_jobs_release_their_slots() {
        let loader = Loader::default();
        for _ in 0..5 {
            let mut load = Box::pin(loader.run(|permit| async move {
                let _permit = permit;
                Err::<(), _>("synthetic fetch failure")
            }));
            assert_eq!(
                poll!(load.as_mut()),
                Poll::Ready(Err("synthetic fetch failure"))
            );
        }
    }
}
