//! Theme registry: nvim-style highlight groups interned to stable [`HlGroup`] ids.
//! Unknown names resolve to `Style::default()` without panicking.
//!
//! A `Theme` is a flat `HlGroup → Style` map plus an `is_light` hint. There
//! is no aliasing layer - a colorscheme that wants two names to share a
//! color either sets both directly, or expresses the relationship in its
//! source spec (e.g. `SmeltToolPending = "Comment"` resolved at compile time in
//! `smelt_tui::theme::compile`). The runtime sees only resolved styles.

use crate::style::Style;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

static NEXT_THEME_REVISION: AtomicU64 = AtomicU64::new(1);

/// Interned highlight-group id. Stable for the process lifetime.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HlGroup(pub u32);

/// Process-global name → id interner. Decoupled from `Theme` so ids stay stable across
/// theme switches and multiple `Theme` instances.
struct HlGroupRegistry {
    name_to_id: HashMap<String, HlGroup>,
    id_to_name: Vec<String>,
}

impl HlGroupRegistry {
    fn new() -> Self {
        Self {
            name_to_id: HashMap::new(),
            id_to_name: Vec::new(),
        }
    }

    fn intern(&mut self, name: &str) -> HlGroup {
        if let Some(id) = self.name_to_id.get(name) {
            return *id;
        }
        let id = HlGroup(self.id_to_name.len() as u32);
        self.name_to_id.insert(name.to_string(), id);
        self.id_to_name.push(name.to_string());
        id
    }
}

fn registry() -> &'static RwLock<HlGroupRegistry> {
    static REG: OnceLock<RwLock<HlGroupRegistry>> = OnceLock::new();
    REG.get_or_init(|| RwLock::new(HlGroupRegistry::new()))
}

/// Get-or-mint the [`HlGroup`] id for `name`.
pub fn intern(name: &str) -> HlGroup {
    if let Some(id) = registry().read().unwrap().name_to_id.get(name).copied() {
        return id;
    }
    registry().write().unwrap().intern(name)
}

/// Reverse the interner: id → name.
pub fn name_of(g: HlGroup) -> Option<String> {
    registry()
        .read()
        .unwrap()
        .id_to_name
        .get(g.0 as usize)
        .cloned()
}

/// Intern a `Style` as an anonymous group keyed by content hash.
/// Anonymous groups bypass theme switches; use [`intern`] with a stable name for theme-reactive styling.
///
/// Called per span by syntect/markdown renderers; the read-locked fast path keeps
/// parallel block workers from serializing on the anon-styles write lock.
pub fn intern_anonymous_style(style: Style) -> HlGroup {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    style.hash(&mut h);
    let style_hash = h.finish();

    if let Some(&id) = anon_hash_to_group().read().unwrap().get(&style_hash) {
        return id;
    }

    let key = format!("__anon__/{:016x}", style_hash);
    let id = intern(&key);
    anon_hash_to_group().write().unwrap().insert(style_hash, id);
    anon_styles().write().unwrap().insert(id, style);
    id
}

fn anon_styles() -> &'static RwLock<HashMap<HlGroup, Style>> {
    static MAP: OnceLock<RwLock<HashMap<HlGroup, Style>>> = OnceLock::new();
    MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Style-hash → interned id. Backs the read-locked fast path of `intern_anonymous_style`.
fn anon_hash_to_group() -> &'static RwLock<HashMap<u64, HlGroup>> {
    static MAP: OnceLock<RwLock<HashMap<u64, HlGroup>>> = OnceLock::new();
    MAP.get_or_init(|| RwLock::new(HashMap::new()))
}

fn anon_resolve(id: HlGroup) -> Option<Style> {
    anon_styles().read().unwrap().get(&id).copied()
}

/// Reset all process-global interners. Intended for deterministic-simulation
/// tests and fuzz harnesses that reuse one process across scenarios; in
/// production these maps grow monotonically and are never reset.
///
/// Not safe to call while other threads hold writes through `intern` or
/// `intern_anonymous_style`.
pub fn reset_for_test() {
    let mut r = registry().write().unwrap();
    r.name_to_id.clear();
    r.id_to_name.clear();
    anon_styles().write().unwrap().clear();
    anon_hash_to_group().write().unwrap().clear();
}

