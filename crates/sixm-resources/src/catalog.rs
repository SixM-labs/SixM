//! The set of installed resources and the order to start and stop them in.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use sixm_core::ResourceName;

use crate::error::{DependencyError, ResourceError};
use crate::resource::{ResourceInfo, find_manifest};

/// A problem found while scanning resource folders. The scan continues past it.
#[derive(Debug)]
pub enum ScanIssue {
    /// A later folder has the same resource name as one already loaded. The
    /// later folder is ignored.
    Duplicate {
        /// The shared resource name.
        name: ResourceName,
        /// The folder that was loaded.
        kept: PathBuf,
        /// The folder that was ignored.
        ignored: PathBuf,
    },
    /// A resource folder, scan root, or `provide` alias is invalid. The
    /// offending resource or alias is skipped.
    Invalid {
        /// The folder that failed.
        path: PathBuf,
        /// Why it failed.
        error: ResourceError,
    },
}

/// Every installed resource, keyed by name, plus `provide` aliases.
#[derive(Debug, Default)]
pub struct Catalog {
    resources: BTreeMap<ResourceName, ResourceInfo>,
    aliases: BTreeMap<ResourceName, ResourceName>,
}

impl Catalog {
    /// Finds every resource under `roots`.
    ///
    /// Each root's direct subfolders are checked. A folder with a manifest is a
    /// resource. A folder named `[category]` is searched in turn, and categories
    /// nest. Other folders and dot-folders are ignored, and resources are not
    /// searched for resources inside them. Folders are visited in sorted order,
    /// so the first copy of a name wins.
    pub fn scan(roots: &[PathBuf]) -> (Self, Vec<ScanIssue>) {
        let mut catalog = Self::default();
        let mut issues = Vec::new();
        let mut scan_order = Vec::new();
        for root in roots {
            scan_dir(root, &mut catalog, &mut scan_order, &mut issues);
        }
        register_aliases(&mut catalog, &scan_order, &mut issues);
        (catalog, issues)
    }

    /// Resolves `name` to its canonical installed resource name if it is a
    /// `provide` alias, or returns `name` unchanged.
    pub fn resolve_name<'a>(&'a self, name: &'a str) -> &'a str {
        if self.resources.contains_key(name) {
            return name;
        }
        self.aliases.get(name).map_or(name, AsRef::as_ref)
    }

    /// All registered `(alias, provider)` pairs, sorted by alias name.
    pub fn alias_names(&self) -> Vec<(ResourceName, ResourceName)> {
        self.aliases
            .iter()
            .map(|(alias, provider)| (alias.clone(), provider.clone()))
            .collect()
    }

    /// The resource called `name`, or the resource providing `name` as an
    /// alias.
    pub fn get(&self, name: &str) -> Option<&ResourceInfo> {
        self.resources.get(self.resolve_name(name))
    }

