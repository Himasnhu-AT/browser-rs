//! [`CheckpointManager`]: versioned JSON (not pickle) written via temp file plus atomic
//! rename, with `save` re-raising and `cleanup` swallowing.
//!
//! # Why JSON
//!
//! Python writes `pickle.dumps(CheckpointData, HIGHEST_PROTOCOL)` to `checkpoint.pkl`.
//! That format is unreadable outside CPython and is a remote-code-execution primitive when
//! the file is not trusted, so the port writes a **versioned JSON document** instead. This
//! is the deliberate divergence recorded in `docs/DIVERGENCES.md`; a checkpoint written by
//! Python is not resumable here and vice versa. Everything else is preserved:
//!
//! * the same directory layout — one checkpoint file inside `crawldir`, written through a
//!   `checkpoint.tmp` sibling;
//! * the same atomic write (temp file, then rename over the target), so a crash mid-save
//!   never truncates a good checkpoint;
//! * the same `interval` validation and semantics (`0` disables *periodic* saves but not
//!   the final one on pause);
//! * the same log lines, at the same levels;
//! * the same error asymmetry, which the Python tests pin: **[`CheckpointManager::save`]
//!   propagates its error** (the engine must not mark the crawl "paused" when the
//!   checkpoint did not land), while [`CheckpointManager::load`] and
//!   [`CheckpointManager::cleanup`] swallow theirs and degrade to "start fresh" / "leave
//!   the file".

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SpiderError};
use crate::request::{Fingerprint, Request};

/// The checkpoint file name inside `crawldir`.
///
/// Python calls it `checkpoint.pkl`; the extension follows the format, which here is JSON.
pub const CHECKPOINT_FILE: &str = "checkpoint.json";

/// The temp file the checkpoint is written through before the atomic rename.
///
/// Python derives it with `Path.with_suffix(".tmp")`, giving exactly this name.
pub const CHECKPOINT_TEMP_FILE: &str = "checkpoint.tmp";

/// The on-disk format version. Bumped whenever [`CheckpointData`]'s shape changes; a file
/// carrying any other value is refused by [`CheckpointManager::load`] (and, like every
/// other load failure, that just means the crawl starts fresh).
pub const CHECKPOINT_VERSION: u32 = 1;

/// Python's default checkpoint interval, in seconds.
pub const DEFAULT_INTERVAL_SECS: f64 = 300.0;

/// Everything a crawl needs to resume: the pending queue and the dedup set.
///
/// AutoThrottle delays are deliberately **not** included, matching Python — every domain
/// restarts at `autothrottle_start_delay` after a resume.
/// (No `PartialEq`: [`Request`] deliberately has none — Python's `__eq__` raises when a
/// fingerprint is missing, so equality is the explicit
/// [`Request::same_fingerprint`](crate::request::Request::same_fingerprint).)
#[derive(Clone, Debug, Default)]
pub struct CheckpointData {
    /// Queued and in-flight requests, in queue order.
    pub requests: Vec<Request>,
    /// Every fingerprint the scheduler has seen.
    pub seen: HashSet<Fingerprint>,
}

impl From<(Vec<Request>, HashSet<Fingerprint>)> for CheckpointData {
    /// Builds a [`CheckpointData`] straight from [`crate::scheduler::Scheduler::snapshot`].
    fn from((requests, seen): (Vec<Request>, HashSet<Fingerprint>)) -> Self {
        Self { requests, seen }
    }
}

/// The serialised shape. Kept separate from [`CheckpointData`] so the in-memory type can
/// use `HashSet<[u8; 20]>` while the file stores sorted hex strings (stable across runs,
/// and diffable).
#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    version: u32,
    requests: Vec<Request>,
    seen: Vec<String>,
}

impl CheckpointFile {
    fn encode(data: &CheckpointData) -> Self {
        let mut seen: Vec<String> = data.seen.iter().map(hex::encode).collect();
        seen.sort_unstable();
        Self {
            version: CHECKPOINT_VERSION,
            requests: data.requests.clone(),
            seen,
        }
    }

