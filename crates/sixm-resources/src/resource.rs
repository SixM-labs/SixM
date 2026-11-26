//! A resource folder: its manifest and the script files it declares.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use globset::GlobBuilder;
use sixm_core::ResourceName;
use walkdir::WalkDir;

use crate::error::ResourceError;
use crate::manifest::{Manifest, parse_manifest};

/// Manifest file names, in order of preference.
const MANIFEST_FILES: [&str; 2] = ["fxmanifest.lua", "__resource.lua"];
/// Characters that make a script pattern a glob rather than a literal path.
const GLOB_META: [char; 6] = ['*', '?', '[', ']', '{', '}'];

/// A loaded resource: its folder, manifest and parsed declarations.
#[derive(Debug, Clone)]
pub struct ResourceInfo {
    /// Folder name, which is the resource name.
    pub name: ResourceName,
    /// Resource folder.
    pub root: PathBuf,
    /// The manifest file that was evaluated (`fxmanifest.lua` or legacy `__resource.lua`).
    pub manifest_path: PathBuf,
    /// Declarations from the manifest.
    pub manifest: Manifest,
}

impl ResourceInfo {
    /// Loads the resource in `root`.
    ///
    /// The folder must contain `fxmanifest.lua`, or the legacy `__resource.lua`
    /// when there is no `fxmanifest.lua`. Scripts are not checked here; see
    /// [`server_script_files`](Self::server_script_files).
    pub fn load(root: &Path) -> Result<Self, ResourceError> {
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ResourceError::InvalidName {
                path: root.to_path_buf(),
            })?;
        let manifest_path = find_manifest(root).ok_or_else(|| ResourceError::NoManifest {
            path: root.to_path_buf(),
        })?;
        let source = fs::read_to_string(&manifest_path).map_err(|source| ResourceError::Io {
            path: manifest_path.clone(),
            source,
        })?;
        let file_name = manifest_path
            .file_name()
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_default();
        let manifest =
            parse_manifest(&source, &format!("@{name}/{file_name}")).map_err(|source| {
                ResourceError::Manifest {
                    path: manifest_path.clone(),
                    source,
                }
            })?;
        Ok(Self {
            name: ResourceName::from(name),
            root: root.to_path_buf(),
            manifest_path,
            manifest,
        })
    }

    /// Server-side script files in load order: `shared_scripts` first, then
    /// `server_scripts`.
    ///
    /// Each pattern expands in declaration order. The matches of one glob are
    /// sorted by relative path, and a file that several patterns match appears
    /// once, at its first position. Patterns starting with `@other/` are
    /// resolved through `resolve_resource`, which returns the folder of an
    /// installed resource.
    pub fn server_script_files(
        &self,
        resolve_resource: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<Vec<PathBuf>, ResourceError> {
        let mut expansion = Expansion {
            owner: &self.name,
            resolve: &resolve_resource,
            listings: HashMap::new(),
            seen: HashSet::new(),
            matches: Vec::new(),
        };
        for pattern in self
            .manifest
            .shared_scripts
            .iter()
            .chain(&self.manifest.server_scripts)
        {
            expansion.add(&self.root, pattern)?;
        }
        Ok(expansion.matches)
    }
}

/// Path of the manifest in `root`, if it has one.
pub(crate) fn find_manifest(root: &Path) -> Option<PathBuf> {
    MANIFEST_FILES
        .iter()
        .map(|file| root.join(file))
        .find(|path| path.is_file())
}

/// State of one `server_script_files` call.
struct Expansion<'a> {
    /// The resource whose manifest declares the patterns.
    owner: &'a ResourceName,
    /// Finds the folder of an installed resource by name.
    resolve: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Sorted relative file listing per walked folder, filled on first use.
    listings: HashMap<PathBuf, Vec<String>>,
    /// Every path already in `matches`.
    seen: HashSet<PathBuf>,
    /// Result, in load order.
    matches: Vec<PathBuf>,
}