    /// Number of resources.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Whether the catalog has no resources.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// All resources, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &ResourceInfo> {
        self.resources.values()
    }

    /// `name`'s transitive dependencies followed by `name`, dependencies first.
    ///
    /// Dependencies starting with `/` are server or game constraints and are
    /// skipped. Each resource appears once.
    pub fn start_order(&self, name: &str) -> Result<Vec<ResourceName>, DependencyError> {
        let canonical = self.resolve_name(name);
        let (key, info) = self
            .resources
            .get_key_value(canonical)
            .ok_or_else(|| DependencyError::UnknownResource(name.to_owned()))?;
        let mut order = Vec::new();
        self.visit(key, info, &mut Vec::new(), &mut HashSet::new(), &mut order)?;
        Ok(order)
    }

    /// Resources that transitively depend on `name`, not including `name`.
    ///
    /// Dependents come before anything they depend on, so stopping them in this
    /// order never stops a resource while something running still needs it.
    /// Dependency cycles do not cause an error here. Each member is listed once.
    pub fn dependents_stop_order(&self, name: &str) -> Vec<ResourceName> {
        let target = self.resolve_name(name);
        let mut dependents_of: HashMap<&str, Vec<&ResourceName>> = HashMap::new();
        for info in self.resources.values() {
            for dependency in dependencies(info) {
                dependents_of
                    .entry(self.resolve_name(dependency))
                    .or_default()
                    .push(&info.name);
            }
        }

        let mut affected: BTreeSet<ResourceName> = BTreeSet::new();
        let mut queue = vec![target];
        while let Some(current) = queue.pop() {
            for dependent in dependents_of.get(current).into_iter().flatten() {
                if dependent.as_ref() != target && affected.insert((*dependent).clone()) {
                    queue.push(dependent.as_ref());
                }
            }
        }

        let mut post_order = Vec::new();
        let mut visited = HashSet::new();
        for node in &affected {
            self.post_order(node, &affected, &mut visited, &mut post_order);
        }
        post_order.reverse();
        post_order
    }

    /// Depth-first walk that appends each resource after its dependencies.
    fn visit(
        &self,
        name: &ResourceName,
        info: &ResourceInfo,
        stack: &mut Vec<ResourceName>,
        done: &mut HashSet<ResourceName>,
        order: &mut Vec<ResourceName>,
    ) -> Result<(), DependencyError> {
        if done.contains(name) {
            return Ok(());
        }
        if let Some(start) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.clone());
            return Err(DependencyError::Cycle(cycle));
        }
        stack.push(name.clone());
        for dependency in dependencies(info) {
            let resolved = self.resolve_name(dependency);
            let Some((key, dependency_info)) = self.resources.get_key_value(resolved) else {
                return Err(DependencyError::MissingDependency {
                    resource: name.clone(),
                    dependency: dependency.to_owned(),
                });
            };
            self.visit(key, dependency_info, stack, done, order)?;
        }
        stack.pop();
        done.insert(name.clone());
        order.push(name.clone());
        Ok(())
    }

    /// Depth-first walk over dependency edges that stay inside `allowed`.
    fn post_order(
        &self,
        node: &ResourceName,
        allowed: &BTreeSet<ResourceName>,
        visited: &mut HashSet<ResourceName>,
        out: &mut Vec<ResourceName>,
    ) {
        if !visited.insert(node.clone()) {
            return;
        }
        if let Some(info) = self.resources.get(node) {
            for dependency in dependencies(info) {
                if let Some(key) = allowed.get(self.resolve_name(dependency)) {
                    self.post_order(key, allowed, visited, out);
                }
            }
        }
        out.push(node.clone());
    }
}

/// Dependencies of `info` that name resources. `/` entries are server or game
/// constraints and are left out.
fn dependencies(info: &ResourceInfo) -> impl Iterator<Item = &str> {
    info.manifest
        .dependencies
        .iter()
        .map(String::as_str)
        .filter(|dependency| !dependency.starts_with('/'))
}

/// Scans the subfolders of `dir`, recursing into categories.
fn scan_dir(
    dir: &Path,
    catalog: &mut Catalog,
    scan_order: &mut Vec<ResourceName>,
    issues: &mut Vec<ScanIssue>,
) {
    let folders = match subfolders(dir) {
        Ok(folders) => folders,
        Err(error) => {
            issues.push(ScanIssue::Invalid {
                path: dir.to_path_buf(),
                error,
            });
            return;
        }
    };
    for folder in folders {
        if find_manifest(&folder).is_some() {
            add_resource(&folder, catalog, scan_order, issues);
        } else if is_category(&folder) {
            scan_dir(&folder, catalog, scan_order, issues);
        }
    }
}

/// Loads the resource in `folder` into the catalog, or records why it could not be.
fn add_resource(
    folder: &Path,
    catalog: &mut Catalog,
    scan_order: &mut Vec<ResourceName>,
    issues: &mut Vec<ScanIssue>,
) {
    let info = match ResourceInfo::load(folder) {
        Ok(info) => info,
        Err(error) => {
            issues.push(ScanIssue::Invalid {
                path: folder.to_path_buf(),
                error,
            });
            return;
        }
    };
    match catalog.resources.entry(info.name.clone()) {
        Entry::Occupied(kept) => issues.push(ScanIssue::Duplicate {
            name: info.name,
            kept: kept.get().root.clone(),
            ignored: info.root,
        }),
        Entry::Vacant(slot) => {
            scan_order.push(info.name.clone());
            slot.insert(info);
        }
    }
}

