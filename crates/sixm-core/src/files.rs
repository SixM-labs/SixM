//! Paths of the files a script reads and writes inside a resource.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::Path;

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};

/// Script file types that the server and client can execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptLanguage {
    /// Lua source.
    Lua,
    /// JavaScript source.
    JavaScript,
}

impl ScriptLanguage {
    /// Recognizes Lua and JavaScript source extensions without regard to case.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("lua") {
            Some(Self::Lua)
        } else if extension.eq_ignore_ascii_case("js") {
            Some(Self::JavaScript)
        } else {
            None
        }
    }
}

/// Normalizes a resource-relative file name as scripts pass it to
/// `LoadResourceFile` and `SaveResourceFile`: `/` and `\` both separate
/// components, empty and `.` components are dropped, and the result is joined
/// with `/`. Returns `None` for absolute paths, parent components, names that
/// are unsafe on either supported system, and empty normalized paths.
pub fn normalize_resource_path(path: &str) -> Option<String> {
    if path.starts_with(['/', '\\']) {
        return None;
    }
    let mut normalized = String::with_capacity(path.len());
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return None,
            _ if !is_resource_component(component) => return None,
            _ => {
                if !normalized.is_empty() {
                    normalized.push('/');
                }
                normalized.push_str(component);
            }
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}

/// Whether a name is one normal path component on both supported systems.
pub fn is_resource_component(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(|ch| ch <= '\u{1f}')
    {
        return false;
    }
    let base = name.split('.').next().unwrap_or(name).trim_end();
    let base = base.to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "AUX" | "CON" | "NUL" | "PRN" | "CONIN$" | "CONOUT$"
    ) && !base
        .strip_prefix("COM")
        .or_else(|| base.strip_prefix("LPT"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

/// File access relative to an opened resource directory. Each path component
/// is opened without following links, including the final file.
#[derive(Debug)]
pub struct ResourceDirectory(Dir);

impl ResourceDirectory {
    /// Opens a resource under its operator-configured parent directory.
    pub fn open(root: &Path) -> io::Result<Self> {
        let name = root.file_name().ok_or_else(invalid_resource_path)?;
        let parent = root
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
        Ok(Self(parent.open_dir_nofollow(name)?))
    }

    /// Opens an existing regular file for reading.
    pub fn open_file(&self, path: &str) -> io::Result<File> {
        let path = normalize_resource_path(path).ok_or_else(invalid_resource_path)?;
        let (directory, name) = self.file_parent(&path, false)?;
        if !directory.symlink_metadata(name)?.is_file() {
            return Err(invalid_resource_path());
        }
        let file = directory.open_with(
            name,
            OpenOptions::new().read(true).follow(FollowSymlinks::No),
        )?;
        Ok(file.into_std())
    }

    /// Reads an existing regular file.
    pub fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        self.open_file(path)?.read_to_end(&mut data)?;
        Ok(data)
    }

    /// Writes a regular file, creating missing directories inside the resource.
    pub fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let path = normalize_resource_path(path).ok_or_else(invalid_resource_path)?;
        let (directory, name) = self.file_parent(&path, true)?;
        match directory.symlink_metadata(name) {
            Ok(metadata) if !metadata.is_file() => return Err(invalid_resource_path()),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let mut file = directory.open_with(
            name,
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .follow(FollowSymlinks::No),
        )?;
        file.write_all(data)
    }

    fn file_parent<'a>(&self, path: &'a str, create: bool) -> io::Result<(Dir, &'a str)> {
        let mut parts = path.split('/').peekable();
        let mut directory = self.0.try_clone()?;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok((directory, part));
            }
            directory = match directory.open_dir_nofollow(part) {
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    if let Err(error) = directory.create_dir(part)
                        && error.kind() != io::ErrorKind::AlreadyExists
                    {
                        return Err(error);
                    }
                    directory.open_dir_nofollow(part)?
                }
                opened => opened?,
            };
        }
        Err(invalid_resource_path())
    }
}

fn invalid_resource_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected a regular resource file",
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{ResourceDirectory, normalize_resource_path};

    #[test]
    fn keeps_plain_relative_names() {
        assert_eq!(
            normalize_resource_path("data/a.json").as_deref(),
            Some("data/a.json")
        );
        assert_eq!(
            normalize_resource_path("@other/lib.lua").as_deref(),
            Some("@other/lib.lua")
        );
    }

    #[test]
    fn drops_empty_and_dot_components() {
        assert_eq!(
            normalize_resource_path("./data//a.json").as_deref(),
            Some("data/a.json")
        );
    }

    #[test]
    fn accepts_backslashes_as_separators() {
        assert_eq!(
            normalize_resource_path("data\\a.json").as_deref(),
            Some("data/a.json")
        );
    }

    #[test]
    fn rejects_names_that_leave_the_resource_or_name_nothing() {
        for path in [
            "/etc/passwd",
            "\\x",
            "../x",
            "a/../b",
            "C:/x",
            "a/b:c",
            "data/.. /secret",
            "data/file.txt.",
            "data/CoN.json",
            "data/LPT².txt",
            "",
            ".",
            "./",
        ] {
            assert_eq!(normalize_resource_path(path), None, "{path:?}");
        }
    }

    #[test]
    fn regular_resource_files_can_be_created_and_replaced() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let root = fixture.path().join("resource");
        fs::create_dir(&root).expect("resource directory");
        let directory = ResourceDirectory::open(&root).expect("open resource");
        directory
            .write("data/nested/test.txt", b"longer text")
            .expect("write file");
        directory
            .write("data/nested/test.txt", b"short")
            .expect("replace file");
        assert_eq!(
            directory.read("data/nested/test.txt").expect("read file"),
            b"short"
        );
        assert!(directory.write("../outside.txt", b"ignored").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_during_reads_and_writes() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().expect("temporary directory");
        let root = fixture.path().join("resource");
        fs::create_dir(&root).expect("resource directory");
        let outside = fixture.path().join("outside.txt");
        fs::write(&outside, b"original").expect("outside fixture");
        symlink(&outside, root.join("linked.txt")).expect("file link");
        symlink(fixture.path(), root.join("linked-dir")).expect("directory link");
        let directory = ResourceDirectory::open(&root).expect("open resource");
        for name in ["linked.txt", "linked-dir/outside.txt"] {
            assert!(directory.read(name).is_err());
            assert!(directory.write(name, b"ignored").is_err());
        }
        assert_eq!(fs::read(outside).expect("outside data"), b"original");
    }
}
