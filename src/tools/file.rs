use super::*;

pub(super) async fn read_file(args: &str, cwd: &Path) -> Result<String> {
    let args: ReadArgs = serde_json::from_str(args)?;
    let path = resolve_workspace_path(cwd, &args.path)?;
    let text = tokio::fs::read_to_string(&path).await?;
    let selected = select_lines(&text, args.start_line, args.end_line)?;
    let trimmed = selected.trim_end().to_string();
    if trimmed.len() > READ_OUTPUT_LIMIT {
        Ok(truncate_output_str(&trimmed))
    } else {
        Ok(trimmed)
    }
}

pub(super) async fn write_file(args: &str, cwd: &Path) -> Result<String> {
    let args: WriteArgs = serde_json::from_str(args)?;
    let path = resolve_workspace_path(cwd, &args.path)?;
    if let Some(old_string) = &args.old_string {
        if old_string.is_empty() {
            return Err(anyhow!("oldString must not be empty"));
        }
        let original = tokio::fs::read_to_string(&path).await?;
        // When oldString is provided, a missing/null content means
        // "delete the matched text" — treat it as an empty string
        // instead of erroring.
        let content = args.content.as_deref().unwrap_or("");
        let updated = replace_string(
            &original,
            old_string,
            content,
            args.replace_all.unwrap_or(false),
            args.start_line,
            args.end_line,
        )?;
        tokio::fs::write(&path, &updated).await?;
        Ok(write_diff_result(
            &args.path,
            &original,
            &updated,
            "Edit applied successfully.",
        ))
    } else {
        let content = args
            .content
            .as_ref()
            .ok_or_else(|| anyhow!("content is required when oldString is omitted"))?;
        let original = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, content).await?;
        Ok(write_diff_result(
            &args.path,
            &original,
            content,
            "Wrote file successfully.",
        ))
    }
}

pub(super) async fn write_new_file(args: &str, cwd: &Path) -> Result<String> {
    let args: WriteNewArgs = serde_json::from_str(args)?;
    if args.path.trim().is_empty() {
        return Err(anyhow!("path is empty"));
    }
    let path = resolve_workspace_path(cwd, &args.path)?;
    let original = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &args.content).await?;
    Ok(write_diff_result(
        &args.path,
        &original,
        &args.content,
        "Wrote file successfully.",
    ))
}

fn write_diff_result(path: &str, old: &str, new: &str, ai_output: &str) -> String {
    json!({
        "kind": "edit_diff",
        "path": path,
        "old": old,
        "new": new,
        "output": ai_output,
    })
    .to_string()
}

