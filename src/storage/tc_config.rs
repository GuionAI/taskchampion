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
//!   "tags": ["abc", "efg"]
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
/// Tags are stored as a JSON array of strings (e.g. `["work", "home"]`),
/// with legacy support for comma-separated strings on deserialization.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TcConfig {
    /// Extended-status definitions. Each entry adds a named xstatus category.
    ///
    /// Use [`add_xstatus`](TcConfig::add_xstatus) /
    /// [`remove_xstatus`](TcConfig::remove_xstatus) to mutate — direct field
    /// access is intentionally restricted to prevent duplicate names.
    #[serde(default)]
    pub(crate) xstatus: Vec<XStatusDef>,

    /// List of configured tag names.
    ///
    /// Use [`remove_tag`](TcConfig::remove_tag) /
    /// [`rename_tag`](TcConfig::rename_tag) to mutate — direct field access is
    /// intentionally restricted to prevent duplicate entries via `add_tag`'s dedup guard.
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

impl TcConfig {
    /// Return tag names as a sorted, deduplicated `Vec<String>`.
    pub fn tag_list(&self) -> Vec<String> {
        let mut names = self.tags.clone();
        names.sort();
        names.dedup();
        names
    }

    /// Return `true` if the given tag name is in the config.
    pub fn has_tag(&self, name: &str) -> bool {
        self.tags.iter().any(|t| t == name)
    }

    /// Remove `name` from the tag list. Returns `false` if not present.
    pub fn remove_tag(&mut self, name: &str) -> bool {
        let before_len = self.tags.len();
        self.tags.retain(|t| t != name);
        if self.tags.len() == before_len {
            return false;
        }
        true
    }

    /// Add `name` to the tag list.
    ///
    /// Returns `false` if the tag already exists (no-op); `true` if it was added.
    pub fn add_tag(&mut self, name: &str) -> bool {
        if self.has_tag(name) {
            return false;
        }
        self.tags.push(name.to_string());
        true
    }

    /// Rename `old` → `new` in the tag list.
    ///
    /// Returns `Err` (message) if `old` is not present or `new` already exists.
    /// Returns `Err` if `old == new` (renaming to the same name is a no-op error).
    pub fn rename_tag(&mut self, old: &str, new: &str) -> Result<(), String> {
        if !self.tags.iter().any(|t| t == old) {
            return Err(format!("Tag not found: {old}"));
        }
        if self.tags.iter().any(|t| t == new) {
            return Err(format!("Tag already exists: {new}"));
        }
        for t in &mut self.tags {
            if t == old {
                *t = new.to_string();
            }
        }
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

    fn config_with_tags(tags: &[&str]) -> TcConfig {
        TcConfig {
            tags: tags.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    // ── tag_list ──────────────────────────────────────────────────────────────

    #[test]
    fn tag_list_empty_string_returns_empty() {
        assert_eq!(config_with_tags(&[]).tag_list(), Vec::<String>::new());
    }

    #[test]
    fn tag_list_deduplicates_and_sorts() {
        // Duplicates are collapsed; result is sorted alphabetically.
        let cfg = config_with_tags(&["work", "home", "work", "urgent", "home"]);
        assert_eq!(cfg.tag_list(), vec!["home", "urgent", "work"]);
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
        let mut cfg = config_with_tags(&["home"]);
        assert!(cfg.add_tag("work"));
        assert!(cfg.has_tag("work"));
        assert!(cfg.has_tag("home"));
    }

    #[test]
    fn add_tag_dedup_returns_false() {
        let mut cfg = config_with_tags(&["work"]);
        assert!(!cfg.add_tag("work"), "duplicate should return false");
        assert_eq!(cfg.tag_list(), vec!["work"], "list unchanged");
    }

    // ── remove_tag ────────────────────────────────────────────────────────────

    #[test]
    fn remove_tag_returns_false_when_absent() {
        let mut cfg = config_with_tags(&["work", "home"]);
        assert!(!cfg.remove_tag("urgent"), "absent tag should return false");
        assert_eq!(cfg.tag_list(), vec!["home", "work"], "list unchanged");
    }

    #[test]
    fn remove_tag_removes_present_tag() {
        let mut cfg = config_with_tags(&["work", "home"]);
        assert!(cfg.remove_tag("work"));
        assert_eq!(cfg.tag_list(), vec!["home"]);
    }

    // ── rename_tag ────────────────────────────────────────────────────────────

    #[test]
    fn rename_tag_same_name_is_error() {
        // Renaming to the same name is an error: "Tag already exists".
        let mut cfg = config_with_tags(&["work"]);
        let result = cfg.rename_tag("work", "work");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag already exists"),
            "error message should start with 'Tag already exists'"
        );
    }

    #[test]
    fn rename_tag_old_not_found_prefix() {
        let mut cfg = config_with_tags(&["work"]);
        let result = cfg.rename_tag("ghost", "new");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag not found"),
            "error message should start with 'Tag not found'"
        );
    }

    #[test]
    fn rename_tag_new_already_exists_prefix() {
        let mut cfg = config_with_tags(&["old", "new"]);
        let result = cfg.rename_tag("old", "new");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().starts_with("Tag already exists"),
            "error message should start with 'Tag already exists'"
        );
    }

    #[test]
    fn rename_tag_succeeds() {
        let mut cfg = config_with_tags(&["old", "home"]);
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

    // ── serde shape ───────────────────────────────────────────────────────────────

    #[test]
    fn has_tag_exact_match_no_trim() {
        // has_tag uses exact Vec equality — no whitespace trimming.
        // (The old comma-split impl did trim; the new one does not.)
        let cfg = config_with_tags(&["work"]);
        assert!(cfg.has_tag("work"), "exact match");
        assert!(!cfg.has_tag(" work"), "leading space must not match");
        assert!(!cfg.has_tag("work "), "trailing space must not match");
    }

    // ── serde shape ───────────────────────────────────────────────────────────────

    #[test]
    fn tags_serializes_as_json_array() {
        let mut cfg = TcConfig::default();
        cfg.add_tag("work");
        cfg.add_tag("home");
        let json = serde_json::to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = v["tags"].as_array().expect("tags should be a JSON array");
        let mut names: Vec<&str> = arr.iter().map(|t| t.as_str().unwrap()).collect();
        names.sort();
        assert_eq!(names, vec!["home", "work"]);
    }

    #[test]
    fn tags_deserializes_from_json_array() {
        let json = r#"{"tags":["delta","echo"]}"#;
        let cfg: TcConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.tag_list(), vec!["delta", "echo"]);
    }
}
