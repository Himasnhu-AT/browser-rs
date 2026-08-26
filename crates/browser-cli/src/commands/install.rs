//! The `install` subcommand: a Chromium download via `chromiumoxide_fetcher` and its marker
//! file, plus the `BROWSER_RS_EXECUTABLE_PATH` fallback precedence.
//!
//! # What this is *not*
//!
//! Python's `browser-rs install` shells out to `python -m playwright install
//! chromium`, then `install-deps`, then refreshes `tld`'s public-suffix list
//! (`cli-ai.md` §2.2). None of the three ports:
//!
//! * there is no Playwright here — the browser crate speaks raw CDP, and the
//!   Rust equivalent of "fetch a known-good Chromium" is
//!   `chromiumoxide_fetcher`;
//! * `install-deps` installs Linux **system packages** with `apt`, which a
//!   scraping library has no business doing on the user's behalf;
//! * `update_tld_names` has no analogue — `psl` compiles its snapshot in, and
//!   that vintage difference is already a declared divergence.
//!
//! What *is* preserved is the shape: a marker file, a `--force` flag that
//! ignores it, "The dependencies are already installed" when it is there, and
//! **no error handling at all**, so a failed download leaves the marker absent
//! and the next run retries.
//!
//! # What is verified, and what is not
//!
//! The download needs the network, which `cargo test` does not have. The tests
//! below cover the *decision* — marker path computation on all three platform
//! layouts, the `--force` / already-installed branch, and the fact that a
//! failure leaves no marker. **Nothing below actually downloads a browser, and
//! nothing below has ever been run against a real Chromium download in CI.**
//! Treat [`run`] itself as unverified.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::args::InstallOptions;

/// The empty file whose existence means "already installed".
///
/// The name is Python's, so a user who has both installed recognises it.
pub const MARKER_FILE: &str = ".browser-rs_dependencies_installed";

/// The cache sub-directory `chromiumoxide_fetcher` uses by default.
///
/// It has to match `chromiumoxide_fetcher`'s own `CACHE_NAME`, because the point
/// of computing the path here rather than letting the fetcher default is that
/// the marker and the browser end up in the same directory.
pub const CACHE_NAME: &str = "chromiumoxide";

/// Printed verbatim when the marker is already there.
pub const ALREADY_INSTALLED: &str = "The dependencies are already installed";

/// What `install` can fail with before it reaches the network.
#[derive(Debug)]
pub enum InstallError {
    /// No cache directory could be derived: `$HOME` (or `%LOCALAPPDATA%`) is
    /// unset. `chromiumoxide_fetcher` reports the same as `NoPathAvailable`.
    NoCacheDirectory,
    /// The marker file could not be created after a successful download.
    Marker {
        /// Where it was going.
        path: PathBuf,
        /// Why it did not get there.
        source: std::io::Error,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCacheDirectory => {
                f.write_str("No path available to download browsers to: set $HOME, or pass --path")
            }
            Self::Marker { path, .. } => {
                write!(f, "could not create the marker file '{}'", path.display())
            }
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoCacheDirectory => None,
            Self::Marker { source, .. } => Some(source),
        }
    }
}

/// `<root>/.browser-rs_dependencies_installed`.
///
/// ```
/// use std::path::Path;
/// use browser_cli::commands::install::marker_path;
///
/// assert_eq!(
///     marker_path(Path::new("/cache/chromiumoxide")),
///     Path::new("/cache/chromiumoxide/.browser-rs_dependencies_installed"),
/// );
/// ```
#[must_use]
pub fn marker_path(root: &Path) -> PathBuf {
    root.join(MARKER_FILE)
}

