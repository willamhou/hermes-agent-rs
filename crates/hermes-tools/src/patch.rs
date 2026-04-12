use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::path_utils;

pub struct PatchTool;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternPart {
    Literal(String),
    Whitespace,
}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn toolset(&self) -> &str {
        "file"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Find and replace targeted text inside a file.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: path"));
        };
        let Some(old_string) = args.get("old_string").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: old_string"));
        };
        let Some(new_string) = args.get("new_string").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: new_string"));
        };
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string.is_empty() {
            return Ok(ToolResult::error("old_string cannot be empty"));
        }

        let resolved = path_utils::resolve_path(path_str, &ctx.working_dir);
        if let Err(e) = path_utils::check_sandbox(&resolved, &ctx.tool_config.workspace_root) {
            return Ok(ToolResult::error(e));
        }
        if path_utils::is_blocked_write_path(&resolved, &ctx.tool_config) {
            return Ok(ToolResult::error("write to this path is blocked"));
        }

        let content = match std::fs::read_to_string(&resolved) {
            Ok(content) => content,
            Err(e) => return Ok(ToolResult::error(format!("failed to read file: {e}"))),
        };

        let mut ranges = exact_matches(&content, old_string);
        if ranges.is_empty() {
            ranges = whitespace_aware_matches(&content, old_string);
        }

        if ranges.is_empty() {
            return Ok(ToolResult::error("old_string not found"));
        }
        if ranges.len() > 1 && !replace_all {
            return Ok(ToolResult::error("multiple matches, set replace_all=true"));
        }

        let applied = if replace_all { ranges } else { vec![ranges[0]] };
        let patched = apply_replacements(&content, &applied, new_string);

        if let Err(e) = atomic_write(&resolved, &patched) {
            return Ok(ToolResult::error(format!("failed to write file: {e}")));
        }

        Ok(ToolResult::ok(
            json!({
                "path": resolved.to_string_lossy(),
                "replacements": applied.len(),
            })
            .to_string(),
        ))
    }
}

fn exact_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    haystack
        .match_indices(needle)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect()
}

fn whitespace_aware_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let parts = tokenize_pattern(needle);
    if parts.is_empty() {
        return Vec::new();
    }

    let starts = candidate_starts(haystack, &parts[0]);
    let mut matches = Vec::new();

    for start in starts {
        if let Some(end) = match_from(haystack, start, &parts) {
            if matches.last().copied() != Some((start, end)) {
                matches.push((start, end));
            }
        }
    }

    matches
}

fn tokenize_pattern(pattern: &str) -> Vec<PatternPart> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut in_whitespace = false;

    for ch in pattern.chars() {
        if ch.is_ascii_whitespace() {
            if !literal.is_empty() {
                parts.push(PatternPart::Literal(std::mem::take(&mut literal)));
            }
            if !in_whitespace {
                parts.push(PatternPart::Whitespace);
                in_whitespace = true;
            }
        } else {
            literal.push(ch);
            in_whitespace = false;
        }
    }

    if !literal.is_empty() {
        parts.push(PatternPart::Literal(literal));
    }

    parts
}

fn candidate_starts(haystack: &str, first: &PatternPart) -> Vec<usize> {
    match first {
        PatternPart::Literal(literal) => haystack
            .match_indices(literal)
            .map(|(idx, _)| idx)
            .collect(),
        PatternPart::Whitespace => {
            let bytes = haystack.as_bytes();
            let mut starts = Vec::new();
            for idx in 0..bytes.len() {
                if bytes[idx].is_ascii_whitespace()
                    && (idx == 0 || !bytes[idx - 1].is_ascii_whitespace())
                {
                    starts.push(idx);
                }
            }
            starts
        }
    }
}

fn match_from(haystack: &str, start: usize, parts: &[PatternPart]) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut pos = start;

    for part in parts {
        match part {
            PatternPart::Literal(literal) => {
                if haystack.get(pos..)?.starts_with(literal) {
                    pos += literal.len();
                } else {
                    return None;
                }
            }
            PatternPart::Whitespace => {
                let mut consumed = false;
                while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                    consumed = true;
                    pos += 1;
                }
                if !consumed {
                    return None;
                }
            }
        }
    }

    Some(pos)
}

fn apply_replacements(content: &str, ranges: &[(usize, usize)], replacement: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;

    for (start, end) in ranges {
        out.push_str(&content[cursor..*start]);
        out.push_str(replacement);
        cursor = *end;
    }
    out.push_str(&content[cursor..]);
    out
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("patch-target");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = parent.join(format!(".{file_name}.hermes-tmp-{unique}"));

    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(PatchTool) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hermes_core::tool::ToolConfig;

    use super::*;

    fn make_ctx(working_dir: std::path::PathBuf) -> ToolContext {
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, _delta_rx) = tokio::sync::mpsc::channel(8);
        let config = ToolConfig {
            workspace_root: working_dir.clone(),
            ..ToolConfig::default()
        };
        ToolContext {
            session_id: "test-session".to_string(),
            working_dir,
            approval_tx,
            delta_tx,
            tool_config: Arc::new(config),
            memory: None,
            aux_provider: None,
            skills: None,
        }
    }

    #[test]
    fn whitespace_match_finds_concrete_ranges() {
        let matches = whitespace_aware_matches("foo(\n    bar)", "foo( bar)");
        assert_eq!(matches, vec![(0, 13)]);
    }

    #[tokio::test]
    async fn patch_exact_match() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "hello world").unwrap();

        let tool = PatchTool;
        let ctx = make_ctx(tmp.path().to_path_buf());
        let result = tool
            .execute(
                json!({
                    "path": "sample.txt",
                    "old_string": "world",
                    "new_string": "there"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello there");
    }
}
