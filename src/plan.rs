//! Markdown plan parser for creating subtask trees.
//!
//! Converts markdown headings into a flat list of [`TaskSpec`] values representing
//! a tree of subtasks to be created under a parent task.  The actual task creation
//! is left to the caller (typically via [`crate::Replica`] operations).
//!
//! ## Example
//!
//! ```rust
//! use taskchampion::plan::{parse_markdown, plan_tasks};
//! use taskchampion::Uuid;
//!
//! let parent = Uuid::new_v4();
//! let sections = parse_markdown("## Step 1\nDo this first.\n\n## Step 2\nThen this.");
//! let specs = plan_tasks(parent, &sections);
//! assert_eq!(specs.len(), 2);
//! assert_eq!(specs[0].description, "Step 1");
//! assert_eq!(specs[0].annotation.as_deref(), Some("Do this first."));
//! ```

use uuid::Uuid;

use crate::position::sequential_positions;

/// A parsed section from a markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Heading depth: 1 for `#`, 2 for `##`, etc.
    pub level: usize,
    /// Heading text (without leading `#` characters).
    pub heading: String,
    /// Body text under the heading (trimmed).
    pub body: String,
}

/// A task to be created as part of a plan.
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// The UUID to use when creating this task.
    pub uuid: Uuid,
    /// The parent task UUID.
    pub parent: Uuid,
    /// The task description (from the heading text).
    pub description: String,
    /// Optional annotation (from the heading body text).
    pub annotation: Option<String>,
    /// Fractional-index position within its sibling group.
    pub position: String,
}

/// Parse markdown input into a flat list of sections.
///
/// Only lines that start with one or more `#` characters followed by a space (or end of line)
/// are treated as headings.  Text before the first heading is silently dropped.
/// Empty headings are skipped with a warning.
///
/// Lines inside fenced code blocks (delimited by ` ``` ` or `~~~`) are never treated as
/// headings, regardless of their content.
pub fn parse_markdown(input: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current_level: Option<usize> = None;
    let mut current_heading = String::new();
    let mut current_body = String::new();
    let mut in_fenced_block = false;
    let mut fence_char = '`';
    let mut fence_len: usize = 0;

    for line in input.lines() {
        // Detect fence open/close before heading detection.
        if let Some((ch, len)) = fence_marker(line) {
            if !in_fenced_block {
                in_fenced_block = true;
                fence_char = ch;
                fence_len = len;
            } else if ch == fence_char && len >= fence_len {
                in_fenced_block = false;
            }
            // Fence marker lines are body text (not headings).
            if current_level.is_some() {
                current_body.push_str(line);
                current_body.push('\n');
            }
            continue;
        }

        if in_fenced_block {
            // Inside a fenced block: skip heading detection, treat as body text.
            if current_level.is_some() {
                current_body.push_str(line);
                current_body.push('\n');
            }
            continue;
        }

        if let Some(level) = heading_level(line) {
            // Save previous section (skip if heading is empty)
            if let Some(prev_level) = current_level {
                let heading = current_heading.trim().to_string();
                if !heading.is_empty() {
                    sections.push(Section {
                        level: prev_level,
                        heading,
                        body: current_body.trim_end().to_string(),
                    });
                } else {
                    log::warn!("empty heading at level {prev_level} — skipped");
                }
            }
            current_level = Some(level);
            current_heading = strip_heading(line).to_string();
            current_body = String::new();
        } else if current_level.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Save last section
    if let Some(level) = current_level {
        let heading = current_heading.trim().to_string();
        if !heading.is_empty() {
            sections.push(Section {
                level,
                heading,
                body: current_body.trim_end().to_string(),
            });
        } else {
            log::warn!("empty heading at level {level} — skipped");
        }
    }

    sections
}

/// Convert a slice of sections into a flat list of [`TaskSpec`] values.
///
/// Recurses into nested heading levels, generating independently-ordered
/// positions for each sibling group.
pub fn plan_tasks(parent: Uuid, sections: &[Section]) -> Vec<TaskSpec> {
    plan_tasks_inner(parent, sections, 0, sections.len())
}

fn plan_tasks_inner(parent: Uuid, sections: &[Section], start: usize, end: usize) -> Vec<TaskSpec> {
    let mut result = Vec::new();

    // Collect direct children at this level first to count siblings.
    let mut direct_children: Vec<(usize, usize)> = Vec::new(); // (section_idx, child_end)
    {
        let mut j = start;
        while j < end {
            let child_start = j + 1;
            let mut child_end = child_start;
            while child_end < end && sections[child_end].level > sections[j].level {
                child_end += 1;
            }
            direct_children.push((j, child_end));
            j = child_end;
        }
    }

    // Generate positions for this batch of siblings.
    let positions = sequential_positions(direct_children.len());

    for (idx, (section_idx, child_end)) in direct_children.iter().enumerate() {
        let section = &sections[*section_idx];
        let task_uuid = Uuid::new_v4();

        result.push(TaskSpec {
            uuid: task_uuid,
            parent,
            description: section.heading.clone(),
            annotation: if section.body.trim().is_empty() {
                None
            } else {
                Some(section.body.trim().to_string())
            },
            position: positions[idx].clone(),
        });

        // Recurse into children
        let sub_start = *section_idx + 1;
        if *child_end > sub_start {
            let mut children = plan_tasks_inner(task_uuid, sections, sub_start, *child_end);
            result.append(&mut children);
        }
    }

    result
}

fn heading_level(line: &str) -> Option<usize> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.chars().take_while(|&c| c == '#').count();
    let rest = &line[level..];
    if rest.starts_with(' ') || rest.is_empty() {
        Some(level)
    } else {
        None
    }
}

