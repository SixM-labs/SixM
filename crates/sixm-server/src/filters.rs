//! Console channel filters: suppress or allow log channels by target or
//! `script:<resource>` glob pattern.

use std::fmt::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use sixm_core::convar_filter_matches;
use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Filter};

/// Tracing target used by server-side script `print` and error logs.
const SCRIPT_TARGET: &str = "sixm::script";
const SCRIPT_PREFIX: &str = "script:";

/// Whether a matching console channel rule prints or suppresses events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelAction {
    Print,
    None,
}

impl ChannelAction {
    /// Parses `"print"` or `"none"` case-insensitively.
    pub fn parse(word: &str) -> Option<Self> {
        if word.eq_ignore_ascii_case("print") {
            Some(Self::Print)
        } else if word.eq_ignore_ascii_case("none") {
            Some(Self::None)
        } else {
            None
        }
    }

    /// Returns `"print"` or `"none"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::None => "none",
        }
    }
}

impl fmt::Display for ChannelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
struct ChannelRule {
    pattern: String,
    action: ChannelAction,
}

/// Thread-safe store of console channel filter rules.
///
/// Cloning shares the same underlying rule set. When no rules are active,
/// [`allows`](Self::allows) returns immediately via an atomic flag without
/// locking or allocating.
#[derive(Clone, Debug, Default)]
pub struct ChannelFilters {
    has_filters: Arc<AtomicBool>,
    rules: Arc<RwLock<Vec<ChannelRule>>>,
}

impl ChannelFilters {
    /// Creates an empty filter store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a rule matching `channel` (ASCII case-insensitive).
    pub fn set(&self, channel: &str, action: ChannelAction) {
        let mut rules = self.rules.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = rules
            .iter_mut()
            .find(|rule| rule.pattern.eq_ignore_ascii_case(channel))
        {
            existing.pattern.clear();
            existing.pattern.push_str(channel);
            existing.action = action;
        } else {
            rules.push(ChannelRule {
                pattern: channel.to_owned(),
                action,
            });
        }
        self.has_filters.store(!rules.is_empty(), Ordering::Release);
    }

    /// Removes the rule matching `channel` (ASCII case-insensitive).
    /// Returns whether a rule was removed.
    pub fn remove(&self, channel: &str) -> bool {
        let mut rules = self.rules.write().unwrap_or_else(PoisonError::into_inner);
        let before = rules.len();
        rules.retain(|rule| !rule.pattern.eq_ignore_ascii_case(channel));
        let removed = rules.len() != before;
        self.has_filters.store(!rules.is_empty(), Ordering::Release);
        removed
    }

    /// Returns a snapshot of active rules in insertion order.
    pub fn list(&self) -> Vec<(String, ChannelAction)> {
        if !self.has_filters.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.rules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|rule| (rule.pattern.clone(), rule.action))
            .collect()
    }

    /// Returns whether an event with `target` and optional formatted `message`
    /// should be emitted.
    ///
    /// Rules are evaluated in insertion order (last matching rule wins). When
    /// no rule matches, events are allowed by default.
    pub fn allows(&self, target: &str, message: Option<&str>) -> bool {
        if !self.has_filters.load(Ordering::Acquire) {
            return true;
        }
        let rules = self.rules.read().unwrap_or_else(PoisonError::into_inner);
        rules
            .iter()
            .rev()
            .find(|rule| rule_matches(&rule.pattern, target, message))
            .is_none_or(|rule| rule.action == ChannelAction::Print)
    }

    /// Returns a [`Filter`] backed by this store for `tracing_subscriber` layers.
    pub fn tracing_filter(&self) -> ChannelTracingFilter {
        ChannelTracingFilter {
            filters: self.clone(),
        }
    }
}

/// A [`tracing_subscriber::layer::Filter`] backed by [`ChannelFilters`].
#[derive(Clone, Debug)]
pub struct ChannelTracingFilter {
    filters: ChannelFilters,
}

impl<S: Subscriber> Filter<S> for ChannelTracingFilter {
    fn enabled(&self, meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        if !self.filters.has_filters.load(Ordering::Acquire) {
            return true;
        }
        // Defer `sixm::script` filtering to `event_enabled` so a later
        // `script:<resource> print` rule can still override a `sixm::* none` rule.
        if meta.target() == SCRIPT_TARGET {
            return true;
        }
        self.filters.allows(meta.target(), None)
    }