/// Registers `provide` aliases for all loaded resources in scan order.
/// Conflicting aliases are skipped and reported as [`ScanIssue::Invalid`].
fn register_aliases(
    catalog: &mut Catalog,
    scan_order: &[ResourceName],
    issues: &mut Vec<ScanIssue>,
) {
    for provider_name in scan_order {
        let Some(info) = catalog.resources.get(provider_name) else {
            continue;
        };
        let provider = info.name.clone();
        let root = info.root.clone();
        let declared_aliases = info.provides().to_vec();
        for alias in declared_aliases {
            if let Some(existing) = catalog.resources.get(alias.as_str()) {
                issues.push(ScanIssue::Invalid {
                    path: root.clone(),
                    error: ResourceError::AliasConflict {
                        resource: provider.clone(),
                        alias: ResourceName::from(alias),
                        existing: existing.name.clone(),
                    },
                });
                continue;
            }
            match catalog.aliases.entry(ResourceName::from(alias)) {
                Entry::Occupied(existing) if existing.get() != &provider => {
                    let alias = existing.key().clone();
                    let existing = existing.get().clone();
                    issues.push(ScanIssue::Invalid {
                        path: root.clone(),
                        error: ResourceError::AliasConflict {
                            resource: provider.clone(),
                            alias,
                            existing,
                        },
                    });
                }
                Entry::Occupied(_) => {}
                Entry::Vacant(slot) => {
                    slot.insert(provider.clone());
                }
            }
        }
    }
}

/// Non-hidden subfolders of `dir`, sorted.
fn subfolders(dir: &Path) -> Result<Vec<PathBuf>, ResourceError> {
    let io_error = |source| ResourceError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut folders = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let path = entry.map_err(io_error)?.path();
        if path.is_dir() && !is_hidden(&path) {
            folders.push(path);
        }
    }
    folders.sort();
    Ok(folders)
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

