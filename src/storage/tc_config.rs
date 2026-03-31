//! TcConfig — per-device configuration stored in the `tc_settings` table.
//!
//! The config is a single JSON value stored as a singleton row
//! (`id='tc_config'`). Swift reads this table directly via PowerSync;
//! Rust reads/writes it through the `get_tc_config`/`set_tc_config`
//! `StorageTxn` methods.
//!
//! JSON shape (all fields optional; use `Default` for absent config):
//! ```json
//! {
//!   "xstatus": [{"name": "blocked", "icon": 128721}],
//!   "tags": "abc,efg"
//! }
//! ```

use serde::{Deserialize, Serialize};

/// An extended-status definition.
///
/// `icon` is a Unicode codepoint value (e.g. `128721` for 🚩).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct XStatusDef {
    pub name: String,
    pub icon: u32,
}

/// Per-device task configuration stored in `tc_settings`.
///
/// Tags are stored as a comma-separated string (e.g. `"work,home,urgent"`).
/// An empty string means no tags are configured.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TcConfig {
    /// Extended-status definitions. Each entry adds a named xstatus category.
    ///
    /// Use [`add_xstatus`](TcConfig::add_xstatus) /
    /// [`remove_xstatus`](TcConfig::remove_xstatus) to mutate — direct field
    /// access is intentionally restricted to prevent duplicate names.
    #[serde(default)]
    pub(crate) xstatus: Vec<XStatusDef>,

    /// Comma-separated list of configured tag names. Empty string = no tags.
    ///
    /// Use [`remove_tag`](TcConfig::remove_tag) /
    /// [`rename_tag`](TcConfig::rename_tag) to mutate — direct field access is
    /// intentionally restricted to keep the comma-separated invariant intact.
    #[serde(default)]
    pub(crate) tags: String,
}

