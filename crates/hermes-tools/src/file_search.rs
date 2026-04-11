use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use walkdir::WalkDir;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::path_utils;

pub struct SearchFilesTool;

/// Convert a simple glob pattern to a regex string.
/// Supports: `*` (any chars except `/`), `**` (any chars including `/`), `?` (one char), `[...]`
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("(?i)^");
    let chars: Vec<char> = glob.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                re.push_str(".*");
                i += 2;
            }
            '*' => {
                re.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                re.push('.');
                i += 1;
            }
            '.' | '+' | '(' | ')' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                re.push('\\');
                re.push(chars[i]);
                i += 1;
            }
            c => {
                re.push(c);
                i += 1;
            }
        }
    }
    re.push('$');
    re
}

/// Check if a filename matches a glob pattern.
fn filename_matches_glob(filename: &str, glob: &str) -> bool {
    let re_str = glob_to_regex(glob);
    Regex::new(&re_str)
        .map(|re| re.is_match(filename))
        .unwrap_or(false)
}

/// Return true if the file is likely binary (check first 8KB for null bytes).
fn is_likely_binary(path: &std::path::Path) -> bool {
    if path_utils::has_binary_extension(path) {
        return true;
    }
    // Heuristic: sample first 8KB for null bytes
    if let Ok(bytes) = std::fs::read(path) {
        let sample = &bytes[..bytes.len().min(8192)];
        return sample.contains(&0u8);
    }
    false
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn toolset(&self) -> &str {
        "file"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search_files".to_string(),
            description: "Search file contents with regex or find files by filename glob pattern."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern for content search, or glob pattern for file search"
                    },
                    "target": {
                        "type": "string",
                        "enum": ["content", "files"],
                        "default": "content",
                        "description": "Search target: 'content' searches inside files, 'files' matches filenames"
                    },
                    "path": {
                        "type": "string",
                        "default": ".",
                        "description": "Root directory to search in"
                    },
                    "file_glob": {
                        "type": "string",
                        "description": "Optional glob pattern to filter files by name (e.g. '*.rs')"
                    },
                    "limit": {
                        "type": "integer",
                        "default": 50,
                        "description": "Maximum number of matches to return"
                    },
                    "context": {
                        "type": "integer",
                        "default": 0,
                        "description": "Number of context lines to show before and after each match"
                    }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Ok(ToolResult::error("missing required parameter: pattern")),
        };

        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("content")
            .to_string();

        let search_path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let file_glob = args
            .get("file_glob")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

        let context_lines = args.get("context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let search_root = path_utils::resolve_path(search_path_str, &ctx.working_dir);

        match target.as_str() {
            "files" => search_files_mode(&pattern, &search_root, limit),
            _ => search_content_mode(
                &pattern,
                &search_root,
                file_glob.as_deref(),
                limit,
                context_lines,
            ),
        }
    }
}

fn search_content_mode(
    pattern: &str,
    search_root: &std::path::Path,
    file_glob: Option<&str>,
    limit: usize,
    context_lines: usize,
) -> Result<ToolResult> {
    let re = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return Ok(ToolResult::error(format!("invalid regex pattern: {e}"))),
    };

    let mut matches = Vec::new();
    let mut total_matches = 0usize;

    'outer: for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden directories (starting with '.')
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if name.starts_with('.') && name != "." {
                    return false;
                }
            }
            true
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let file_path = entry.path();

        // Apply file_glob filter if provided
        if let Some(glob) = file_glob {
            let filename = file_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !filename_matches_glob(&filename, glob) {
                continue;
            }
        }

        // Skip binary files
        if is_likely_binary(file_path) {
            continue;
        }

        // Read content
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();

        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                total_matches += 1;

                if matches.len() < limit {
                    // Build context
                    let ctx_start = idx.saturating_sub(context_lines);
                    let ctx_end = (idx + context_lines + 1).min(lines.len());

                    let match_content = if context_lines == 0 {
                        line.to_string()
                    } else {
                        lines[ctx_start..ctx_end]
                            .iter()
                            .enumerate()
                            .map(|(i, l)| format!("{}:{}", ctx_start + i + 1, l))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };

                    matches.push(json!({
                        "path": file_path.to_string_lossy(),
                        "line": idx + 1,
                        "content": match_content
                    }));
                }

                if total_matches > limit {
                    break 'outer;
                }
            }
        }
    }

    let truncated = total_matches > limit;
    let result = json!({
        "matches": matches,
        "total_matches": total_matches,
        "truncated": truncated
    });

    Ok(ToolResult::ok(result.to_string()))
}

