//! Recent files list — persisted to the platform per-app config dir
//! (`~/.config/vector-editor/recent.json` on Linux, equivalent on macOS /
//! Windows via the `directories` crate). Survives across sessions; capped
//! at [`MAX_RECENT`] entries.
//!
//! All on-disk failures are demoted to logged warnings — losing or
//! mis-parsing the recent-files file must never break the editor itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Max entries kept in the list. Older entries are dropped when this is
/// exceeded. Matches the convention of editors like VS Code / Inkscape.
pub(crate) const MAX_RECENT: usize = 10;

const CONFIG_FILE: &str = "recent.json";

/// In-memory + on-disk recent files list. Most-recently-opened first.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct RecentFiles {
    /// `#[serde(default)]` so a future addition of fields stays
    /// backwards-compatible with existing config files.
    #[serde(default)]
    paths: Vec<PathBuf>,
}

impl RecentFiles {
    /// Load from disk. On any failure (missing file, parse error, IO error)
    /// returns an empty list. Missing-file is silent (first run); other
    /// errors are logged at warn level.
    pub(crate) fn load() -> Self {
        let Some(path) = config_path() else {
            log::warn!("Recent files: no config dir available, list will not persist");
            return Self::default();
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(rf) => rf,
                Err(e) => {
                    log::warn!("Recent files: failed to parse {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!("Recent files: failed to read {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Save to disk. Best-effort — errors are logged but not propagated, so
    /// failing to persist the list never breaks a successful file open.
    pub(crate) fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            log::warn!(
                "Recent files: failed to create config dir {}: {e}",
                parent.display()
            );
            return;
        }
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    log::warn!("Recent files: failed to write {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("Recent files: failed to serialise: {e}"),
        }
    }

    /// Insert (or move) `path` to the front, deduplicated, capped at
    /// [`MAX_RECENT`]. Stores the canonicalised path when canonicalisation
    /// succeeds, so the same file accessed via different relative paths or
    /// symlinks occupies a single slot.
    pub(crate) fn add(&mut self, path: &Path) {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.paths.retain(|p| p != &canonical);
        self.paths.insert(0, canonical);
        self.paths.truncate(MAX_RECENT);
    }

    /// Most-recently-opened first.
    pub(crate) fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub(crate) fn clear(&mut self) {
        self.paths.clear();
    }
}

/// Resolve the on-disk path of the recent files config. Returns `None` if
/// the platform doesn't expose a per-app config dir (extremely rare — just
/// means the list won't persist that session).
fn config_path() -> Option<PathBuf> {
    // Empty qualifier + organisation produce the cleanest per-platform
    // result: `~/.config/vector-editor` on Linux, sensible defaults on
    // macOS / Windows. The `application` arg drives the bundle name.
    directories::ProjectDirs::from("", "", "vector-editor")
        .map(|d| d.config_dir().join(CONFIG_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes_and_moves_to_front() {
        let mut rf = RecentFiles::default();
        // Use paths that won't canonicalise (don't exist) so the test is
        // independent of the local filesystem layout.
        let a = PathBuf::from("/nonexistent/a.svg");
        let b = PathBuf::from("/nonexistent/b.svg");
        rf.add(&a);
        rf.add(&b);
        rf.add(&a); // re-add a → moves to front
        assert_eq!(rf.paths(), &[a, b]);
    }

    #[test]
    fn add_caps_at_max_recent() {
        let mut rf = RecentFiles::default();
        for i in 0..(MAX_RECENT + 5) {
            rf.add(&PathBuf::from(format!("/nonexistent/file{i}.svg")));
        }
        assert_eq!(rf.paths().len(), MAX_RECENT);
        // Newest first: the last-added file should be at index 0.
        assert_eq!(
            rf.paths()[0],
            PathBuf::from(format!("/nonexistent/file{}.svg", MAX_RECENT + 4))
        );
    }

    #[test]
    fn clear_empties() {
        let mut rf = RecentFiles::default();
        rf.add(&PathBuf::from("/nonexistent/a.svg"));
        rf.clear();
        assert!(rf.paths().is_empty());
    }

    #[test]
    fn roundtrips_through_json() {
        let mut rf = RecentFiles::default();
        rf.add(&PathBuf::from("/nonexistent/a.svg"));
        rf.add(&PathBuf::from("/nonexistent/b.svg"));
        let json = serde_json::to_string(&rf).unwrap();
        let parsed: RecentFiles = serde_json::from_str(&json).unwrap();
        assert_eq!(rf.paths(), parsed.paths());
    }
}