fn strip_heading(line: &str) -> &str {
    line.trim_start_matches('#').trim_start()
}

/// Returns `(fence_char, count)` if `line` (after stripping leading whitespace) starts with
/// three or more backticks (`` ` ``) or tildes (`~`), otherwise `None`.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == ch).count();
    if count >= 3 {
        Some((ch, count))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_markdown tests ---

    #[test]
    fn single_heading() {
        let sections = parse_markdown("## Step 1");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 2);
        assert_eq!(sections[0].heading, "Step 1");
        assert_eq!(sections[0].body, "");
    }

    #[test]
    fn nested_headings() {
        let input = "# Top\n## Child";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].heading, "Top");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].heading, "Child");
    }

    #[test]
    fn heading_with_body() {
        let input = "## Step 1\nDo this first.\nAnd this.";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].body, "Do this first.\nAnd this.");
    }

    #[test]
    fn heading_with_no_body() {
        let sections = parse_markdown("## Step 1");
        assert_eq!(sections[0].body, "");
    }

    #[test]
    fn empty_input() {
        assert!(parse_markdown("").is_empty());
    }

    #[test]
    fn no_headings() {
        assert!(parse_markdown("just some text\nno headings here").is_empty());
    }

    #[test]
    fn empty_heading_skipped() {
        // "## " with nothing after — heading text is empty after trim
        let sections = parse_markdown("## \n## Real");
        // The empty one should be skipped
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Real");
    }

    #[test]
    fn empty_heading_as_last_line_skipped() {
        // Tests the final-flush code path with an empty heading
        let sections = parse_markdown("## Real\n## ");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Real");
    }

    #[test]
    fn text_before_first_heading_dropped() {
        let sections = parse_markdown("this text before\nno heading\n## First");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "First");
    }

    // --- plan_tasks tests ---

    #[test]
    fn single_section_spec() {
        let parent = Uuid::new_v4();
        let sections = parse_markdown("## Step 1");
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].parent, parent);
        assert_eq!(specs[0].description, "Step 1");
        assert_eq!(specs[0].annotation, None);
        assert!(!specs[0].position.is_empty());
    }

    #[test]
    fn two_sibling_sections() {
        let parent = Uuid::new_v4();
        let sections = parse_markdown("## Step 1\n## Step 2");
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs.len(), 2);
        assert!(specs[1].position > specs[0].position);
    }

    #[test]
    fn nested_sections_correct_parent() {
        let parent = Uuid::new_v4();
        let input = "## Phase 1\n### Task A";
        let sections = parse_markdown(input);
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs.len(), 2);
        // First spec is Phase 1, parent is `parent`
        assert_eq!(specs[0].parent, parent);
        assert_eq!(specs[0].description, "Phase 1");
        // Second spec is Task A, parent is Phase 1's UUID
        assert_eq!(specs[1].parent, specs[0].uuid);
        assert_eq!(specs[1].description, "Task A");
    }

    #[test]
    fn deep_nesting_three_levels() {
        let parent = Uuid::new_v4();
        let input = "## L1\n### L2\n#### L3";
        let sections = parse_markdown(input);
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].parent, parent);
        assert_eq!(specs[1].parent, specs[0].uuid);
        assert_eq!(specs[2].parent, specs[1].uuid);
    }

    #[test]
    fn sibling_positions_restart_per_level() {
        // Two groups of siblings at the same depth level but different parents.
        // Each group should start from the same base position.
        let parent = Uuid::new_v4();
        let input = "## A\n### A1\n### A2\n## B\n### B1\n### B2";
        let sections = parse_markdown(input);
        let specs = plan_tasks(parent, &sections);
        // specs: A, A1, A2, B, B1, B2
        assert_eq!(specs.len(), 6);
        let a1_pos = &specs[1].position;
        let b1_pos = &specs[4].position;
        // A1 and B1 are both first children in their respective groups,
        // so they should start from the same base position.
        assert_eq!(
            a1_pos, b1_pos,
            "first sibling positions should restart per level"
        );
        let a2_pos = &specs[2].position;
        let b2_pos = &specs[5].position;
        assert_eq!(
            a2_pos, b2_pos,
            "second sibling positions should restart per level"
        );
    }

    // --- fenced code block tests ---

    #[test]
    fn heading_inside_backtick_fence_not_parsed() {
        let input = "## Real\n```\n# Not a heading\n```\n## Also Real";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Real");
        assert_eq!(sections[1].heading, "Also Real");
    }

    #[test]
    fn heading_inside_tilde_fence_not_parsed() {
        let input = "## Real\n~~~\n# Not a heading\n~~~\n## Also Real";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Real");
        assert_eq!(sections[1].heading, "Also Real");
    }

    #[test]
    fn fence_with_lang_specifier() {
        let input = "## Step\n```rust\n# comment\nlet x = 1;\n```";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Step");
        // The fenced block lines appear in the body
        assert!(sections[0].body.contains("# comment"));
        assert!(sections[0].body.contains("let x = 1;"));
    }

    #[test]
    fn unclosed_fence_rest_is_body() {
        let input = "## Step\n```\n# still body\nmore body";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Step");
        assert!(sections[0].body.contains("# still body"));
        assert!(sections[0].body.contains("more body"));
    }

    #[test]
    fn multiple_code_blocks_between_headings() {
        let input = "## Step 1\n```\ncode\n```\nsome text\n```\nmore code\n```\n## Step 2";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Step 1");
        assert_eq!(sections[1].heading, "Step 2");
        let body = &sections[0].body;
        assert!(body.contains("code"));
        assert!(body.contains("some text"));
        assert!(body.contains("more code"));
    }

    #[test]
    fn derive_attribute_inside_fence_is_body() {
        // Regression: #[derive(Debug)] has no space after # so heading_level() already rejects
        // it outside fences — confirm it also works correctly inside a fence.
        let input = "## Step\n```rust\n#[derive(Debug)]\nstruct Foo;\n```";
        let sections = parse_markdown(input);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Step");
        assert!(sections[0].body.contains("#[derive(Debug)]"));
    }

    #[test]
    fn annotation_some_when_body_present() {
        let parent = Uuid::new_v4();
        let sections = parse_markdown("## Step 1\nSome body text.");
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs[0].annotation.as_deref(), Some("Some body text."));
    }

    #[test]
    fn annotation_none_when_empty_body() {
        let parent = Uuid::new_v4();
        let sections = parse_markdown("## Step 1");
        let specs = plan_tasks(parent, &sections);
        assert_eq!(specs[0].annotation, None);
    }

    #[test]
    fn empty_sections_returns_empty() {
        let parent = Uuid::new_v4();
        let specs = plan_tasks(parent, &[]);
        assert!(specs.is_empty());
    }
}