    fn decode(self) -> std::result::Result<CheckpointData, String> {
        if self.version != CHECKPOINT_VERSION {
            return Err(format!(
                "unsupported checkpoint version {} (this build writes {CHECKPOINT_VERSION})",
                self.version
            ));
        }
        let mut seen = HashSet::with_capacity(self.seen.len());
        for text in &self.seen {
            let mut fingerprint = Fingerprint::default();
            hex::decode_to_slice(text, &mut fingerprint)
                .map_err(|e| format!("bad fingerprint {text:?}: {e}"))?;
            seen.insert(fingerprint);
        }
        Ok(CheckpointData {
            requests: self.requests,
            seen,
        })
    }
}

/// Reads and writes the crawl's checkpoint file.
///
/// ```
/// # tokio_test_block(async {
/// use browser_spider::checkpoint::{CheckpointData, CheckpointManager};
/// use browser_spider::request::Request;
///
/// let dir = tempfile::tempdir().unwrap();
/// let manager = CheckpointManager::new(dir.path(), 300.0).unwrap();
/// assert!(!manager.has_checkpoint().await);
///
/// let data = CheckpointData {
///     requests: vec![Request::new("http://example.com/")],
///     seen: Default::default(),
/// };
/// manager.save(&data).await.unwrap();
/// assert_eq!(manager.load().await.unwrap().requests.len(), 1);
///
/// manager.cleanup().await;
/// assert!(!manager.has_checkpoint().await);
/// # });
/// # fn tokio_test_block<F: std::future::Future>(f: F) { tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f); }
/// ```
#[derive(Clone, Debug)]
pub struct CheckpointManager {
    crawldir: PathBuf,
    checkpoint_path: PathBuf,
    temp_path: PathBuf,
    interval: f64,
}

impl CheckpointManager {
    /// A manager writing into `crawldir` every `interval` seconds.
    ///
    /// `interval == 0.0` disables *periodic* saves; the final save on pause still happens.
    ///
    /// # Errors
    ///
    /// [`SpiderError::CheckpointIntervalNegative`] for a negative interval and
    /// [`SpiderError::CheckpointIntervalNotANumber`] for a NaN or infinity — the two
    /// exceptions Python's constructor raises.
    pub fn new(crawldir: impl Into<PathBuf>, interval: f64) -> Result<Self> {
        if !interval.is_finite() {
            return Err(SpiderError::CheckpointIntervalNotANumber);
        }
        if interval < 0.0 {
            return Err(SpiderError::CheckpointIntervalNegative);
        }
        let crawldir = crawldir.into();
        Ok(Self {
            checkpoint_path: crawldir.join(CHECKPOINT_FILE),
            temp_path: crawldir.join(CHECKPOINT_TEMP_FILE),
            crawldir,
            interval,
        })
    }

    /// The directory checkpoints are written into.
    pub fn crawldir(&self) -> &Path {
        &self.crawldir
    }

    /// The checkpoint file's path.
    pub fn path(&self) -> &Path {
        &self.checkpoint_path
    }

    /// The configured interval in seconds. `0.0` disables periodic saves.
    pub fn interval(&self) -> f64 {
        self.interval
    }

    /// The interval as a [`Duration`], for use with `tokio::time`.
    pub fn interval_duration(&self) -> Duration {
        Duration::from_secs_f64(self.interval)
    }

    /// Whether a checkpoint file exists.
    pub async fn has_checkpoint(&self) -> bool {
        tokio::fs::try_exists(&self.checkpoint_path)
            .await
            .unwrap_or(false)
    }