impl Expansion<'_> {
    /// Adds the files matched by one manifest pattern, relative to `root`.
    fn add(&mut self, root: &Path, pattern: &str) -> Result<(), ResourceError> {
        let normalized = pattern.replace('\\', "/");
        let Some(rest) = normalized.strip_prefix('@') else {
            return self.add_in(root, pattern, &normalized);
        };
        let (other, inner) = rest
            .split_once('/')
            .ok_or_else(|| ResourceError::BadPattern {
                pattern: pattern.to_owned(),
                message: "a `@` reference needs a path after the resource name".to_owned(),
            })?;
        let other_root = (self.resolve)(other).ok_or_else(|| ResourceError::UnknownResource {
            resource: self.owner.clone(),
            referenced: other.to_owned(),
        })?;
        self.add_in(&other_root, pattern, inner)
    }

    /// Expands `relative` (a `/`-separated pattern) inside `root`. `original` is
    /// the pattern as written, for error messages.
    fn add_in(&mut self, root: &Path, original: &str, relative: &str) -> Result<(), ResourceError> {
        let pattern = self.clean(original, relative)?;

        if !pattern.contains(GLOB_META) {
            let path = root.join(&pattern);
            if !path.is_file() {
                return Err(ResourceError::MissingFile {
                    resource: self.owner.clone(),
                    pattern: original.to_owned(),
                });
            }
            self.push(path);
            return Ok(());
        }

        let matcher = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| ResourceError::BadPattern {
                pattern: original.to_owned(),
                message: error.to_string(),
            })?
            .compile_matcher();
        let files = self.listing(root)?;
        let hits: Vec<PathBuf> = files
            .iter()
            .filter(|file| matcher.is_match(file))
            .map(|file| root.join(file))
            .collect();
        if hits.is_empty() {
            tracing::warn!(resource = %self.owner, pattern = %original, "script pattern matched no files");
        }
        for path in hits {
            self.push(path);
        }
        Ok(())
    }

    /// Normalises a pattern to `/`-separated components. Absolute paths and
    /// `..` are rejected so scripts stay inside the resource folder.
    fn clean(&self, original: &str, relative: &str) -> Result<String, ResourceError> {
        let mut cleaned = String::new();
        for component in Path::new(relative).components() {
            match component {
                Component::Normal(part) => {
                    if !cleaned.is_empty() {
                        cleaned.push('/');
                    }
                    cleaned.push_str(&part.to_string_lossy());
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(ResourceError::PathEscape {
                        resource: self.owner.clone(),
                        pattern: original.to_owned(),
                    });
                }
            }
        }
        Ok(cleaned)
    }

    /// Sorted file listing of `root`, walked at most once per call.
    fn listing(&mut self, root: &Path) -> Result<&[String], ResourceError> {
        let files = match self.listings.entry(root.to_path_buf()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(walk(root)?),
        };
        Ok(files.as_slice())
    }

    fn push(&mut self, path: PathBuf) {
        if self.seen.insert(path.clone()) {
            self.matches.push(path);
        }
    }
}