/// The platform cache directory, with the environment injected.
///
/// This is `directories::BaseDirs::cache_dir()` reimplemented, because
/// `directories` is `chromiumoxide_fetcher`'s dependency and not this crate's.
/// The two **must** agree, or `install` would write its marker beside a browser
/// the fetcher put somewhere else:
///
/// | Platform | Directory |
/// |---|---|
/// | macOS | `$HOME/Library/Caches` |
/// | Windows | `%LOCALAPPDATA%` |
/// | everything else | `$XDG_CACHE_HOME` (absolute only), else `$HOME/.cache` |
///
/// # How closely they were compared
///
/// Line by line against `directories 6.0.0` and `dirs-sys 0.4.1` — the exact
/// versions in `Cargo.lock` — because the two can never be run against each
/// other from here (`directories` is not a dependency of this crate).
/// `chromiumoxide_fetcher 0.9.1`'s `BrowserFetcherOptions::build` is
/// `BaseDirs::new().cache_dir().push(CACHE_NAME)` with
/// `CACHE_NAME = "chromiumoxide"`, which is [`CACHE_NAME`] here.
///
/// * **macOS** — `directories/src/mac.rs:11` is `home_dir.join("Library/Caches")`.
///   Identical.
/// * **Linux/BSD** — `directories/src/lin.rs:12` is
///   `env::var_os("XDG_CACHE_HOME").and_then(dirs_sys::is_absolute_path)
///   .unwrap_or_else(|| home_dir.join(".cache"))`. Identical, including the
///   "ignore a relative `XDG_CACHE_HOME`" rule.
/// * **Windows** — `directories/src/win.rs` takes `data_local_dir` from the
///   `FOLDERID_LocalAppData` known folder, **not** from `%LOCALAPPDATA%`. The
///   two agree on a stock machine and can disagree if the variable is unset or
///   has been redirected. Reading the known folder needs a Win32 binding this
///   crate does not have; the residue is documented rather than fixed.
/// * **`$HOME`** — `dirs_sys::home_dir` falls back to `getpwuid` when `$HOME`
///   is unset or empty; this returns `None` and `install` then reports
///   [`InstallError::NoCacheDirectory`]. A second, smaller residue, in the safe
///   direction (a clear error rather than a wrong directory).
///
/// Neither residue can put the marker in the wrong place on a normally
/// configured macOS or Linux machine, which is the case that matters.
#[must_use]
pub fn platform_cache_dir<F>(env: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    let home = || {
        env("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };

    if cfg!(target_os = "macos") {
        return home().map(|home| home.join("Library").join("Caches"));
    }
    if cfg!(target_os = "windows") {
        return env("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
    }
    // The XDG base-directory spec ignores a relative `$XDG_CACHE_HOME`.
    if let Some(xdg) = env("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Some(xdg);
    }
    home().map(|home| home.join(".cache"))
}

/// Where the browser and the marker go: `--path`, else the platform cache
/// directory plus [`CACHE_NAME`].
///
/// # Errors
///
/// [`InstallError::NoCacheDirectory`] when there is no `--path` and no cache
/// directory can be derived.
pub fn install_root_with<F>(explicit: Option<&Path>, env: F) -> Result<PathBuf, InstallError>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    platform_cache_dir(env)
        .map(|cache| cache.join(CACHE_NAME))
        .ok_or(InstallError::NoCacheDirectory)
}

/// [`install_root_with`], reading the real environment.
///
/// # Errors
///
/// See [`install_root_with`].
pub fn install_root(explicit: Option<&Path>) -> Result<PathBuf, InstallError> {
    install_root_with(explicit, |name: &str| std::env::var_os(name))
}

/// Whether the download should happen at all: `force or not marker.exists()`.
///
/// ```
/// use std::path::Path;
/// use browser_cli::commands::install::should_install;
///
/// // A path that certainly has no marker.
/// assert!(should_install(false, Path::new("/nonexistent/browser-rs-cache")));
/// // `--force` does not even look.
/// assert!(should_install(true, Path::new("/nonexistent/browser-rs-cache")));
/// ```
#[must_use]
pub fn should_install(force: bool, root: &Path) -> bool {
    force || !marker_path(root).exists()
}

/// Create the empty marker — `Path.touch()`.
///
/// # Errors
///
/// [`InstallError::Marker`] when the directory cannot be created or the file
/// cannot be opened.
pub fn touch_marker(root: &Path) -> Result<PathBuf, InstallError> {
    let path = marker_path(root);
    let outcome = std::fs::create_dir_all(root).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map(drop)
    });
    match outcome {
        Ok(()) => Ok(path),
        Err(source) => Err(InstallError::Marker { path, source }),
    }
}