    /// Write `data` atomically.
    ///
    /// # Errors
    ///
    /// Every failure is logged at `ERROR` and then **returned**, after a best-effort
    /// cleanup of the temp file. The engine depends on this: on a force stop it saves
    /// before cancelling, and a failed save must not be mistaken for a successful pause
    /// (which would let the `finally` block delete the user's previous checkpoint).
    pub async fn save(&self, data: &CheckpointData) -> Result<()> {
        match self.try_save(data).await {
            Ok(()) => {
                tracing::info!(
                    "Checkpoint saved: {} requests, {} seen URLs",
                    data.requests.len(),
                    data.seen.len()
                );
                Ok(())
            }
            Err(error) => {
                // Best effort, exactly like Python's `if await temp_path.exists(): unlink()`.
                let _ = tokio::fs::remove_file(&self.temp_path).await;
                tracing::error!("Failed to save checkpoint: {error}");
                Err(error)
            }
        }
    }

    async fn try_save(&self, data: &CheckpointData) -> Result<()> {
        tokio::fs::create_dir_all(&self.crawldir)
            .await
            .map_err(|source| SpiderError::CheckpointSave {
                path: self.crawldir.clone(),
                source,
            })?;

        let encoded = serde_json::to_vec(&CheckpointFile::encode(data)).map_err(|source| {
            SpiderError::Serialize {
                what: "the checkpoint",
                source,
            }
        })?;

        tokio::fs::write(&self.temp_path, &encoded)
            .await
            .map_err(|source| SpiderError::CheckpointSave {
                path: self.temp_path.clone(),
                source,
            })?;

        // `rename` over an existing file is atomic on every platform we target.
        tokio::fs::rename(&self.temp_path, &self.checkpoint_path)
            .await
            .map_err(|source| SpiderError::CheckpointSave {
                path: self.checkpoint_path.clone(),
                source,
            })
    }