/// Total distinct interned entries. Anonymous styles share the named
/// registry under `__anon__/<hash>` keys (see `intern_anonymous_style`),
/// so the named map's length already includes them - no separate count
/// is added. Used by leak invariants to confirm registries don't grow
/// across scenario repeats.
pub fn registry_len() -> usize {
    registry().read().unwrap().id_to_name.len()
}

/// `(named, anon)` interner sizes for diagnostics. `named` is the full
/// `id_to_name` length (which already contains every anon entry);
/// `anon` is the subset that has a resolved style in the `anon_styles`
/// map. Their difference equals the count of "real" named groups
/// registered by themes or callsites.
pub fn registry_counts() -> (usize, usize) {
    let named = registry().read().unwrap().id_to_name.len();
    let anon = anon_styles().read().unwrap().len();
    (named, anon)
}

/// Named groups registered at or after `from_idx`. Lets leak invariants
/// list the *newly-added* names rather than print a count alone.
pub fn names_since(from_idx: usize) -> Vec<String> {
    let r = registry().read().unwrap();
    r.id_to_name.get(from_idx..).unwrap_or(&[]).to_vec()
}

/// Resolved highlight-group → style map. `Theme` is materialized state:
/// every group has its final `Style` baked in. Construct via
/// `smelt_tui::theme::compile` or hand-build with [`Theme::set`].
#[derive(Debug, Clone)]
pub struct Theme {
    styles: HashMap<HlGroup, Style>,
    is_light: bool,
    syntax_theme: Option<String>,
    revision: u64,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            styles: HashMap::new(),
            is_light: false,
            syntax_theme: None,
            revision: next_theme_revision(),
        }
    }
}

fn next_theme_revision() -> u64 {
    NEXT_THEME_REVISION.fetch_add(1, Ordering::Relaxed)
}

impl Theme {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `name` to `style`. Overwrites any prior value.
    pub fn set(&mut self, name: impl Into<String>, style: Style) {
        let id = intern(&name.into());
        if self.styles.get(&id) == Some(&style) {
            return;
        }
        self.styles.insert(id, style);
        self.bump_revision();
    }