    fn event_enabled(&self, event: &Event<'_>, _cx: &Context<'_, S>) -> bool {
        if !self.filters.has_filters.load(Ordering::Acquire) {
            return true;
        }
        if event.metadata().target() != SCRIPT_TARGET {
            return true;
        }
        let mut visitor = ScriptPrefixVisitor::default();
        event.record(&mut visitor);
        self.filters.allows(SCRIPT_TARGET, Some(visitor.as_str()))
    }
}

fn rule_matches(pattern: &str, target: &str, message: Option<&str>) -> bool {
    if convar_filter_matches(pattern, target) {
        return true;
    }
    if target == SCRIPT_TARGET
        && let Some(resource) = message.and_then(extract_script_resource)
    {
        return glob_matches_prefixed(pattern, SCRIPT_PREFIX, resource);
    }
    false
}

/// Extracts `<resource>` when `message` starts with `"[<resource>] "` or `"[<resource>]"`.
fn extract_script_resource(message: &str) -> Option<&str> {
    let rest = message.strip_prefix('[')?;
    let end = rest.find(']')?;
    let resource = &rest[..end];
    if resource.is_empty() {
        return None;
    }
    let after = &rest[end + 1..];
    (after.is_empty() || after.starts_with(' ')).then_some(resource)
}

/// Case-insensitive glob matching against `prefix + suffix` without allocating.
fn glob_matches_prefixed(filter: &str, prefix: &str, suffix: &str) -> bool {
    if filter.is_empty() || filter == "*" {
        return true;
    }
    let f = filter.as_bytes();
    let p = prefix.as_bytes();
    let s = suffix.as_bytes();
    let total = p.len() + s.len();
    let byte_at = |idx: usize| -> u8 {
        if idx < p.len() {
            p[idx]
        } else {
            s[idx - p.len()]
        }
    };
    let (mut fi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_ni = 0usize;

    while ni < total {
        if fi < f.len() && (f[fi] == b'?' || f[fi].eq_ignore_ascii_case(&byte_at(ni))) {
            fi += 1;
            ni += 1;
        } else if fi < f.len() && f[fi] == b'*' {
            star = Some(fi);
            fi += 1;
            match_ni = ni;
        } else if let Some(st) = star {
            fi = st + 1;
            match_ni += 1;
            ni = match_ni;
        } else {
            return false;
        }
    }
    while fi < f.len() && f[fi] == b'*' {
        fi += 1;
    }
    fi == f.len()
}

struct ScriptPrefixVisitor {
    buf: [u8; 264],
    len: usize,
    truncated: bool,
}

impl Default for ScriptPrefixVisitor {
    fn default() -> Self {
        Self {
            buf: [0; 264],
            len: 0,
            truncated: false,
        }
    }
}

impl ScriptPrefixVisitor {
    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl fmt::Write for ScriptPrefixVisitor {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }
        let avail = self.buf.len().saturating_sub(self.len);
        if avail == 0 {
            self.truncated = true;
            return Ok(());
        }
        let mut end = s.len().min(avail);
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        self.buf[self.len..self.len + end].copy_from_slice(&s.as_bytes()[..end]);
        self.len += end;
        if end < s.len() {
            self.truncated = true;
        }
        Ok(())
    }
}