    /// Read the checkpoint, or `None` when there is nothing usable to read.
    ///
    /// A missing file is not an error. A corrupt, truncated or wrong-version file is
    /// logged at `ERROR` and swallowed, so a bad checkpoint costs a fresh crawl rather
    /// than a failed one — Python's `"Failed to load checkpoint (starting fresh)"` path.
    pub async fn load(&self) -> Option<CheckpointData> {
        if !self.has_checkpoint().await {
            return None;
        }
        let data = match tokio::fs::read(&self.checkpoint_path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!("Failed to load checkpoint (starting fresh): {error}");
                return None;
            }
        };
        let file: CheckpointFile = match serde_json::from_slice(&data) {
            Ok(file) => file,
            Err(error) => {
                tracing::error!("Failed to load checkpoint (starting fresh): {error}");
                return None;
            }
        };
        match file.decode() {
            Ok(data) => {
                tracing::info!(
                    "Checkpoint loaded: {} requests, {} seen URLs",
                    data.requests.len(),
                    data.seen.len()
                );
                Some(data)
            }
            Err(error) => {
                tracing::error!("Failed to load checkpoint (starting fresh): {error}");
                None
            }
        }
    }

    /// Delete the checkpoint after a clean finish.
    ///
    /// Never fails: a failure is logged at `WARN` and swallowed. Deleting a checkpoint is
    /// housekeeping, and a crawl that finished successfully must not be reported as failed
    /// because the file could not be removed.
    pub async fn cleanup(&self) {
        match tokio::fs::remove_file(&self.checkpoint_path).await {
            Ok(()) => tracing::debug!("Checkpoint file cleaned up"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("Checkpoint file cleaned up");
            }
            Err(error) => tracing::warn!("Failed to cleanup checkpoint file: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CallbackId, FingerprintOptions};

    fn sample() -> CheckpointData {
        let queued = Request::new("http://example.com/a")
            .with_priority(5)
            .with_callback(CallbackId::from_static("parse_item"));
        queued.update_fingerprint(FingerprintOptions::default());
        let mut seen = HashSet::new();
        seen.insert(queued.fingerprint().expect("fingerprint"));
        seen.insert([7u8; 20]);
        CheckpointData {
            requests: vec![queued, Request::new("http://example.com/b")],
            seen,
        }
    }

    #[tokio::test]
    async fn saves_and_loads_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(dir.path(), 300.0).expect("manager");
        assert!(!manager.has_checkpoint().await);
        assert!(manager.load().await.is_none());

        let data = sample();
        manager.save(&data).await.expect("save");
        assert!(manager.has_checkpoint().await);

        let loaded = manager.load().await.expect("load");
        assert_eq!(loaded.requests.len(), 2);
        assert_eq!(loaded.requests[0].priority, 5);
        assert_eq!(
            loaded.requests[0].callback.as_ref().map(CallbackId::as_str),
            Some("parse_item")
        );
        assert!(loaded.requests[0].fingerprint().is_some());
        assert_eq!(loaded.seen, data.seen);
    }

    #[tokio::test]
    async fn creates_the_crawldir_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("deep/er/still");
        let manager = CheckpointManager::new(&nested, 0.0).expect("manager");
        manager.save(&sample()).await.expect("save");

        assert!(nested.join(CHECKPOINT_FILE).exists());
        assert!(!nested.join(CHECKPOINT_TEMP_FILE).exists());
    }

    #[tokio::test]
    async fn a_second_save_replaces_the_first_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(dir.path(), 1.0).expect("manager");
        manager.save(&sample()).await.expect("save");
        manager
            .save(&CheckpointData {
                requests: vec![Request::new("http://example.com/only")],
                seen: HashSet::new(),
            })
            .await
            .expect("save");

        let loaded = manager.load().await.expect("load");
        assert_eq!(loaded.requests.len(), 1);
        assert_eq!(loaded.requests[0].url, "http://example.com/only");
    }

    #[tokio::test]
    async fn a_corrupt_checkpoint_starts_fresh_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(dir.path(), 1.0).expect("manager");
        tokio::fs::write(manager.path(), b"{not json")
            .await
            .expect("write");
        assert!(manager.load().await.is_none());
    }

    #[tokio::test]
    async fn a_future_version_starts_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(dir.path(), 1.0).expect("manager");
        tokio::fs::write(
            manager.path(),
            br#"{"version":9999,"requests":[],"seen":[]}"#,
        )
        .await
        .expect("write");
        assert!(manager.load().await.is_none());
    }

    #[tokio::test]
    async fn save_propagates_its_error_but_cleanup_swallows() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A *file* where the crawldir should be makes `create_dir_all` fail.
        let blocked = dir.path().join("not-a-dir");
        tokio::fs::write(&blocked, b"x").await.expect("write");

        let manager = CheckpointManager::new(&blocked, 1.0).expect("manager");
        let error = manager.save(&sample()).await.expect_err("save must fail");
        assert!(matches!(error, SpiderError::CheckpointSave { .. }));

        // ... and cleanup is infallible even though the path is nonsense.
        manager.cleanup().await;
    }

    #[tokio::test]
    async fn cleanup_is_a_no_op_without_a_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = CheckpointManager::new(dir.path(), 1.0).expect("manager");
        manager.cleanup().await;
        assert!(!manager.has_checkpoint().await);
    }

    #[test]
    fn interval_is_validated_like_pythons() {
        let dir = std::env::temp_dir();
        assert!(CheckpointManager::new(&dir, 0.0).is_ok());
        assert!(CheckpointManager::new(&dir, 300.0).is_ok());
        assert!(matches!(
            CheckpointManager::new(&dir, -1.0),
            Err(SpiderError::CheckpointIntervalNegative)
        ));
        assert!(matches!(
            CheckpointManager::new(&dir, f64::NAN),
            Err(SpiderError::CheckpointIntervalNotANumber)
        ));
        assert_eq!(
            CheckpointManager::new(&dir, -1.0).unwrap_err().to_string(),
            "Checkpoints interval must be equal or greater than 0."
        );
        assert_eq!(
            CheckpointManager::new(&dir, f64::NAN)
                .unwrap_err()
                .to_string(),
            "Checkpoints interval must be integer or float."
        );
    }

    #[test]
    fn the_file_names_match_pythons_layout() {
        let manager = CheckpointManager::new("/tmp/crawl", 1.0).expect("manager");
        assert_eq!(manager.path(), Path::new("/tmp/crawl/checkpoint.json"));
        assert_eq!(manager.crawldir(), Path::new("/tmp/crawl"));
    }
}
