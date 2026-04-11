use std::path::{Path, PathBuf};

use hermes_core::error::{HermesError, Result};

const MEMORY_MAX_CHARS: usize = 2200;
const USER_MAX_CHARS: usize = 1375;
pub const ENTRY_SEPARATOR: &str = "\n§\n";

/// Truncate content to `max_chars` by dropping oldest entries (from the start).
/// Entries are split by `ENTRY_SEPARATOR`. Keeps entries from the END (newest).
pub fn truncate_entries(content: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }

    // Split into entries
    let entries: Vec<&str> = content.split('§').collect();

    // Walk from the end, accumulating entries until we would exceed max_chars
    let mut kept: Vec<&str> = Vec::new();
    let mut total = 0usize;
    // We need to count the separators too: between N entries there are N-1 separators
    // Separator is "\n§\n" — length 3
    const SEP_LEN: usize = 3;

    for entry in entries.iter().rev() {
        let entry_len = entry.len();
        let added_sep = if kept.is_empty() { 0 } else { SEP_LEN };
        if total + entry_len + added_sep <= max_chars {
            total += entry_len + added_sep;
            kept.push(entry);
        } else {
            break;
        }
    }

    kept.reverse();
    kept.join("§")
}

/// File-backed memory with a frozen snapshot pattern.
///
/// On [`load`], file contents are read into snapshots. [`system_prompt_block`]
/// returns the snapshot (not live). [`write`] updates disk only.
/// Call [`refresh_snapshot`] to bring the snapshot up to date.
#[derive(Clone)]
pub struct BuiltinMemory {
    dir: PathBuf,
    memory_snapshot: String,
    user_snapshot: String,
}

impl BuiltinMemory {
    /// Load from `dir`. Creates the directory if it does not exist.
    pub fn load(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| HermesError::Memory(format!("failed to create memory dir: {e}")))?;

        let memory_snapshot = Self::read_file(&dir, "MEMORY.md")?;
        let user_snapshot = Self::read_file(&dir, "USER.md")?;

        Ok(Self {
            dir,
            memory_snapshot,
            user_snapshot,
        })
    }

    fn read_file(dir: &Path, filename: &str) -> Result<String> {
        let path = dir.join(filename);
        if path.exists() {
            std::fs::read_to_string(&path)
                .map_err(|e| HermesError::Memory(format!("failed to read {filename}: {e}")))
        } else {
            Ok(String::new())
        }
    }

    /// Returns a formatted block containing both snapshots, or `None` if both are empty.
    pub fn system_prompt_block(&self) -> Option<String> {
        if self.memory_snapshot.is_empty() && self.user_snapshot.is_empty() {
            return None;
        }
        Some(format!(
            "## Notes\n{}\n\n## User Profile\n{}",
            self.memory_snapshot, self.user_snapshot
        ))
    }

    /// Read the current on-disk value for `key` (bypasses snapshot).
    pub fn read_live(&self, key: &str) -> Result<Option<String>> {
        let filename = self.key_to_filename(key)?;
        let path = self.dir.join(filename);
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| HermesError::Memory(format!("failed to read {key}: {e}")))?;
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    /// Write `content` to disk for `key`. Snapshot is NOT updated.
    /// Content is truncated if over the per-key limit.
    pub fn write(&self, key: &str, content: &str) -> Result<()> {
        let filename = self.key_to_filename(key)?;
        let max = match key {
            "MEMORY" => MEMORY_MAX_CHARS,
            "USER" => USER_MAX_CHARS,
            _ => unreachable!("key_to_filename already validated"),
        };
        let truncated = truncate_entries(content, max);
        let path = self.dir.join(filename);
        std::fs::write(&path, truncated)
            .map_err(|e| HermesError::Memory(format!("failed to write {key}: {e}")))
    }

    /// Re-read files from disk into snapshot fields.
    pub fn refresh_snapshot(&mut self) -> Result<()> {
        self.memory_snapshot = Self::read_file(&self.dir, "MEMORY.md")?;
        self.user_snapshot = Self::read_file(&self.dir, "USER.md")?;
        Ok(())
    }

    fn key_to_filename<'a>(&self, key: &'a str) -> Result<&'a str> {
        match key {
            "MEMORY" => Ok("MEMORY.md"),
            "USER" => Ok("USER.md"),
            other => Err(HermesError::Memory(format!("unknown memory key: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_load_empty_dir() {
        let dir = tmp();
        let mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        assert!(mem.memory_snapshot.is_empty());
        assert!(mem.user_snapshot.is_empty());
    }

    #[test]
    fn test_write_and_read_live() {
        let dir = tmp();
        let mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        mem.write("MEMORY", "hello world").unwrap();
        let live = mem.read_live("MEMORY").unwrap();
        assert_eq!(live, Some("hello world".to_string()));
    }

    #[test]
    fn test_snapshot_frozen_after_write() {
        let dir = tmp();
        let mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        // snapshot is empty at load
        assert!(mem.system_prompt_block().is_none());
        // write to disk
        mem.write("MEMORY", "new content").unwrap();
        // snapshot should still be empty (frozen)
        assert!(mem.system_prompt_block().is_none());
    }

    #[test]
    fn test_refresh_snapshot() {
        let dir = tmp();
        let mut mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        mem.write("MEMORY", "refreshed content").unwrap();
        // before refresh, snapshot is stale
        assert!(mem.system_prompt_block().is_none());
        // after refresh, snapshot is updated
        mem.refresh_snapshot().unwrap();
        let block = mem.system_prompt_block().unwrap();
        assert!(block.contains("refreshed content"));
    }

    #[test]
    fn test_system_prompt_block_empty() {
        let dir = tmp();
        let mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        assert!(mem.system_prompt_block().is_none());
    }

    #[test]
    fn test_system_prompt_block_with_content() {
        let dir = tmp();
        // write files BEFORE load so they appear in snapshot
        std::fs::write(dir.path().join("MEMORY.md"), "some notes").unwrap();
        std::fs::write(dir.path().join("USER.md"), "user profile").unwrap();
        let mem = BuiltinMemory::load(dir.path().to_path_buf()).unwrap();
        let block = mem.system_prompt_block().unwrap();
        assert!(block.contains("## Notes"));
        assert!(block.contains("some notes"));
        assert!(block.contains("## User Profile"));
        assert!(block.contains("user profile"));
    }

    #[test]
    fn test_truncate_entries_under_limit() {
        let content = "short content";
        assert_eq!(truncate_entries(content, 1000), content);
    }

    #[test]
    fn test_truncate_entries_over_limit() {
        // Build many entries that together exceed the limit
        let entries: Vec<String> = (0..20).map(|i| format!("entry_{i:02}")).collect();
        let content = entries.join("§");
        // max_chars that fits only the last few entries
        let max = 50usize;
        let result = truncate_entries(&content, max);
        // Oldest entries should be dropped
        assert!(
            !result.contains("entry_00"),
            "oldest entry should be dropped"
        );
        // Newest entries should be kept
        assert!(result.contains("entry_19"), "newest entry should be kept");
        assert!(result.len() <= max);
    }
}
