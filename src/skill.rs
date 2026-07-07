//! Skill registry: named prompt templates the user can expand into the
//! input buffer with `/skill <name>` (or pick from the completion list
//! that appears while typing `/skill <partial>`).
//!
//! Skills are loaded from disk on each call — there are no built-ins.
//! The convention mirrors Anthropic's `~/.claude/skills/` layout:
//!
//! ```text
//! ~/.agents/skills/<name>/SKILL.md
//! ```
//!
//! Each `SKILL.md` is markdown with optional YAML frontmatter:
//!
//! ```text
//! ---
//! name: <id>
//! description: <one-line summary>
//! license: <optional>
//! ---
//!
//! <template body — markdown is preserved verbatim>
//! ```
//!
//! The body (everything after the closing `---`) becomes the skill
//! template; the user can edit it before sending. Pressing Tab on a
//! focused skill candidate in the completion list **directly fills**
//! the input with `/skill <name>`, matching the existing
//! `complete_focused_candidate` contract.

use std::path::{Path, PathBuf};

/// Hardcoded location of the skills directory. Resolved against the
/// user's home directory once per lookup; the path is cheap to
/// rebuild, so we don't cache it.
const SKILLS_DIR: &str = ".agents/skills";

const SKILL_FILE: &str = "SKILL.md";

#[derive(Debug, Clone)]
pub struct Skill {
    /// Skill id, sourced from frontmatter `name` or the directory name.
    pub name: String,
    /// One-line description from frontmatter; falls back to empty.
    pub description: String,
    /// Template body (markdown after the frontmatter).
    pub template: String,
}

/// Resolve the skills root directory under the user's home.
///
/// Returns `None` if `$HOME` is not set or the directory does not
/// exist — silently: the user just gets no skill completions.
fn skills_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let root = PathBuf::from(home).join(SKILLS_DIR);
    if root.is_dir() {
        Some(root)
    } else {
        None
    }
}

/// Resolve the on-disk path of a skill by name. Returns `None` when
/// the skills root does not exist or no matching directory is found.
/// The path is shown verbatim in the rendered `[skill]` block so
/// the user can locate / edit the file.
pub fn skill_path(name: &str) -> Option<PathBuf> {
    let root = skills_root()?;
    let needle = name.trim();
    if needle.is_empty() {
        return None;
    }
    let candidate = root.join(needle);
    if candidate.is_dir() {
        Some(candidate.join(SKILL_FILE))
    } else {
        None
    }
}

/// Read every `<name>/SKILL.md` under the skills root. Returns one
/// `Skill` per file, in directory-name order so completion lists are
/// deterministic. Missing / malformed files are skipped silently —
/// the picker shows what works, and a missing skills dir just yields
/// an empty list (the user still has `/skill` and the rest of the
/// slash commands).
pub fn load_all() -> Vec<Skill> {
    let Some(root) = skills_root() else {
        return Vec::new();
    };
    let read = match std::fs::read_dir(&root) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Skill> = read
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let file = path.join(SKILL_FILE);
            parse_skill_file(&name, &file)
        })
        .collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Parse a single `<name>/SKILL.md`. Tolerates a missing frontmatter
/// block by falling back to the directory name and the whole file as
/// the template.
fn parse_skill_file(dir_name: &str, path: &Path) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    Some(parse_skill_markdown(dir_name, &raw))
}

/// Split a `SKILL.md` body into (frontmatter fields, markdown body).
/// The frontmatter must be the very first line `---`, end with `---`,
/// and contain simple `key: value` lines. Anything else is treated as
/// the body, with `name` defaulting to the directory name and
/// `description` defaulting to empty.
fn parse_skill_markdown(dir_name: &str, raw: &str) -> Skill {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let mut name = dir_name.to_string();
    let mut description = String::new();
    let body;

    if let Some(rest) = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
    {
        if let Some(end) = find_frontmatter_end(rest) {
            let header = &rest[..end];
            for line in header.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim();
                    let v = v.trim().trim_matches('"');
                    match k {
                        "name" if !v.is_empty() => name = v.to_string(),
                        "description" if !v.is_empty() => description = v.to_string(),
                        _ => {}
                    }
                }
            }
            body = rest[end..]
                .trim_start_matches('-')
                .trim_start_matches('\r')
                .trim_start_matches('\n')
                .to_string();
        } else {
            body = trimmed.to_string();
        }
    } else {
        body = trimmed.to_string();
    }

    Skill {
        name,
        description,
        template: body,
    }
}