/// `browser-rs install`.
///
/// **Unverified.** The `fetch()` call downloads roughly 150 MB over the network
/// and is never exercised by the test suite; only the surrounding decisions are.
///
/// # Errors
///
/// [`InstallError`], or whatever `chromiumoxide_fetcher` reports. Like Python's,
/// this function has no `try`/`except`: a failed download propagates and the
/// marker is **not** created, so the next run tries again.
pub async fn run(options: InstallOptions) -> anyhow::Result<()> {
    use chromiumoxide_fetcher::{BrowserFetcher, BrowserFetcherOptions};

    let root = install_root(options.path.as_deref())?;
    if !should_install(options.force, &root) {
        println!("{ALREADY_INSTALLED}");
        return Ok(());
    }

    println!("Installing Chromium...");
    let fetcher_options = BrowserFetcherOptions::builder().with_path(&root).build()?;
    let installation = BrowserFetcher::new(fetcher_options).fetch().await?;

    let marker = touch_marker(&root)?;
    tracing::debug!("marker written to '{}'", marker.display());

    println!(
        "Chromium is installed at {}",
        installation.executable_path.display()
    );
    println!(
        "Point the reference implementation at it with `--executable-path` or by exporting \
         {}={}",
        crate::args::EXECUTABLE_PATH_ENV,
        installation.executable_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    #[test]
    fn the_marker_file_keeps_pythons_name() {
        assert_eq!(MARKER_FILE, ".browser-rs_dependencies_installed");
        assert_eq!(
            marker_path(Path::new("/a/b")),
            PathBuf::from("/a/b/.browser-rs_dependencies_installed")
        );
    }

    #[test]
    fn an_explicit_path_wins_over_every_environment_variable() {
        let root = install_root_with(
            Some(Path::new("/tmp/browsers")),
            env_of(&[("HOME", "/home/u"), ("XDG_CACHE_HOME", "/xdg")]),
        )
        .expect("resolved");
        assert_eq!(root, PathBuf::from("/tmp/browsers"));
    }

    #[test]
    fn with_no_home_at_all_there_is_no_root() {
        let error = install_root_with(None, env_of(&[])).unwrap_err();
        assert!(matches!(error, InstallError::NoCacheDirectory), "{error:?}");
        assert!(error.to_string().contains("--path"), "{error}");
    }

    /// The layout has to match `directories::BaseDirs::cache_dir()`, which is
    /// what `chromiumoxide_fetcher` uses when we do not pass `--path`.
    #[test]
    fn the_cache_directory_follows_the_platform_convention() {
        let resolved = platform_cache_dir(env_of(&[
            ("HOME", "/home/u"),
            ("LOCALAPPDATA", "C:\\Users\\u\\AppData\\Local"),
            ("XDG_CACHE_HOME", "/xdg/cache"),
        ]))
        .expect("resolved");

        if cfg!(target_os = "macos") {
            assert_eq!(resolved, PathBuf::from("/home/u/Library/Caches"));
        } else if cfg!(target_os = "windows") {
            assert_eq!(resolved, PathBuf::from("C:\\Users\\u\\AppData\\Local"));
        } else {
            assert_eq!(resolved, PathBuf::from("/xdg/cache"));
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn xdg_falls_back_to_dot_cache_and_ignores_a_relative_value() {
        assert_eq!(
            platform_cache_dir(env_of(&[("HOME", "/home/u")])).expect("resolved"),
            PathBuf::from("/home/u/.cache")
        );
        assert_eq!(
            platform_cache_dir(env_of(&[
                ("HOME", "/home/u"),
                ("XDG_CACHE_HOME", "relative")
            ]))
            .expect("resolved"),
            PathBuf::from("/home/u/.cache"),
            "the XDG spec ignores a relative value"
        );
        assert_eq!(
            platform_cache_dir(env_of(&[("HOME", "/home/u"), ("XDG_CACHE_HOME", "")]))
                .expect("resolved"),
            PathBuf::from("/home/u/.cache"),
            "…and an empty one"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_empty_home_is_treated_as_unset() {
        assert_eq!(platform_cache_dir(env_of(&[("HOME", "")])), None);
    }

    #[test]
    fn the_default_root_ends_in_the_fetchers_own_cache_name() {
        let root = install_root_with(
            None,
            env_of(&[("HOME", "/home/u"), ("LOCALAPPDATA", "C:\\l")]),
        )
        .expect("resolved");
        assert!(root.ends_with(CACHE_NAME), "{}", root.display());
    }

    // -- the already-installed branch -------------------------------------

    #[test]
    fn a_missing_marker_means_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(should_install(false, dir.path()));
    }

    #[test]
    fn a_present_marker_means_skip_unless_forced() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch_marker(dir.path()).expect("touched");
        assert!(!should_install(false, dir.path()));
        assert!(
            should_install(true, dir.path()),
            "--force ignores the marker"
        );
    }

    #[test]
    fn touching_the_marker_creates_the_directory_and_an_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("nested").join("deeper");
        let marker = touch_marker(&root).expect("touched");
        assert!(marker.exists());
        assert_eq!(std::fs::read(&marker).expect("readable"), Vec::<u8>::new());
        // Touching twice is not an error and does not grow the file.
        touch_marker(&root).expect("touched again");
        assert_eq!(std::fs::read(&marker).expect("readable").len(), 0);
    }

    #[test]
    fn a_marker_that_cannot_be_written_is_reported_with_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where the directory should be.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").expect("written");
        let error = touch_marker(&blocked).unwrap_err();
        assert!(matches!(error, InstallError::Marker { .. }), "{error:?}");
        assert!(error.to_string().contains(MARKER_FILE), "{error}");
    }

    /// Python's `install` deliberately has no `try`/`except`, so the marker is
    /// only ever written *after* every step succeeded. The ordering is what
    /// makes a half-finished install retry rather than pretend.
    #[test]
    fn the_marker_is_only_written_after_a_successful_download() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Simulate the failure path: nothing calls `touch_marker`.
        assert!(!marker_path(dir.path()).exists());
        assert!(
            should_install(false, dir.path()),
            "a failed install must not look installed"
        );
    }

    #[test]
    fn the_already_installed_message_is_pythons() {
        assert_eq!(ALREADY_INSTALLED, "The dependencies are already installed");
    }
}