/// Split a tool's raw result value into `(ai_output, metadata)`.
///
/// For `edit`/`write` results (an `edit_diff` JSON carrying an
/// `output` field), returns `(output, full edit_diff JSON)` so the
/// AI only sees the short success message while the diff is preserved
/// as UI-only metadata. For every other tool, returns `(value, "")`.
pub(super) fn split_edit_diff(name: &str, value: &str) -> (String, String) {
    if name != "edit" {
        return (value.to_string(), String::new());
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(value) else {
        return (value.to_string(), String::new());
    };
    if v.get("kind").and_then(|k| k.as_str()) != Some("edit_diff") {
        return (value.to_string(), String::new());
    }
    let output = v
        .get("output")
        .and_then(|o| o.as_str())
        .unwrap_or("Edit applied successfully.")
        .to_string();
    (output, value.to_string())
}

/// Extract the `metadata` field from a `{"ok":true,"result":...,"metadata":...}`
/// envelope produced by `execute_tool_with_agent`. Returns an empty
/// string when absent (non-edit tools).
pub fn extract_metadata(envelope: &str) -> String {
    serde_json::from_str::<serde_json::Value>(envelope)
        .ok()
        .and_then(|v| {
            v.get("metadata")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Re-serialize an `{"ok":...,"result":...,"metadata":...}` envelope
/// with the `metadata` field removed, so UI-only payload (file diffs)
/// never reaches the AI context. Non-JSON values are returned unchanged.
pub fn strip_metadata(envelope: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(envelope) else {
        return envelope.to_string();
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("metadata");
    }
    serde_json::to_string(&v).unwrap_or_else(|_| envelope.to_string())
}

pub(super) async fn grep_text(args: &str, cwd: &Path) -> Result<String> {
    let args: GrepArgs = serde_json::from_str(args)?;
    if args.pattern.is_empty() {
        return Err(anyhow!("pattern is empty"));
    }
    let re = Regex::new(&args.pattern).map_err(|e| anyhow!("invalid regex pattern: {e}"))?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_workspace_path(cwd, &rel)?;
    let glob_re = match args.glob.as_deref() {
        Some(g) if !g.trim().is_empty() => {
            Some(glob::Pattern::new(g).map_err(|e| anyhow!("invalid glob pattern: {e}"))?)
        }
        _ => None,
    };
    let mut out = Vec::new();
    grep_path(&root, &re, cwd, &mut out, 200, &glob_re)?;
    if out.is_empty() {
        Ok(format!("no matches for {:?} in {}", args.pattern, rel))
    } else {
        Ok(truncate(out.join("\n"), READ_OUTPUT_LIMIT))
    }
}

fn grep_path(
    path: &Path,
    re: &Regex,
    cwd: &Path,
    out: &mut Vec<String>,
    limit: usize,
    glob: &Option<glob::Pattern>,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if should_skip_dir(&name) {
                continue;
            }
            grep_path(&p, re, cwd, out, limit, glob)?;
            if out.len() >= limit {
                break;
            }
        }
    } else if path.is_file() {
        let rel = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        if let Some(g) = glob {
            let rel_path = Path::new(&rel);
            if !g.matches_path(rel_path) {
                return Ok(());
            }
        }
        if let Ok(text) = std::fs::read_to_string(path) {
            for (idx, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    out.push(format!("{}:{}:{}", rel, idx + 1, line));
                    if out.len() >= limit {
                        out.push("[match limit reached]".to_string());
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn list_path(args: &str, cwd: &Path) -> Result<String> {
    let args: ListArgs = serde_json::from_str(args)?;
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let path = resolve_workspace_path(cwd, &rel)?;
    if !path.is_dir() {
        return Err(anyhow!("path is not a directory"));
    }
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mut name = entry.file_name().to_string_lossy().to_string();
        if meta.is_dir() {
            name.push('/');
        }
        rows.push(name);
    }
    rows.sort();
    Ok(rows.join("\n"))
}

pub(super) async fn plan_review(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Plan");
    let content = value
        .get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            value
                .get("content")
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
        })
        .unwrap_or_else(|| {
            value
                .get("steps")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .enumerate()
                        .map(|(i, s)| {
                            // Strip existing "<N>. " prefix if present
                            // so auto-numbering doesn't create nested
                            // lists (e.g. "1. 1. Add X" → outer list
                            // marker + sub-list marker in markdown).
                            let step = match s.find(". ") {
                                Some(pos)
                                    if pos > 0 && s[..pos].chars().all(|c| c.is_ascii_digit()) =>
                                {
                                    s[pos + 2..].trim_start()
                                }
                                _ => s,
                            };
                            format!("{}. {}", i + 1, step)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        });
    if content.trim().is_empty() {
        return Err(anyhow!("plan content or steps must be non-empty. Provide 'content' (a string describing the plan) or 'steps' (an array of step strings)."));
    }
    Ok(json!({
        "kind": "plan",
        "title": title,
        "content": content,
        "status": "pending",
        "instruction": "Do not call this tool again. The plan is now shown to the user in the function panel. Stop and wait for the user to approve, reject, or request changes. The user will submit their decision and the conversation will resume automatically -- you will be re-prompted with the user's response."
    }).to_string())
}

pub(super) async fn ask_question(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let question = value
        .get("question")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if question.is_empty() {
        return Err(anyhow!("question is empty"));
    }
    let options: Vec<String> = value
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "kind": "ask",
        "question": question,
        "options": options,
        "status": "pending",
        "instruction": "Do not call this tool again. The question is now shown to the user in the session. Stop and wait for the user to type their answer into the main input. Their reply will be sent back to you automatically -- you will be re-prompted with the user's response."
    })
    .to_string())
}

pub(super) async fn todowrite(args: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(args)?;
    let todos = value
        .get("todos")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("todowrite: missing or invalid `todos` array"))?;
    if todos.is_empty() {
        return Ok(json!({
            "kind": "todowrite",
            "action": "clear",
            "todos": [],
            "status": "ok",
            "summary": "Todo list cleared."
        })
        .to_string());
    }
    let mut validated = Vec::new();
    for (i, item) in todos.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("todowrite: todos[{}] missing or empty `content`", i))?;
        let status = item
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending");
        let status = match status {
            "pending" | "in_progress" | "completed" => status,
            _ => return Err(anyhow!(
                "todowrite: todos[{}] invalid status `{status}` (must be pending, in_progress, or completed)", i
            )),
        };
        validated.push(json!({
            "content": content,
            "status": status,
        }));
    }
    let pending = validated
        .iter()
        .filter(|v| v["status"] == "pending")
        .count();
    let in_progress = validated
        .iter()
        .filter(|v| v["status"] == "in_progress")
        .count();
    let completed = validated
        .iter()
        .filter(|v| v["status"] == "completed")
        .count();
    Ok(json!({
        "kind": "todowrite",
        "action": "replace",
        "todos": validated,
        "status": "ok",
        "summary": format!("{} pending, {} in progress, {} completed", pending, in_progress, completed),
    })
    .to_string())
}

