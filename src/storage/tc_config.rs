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
    #[serde(default)]
    pub xstatus: Vec<XStatusDef>,

    /// Comma-separated list of configured tag names. Empty string = no tags.
    #[serde(default)]
    pub tags: String,
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
        self.tag_list().iter().any(|t| t == name)
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

    /// Rename `old` → `new` in the tag list.
    ///
    /// Returns `Err` (message) if `old` is not present or `new` already exists.
    pub fn rename_tag(&mut self, old: &str, new: &str) -> Result<(), String> {
        let mut list = self.tag_list();
        if !list.iter().any(|t| t == old) {
            return Err(format!("tag not found: {old}"));
        }
        if list.iter().any(|t| t == new) {
            return Err(format!("tag already exists: {new}"));
        }
        for t in &mut list {
            if t == old {
                *t = new.to_string();
            }
        }
        self.tags = list.join(",");
        Ok(())
    }

    /// Return `true` if `name` is a known xstatus definition.
    pub fn has_xstatus(&self, name: &str) -> bool {
        self.xstatus.iter().any(|x| x.name == name)
    }
}