impl Visit for ScriptPrefixVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.len = 0;
            self.truncated = false;
            let _ = self.write_str(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.len = 0;
            self.truncated = false;
            let _ = write!(self, "{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::{EnvFilter, Registry};

    use super::*;

    #[test]
    fn action_parse_and_display() {
        assert_eq!(ChannelAction::parse("print"), Some(ChannelAction::Print));
        assert_eq!(ChannelAction::parse("PRINT"), Some(ChannelAction::Print));
        assert_eq!(ChannelAction::parse("none"), Some(ChannelAction::None));
        assert_eq!(ChannelAction::parse("NoNe"), Some(ChannelAction::None));
        assert_eq!(ChannelAction::parse("drop"), None);
        assert_eq!(ChannelAction::Print.to_string(), "print");
        assert_eq!(ChannelAction::None.to_string(), "none");
    }

    #[test]
    fn fast_path_and_case_insensitive_crud() {
        let filters = ChannelFilters::new();
        assert!(!filters.has_filters.load(Ordering::Relaxed));
        assert!(filters.allows("sixm_server::commands", Some("hello")));
        assert_eq!(filters.list(), []);

        filters.set("Script:App", ChannelAction::None);
        assert!(filters.has_filters.load(Ordering::Relaxed));
        assert_eq!(
            filters.list(),
            vec![("Script:App".to_owned(), ChannelAction::None)]
        );

        // Case-insensitive update replaces in place without duplicating.
        filters.set("script:app", ChannelAction::Print);
        assert_eq!(
            filters.list(),
            vec![("script:app".to_owned(), ChannelAction::Print)]
        );

        assert!(filters.remove("SCRIPT:APP"));
        assert!(!filters.remove("script:app"));
        assert!(!filters.has_filters.load(Ordering::Relaxed));
        assert_eq!(filters.list(), []);
    }

    #[test]
    fn glob_and_script_channel_matching() {
        let filters = ChannelFilters::new();
        filters.set("sixm_server::web*", ChannelAction::None);
        assert!(!filters.allows("sixm_server::web", None));
        assert!(!filters.allows("SIXM_SERVER::WebSocket", None));
        assert!(filters.allows("sixm_server::commands", None));

        filters.set("script:chat?", ChannelAction::None);
        assert!(!filters.allows("sixm::script", Some("[chat1] hello")));
        assert!(!filters.allows("sixm::script", Some("[CHAT2]")));
        assert!(filters.allows("sixm::script", Some("[chat] hello")));
        assert!(filters.allows("sixm::script", Some("[chat12] hello")));
        assert!(filters.allows("sixm::script", Some("[chat1]no-space")));
        assert!(filters.allows("sixm::script", Some("[] empty")));
        // `script:*` does not match non-`sixm::script` targets.
        assert!(filters.allows("other::target", Some("[chat1] hello")));
    }

    #[test]
    fn last_matching_rule_wins() {
        let filters = ChannelFilters::new();
        filters.set("*", ChannelAction::None);
        assert!(!filters.allows("sixm_server::commands", None));
        assert!(!filters.allows("sixm::script", Some("[app] hi")));

        filters.set("script:app", ChannelAction::Print);
        assert!(filters.allows("sixm::script", Some("[app] hi")));
        assert!(!filters.allows("sixm::script", Some("[other] hi")));

        filters.set("script:app", ChannelAction::None);
        assert!(!filters.allows("sixm::script", Some("[app] hi")));
    }

    #[test]
    fn prefixed_glob_matches_convar_filter_matches() {
        let patterns = [
            "",
            "*",
            "script:*",
            "script:foo",
            "SCRIPT:F?O*",
            "*foo*",
            "script:???",
            "other:*",
        ];
        let resources = ["foo", "FooBar", "f", "abc", "chat", "my_res"];
        for pat in patterns {
            for res in resources {
                let full = format!("script:{res}");
                assert_eq!(
                    glob_matches_prefixed(pat, SCRIPT_PREFIX, res),
                    convar_filter_matches(pat, &full),
                    "pattern={pat:?}, res={res:?}"
                );
            }
        }
    }

    #[test]
    fn tracing_filter_dynamically_updates_across_repeated_callsites() {
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<String>>>);

        impl<S: Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &Event<'_>, _cx: Context<'_, S>) {
                let mut visitor = ScriptPrefixVisitor::default();
                event.record(&mut visitor);
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(visitor.as_str().to_owned());
            }
        }

        let filters = ChannelFilters::new();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default()
            .with(EnvFilter::new("info"))
            .with(Capture(Arc::clone(&captured)).with_filter(filters.tracing_filter()));

        tracing::subscriber::with_default(subscriber, || {
            let emit = |res: &str, msg: &str| {
                tracing::info!(target: "sixm::script", "[{res}] {msg}");
            };
            let emit_server = |msg: &str| {
                tracing::info!("{msg}");
            };

            emit("noisy", "first");
            emit_server("srv1");

            filters.set("sixm::*", ChannelAction::None);
            filters.set("script:keep", ChannelAction::Print);
            emit("noisy", "dropped");
            emit("keep", "kept");
            emit_server("srv2");

            filters.set("sixm_server::*", ChannelAction::None);
            emit_server("srv_dropped");

            filters.remove("sixm::*");
            filters.remove("sixm_server::*");
            emit("noisy", "restored");
            emit_server("srv3");
        });

        let lines = captured.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            *lines,
            vec![
                "[noisy] first",
                "srv1",
                "[keep] kept",
                "srv2",
                "[noisy] restored",
                "srv3",
            ]
        );
    }
}