/// Find the byte offset of the closing `---` line in a frontmatter
/// body. Returns the offset of the line's leading newline, so
/// `&header[..end]` slices only the frontmatter lines.
fn find_frontmatter_end(s: &str) -> Option<usize> {
    let mut start = 0usize;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if start > 0 && trimmed == "---" {
            return Some(start);
        }
        start += line.len();
        if start > 4096 {
            // Frontmatter is unbounded in principle, but a 4 KiB cap
            // is plenty for a name/description header and protects
            // against pathological files masquerading as frontmatter.
            return None;
        }
    }
    None
}

/// Completion candidates for the `/skill:<name>` form. Performs
/// fuzzy subsequence matching (see [`crate::fuzzy`]) so partial
/// queries like `kpgy` still surface `karpathy-guidelines`. Empty
/// query returns every skill, alphabetically sorted.
///
/// Returned strings are the full slash form (`/skill:<name>`) so they
/// can be inserted verbatim by the existing
/// `complete_focused_candidate` logic, which already implements the
/// "directly fill the input from the focused selection" contract.
pub fn completion_candidates(query: &str) -> Vec<String> {
    let q = query.trim();
    let mut scored: Vec<(u32, String)> = load_all()
        .into_iter()
        .filter_map(|s| {
            crate::fuzzy::score(q, &s.name).map(|sc| (sc, format!("/skill:{}", s.name)))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, s)| s).collect()
}

/// List every available skill's name. Used by the `/skill` command
/// (no-arg form) to print the available names.
pub fn list_names() -> Vec<String> {
    load_all().into_iter().map(|s| s.name).collect()
}

/// Format the available skills as a Markdown list for inclusion in the
/// system prompt. Returns an empty string when no skills are installed.
pub fn skills_for_system_prompt() -> String {
    let skills = load_all();
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str("\n## Available Skills\n\n");
    out.push_str("The user can invoke skills via `/skill:<name>`. Available skills:\n\n");
    for skill in &skills {
        out.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
    }
    out
}

/// Look up a skill by name (case-insensitive). Returns `None` if no
/// matching `SKILL.md` is found under the skills root.
pub fn find(name: &str) -> Option<Skill> {
    let needle = name.trim().to_ascii_lowercase();
    load_all()
        .into_iter()
        .find(|s| s.name.to_ascii_lowercase() == needle)
}

/// Expand a skill into the input buffer. Returns the template body
/// with no transformation — frontmatter is already stripped by
/// `parse_skill_markdown`. Returns `None` when the skill is unknown.
pub fn expand_into(name: &str) -> Option<String> {
    find(name).map(|s| s.template)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(body: &str) -> Skill {
        parse_skill_markdown("demo", body)
    }

    #[test]
    fn frontmatter_name_and_description_override_dirname() {
        let s = fixture(
            "---\nname: actual-id\ndescription: hello world\nlicense: MIT\n---\n\nbody text",
        );
        assert_eq!(s.name, "actual-id");
        assert_eq!(s.description, "hello world");
        assert_eq!(s.template, "body text");
    }

    #[test]
    fn missing_frontmatter_uses_dirname_and_full_body() {
        let s = fixture("# Just a heading\n\nSome body.");
        assert_eq!(s.name, "demo");
        assert_eq!(s.description, "");
        assert!(s.template.contains("# Just a heading"));
    }

    #[test]
    fn unterminated_frontmatter_treated_as_body() {
        let s = fixture("---\nname: x\nno closing fence\n");
        assert_eq!(s.name, "demo");
        assert!(s.template.starts_with("---"));
    }

    #[test]
    fn frontmatter_with_crlf_terminators() {
        let s = fixture("---\r\nname: crlf-id\r\ndescription: ok\r\n---\r\nbody");
        assert_eq!(s.name, "crlf-id");
        assert_eq!(s.description, "ok");
        assert_eq!(s.template, "body");
    }

    #[test]
    fn completion_lists_from_real_skills_dir() {
        // The host machine's `~/.agents/skills` is the source of truth;
        // if the user has skills installed, the picker must surface
        // them. We only assert the *shape* of the candidates to stay
        // robust against an empty skills dir.
        let all = completion_candidates("");
        for c in &all {
            assert!(c.starts_with("/skill:"), "bad candidate: {c}");
        }
    }
}
