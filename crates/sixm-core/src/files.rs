//! Paths of the files a script reads and writes inside a resource.

use std::path::Path;

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
/// with `/`. Returns `None` for a name that would leave the resource or names
/// nothing: one that starts with a separator, has a `..` component, has a
/// component containing `:` (a Windows drive or stream), or is empty after
/// normalization.
pub fn normalize_resource_path(path: &str) -> Option<String> {
    if path.starts_with(['/', '\\']) {
        return None;
    }
    let mut normalized = String::with_capacity(path.len());
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return None,
            _ if component.contains(':') => return None,
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

#[cfg(test)]
mod tests {
    use super::normalize_resource_path;

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
            "",
            ".",
            "./",
        ] {
            assert_eq!(normalize_resource_path(path), None, "{path:?}");
        }
    }
}
