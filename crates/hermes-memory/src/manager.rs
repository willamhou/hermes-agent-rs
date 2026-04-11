use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use hermes_core::error::Result;
use hermes_core::memory::MemoryProvider;
use hermes_core::message::Message;

use crate::builtin::BuiltinMemory;

/// Orchestrates [`BuiltinMemory`] and an optional external [`MemoryProvider`].
pub struct MemoryManager {
    builtin: BuiltinMemory,
    external: Option<Arc<dyn MemoryProvider>>,
    prefetch_cache: Arc<Mutex<HashMap<String, String>>>,
}

impl MemoryManager {
    /// Create a new [`MemoryManager`], loading builtin memory from `memory_dir`.
    pub fn new(memory_dir: PathBuf, external: Option<Arc<dyn MemoryProvider>>) -> Result<Self> {
        let builtin = BuiltinMemory::load(memory_dir)?;
        Ok(Self {
            builtin,
            external,
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Aggregate system prompt blocks from builtin and external, wrapped in
    /// `<memory-context>` tags. Returns empty string if there are no blocks.
    pub fn system_prompt_blocks(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(block) = self.builtin.system_prompt_block() {
            parts.push(block);
        }
        if let Some(ext) = &self.external {
            if let Some(block) = ext.system_prompt_block() {
                parts.push(block);
            }
        }

        if parts.is_empty() {
            return String::new();
        }

        format!(
            "<memory-context>\n{}\n</memory-context>",
            parts.join("\n\n")
        )
    }

    /// Spawn a background prefetch from the external provider and cache the result.
    pub fn queue_prefetch(&self, hint: &str, session_id: &str) {
        if let Some(ext) = self.external.clone() {
            let cache = Arc::clone(&self.prefetch_cache);
            let hint = hint.to_string();
            let sid = session_id.to_string();
            tokio::spawn(async move {
                match ext.prefetch(&hint, &sid).await {
                    Ok(data) => {
                        let mut guard = cache.lock().await;
                        guard.insert(sid, data);
                    }
                    Err(e) => {
                        tracing::warn!("prefetch failed: {e}");
                    }
                }
            });
        }
    }

    /// Take a prefetched value for `session_id` from cache.
    /// Falls back to a synchronous prefetch if external exists but cache misses.
    pub async fn take_prefetched(&self, session_id: &str) -> Option<String> {
        // Lock scope: drop the guard before any await
        let cached = {
            let mut guard = self.prefetch_cache.lock().await;
            guard.remove(session_id)
        };

        if cached.is_some() {
            return cached;
        }

        // Fallback: sync prefetch
        if let Some(ext) = &self.external {
            match ext.prefetch("", session_id).await {
                Ok(data) if !data.is_empty() => return Some(data),
                _ => {}
            }
        }

        None
    }

    /// Fire-and-forget: sync the turn to the external provider.
    pub fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) {
        if let Some(ext) = self.external.clone() {
            let user = user.to_string();
            let assistant = assistant.to_string();
            let sid = session_id.to_string();
            tokio::spawn(async move {
                if let Err(e) = ext.sync_turn(&user, &assistant, &sid).await {
                    tracing::warn!("sync_turn failed: {e}");
                }
            });
        }
    }

    /// Delegate to the external provider's `on_pre_compress`.
    pub async fn on_pre_compress(&self, messages: &[Message]) -> Option<String> {
        if let Some(ext) = &self.external {
            match ext.on_pre_compress(messages).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!("on_pre_compress failed: {e}");
                    None
                }
            }
        } else {
            None
        }
    }

    /// Re-read files into the builtin snapshot.
    pub fn refresh_snapshot(&mut self) -> Result<()> {
        self.builtin.refresh_snapshot()
    }

    /// Borrow the underlying [`BuiltinMemory`].
    pub fn builtin(&self) -> &BuiltinMemory {
        &self.builtin
    }

    /// Create a child manager: clones builtin state, no external, fresh cache.
    pub fn new_child(&self) -> Result<Self> {
        Ok(Self {
            builtin: self.builtin.clone(),
            external: None,
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hermes_core::error::Result as HermesResult;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    struct MockMemoryProvider;

    #[async_trait]
    impl MemoryProvider for MockMemoryProvider {
        fn system_prompt_block(&self) -> Option<String> {
            Some("external block".into())
        }

        async fn prefetch(&self, _q: &str, _sid: &str) -> HermesResult<String> {
            Ok("prefetched data".into())
        }
    }

    #[test]
    fn test_system_prompt_blocks_empty() {
        let dir = tmp();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), None).unwrap();
        assert_eq!(mgr.system_prompt_blocks(), "");
    }

    #[test]
    fn test_system_prompt_blocks_with_content() {
        let dir = tmp();
        std::fs::write(dir.path().join("MEMORY.md"), "some notes").unwrap();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), None).unwrap();
        let blocks = mgr.system_prompt_blocks();
        assert!(blocks.contains("<memory-context>"), "missing opening tag");
        assert!(blocks.contains("</memory-context>"), "missing closing tag");
        assert!(blocks.contains("some notes"));
    }

    #[test]
    fn test_new_child_isolation() {
        let dir = tmp();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), Some(Arc::new(MockMemoryProvider)))
            .unwrap();
        let child = mgr.new_child().unwrap();
        // child has no external
        assert!(child.external.is_none());
    }

    #[tokio::test]
    async fn test_take_prefetched_cache_miss() {
        let dir = tmp();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), None).unwrap();
        let result = mgr.take_prefetched("no-session").await;
        assert!(result.is_none());
    }

    #[test]
    fn test_manager_new_creates_dir() {
        let dir = tmp();
        let new_subdir = dir.path().join("memory_subdir");
        let mgr = MemoryManager::new(new_subdir.clone(), None);
        assert!(mgr.is_ok(), "should create dir and succeed");
        assert!(new_subdir.exists(), "directory should have been created");
    }

    #[tokio::test]
    async fn test_refresh_snapshot_updates() {
        let dir = tmp();
        let mut mgr = MemoryManager::new(dir.path().to_path_buf(), None).unwrap();
        // initially empty
        assert_eq!(mgr.system_prompt_blocks(), "");
        // write to disk directly
        mgr.builtin().write("MEMORY", "updated notes").unwrap();
        // blocks still empty (snapshot frozen)
        assert_eq!(mgr.system_prompt_blocks(), "");
        // refresh and check
        mgr.refresh_snapshot().unwrap();
        let blocks = mgr.system_prompt_blocks();
        assert!(blocks.contains("updated notes"));
    }

    #[tokio::test]
    async fn test_queue_prefetch_and_take() {
        let dir = tmp();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), Some(Arc::new(MockMemoryProvider)))
            .unwrap();

        mgr.queue_prefetch("some hint", "session-abc");

        // Give the background task time to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let result = mgr.take_prefetched("session-abc").await;
        assert_eq!(result, Some("prefetched data".to_string()));
    }

    #[tokio::test]
    async fn test_sync_turn_no_panic() {
        let dir = tmp();
        let mgr = MemoryManager::new(dir.path().to_path_buf(), Some(Arc::new(MockMemoryProvider)))
            .unwrap();
        // Should not panic or block
        mgr.sync_turn("user message", "assistant response", "session-xyz");
        // give background task a moment
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}
