//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use browser_core::fingerprint::AttributeDict;
use browser_core::ElementFingerprint;

/// A database file in the system temp directory that removes itself — together
/// with its `-wal` and `-shm` companions — when the test ends.
///
/// `tempfile` is not a dependency of this crate; a dozen lines here are cheaper
/// than adding one for the tests alone.
pub struct TempDb {
    path: PathBuf,
}

impl TempDb {
    /// Reserves a unique, not-yet-created database path. `label` only makes the
    /// file recognisable if a test ever leaves one behind.
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "browser-storage-it-{label}-{}-{unique}.db",
            std::process::id(),
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    /// The reserved path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.path.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(path));
        }
    }
}

/// The fingerprint `_StorageTools.element_to_dict()` produces for the `<p>` of
/// `<div><p id="target" class="main">Hello</p></div>`, which is the element
/// `tests/core/test_storage_core.py` saves everywhere.
pub fn target_paragraph() -> ElementFingerprint {
    let mut attributes = AttributeDict::new();
    attributes.insert("id".to_owned(), "target".to_owned());
    attributes.insert("class".to_owned(), "main".to_owned());

    ElementFingerprint {
        tag: "p".to_owned(),
        attributes,
        text: Some("Hello".to_owned()),
        path: vec![
            "html".to_owned(),
            "body".to_owned(),
            "div".to_owned(),
            "p".to_owned(),
        ],
        parent_name: Some("div".to_owned()),
        parent_attribs: Some(AttributeDict::new()),
        parent_text: None,
        siblings: None,
        children: None,
    }
}

/// A fingerprint with a single distinguishing attribute and text, for tests
/// that need many different values.
pub fn numbered_paragraph(index: usize) -> ElementFingerprint {
    let mut fingerprint = target_paragraph();
    fingerprint
        .attributes
        .insert("id".to_owned(), format!("elem-{index}"));
    fingerprint.text = Some(format!("Text {index}"));
    fingerprint
}
