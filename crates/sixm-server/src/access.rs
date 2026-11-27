//! Access control entries: which principals may use which objects.
//!
//! A principal is a name such as `identifier.license:<hex>`, `group.admin` or
//! `player.3`. An entry gives one principal a permission on one object, and
//! `add_principal` makes one principal inherit from another. Objects are
//! dotted paths, so an entry on `command` also covers `command.stop`. Names
//! are case-insensitive and stored in lower case.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

/// The principal that every principal inherits from.
pub(crate) const EVERYONE: &str = "builtin.everyone";

/// What an entry does for its principal and object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Permission {
    Allow,
    Deny,
}

impl Permission {
    /// Reads `allow` or `deny`, in any case.
    pub(crate) fn parse(word: &str) -> Option<Self> {
        if word.eq_ignore_ascii_case("allow") {
            Some(Self::Allow)
        } else if word.eq_ignore_ascii_case("deny") {
            Some(Self::Deny)
        } else {
            None
        }
    }

    /// The word that `parse` reads for this permission.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Every entry and every principal link, as the server holds them.
#[derive(Debug, Default)]
pub(crate) struct AccessStore {
    /// The permission of each principal for each object path.
    entries: BTreeMap<String, BTreeMap<String, Permission>>,
    /// The parents of each principal.
    parents: BTreeMap<String, BTreeSet<String>>,
}

/// `name` in lower case. Borrows it when it has no upper-case letters, so
/// checks on names that are already lower case do not allocate.
fn lower(name: &str) -> Cow<'_, str> {
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

impl AccessStore {
    /// Sets the entry of `principal` for `object`. An earlier entry for the same
    /// pair is replaced.
    pub(crate) fn add_ace(&mut self, principal: &str, object: &str, permission: Permission) {
        self.entries
            .entry(lower(principal).into_owned())
            .or_default()
            .insert(lower(object).into_owned(), permission);
    }

    /// Removes the entry of `principal` for `object`, if it has `permission`.
    /// Returns whether an entry was removed.
    pub(crate) fn remove_ace(
        &mut self,
        principal: &str,
        object: &str,
        permission: Permission,
    ) -> bool {
        let principal = lower(principal);
        let object = lower(object);
        let Some(objects) = self.entries.get_mut(&*principal) else {
            return false;
        };
        if objects.get(&*object) != Some(&permission) {
            return false;
        }
        objects.remove(&*object);
        if objects.is_empty() {
            self.entries.remove(&*principal);
        }
        true
    }

    /// Makes `child` inherit from `parent`.
    pub(crate) fn add_principal(&mut self, child: &str, parent: &str) {
        self.parents
            .entry(lower(child).into_owned())
            .or_default()
            .insert(lower(parent).into_owned());
    }

    /// Removes an inheritance link. Returns whether it existed.
    pub(crate) fn remove_principal(&mut self, child: &str, parent: &str) -> bool {
        let child = lower(child);
        let parent = lower(parent);
        let Some(parents) = self.parents.get_mut(&*child) else {
            return false;
        };
        let removed = parents.remove(&*parent);
        if parents.is_empty() {
            self.parents.remove(&*child);
        }
        removed
    }

    /// Every entry as `(principal, object, permission)`, sorted by principal and object.
    pub(crate) fn aces(&self) -> impl Iterator<Item = (&str, &str, Permission)> {
        self.entries.iter().flat_map(|(principal, objects)| {
            objects
                .iter()
                .map(move |(object, permission)| (principal.as_str(), object.as_str(), *permission))
        })
    }

    /// Every inheritance link as `(child, parent)`, sorted by child and parent.
    pub(crate) fn links(&self) -> impl Iterator<Item = (&str, &str)> {
        self.parents.iter().flat_map(|(child, parents)| {
            parents
                .iter()
                .map(move |parent| (child.as_str(), parent.as_str()))
        })
    }

    /// Whether `principal`, which also inherits from `extra_parents`, may use
    /// `object`. The extra parents must be in lower case.
    ///
    /// The ancestors of the principal are collected breadth first: the principal
    /// itself, the extra parents, `builtin.everyone`, and the parents of each of
    /// them. A set of names stops the walk at cycles. The object is then checked
    /// at its full path and at each shorter dotted prefix, from the longest to
    /// the shortest. At the first path that any ancestor has an entry for, a deny
    /// means no and an allow means yes. Without an entry at any path, the answer
    /// is no. The prefixes are slices of the object, so only the ancestor list
    /// allocates.
    pub(crate) fn is_allowed<'a>(
        &self,
        principal: &str,
        object: &str,
        extra_parents: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let principal = lower(principal);
        let object = lower(object);
        let mut ancestors: Vec<&str> = vec![&*principal];
        for parent in extra_parents {
            ancestors.push(parent);
        }
        ancestors.push(EVERYONE);
        let mut next = 0;
        while next < ancestors.len() {
            let node = ancestors[next];
            next += 1;
            if let Some(parents) = self.parents.get(node) {
                for parent in parents {
                    if !ancestors.contains(&parent.as_str()) {
                        ancestors.push(parent.as_str());
                    }
                }
            }
        }

