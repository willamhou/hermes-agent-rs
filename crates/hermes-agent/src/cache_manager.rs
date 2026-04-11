//! Prompt cache manager: freezes the system prompt segments to avoid redundant cache busting.

use std::hash::{DefaultHasher, Hash, Hasher};

use hermes_core::provider::CacheSegment;

/// Manages frozen system prompt cache segments across conversation turns.
pub struct PromptCacheManager {
    frozen: Option<FrozenSystemPrompt>,
}

struct FrozenSystemPrompt {
    hash: u64,
    segments: Vec<CacheSegment>,
}

impl PromptCacheManager {
    /// Create a new, unfrozen cache manager.
    pub fn new() -> Self {
        Self { frozen: None }
    }

    /// Return cached segments if the hash matches; otherwise build, freeze, and return new segments.
    pub fn get_or_freeze(&mut self, system_prompt: &str, memory_block: &str) -> Vec<CacheSegment> {
        let hash = Self::compute_hash(system_prompt, memory_block);

        if let Some(ref frozen) = self.frozen {
            if frozen.hash == hash {
                return frozen.segments.clone();
            }
        }

        let segments = Self::build_segments(system_prompt, memory_block);
        self.frozen = Some(FrozenSystemPrompt {
            hash,
            segments: segments.clone(),
        });
        segments
    }

    /// Clear the frozen state, forcing a rebuild on the next call.
    pub fn invalidate(&mut self) {
        self.frozen = None;
    }

    /// Whether segments are currently frozen.
    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn build_segments(system: &str, memory: &str) -> Vec<CacheSegment> {
        let mut segments = vec![CacheSegment {
            text: system.to_string(),
            label: "base",
            cache_control: true,
        }];

        if !memory.is_empty() {
            segments.push(CacheSegment {
                text: memory.to_string(),
                label: "memory",
                cache_control: true,
            });
        }

        segments
    }

    fn compute_hash(system: &str, memory: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        system.hash(&mut hasher);
        memory.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for PromptCacheManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_is_not_frozen() {
        let mgr = PromptCacheManager::new();
        assert!(!mgr.is_frozen());
    }

    #[test]
    fn test_freeze_creates_segments() {
        let mut mgr = PromptCacheManager::new();
        let segs = mgr.get_or_freeze("system", "memory");
        assert!(!segs.is_empty());
        assert!(mgr.is_frozen());
    }

    #[test]
    fn test_get_or_freeze_returns_cached() {
        let mut mgr = PromptCacheManager::new();
        let first = mgr.get_or_freeze("system", "memory");
        assert!(mgr.is_frozen());
        let second = mgr.get_or_freeze("system", "memory");
        assert!(mgr.is_frozen());
        // Same content returned both times.
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.text, b.text);
            assert_eq!(a.label, b.label);
        }
    }

    #[test]
    fn test_get_or_freeze_rebuilds_on_change() {
        let mut mgr = PromptCacheManager::new();
        let first = mgr.get_or_freeze("system A", "memory A");
        let second = mgr.get_or_freeze("system B", "memory B");
        // Content must differ.
        assert_ne!(first[0].text, second[0].text);
    }

    #[test]
    fn test_invalidate_clears() {
        let mut mgr = PromptCacheManager::new();
        mgr.get_or_freeze("system", "memory");
        assert!(mgr.is_frozen());
        mgr.invalidate();
        assert!(!mgr.is_frozen());
    }

    #[test]
    fn test_segments_have_cache_control() {
        let mut mgr = PromptCacheManager::new();
        let segs = mgr.get_or_freeze("system", "memory");
        assert!(segs.iter().all(|s| s.cache_control));
    }

    #[test]
    fn test_empty_memory_skips_segment() {
        let mut mgr = PromptCacheManager::new();
        let segs = mgr.get_or_freeze("system", "");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].label, "base");
    }

    #[test]
    fn test_rebuild_after_invalidate() {
        let mut mgr = PromptCacheManager::new();
        mgr.get_or_freeze("system", "memory");
        mgr.invalidate();
        assert!(!mgr.is_frozen());
        let segs = mgr.get_or_freeze("system", "memory");
        assert!(mgr.is_frozen());
        assert_eq!(segs[0].label, "base");
        assert_eq!(segs[0].text, "system");
    }
}