fn is_category(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with('[') && name.ends_with(']')
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{Catalog, ScanIssue};
    use crate::error::{DependencyError, ResourceError};

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        fs::write(path, contents).expect("write file");
    }

    /// Creates a resource `name` under `parent` that depends on `deps`.
    fn resource(parent: &Path, name: &str, deps: &[&str]) {
        resource_with_provides(parent, name, deps, &[]);
    }

    /// Creates a resource `name` under `parent` with `deps` and `provides`.
    fn resource_with_provides(parent: &Path, name: &str, deps: &[&str], provides: &[&str]) {
        let dependencies = deps
            .iter()
            .map(|dep| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let provides_list = provides
            .iter()
            .map(|alias| format!("'{alias}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let manifest = format!("dependencies {{ {dependencies} }}\nprovides {{ {provides_list} }}");
        write(&parent.join(name).join("fxmanifest.lua"), &manifest);
    }

    fn names(order: &[sixm_core::ResourceName]) -> Vec<&str> {
        order.iter().map(AsRef::<str>::as_ref).collect()
    }

    fn catalog_of(deps: &[(&str, &[&str])]) -> (TempDir, Catalog) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, needs) in deps {
            resource(dir.path(), name, needs);
        }
        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");
        (dir, catalog)
    }

    #[test]
    fn scan_finds_nested_categories_and_skips_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        resource(root, "top", &[]);
        resource(&root.join("[core]"), "inner", &[]);
        resource(&root.join("[core]").join("[deep]"), "deepest", &[]);
        write(&root.join("plain").join("not_a_resource.txt"), "");
        resource(root, ".hidden", &[]);
        resource(root, "outer", &[]);
        write(
            &root.join("outer").join("nested").join("fxmanifest.lua"),
            "",
        );

        let (catalog, issues) = Catalog::scan(&[root.to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");
        let found: Vec<&str> = catalog.iter().map(|info| info.name.as_ref()).collect();
        assert_eq!(found, ["deepest", "inner", "outer", "top"]);
    }

    #[test]
    fn first_copy_wins_and_duplicates_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        resource(&root.join("[a]"), "shared", &[]);
        resource(&root.join("[b]"), "shared", &[]);

        let (catalog, issues) = Catalog::scan(&[root.to_path_buf()]);
        assert_eq!(catalog.len(), 1);
        assert_eq!(
            catalog.get("shared").expect("present").root,
            root.join("[a]").join("shared")
        );
        assert!(matches!(
            &issues[..],
            [ScanIssue::Duplicate { name, kept, ignored }]
                if name.as_ref() == "shared"
                    && *kept == root.join("[a]").join("shared")
                    && *ignored == root.join("[b]").join("shared")
        ));
    }

    #[test]
    fn invalid_manifest_is_reported_and_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("broken").join("fxmanifest.lua"), "author(");
        resource(dir.path(), "fine", &[]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert_eq!(catalog.len(), 1);
        assert!(catalog.get("fine").is_some());
        assert!(matches!(&issues[..], [ScanIssue::Invalid { .. }]));
    }

    #[test]
    fn missing_root_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (catalog, issues) = Catalog::scan(&[dir.path().join("absent")]);
        assert!(catalog.is_empty());
        assert!(matches!(&issues[..], [ScanIssue::Invalid { .. }]));
    }

    #[test]
    fn start_order_puts_dependencies_first_in_a_diamond() {
        let (_dir, catalog) = catalog_of(&[
            ("app", &["left", "right"]),
            ("left", &["base"]),
            ("right", &["base"]),
            ("base", &[]),
        ]);
        let order = catalog.start_order("app").expect("order");
        assert_eq!(names(&order), ["base", "left", "right", "app"]);
    }

    #[test]
    fn start_order_of_unknown_resource_fails() {
        let (_dir, catalog) = catalog_of(&[]);
        assert!(matches!(
            catalog.start_order("nope"),
            Err(DependencyError::UnknownResource(name)) if name == "nope"
        ));
    }

    #[test]
    fn missing_dependency_is_reported() {
        let (_dir, catalog) = catalog_of(&[("app", &["ghost"])]);
        assert!(matches!(
            catalog.start_order("app"),
            Err(DependencyError::MissingDependency { ref resource, ref dependency })
                if resource.as_ref() == "app" && dependency == "ghost"
        ));
    }

    #[test]
    fn cycle_reports_the_path() {
        let (_dir, catalog) = catalog_of(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]);
        let error = catalog.start_order("a").unwrap_err();
        match error {
            DependencyError::Cycle(path) => assert_eq!(names(&path), ["a", "b", "c", "a"]),
            other => panic!("expected a cycle, got {other}"),
        }
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let (_dir, catalog) = catalog_of(&[("a", &["a"])]);
        assert!(matches!(
            catalog.start_order("a"),
            Err(DependencyError::Cycle(path)) if names(&path) == ["a", "a"]
        ));
    }

    #[test]
    fn server_and_game_constraints_are_ignored() {
        let (_dir, catalog) = catalog_of(&[
            ("app", &["/server:5848", "/onesync", "base"]),
            ("base", &[]),
        ]);
        let order = catalog.start_order("app").expect("order");
        assert_eq!(names(&order), ["base", "app"]);
    }

    #[test]
    fn dependents_stop_order_stops_dependents_first() {
        let (_dir, catalog) = catalog_of(&[
            ("base", &[]),
            ("lib", &["base"]),
            ("app", &["lib"]),
            ("top", &["app", "base"]),
            ("unrelated", &[]),
        ]);
        let order = catalog.dependents_stop_order("base");
        assert_eq!(names(&order), ["top", "app", "lib"]);
    }

    #[test]
    fn dependents_stop_order_of_leaf_is_empty() {
        let (_dir, catalog) = catalog_of(&[("base", &[]), ("app", &["base"])]);
        assert_eq!(catalog.dependents_stop_order("app"), Vec::new());
        assert_eq!(catalog.dependents_stop_order("absent"), Vec::new());
    }

    #[test]
    fn dependents_stop_order_terminates_on_cycles() {
        let (_dir, catalog) = catalog_of(&[("a", &["b"]), ("b", &["a"])]);
        assert_eq!(names(&catalog.dependents_stop_order("a")), ["b"]);
    }

    #[test]
    fn scan_accepts_multiple_roots() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        resource(first.path(), "one", &[]);
        resource(second.path(), "two", &[]);
        let roots: Vec<PathBuf> = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        let (catalog, issues) = Catalog::scan(&roots);
        assert!(issues.is_empty());
        assert_eq!(catalog.len(), 2);
    }

    #[test]
    fn aliases_resolve_in_get_resolve_name_and_alias_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            &dir.path().join("oxmysql").join("fxmanifest.lua"),
            "provide 'mysql-async'\nprovides { 'ghmattimysql' }",
        );

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");

        assert_eq!(catalog.resolve_name("oxmysql"), "oxmysql");
        assert_eq!(catalog.resolve_name("mysql-async"), "oxmysql");
        assert_eq!(catalog.resolve_name("ghmattimysql"), "oxmysql");
        assert_eq!(catalog.resolve_name("unknown"), "unknown");

        assert_eq!(
            catalog.get("mysql-async").expect("by alias").name.as_ref(),
            "oxmysql"
        );
        assert_eq!(
            catalog.get("oxmysql").expect("by canonical").name.as_ref(),
            "oxmysql"
        );
        assert!(catalog.get("unknown").is_none());

        let raw_aliases = catalog.alias_names();
        let aliases: Vec<(&str, &str)> = raw_aliases
            .iter()
            .map(|(alias, provider)| (alias.as_ref(), provider.as_ref()))
            .collect();
        assert_eq!(
            aliases,
            [("ghmattimysql", "oxmysql"), ("mysql-async", "oxmysql")]
        );
    }

    #[test]
    fn start_order_resolves_direct_and_transitive_alias_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "core_db", &[], &["db-core"]);
        resource_with_provides(dir.path(), "oxmysql", &["db-core"], &["mysql-async"]);
        resource_with_provides(dir.path(), "left", &["oxmysql"], &[]);
        resource_with_provides(dir.path(), "right", &["mysql-async"], &[]);
        resource_with_provides(dir.path(), "app", &["left", "right"], &[]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");

        let app_order = catalog.start_order("app").expect("app start order");
        assert_eq!(
            names(&app_order),
            ["core_db", "oxmysql", "left", "right", "app"]
        );

        let alias_order = catalog
            .start_order("mysql-async")
            .expect("alias start order");
        assert_eq!(names(&alias_order), ["core_db", "oxmysql"]);
    }

    #[test]
    fn dependents_stop_order_resolves_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "oxmysql", &[], &["mysql-async"]);
        resource_with_provides(dir.path(), "lib", &["mysql-async"], &["lib-alias"]);
        resource_with_provides(dir.path(), "app", &["lib-alias"], &[]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");

        assert_eq!(
            names(&catalog.dependents_stop_order("oxmysql")),
            ["app", "lib"]
        );
        assert_eq!(
            names(&catalog.dependents_stop_order("mysql-async")),
            ["app", "lib"]
        );
    }

    #[test]
    fn conflicting_alias_with_installed_resource_is_reported_and_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "alpha", &[], &["beta"]);
        resource_with_provides(dir.path(), "beta", &[], &["alpha"]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.alias_names(), vec![]);
        assert_eq!(catalog.get("alpha").expect("alpha").name.as_ref(), "alpha");
        assert_eq!(catalog.get("beta").expect("beta").name.as_ref(), "beta");
        assert_eq!(issues.len(), 2);
        assert!(issues.iter().all(|issue| matches!(
            issue,
            ScanIssue::Invalid {
                error: ResourceError::AliasConflict { .. },
                ..
            }
        )));
    }

    #[test]
    fn conflicting_alias_between_providers_keeps_first_and_skips_second() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "first_sql", &[], &["mysql-async"]);
        resource_with_provides(
            dir.path(),
            "second_sql",
            &[],
            &["mysql-async", "unique-sql"],
        );

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert_eq!(catalog.len(), 2);
        assert_eq!(
            catalog
                .get("mysql-async")
                .expect("first wins")
                .name
                .as_ref(),
            "first_sql"
        );
        assert_eq!(
            catalog
                .get("unique-sql")
                .expect("non-conflicting kept")
                .name
                .as_ref(),
            "second_sql"
        );
        assert!(matches!(
            &issues[..],
            [ScanIssue::Invalid {
                path,
                error: ResourceError::AliasConflict { resource, alias, existing },
            }] if *path == dir.path().join("second_sql")
                && resource.as_ref() == "second_sql"
                && alias.as_ref() == "mysql-async"
                && existing.as_ref() == "first_sql"
        ));
    }

    #[test]
    fn duplicate_provide_within_same_resource_is_deduplicated_without_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "oxmysql", &[], &["mysql-async", "mysql-async"]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(catalog.alias_names().len(), 1);
        assert_eq!(
            catalog.get("mysql-async").expect("alias").name.as_ref(),
            "oxmysql"
        );
    }

    #[test]
    fn cycle_through_aliases_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        resource_with_provides(dir.path(), "a", &["b-alias"], &["a-alias"]);
        resource_with_provides(dir.path(), "b", &["a-alias"], &["b-alias"]);
        resource_with_provides(dir.path(), "self_res", &["self-alias"], &["self-alias"]);

        let (catalog, issues) = Catalog::scan(&[dir.path().to_path_buf()]);
        assert!(issues.is_empty(), "{issues:?}");

        let cycle = catalog.start_order("a").unwrap_err();
        assert!(matches!(
            cycle,
            DependencyError::Cycle(ref path) if names(path) == ["a", "b", "a"]
        ));

        let self_cycle = catalog.start_order("self-alias").unwrap_err();
        assert!(matches!(
            self_cycle,
            DependencyError::Cycle(ref path) if names(path) == ["self_res", "self_res"]
        ));
    }
}
