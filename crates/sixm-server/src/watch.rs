//! Development hot-reload watcher for running resources.
//!
//! Disabled by default. When enabled via `--watch` or `watch_resources on`,
//! [`ResourceWatcher`] polls running resource folders every [`POLL_INTERVAL`]
//! (~500 ms) using symlink-safe `symlink_metadata` checks and reports running
//! resources whose manifest or script files changed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use sixm_core::ResourceName;

/// Interval between filesystem watch polls when hot-reload is enabled.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);

type FileSnapshot = BTreeMap<PathBuf, Option<SystemTime>>;

/// Tracks file modification times for running resources.
#[derive(Debug, Default)]
pub(crate) struct ResourceWatcher {
    last_poll: Option<Instant>,
    snapshots: HashMap<ResourceName, FileSnapshot>,
}

impl ResourceWatcher {
    /// Clears all tracked snapshots and poll timing.
    pub(crate) fn reset(&mut self) {
        self.last_poll = None;
        self.snapshots.clear();
    }

    /// Returns `true` and advances the poll timestamp if [`POLL_INTERVAL`] has
    /// elapsed since the previous poll (or no poll has run yet).
    pub(crate) fn poll_due(&mut self, now: Instant) -> bool {
        if self
            .last_poll
            .is_none_or(|prev| now.saturating_duration_since(prev) >= POLL_INTERVAL)
        {
            self.last_poll = Some(now);
            true
        } else {
            false
        }
    }

    /// Compares current watched files of `running` resources against recorded
    /// baselines. Newly observed resources are baselined without triggering a
    /// change. Returns the names of running resources whose watched files
    /// changed.
    pub(crate) fn detect_changes(
        &mut self,
        running: &[(ResourceName, PathBuf)],
    ) -> Vec<ResourceName> {
        let active: HashSet<&ResourceName> = running.iter().map(|(name, _)| name).collect();
        self.snapshots.retain(|name, _| active.contains(name));

        let mut changed = Vec::new();
        for (name, root) in running {
            let current = collect_resource_files(root);
            match self.snapshots.get(name) {
                None => {
                    self.snapshots.insert(name.clone(), current);
                }
                Some(previous) if *previous != current => {
                    self.snapshots.insert(name.clone(), current);
                    changed.push(name.clone());
                }
                Some(_) => {}
            }
        }
        changed
    }

    /// Refreshes baselines for `running` resources without reporting changes.
    pub(crate) fn seed(&mut self, running: &[(ResourceName, PathBuf)]) {
        let active: HashSet<&ResourceName> = running.iter().map(|(name, _)| name).collect();
        self.snapshots.retain(|name, _| active.contains(name));
        for (name, root) in running {
            self.snapshots
                .insert(name.clone(), collect_resource_files(root));
        }
    }
}

/// Walks `root` without following symlinks and records mtimes for manifests and
/// `.lua`/`.js` script files, ignoring hidden entries, `[...]` folders, and
/// `data/` subdirectories.
fn collect_resource_files(root: &Path) -> FileSnapshot {
    let mut files = BTreeMap::new();
    let Ok(root_meta) = fs::symlink_metadata(root) else {
        return files;
    };
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return files;
    }
    if root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_ignored_component)
    {
        return files;
    }

    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_ignored_component(name) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if !name.eq_ignore_ascii_case("data") {
                    dirs.push(path);
                }
            } else if meta.is_file() && is_watched_file(name) {
                files.insert(path, meta.modified().ok());
            }
        }
    }
    files
}

fn is_ignored_component(name: &str) -> bool {
    name.starts_with('.') || (name.starts_with('[') && name.ends_with(']'))
}

fn is_watched_file(file_name: &str) -> bool {
    if file_name.eq_ignore_ascii_case("fxmanifest.lua")
        || file_name.eq_ignore_ascii_case("__resource.lua")
    {
        return true;
    }
    Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("lua") || ext.eq_ignore_ascii_case("js"))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use sixm_core::ResourceName;

    use super::{POLL_INTERVAL, ResourceWatcher};

    fn write(path: &Path, content: &str, modified: SystemTime) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(path, content).expect("write file");
        File::options()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(modified)
            .expect("set_modified");
    }

    fn res(name: &str) -> ResourceName {
        Arc::from(name)
    }

    #[test]
    fn detects_manifest_lua_and_js_changes_but_not_initial_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("app");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = t0 + Duration::from_secs(5);
        let t2 = t0 + Duration::from_secs(10);

        let manifest = root.join("fxmanifest.lua");
        let lua = root.join("server.lua");
        let js = root.join("shared/util.js");
        write(&manifest, "fx_version 'cerulean'\n", t0);
        write(&lua, "print('v1')\n", t0);
        write(&js, "export const x = 1;\n", t0);

        let running = vec![(res("app"), root)];
        let mut watcher = ResourceWatcher::default();

        // First scan only seeds baselines.
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());

        // Modifying a Lua file triggers a change.
        write(&lua, "print('v2')\n", t1);
        assert_eq!(watcher.detect_changes(&running), vec![res("app")]);
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());

        // Modifying a JS file or manifest triggers a change.
        write(&js, "export const x = 2;\n", t2);
        assert_eq!(watcher.detect_changes(&running), vec![res("app")]);
        write(&manifest, "fx_version 'bodacious'\n", t2);
        assert_eq!(watcher.detect_changes(&running), vec![res("app")]);
    }

    #[test]
    fn ignores_data_hidden_category_and_non_script_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("app");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = t0 + Duration::from_secs(5);

        write(&root.join("__resource.lua"), "server_script 's.lua'\n", t0);
        write(&root.join("s.lua"), "return 1\n", t0);

        let running = vec![(res("app"), root.clone())];
        let mut watcher = ResourceWatcher::default();
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());

        // Writing inside data/, .hidden/, [cat]/, or non-script files must not trigger.
        write(&root.join("data/state.json"), "{}", t1);
        write(&root.join("data/generated.lua"), "return 2\n", t1);
        write(&root.join(".git/hooks/pre.lua"), "return 3\n", t1);
        write(&root.join(".secret.lua"), "return 4\n", t1);
        write(&root.join("[nested]/ignored.js"), "console.log(1)\n", t1);
        write(&root.join("README.md"), "# readme\n", t1);
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());

        // Updating __resource.lua does trigger.
        write(
            &root.join("__resource.lua"),
            "server_script 's.lua'\n-- edit\n",
            t1,
        );
        assert_eq!(watcher.detect_changes(&running), vec![res("app")]);
    }

    #[cfg(unix)]
    #[test]
    fn ignores_symlinks_safely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside.lua");
        let root = dir.path().join("app");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let t1 = t0 + Duration::from_secs(5);

        write(&outside, "print('outside')\n", t0);
        write(&root.join("fxmanifest.lua"), "fx_version 'cerulean'\n", t0);
        std::os::unix::fs::symlink(&outside, root.join("linked.lua")).expect("symlink file");

        let running = vec![(res("app"), root)];
        let mut watcher = ResourceWatcher::default();
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());

        write(&outside, "print('outside v2')\n", t1);
        assert_eq!(watcher.detect_changes(&running), Vec::<ResourceName>::new());
    }

    #[test]
    fn poll_due_throttles_by_poll_interval() {
        let mut watcher = ResourceWatcher::default();
        let t0 = Instant::now();
        assert!(watcher.poll_due(t0));
        assert!(!watcher.poll_due(t0 + Duration::from_millis(100)));
        assert!(!watcher.poll_due(t0 + Duration::from_millis(499)));
        assert!(watcher.poll_due(t0 + POLL_INTERVAL));
    }
}