impl TcConfig {
    /// Return tag names as a sorted, deduplicated `Vec<String>`.
    pub fn tag_list(&self) -> Vec<String> {
        if self.tags.is_empty() {
            return Vec::new();
        }
        let mut names: Vec<String> = self
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Return `true` if the given tag name is in the config.
    pub fn has_tag(&self, name: &str) -> bool {
        self.tags.split(',').any(|t| t.trim() == name)
    }

    /// Remove `name` from the tag list. Returns `false` if not present.
    pub fn remove_tag(&mut self, name: &str) -> bool {
        let mut list = self.tag_list();
        let before_len = list.len();
        list.retain(|t| t != name);
        if list.len() == before_len {
            return false;
        }
        self.tags = list.join(",");
        true
    }

    /// Add `name` to the tag list.
    ///
    /// Returns `false` if the tag already exists (no-op); `true` if it was added.
    pub fn add_tag(&mut self, name: &str) -> bool {
        if self.has_tag(name) {
            return false;
        }
        if self.tags.is_empty() {
            self.tags = name.to_string();
        } else {
            self.tags.push(',');
            self.tags.push_str(name);
        }
        true
    }

    /// Rename `old` → `new` in the tag list.
    ///
    /// Returns `Err` (message) if `old` is not present or `new` already exists.
    /// Returns `Err` if `old == new` (renaming to the same name is a no-op error).
    pub fn rename_tag(&mut self, old: &str, new: &str) -> Result<(), String> {
        let mut list = self.tag_list();
        if !list.iter().any(|t| t == old) {
            return Err(format!("Tag not found: {old}"));
        }
        if list.iter().any(|t| t == new) {
            return Err(format!("Tag already exists: {new}"));
        }
        for t in &mut list {
            if t == old {
                *t = new.to_string();
            }
        }
        self.tags = list.join(",");
        Ok(())
    }

    /// Return xstatus names as a `Vec<String>` (preserves insertion order).
    pub fn xstatus_list(&self) -> Vec<String> {
        self.xstatus.iter().map(|x| x.name.clone()).collect()
    }

    /// Return `true` if `name` is a known xstatus definition.
    pub fn has_xstatus(&self, name: &str) -> bool {
        self.xstatus.iter().any(|x| x.name == name)
    }

    /// Add an xstatus definition. No-op if a definition with the same name exists.
    ///
    /// Returns `true` if the definition was added, `false` if already present.
    pub fn add_xstatus(&mut self, def: XStatusDef) -> bool {
        if self.has_xstatus(&def.name) {
            return false;
        }
        self.xstatus.push(def);
        true
    }

    /// Remove the xstatus definition with the given name.
    ///
    /// Returns `true` if it was present and removed, `false` if not found.
    pub fn remove_xstatus(&mut self, name: &str) -> bool {
        let before = self.xstatus.len();
        self.xstatus.retain(|x| x.name != name);
        self.xstatus.len() < before
    }

    /// Rename `old` → `new` in the xstatus definitions.
    ///
    /// Returns `Err` (message) if `old` is not present or `new` already exists.
    /// Returns `Err` if `old == new` (renaming to the same name is a no-op error).
    pub fn rename_xstatus(&mut self, old: &str, new: &str) -> Result<(), String> {
        if !self.xstatus.iter().any(|x| x.name == old) {
            return Err(format!("XStatus not found: {old}"));
        }
        if self.xstatus.iter().any(|x| x.name == new) {
            return Err(format!("XStatus already exists: {new}"));
        }
        for x in &mut self.xstatus {
            if x.name == old {
                x.name = new.to_string();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_tags(tags: &str) -> TcConfig {
        TcConfig {
            tags: tags.to_string(),
            ..Default::default()
        }
    }

    // ── tag_list ──────────────────────────────────────────────────────────────

    #[test]
    fn tag_list_empty_string_returns_empty() {
        assert_eq!(config_with_tags("").tag_list(), Vec::<String>::new());
    }

    #[test]
    fn tag_list_deduplicates_and_sorts() {
        // Duplicates are collapsed; result is sorted alphabetically.
        let cfg = config_with_tags("work,home,work,urgent,home");
        assert_eq!(cfg.tag_list(), vec!["home", "urgent", "work"]);
    }

    #[test]
    fn tag_list_trims_whitespace() {
        let cfg = config_with_tags(" work , home ");
        assert_eq!(cfg.tag_list(), vec!["home", "work"]);
    }

    // ── add_tag ───────────────────────────────────────────────────────────────

    #[test]
    fn add_tag_to_empty_config() {
        let mut cfg = TcConfig::default();
        assert!(cfg.add_tag("work"));
        assert_eq!(cfg.tag_list(), vec!["work"]);
    }

    #[test]
    fn add_tag_appends_to_existing() {
        let mut cfg = config_with_tags("home");
        assert!(cfg.add_tag("work"));
        assert!(cfg.has_tag("work"));
        assert!(cfg.has_tag("home"));
    }

    #[test]
    fn add_tag_dedup_returns_false() {
        let mut cfg = config_with_tags("work");
        assert!(!cfg.add_tag("work"), "duplicate should return false");
        assert_eq!(cfg.tag_list(), vec!["work"], "list unchanged");
    }

    // ── remove_tag ────────────────────────────────────────────────────────────

    #[test]
    fn remove_tag_returns_false_when_absent() {
        let mut cfg = config_with_tags("work,home");
        assert!(!cfg.remove_tag("urgent"), "absent tag should return false");
        assert_eq!(cfg.tag_list(), vec!["home", "work"], "list unchanged");
    }

    #[test]
    fn remove_tag_removes_present_tag() {
        let mut cfg = config_with_tags("work,home");
        assert!(cfg.remove_tag("work"));
        assert_eq!(cfg.tag_list(), vec!["home"]);
    }

    // ── rename_tag ────────────────────────────────────────────────────────────

    #[test]
    fn rename_tag_same_name_is_error() {
        // Renaming to the same name is an error: "Tag already exists".
        let mut cfg = config_with_tags("work");
        let result = cfg.rename_tag("work", "work");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag already exists"),
            "error message should start with 'Tag already exists'"
        );
    }

    #[test]
    fn rename_tag_old_not_found_prefix() {
        let mut cfg = config_with_tags("work");
        let result = cfg.rename_tag("ghost", "new");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag not found"),
            "error message should start with 'Tag not found'"
        );
    }

    #[test]
    fn rename_tag_new_already_exists_prefix() {
        let mut cfg = config_with_tags("old,new");
        let result = cfg.rename_tag("old", "new");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag already exists"),
            "error message should start with 'Tag already exists'"
        );
    }

    #[test]
    fn rename_tag_succeeds() {
        let mut cfg = config_with_tags("old,home");
        cfg.rename_tag("old", "newtag").unwrap();
        assert_eq!(cfg.tag_list(), vec!["home", "newtag"]);
    }

    // ── xstatus ───────────────────────────────────────────────────────────────

    #[test]
    fn has_xstatus_false_for_empty_config() {
        let cfg = TcConfig::default();
        assert!(!cfg.has_xstatus("blocked"));
    }

    #[test]
    fn add_xstatus_dedup() {
        let mut cfg = TcConfig::default();
        let added = cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 128721,
        });
        assert!(added);
        // Adding the same name again should be a no-op.
        let added_again = cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 9999,
        });
        assert!(!added_again);
        assert_eq!(cfg.xstatus.len(), 1);
        assert_eq!(cfg.xstatus[0].icon, 128721, "original entry preserved");
    }

    // ── xstatus_list ──────────────────────────────────────────────────────

    #[test]
    fn xstatus_list_empty() {
        let cfg = TcConfig::default();
        assert!(cfg.xstatus_list().is_empty());
    }

    #[test]
    fn xstatus_list_preserves_order() {
        let mut cfg = TcConfig::default();
        cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 1,
        });
        cfg.add_xstatus(XStatusDef {
            name: "alpha".into(),
            icon: 2,
        });
        assert_eq!(cfg.xstatus_list(), vec!["blocked", "alpha"]);
    }

    // ── rename_xstatus ──────────────────────────────────────────────────

    #[test]
    fn rename_xstatus_succeeds() {
        let mut cfg = TcConfig::default();
        cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 128721,
        });
        cfg.rename_xstatus("blocked", "waiting").unwrap();
        assert!(!cfg.has_xstatus("blocked"));
        assert!(cfg.has_xstatus("waiting"));
        // Icon preserved
        assert_eq!(cfg.xstatus[0].icon, 128721);
    }

    #[test]
    fn rename_xstatus_same_name_is_error() {
        let mut cfg = TcConfig::default();
        cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 1,
        });
        let result = cfg.rename_xstatus("blocked", "blocked");
        assert!(result.is_err());
        assert!(result.unwrap_err().starts_with("XStatus already exists"));
    }

    #[test]
    fn rename_xstatus_old_not_found() {
        let cfg = &mut TcConfig::default();
        let result = cfg.rename_xstatus("ghost", "new");
        assert!(result.is_err());
        assert!(result.unwrap_err().starts_with("XStatus not found"));
    }

    #[test]
    fn rename_xstatus_new_already_exists() {
        let mut cfg = TcConfig::default();
        cfg.add_xstatus(XStatusDef {
            name: "old".into(),
            icon: 1,
        });
        cfg.add_xstatus(XStatusDef {
            name: "new".into(),
            icon: 2,
        });
        let result = cfg.rename_xstatus("old", "new");
        assert!(result.is_err());
        assert!(result.unwrap_err().starts_with("XStatus already exists"));
    }

    #[test]
    fn remove_xstatus_not_present_returns_false() {
        let mut cfg = TcConfig::default();
        assert!(!cfg.remove_xstatus("ghost"));
    }

    #[test]
    fn remove_xstatus_removes_entry() {
        let mut cfg = TcConfig::default();
        cfg.add_xstatus(XStatusDef {
            name: "blocked".into(),
            icon: 1,
        });
        assert!(cfg.remove_xstatus("blocked"));
        assert!(!cfg.has_xstatus("blocked"));
    }
}
