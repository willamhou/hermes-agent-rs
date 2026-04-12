use std::fmt::Write as _;

use sha2::{Digest, Sha256};

pub(crate) fn approval_memory_key(tool_name: &str, payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());

    let mut digest_hex = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(&mut digest_hex, "{byte:02x}");
    }

    format!("{tool_name}:sha256:{digest_hex}")
}

#[cfg(test)]
mod tests {
    use super::approval_memory_key;

    #[test]
    fn approval_memory_key_hashes_payload() {
        let payload = "git push https://token@example.com/repo main --force";
        let key = approval_memory_key("terminal", payload);

        assert!(key.starts_with("terminal:sha256:"));
        assert!(!key.contains(payload));
        assert_eq!(key, approval_memory_key("terminal", payload));
        assert_ne!(key, approval_memory_key("terminal", "different payload"));
    }
}