    /// Process-unique identifier for this exact theme state.
    ///
    /// Clones retain the revision. Any effective mutation assigns a fresh one,
    /// allowing retained rendering to reuse rows only while their styles match.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump_revision(&mut self) {
        self.revision = next_theme_revision();
    }

    /// Resolve a name to its current Style. Unknown names return `Style::default()`.
    pub fn get(&self, name: &str) -> Style {
        self.resolve(intern(name))
    }

    /// Resolve a [`HlGroup`] to its current Style. Anonymous style ids fall
    /// through to the global anon registry; everything else returns
    /// `Style::default()`.
    pub fn resolve(&self, hl: HlGroup) -> Style {
        if let Some(style) = self.styles.get(&hl).copied() {
            return style;
        }
        anon_resolve(hl).unwrap_or_default()
    }

    /// Get-or-mint the HlGroup id for `name`.
    pub fn id_for(&self, name: &str) -> HlGroup {
        intern(name)
    }

    /// True iff this Theme has a Style registered for `hl`. False means
    /// `resolve` will fall back to the anon registry or `Style::default()`.
    pub fn contains(&self, hl: HlGroup) -> bool {
        self.styles.contains_key(&hl)
    }

    pub fn is_light(&self) -> bool {
        self.is_light
    }

    pub fn set_light(&mut self, light: bool) {
        if self.is_light != light {
            self.is_light = light;
            self.bump_revision();
        }
    }

    pub fn syntax_theme(&self) -> Option<&str> {
        self.syntax_theme.as_deref()
    }

    pub fn set_syntax_theme(&mut self, name: Option<String>) {
        if self.syntax_theme != name {
            self.syntax_theme = name;
            self.bump_revision();
        }
    }

    /// Iterator over `(HlGroup, &Style)` for every group set on this theme.
    /// Order is unspecified.
    pub fn iter(&self) -> impl Iterator<Item = (HlGroup, &Style)> {
        self.styles.iter().map(|(k, v)| (*k, v))
    }

    /// Number of groups set on this theme.
    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::Color;

    #[test]
    fn unknown_name_returns_default() {
        let t = Theme::new();
        assert_eq!(t.get("Nonexistent"), Style::default());
    }

    #[test]
    fn set_and_get_round_trip() {
        let mut t = Theme::new();
        let s = Style {
            fg: Some(Color::Red),
            bold: true,
            ..Style::default()
        };
        t.set("Error", s);
        assert_eq!(t.get("Error"), s);
    }

    #[test]
    fn set_overwrites_existing_set() {
        let mut t = Theme::new();
        t.set("X", Style::new().bg(Color::AnsiValue(1)));
        let direct = Style::new().bg(Color::AnsiValue(2));
        t.set("X", direct);
        assert_eq!(t.get("X"), direct);
    }

    #[test]
    fn light_flag_round_trips() {
        let mut t = Theme::new();
        assert!(!t.is_light());
        t.set_light(true);
        assert!(t.is_light());
    }

    #[test]
    fn clone_preserves_revision() {
        let t = Theme::new();
        assert_eq!(t.revision(), t.clone().revision());
    }

    #[test]
    fn separate_themes_have_distinct_revisions() {
        assert_ne!(Theme::new().revision(), Theme::new().revision());
    }

    #[test]
    fn set_advances_revision_only_when_style_changes() {
        let mut t = Theme::new();
        let initial = t.revision();
        let style = Style::new().fg(Color::Green);

        t.set("style_audit_revision", style);
        let changed = t.revision();
        assert_ne!(changed, initial);

        t.set("style_audit_revision", style);
        assert_eq!(t.revision(), changed);

        t.set("style_audit_revision", Style::new().fg(Color::Yellow));
        assert_ne!(t.revision(), changed);
    }

    #[test]
    fn light_flag_advances_revision_only_when_changed() {
        let mut t = Theme::new();
        let initial = t.revision();

        t.set_light(false);
        assert_eq!(t.revision(), initial);

        t.set_light(true);
        let changed = t.revision();
        assert_ne!(changed, initial);

        t.set_light(true);
        assert_eq!(t.revision(), changed);
    }

    #[test]
    fn syntax_theme_advances_revision_only_when_changed() {
        let mut t = Theme::new();
        let initial = t.revision();

        t.set_syntax_theme(None);
        assert_eq!(t.revision(), initial);

        t.set_syntax_theme(Some("base16-ocean.dark".to_string()));
        let changed = t.revision();
        assert_ne!(changed, initial);

        t.set_syntax_theme(Some("base16-ocean.dark".to_string()));
        assert_eq!(t.revision(), changed);
    }

    // ── Interner ──────────────────────────────────────────────────────────
    // The interner is process-global; tests use unique prefixed names so they
    // don't collide with each other or with sibling tests.

    #[test]
    fn intern_returns_same_id_for_same_name() {
        assert_eq!(
            intern("style_audit_intern_a"),
            intern("style_audit_intern_a")
        );
    }

    #[test]
    fn intern_returns_different_ids_for_different_names() {
        assert_ne!(
            intern("style_audit_intern_b"),
            intern("style_audit_intern_c")
        );
    }

    #[test]
    fn name_of_round_trips_interned_id() {
        let id = intern("style_audit_name_of");
        assert_eq!(name_of(id), Some("style_audit_name_of".to_string()));
    }

    #[test]
    fn name_of_returns_none_for_unminted_id() {
        // u32::MAX is far past any id the test process would have minted.
        assert_eq!(name_of(HlGroup(u32::MAX)), None);
    }

    #[test]
    fn intern_anonymous_style_returns_same_id_for_equal_styles() {
        let s = Style::new().fg(Color::Red).bold();
        assert_eq!(intern_anonymous_style(s), intern_anonymous_style(s));
    }

    #[test]
    fn intern_anonymous_style_returns_different_ids_for_distinct_styles() {
        let s1 = Style::new().fg(Color::Red).bold();
        let s2 = Style::new().fg(Color::Blue).bold();
        assert_ne!(intern_anonymous_style(s1), intern_anonymous_style(s2));
    }

    #[test]
    fn theme_resolves_anonymous_style_via_fallthrough() {
        // A Theme with no entry for the anon id must still resolve the style
        // by falling through to the global anon registry.
        let t = Theme::new();
        let style = Style::new().fg(Color::Cyan).italic();
        let id = intern_anonymous_style(style);
        assert_eq!(t.resolve(id), style);
    }

    #[test]
    fn contains_reports_set_names_only() {
        let mut t = Theme::new();
        t.set("style_audit_contains_a", Style::new().bold());
        assert!(t.contains(t.id_for("style_audit_contains_a")));
        assert!(!t.contains(t.id_for("style_audit_contains_unknown")));
    }
}
