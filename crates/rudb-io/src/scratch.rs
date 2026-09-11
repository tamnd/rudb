//! A directory under the system temporary directory that removes itself.
//!
//! Written out rather than pulled in, because a four line struct is cheaper than a dependency and
//! `spec/18-package-layout.md` is strict about what the foundation crates are allowed to depend on.
//!
//! It is here rather than next to the first test that wanted one because it is now the second, and
//! a name that two modules pick out of the air twice is a name that will be picked a third time.

use std::path::{Path, PathBuf};

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// A fresh directory, named after the caller and the clock.
    ///
    /// The clock is in the name because the test binary can be running beside another copy of
    /// itself, and two runs sharing a directory is a failure that only shows up when somebody is
    /// watching something else.
    pub(crate) fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("rudb-io-{tag}-{unique}"));
        std::fs::create_dir_all(&path).expect("could not make a temporary directory");
        Self(path)
    }

    pub(crate) fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// The directory itself, for a test that needs the root and not a file under it.
    #[allow(dead_code, reason = "only the Linux tests in machine.rs want the root on its own")]
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