/// Sorted `/`-separated relative paths of every file under `root`.
fn walk(root: &Path) -> Result<Vec<String>, ResourceError> {
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            let path = error.path().unwrap_or(root).to_path_buf();
            ResourceError::Io {
                path,
                source: error
                    .into_io_error()
                    .unwrap_or_else(|| io::Error::other("directory walk failed")),
            }
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let parts: Option<Vec<&str>> = relative
            .components()
            .map(|component| component.as_os_str().to_str())
            .collect();
        if let Some(parts) = parts {
            files.push(parts.join("/"));
        }
    }
    files.sort_unstable();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::ResourceInfo;
    use crate::error::ResourceError;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        fs::write(path, contents).expect("write file");
    }

    fn resource(manifest: &str, files: &[&str]) -> (TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("res")
            .tempdir()
            .expect("tempdir");
        let root = dir.path().join("my_res");
        write(&root.join("fxmanifest.lua"), manifest);
        for file in files {
            write(&root.join(file), "");
        }
        (dir, root)
    }

    fn names(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| {
                path.strip_prefix(root)
                    .expect("inside root")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    fn no_resolver(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn loads_fxmanifest() {
        let (_dir, root) = resource("author 'me'", &[]);
        let info = ResourceInfo::load(&root).expect("load");
        assert_eq!(&*info.name, "my_res");
        assert_eq!(info.manifest_path, root.join("fxmanifest.lua"));
        assert_eq!(info.manifest.metadata[0].1, "me");
    }

    #[test]
    fn falls_back_to_legacy_resource_lua() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("old");
        write(&root.join("__resource.lua"), "server_script 'main.lua'");
        let info = ResourceInfo::load(&root).expect("load");
        assert_eq!(info.manifest_path, root.join("__resource.lua"));
        assert_eq!(info.manifest.server_scripts, ["main.lua"]);
    }

    #[test]
    fn fxmanifest_wins_over_legacy_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("both");
        write(&root.join("__resource.lua"), "author 'old'");
        write(&root.join("fxmanifest.lua"), "author 'new'");
        let info = ResourceInfo::load(&root).expect("load");
        assert_eq!(info.manifest.metadata[0].1, "new");
    }

    #[test]
    fn folder_without_manifest_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = ResourceInfo::load(dir.path()).unwrap_err();
        assert!(matches!(error, ResourceError::NoManifest { .. }));
    }

    #[test]
    fn literal_missing_file_is_an_error() {
        let (_dir, root) = resource("server_script 'nope.lua'", &[]);
        let info = ResourceInfo::load(&root).expect("load");
        let error = info.server_script_files(no_resolver).unwrap_err();
        assert!(matches!(
            error,
            ResourceError::MissingFile { ref pattern, .. } if pattern == "nope.lua"
        ));
    }

    #[test]
    fn shared_scripts_come_before_server_scripts() {
        let (_dir, root) = resource(
            "server_scripts { 'server.lua' }\nshared_scripts { 'shared.lua' }",
            &["server.lua", "shared.lua"],
        );
        let info = ResourceInfo::load(&root).expect("load");
        let files = info.server_script_files(no_resolver).expect("expand");
        assert_eq!(names(&root, &files), ["shared.lua", "server.lua"]);
    }

    #[test]
    fn glob_matches_are_sorted_and_deduplicated() {
        let (_dir, root) = resource(
            "server_scripts { 'srv/*.lua', 'srv/a.lua' }",
            &["srv/b.lua", "srv/a.lua", "srv/c.lua"],
        );
        let info = ResourceInfo::load(&root).expect("load");
        let files = info.server_script_files(no_resolver).expect("expand");
        assert_eq!(
            names(&root, &files),
            ["srv/a.lua", "srv/b.lua", "srv/c.lua"]
        );
    }

    #[test]
    fn single_star_stays_in_one_folder_and_double_star_crosses() {
        let (_dir, root) = resource(
            "server_scripts { 'srv/*.lua' }\nserver_scripts { 'deep/**/*.lua' }",
            &["srv/a.lua", "srv/sub/b.lua", "deep/x/y/c.lua", "deep/d.lua"],
        );
        let info = ResourceInfo::load(&root).expect("load");
        let files = info.server_script_files(no_resolver).expect("expand");
        assert_eq!(
            names(&root, &files),
            ["srv/a.lua", "deep/d.lua", "deep/x/y/c.lua"]
        );
    }

    #[test]
    fn glob_with_no_matches_contributes_nothing() {
        let (_dir, root) = resource("server_scripts { 'none/*.lua' }", &["a.lua"]);
        let info = ResourceInfo::load(&root).expect("load");
        assert_eq!(
            info.server_script_files(no_resolver).expect("expand"),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn parent_and_absolute_paths_are_rejected() {
        for pattern in [
            "../secret.lua",
            "a/../../b.lua",
            "/etc/passwd",
            "*/../x.lua",
        ] {
            let (_dir, root) = resource(&format!("server_scripts {{ '{pattern}' }}"), &[]);
            let info = ResourceInfo::load(&root).expect("load");
            let error = info.server_script_files(no_resolver).unwrap_err();
            assert!(
                matches!(error, ResourceError::PathEscape { .. }),
                "{pattern}: {error}"
            );
        }
    }

    #[test]
    fn backslashes_are_accepted_as_separators() {
        let (_dir, root) = resource("server_scripts { 'srv\\\\a.lua' }", &["srv/a.lua"]);
        let info = ResourceInfo::load(&root).expect("load");
        let files = info.server_script_files(no_resolver).expect("expand");
        assert_eq!(names(&root, &files), ["srv/a.lua"]);
    }

    #[test]
    fn at_reference_resolves_through_resolver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let other = dir.path().join("oxmysql");
        write(&other.join("lib/MySQL.lua"), "");
        write(&other.join("lib/extra.lua"), "");
        let root = dir.path().join("my_res");
        write(
            &root.join("fxmanifest.lua"),
            "server_scripts { '@oxmysql/lib/MySQL.lua', '@oxmysql/lib/*.lua' }",
        );
        let info = ResourceInfo::load(&root).expect("load");
        let resolve = |name: &str| (name == "oxmysql").then(|| other.clone());
        let files = info.server_script_files(resolve).expect("expand");
        assert_eq!(
            files,
            [other.join("lib/MySQL.lua"), other.join("lib/extra.lua")]
        );
    }

    #[test]
    fn at_reference_to_unknown_resource_is_an_error() {
        let (_dir, root) = resource("server_scripts { '@ghost/main.lua' }", &[]);
        let info = ResourceInfo::load(&root).expect("load");
        let error = info.server_script_files(no_resolver).unwrap_err();
        assert!(matches!(
            error,
            ResourceError::UnknownResource { ref referenced, .. } if referenced == "ghost"
        ));
    }

    #[test]
    fn at_reference_escape_is_rejected() {
        let (_dir, root) = resource("server_scripts { '@other/../x.lua' }", &[]);
        let info = ResourceInfo::load(&root).expect("load");
        let resolve = |_: &str| Some(PathBuf::from("/nonexistent"));
        let error = info.server_script_files(resolve).unwrap_err();
        assert!(matches!(error, ResourceError::PathEscape { .. }));
    }

    #[test]
    fn invalid_glob_is_a_bad_pattern() {
        let (_dir, root) = resource("server_scripts { 'a/[.lua' }", &["a/x.lua"]);
        let info = ResourceInfo::load(&root).expect("load");
        let error = info.server_script_files(no_resolver).unwrap_err();
        assert!(matches!(error, ResourceError::BadPattern { .. }));
    }
}