pub(super) async fn glob_search(args: &str, cwd: &Path) -> Result<String> {
    let args: GlobArgs = serde_json::from_str(args)?;
    if args.pattern.trim().is_empty() {
        return Err(anyhow!("pattern is empty"));
    }
    let rel = args.path.unwrap_or_else(|| ".".to_string());
    let root = resolve_workspace_path(cwd, &rel)?;
    if !root.is_dir() {
        return Err(anyhow!("glob path must be a directory: {}", rel));
    }
    let pattern = Pattern::new(&args.pattern).map_err(|e| anyhow!("invalid glob pattern: {e}"))?;
    let mut matches: Vec<(String, std::time::SystemTime)> = Vec::new();
    collect_glob_matches(&root, &root, &pattern, &mut matches, 100)?;
    matches.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = matches.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
    if out.is_empty() {
        return Ok("No files found".to_string());
    }
    if out.len() >= 100 {
        out.push("[results truncated at 100 — narrow your search]".to_string());
    }
    Ok(out.join("\n"))
}

fn collect_glob_matches(
    search_root: &Path,
    current: &Path,
    pattern: &Pattern,
    out: &mut Vec<(String, std::time::SystemTime)>,
    limit: usize,
) -> Result<()> {
    if out.len() >= limit {
        return Ok(());
    }
    if current.is_dir() {
        let dir = std::fs::read_dir(current)?;
        for entry in dir {
            let entry = entry?;
            let p = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if should_skip_dir(&name) {
                continue;
            }
            let rel = p
                .strip_prefix(search_root)
                .unwrap_or(&p)
                .display()
                .to_string();
            let rel_path = Path::new(&rel);
            if p.is_dir() {
                collect_glob_matches(search_root, &p, pattern, out, limit)?;
            } else if pattern.matches_path(rel_path) {
                if let Ok(meta) = p.metadata() {
                    out.push((rel, meta.modified().unwrap_or(std::time::UNIX_EPOCH)));
                } else {
                    out.push((rel, std::time::UNIX_EPOCH));
                }
                if out.len() >= limit {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn skill_load(args: &str) -> Result<String> {
    let args: SkillArgs = serde_json::from_str(args)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name is empty"));
    }
    let Some(skill) = crate::skill::find(name) else {
        return Err(anyhow!(
            "skill not found: `{name}`. Available skills: {}",
            crate::skill::list_names().join(", ")
        ));
    };
    let skill_dir =
        crate::skill::skill_path(name).and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let mut file_list = Vec::new();
    if let Some(ref dir) = skill_dir {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter(|e| e.file_name().to_string_lossy() != "SKILL.md")
                .map(|e| e.path().display().to_string())
                .take(10)
                .collect();
            files.sort();
            file_list = files;
        }
    }
    let base_dir = skill_dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());
    let mut out = format!(
        "<skill_content name=\"{name}\">\n# Skill: {name}\n\n{content}\n\nBase directory for this skill: {base}\nRelative paths in this skill are relative to this base directory.\nNote: file list is sampled.\n",
        name = skill.name,
        content = skill.template,
        base = base_dir,
    );
    if !file_list.is_empty() {
        out.push_str("<skill_files>\n");
        for f in &file_list {
            out.push_str(&format!("<file>{f}</file>\n"));
        }
        out.push_str("</skill_files>\n");
    }
    out.push_str("</skill_content>");
    Ok(out)
}