        let mut path: &str = &object;
        loop {
            let mut allowed = false;
            for ancestor in &ancestors {
                match self
                    .entries
                    .get(*ancestor)
                    .and_then(|objects| objects.get(path))
                {
                    Some(Permission::Deny) => return false,
                    Some(Permission::Allow) => allowed = true,
                    None => {}
                }
            }
            if allowed {
                return true;
            }
            match path.rfind('.') {
                Some(dot) => path = &path[..dot],
                None => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(store: &AccessStore, principal: &str, object: &str) -> bool {
        store.is_allowed(principal, object, std::iter::empty())
    }

    #[test]
    fn an_empty_store_allows_nothing() {
        let store = AccessStore::default();
        assert!(!allowed(&store, "player.1", "command.quit"));
    }

    #[test]
    fn a_principal_inherits_entries_of_its_parents_transitively() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Allow);
        store.add_principal("identifier.license:abc", "group.staff");
        store.add_principal("group.staff", "group.admin");
        assert!(allowed(&store, "identifier.license:abc", "command.quit"));
        assert!(!allowed(&store, "group.staff.other", "command.quit"));
        assert!(!allowed(&store, "group.other", "command.quit"));
    }

    #[test]
    fn cycles_in_the_principal_links_terminate() {
        let mut store = AccessStore::default();
        store.add_principal("a", "b");
        store.add_principal("b", "c");
        store.add_principal("c", "a");
        store.add_principal("a", "a");
        assert!(!allowed(&store, "a", "command"));
        store.add_ace("c", "command", Permission::Allow);
        assert!(allowed(&store, "a", "command"));
    }

    #[test]
    fn the_most_specific_path_decides() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Allow);
        store.add_ace("group.admin", "command.quit", Permission::Deny);
        assert!(allowed(&store, "group.admin", "command.start"));
        assert!(allowed(&store, "group.admin", "command.stop.now"));
        assert!(!allowed(&store, "group.admin", "command.quit"));
        assert!(!allowed(&store, "group.admin", "command.quit.now"));
    }

    #[test]
    fn a_deeper_allow_beats_a_shallower_deny() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Deny);
        store.add_ace("group.admin", "command.start", Permission::Allow);
        assert!(allowed(&store, "group.admin", "command.start"));
        assert!(!allowed(&store, "group.admin", "command.stop"));
    }

    #[test]
    fn deny_beats_allow_at_the_same_path() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command.stop", Permission::Allow);
        store.add_principal("identifier.license:abc", "group.admin");
        store.add_ace("identifier.license:abc", "command.stop", Permission::Deny);
        assert!(allowed(&store, "group.admin", "command.stop"));
        assert!(!allowed(&store, "identifier.license:abc", "command.stop"));
    }

    #[test]
    fn a_later_entry_for_the_same_pair_replaces_the_earlier_one() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Deny);
        store.add_ace("group.admin", "command", Permission::Allow);
        assert!(allowed(&store, "group.admin", "command"));
        assert_eq!(store.aces().count(), 1);
    }

    #[test]
    fn names_are_case_insensitive() {
        let mut store = AccessStore::default();
        store.add_ace("Group.Admin", "Command", Permission::Allow);
        store.add_principal("Identifier.License:ABC", "GROUP.ADMIN");
        assert!(allowed(&store, "group.admin", "command.Quit"));
        assert!(allowed(&store, "IDENTIFIER.license:abc", "COMMAND"));
        assert!(store.remove_ace("GROUP.admin", "command", Permission::Allow));
        assert!(!allowed(&store, "group.admin", "command"));
    }

    #[test]
    fn everyone_is_a_parent_of_every_principal() {
        let mut store = AccessStore::default();
        store.add_ace("builtin.everyone", "command.help", Permission::Allow);
        assert!(allowed(&store, "player.9", "command.help"));
        assert!(allowed(&store, "builtin.everyone", "command.help"));
        assert!(!allowed(&store, "player.9", "command.quit"));
        store.add_ace("builtin.everyone", "command", Permission::Deny);
        assert!(allowed(&store, "player.9", "command.help"));
        assert!(!allowed(&store, "player.9", "command.stop"));
    }

    #[test]
    fn extra_parents_are_ancestors_with_their_own_parents() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Allow);
        store.add_principal("group.staff", "group.admin");
        assert!(store.is_allowed("player.2", "command.quit", ["group.staff"]));
        assert!(!store.is_allowed("player.2", "command.quit", ["group.user"]));
        assert!(!allowed(&store, "player.2", "command.quit"));
    }

    #[test]
    fn removing_entries_and_links_reports_whether_they_existed() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Allow);
        assert!(!store.remove_ace("group.admin", "command", Permission::Deny));
        assert!(store.remove_ace("group.admin", "command", Permission::Allow));
        assert!(!store.remove_ace("group.admin", "command", Permission::Allow));
        assert_eq!(store.aces().count(), 0);

        store.add_principal("player.1", "group.admin");
        assert!(store.remove_principal("player.1", "group.admin"));
        assert!(!store.remove_principal("player.1", "group.admin"));
        assert_eq!(store.links().count(), 0);
    }

    #[test]
    fn an_object_without_dots_and_an_empty_object_are_handled() {
        let mut store = AccessStore::default();
        store.add_ace("group.admin", "command", Permission::Allow);
        assert!(allowed(&store, "group.admin", "command"));
        assert!(!allowed(&store, "group.admin", ""));
        assert!(allowed(&store, "group.admin", "command.quit"));
    }

    #[test]
    fn permissions_parse_in_any_case() {
        assert_eq!(Permission::parse("Allow"), Some(Permission::Allow));
        assert_eq!(Permission::parse("DENY"), Some(Permission::Deny));
        assert_eq!(Permission::parse("maybe"), None);
        assert_eq!(Permission::Deny.as_str(), "deny");
    }
}