fn search_files_mode(
    pattern: &str,
    search_root: &std::path::Path,
    limit: usize,
) -> Result<ToolResult> {
    let mut found = Vec::new();

    for entry in WalkDir::new(search_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if name.starts_with('.') && name != "." {
                    return false;
                }
            }
            true
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let filename = entry.file_name().to_string_lossy().into_owned();

        if filename_matches_glob(&filename, pattern) {
            found.push(entry.path().to_string_lossy().into_owned());
            if found.len() >= limit {
                break;
            }
        }
    }

    let result = json!({
        "files": found,
        "total": found.len()
    });

    Ok(ToolResult::ok(result.to_string()))
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(SearchFilesTool) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool::ToolConfig;
    use std::io::Write;
    use std::sync::Arc;

    fn make_ctx(working_dir: std::path::PathBuf) -> ToolContext {
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, _delta_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            session_id: "test-session".to_string(),
            working_dir,
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
        }
    }

    fn setup_tmp(suffix: &str) -> std::path::PathBuf {
        let tmp =
            std::env::temp_dir().join(format!("hermes_search_{}_{}", suffix, std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    fn write_file(dir: &std::path::Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[tokio::test]
    async fn test_search_content_match() {
        let tmp = setup_tmp("content");
        write_file(&tmp, "a.txt", "hello world\nfoo bar\nbaz qux\n");
        write_file(&tmp, "b.txt", "another hello line\nnope\n");

        let ctx = make_ctx(tmp.clone());
        let tool = SearchFilesTool;
        let args = serde_json::json!({
            "pattern": "hello",
            "target": "content",
            "path": tmp.to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert!(
            matches.len() >= 2,
            "expected >=2 matches, got {}",
            matches.len()
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_search_content_with_glob() {
        let tmp = setup_tmp("glob");
        write_file(&tmp, "a.txt", "find me\n");
        write_file(&tmp, "b.rs", "find me\n");
        write_file(&tmp, "c.txt", "find me\n");

        let ctx = make_ctx(tmp.clone());
        let tool = SearchFilesTool;
        let args = serde_json::json!({
            "pattern": "find me",
            "target": "content",
            "path": tmp.to_str().unwrap(),
            "file_glob": "*.txt"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        // Only .txt files should match
        for m in matches {
            let path = m["path"].as_str().unwrap();
            assert!(path.ends_with(".txt"), "non-txt file matched: {path}");
        }
        assert_eq!(matches.len(), 2, "expected 2 .txt matches");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_search_content_limit() {
        let tmp = setup_tmp("limit");
        // Write a file with 20 matching lines
        let content: String = (1..=20).map(|i| format!("match line {i}\n")).collect();
        write_file(&tmp, "many.txt", &content);

        let ctx = make_ctx(tmp.clone());
        let tool = SearchFilesTool;
        let args = serde_json::json!({
            "pattern": "match line",
            "target": "content",
            "path": tmp.to_str().unwrap(),
            "limit": 5
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert!(matches.len() <= 5, "should be limited to 5");
        assert_eq!(parsed["truncated"], true, "should be truncated");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_search_files_mode() {
        let tmp = setup_tmp("files");
        write_file(&tmp, "main.rs", "fn main() {}");
        write_file(&tmp, "lib.rs", "// lib");
        write_file(&tmp, "config.toml", "[package]");
        write_file(&tmp, "readme.md", "# readme");

        let ctx = make_ctx(tmp.clone());
        let tool = SearchFilesTool;
        let args = serde_json::json!({
            "pattern": "*.rs",
            "target": "files",
            "path": tmp.to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "expected 2 .rs files, got {}", files.len());
        for f in files {
            assert!(f.as_str().unwrap().ends_with(".rs"));
        }

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let tmp = setup_tmp("noresult");
        write_file(&tmp, "a.txt", "nothing interesting here\n");

        let ctx = make_ctx(tmp.clone());
        let tool = SearchFilesTool;
        let args = serde_json::json!({
            "pattern": "ZZZZZZZUNLIKELY_MATCH_STRING",
            "target": "content",
            "path": tmp.to_str().unwrap()
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 0);
        assert_eq!(parsed["total_matches"], 0);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
