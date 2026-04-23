use std::collections::HashSet;

use hermes_core::error::{HermesError, Result};

pub const MANAGED_BETA_ALLOWED_TOOLS: &[&str] = &[
    "read_file",
    "search_files",
    "write_file",
    "patch",
    "memory_read",
    "memory_write",
    "web_search",
    "web_extract",
    "vision_analyze",
    "skill_list",
    "skill_view",
];

pub fn is_managed_beta_allowed_tool(name: &str) -> bool {
    MANAGED_BETA_ALLOWED_TOOLS.contains(&name)
}

pub fn validate_managed_beta_tools(allowed_tools: &[String]) -> Result<()> {
    let allowed = MANAGED_BETA_ALLOWED_TOOLS
        .iter()
        .copied()
        .collect::<HashSet<_>>();

    let mut disallowed = allowed_tools
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    disallowed.sort();
    disallowed.dedup();

    if disallowed.is_empty() {
        return Ok(());
    }

    Err(HermesError::Config(format!(
        "managed beta tool allowlist contains unsupported tools: {}",
        disallowed.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beta_policy_accepts_supported_tools() {
        let tools = vec!["read_file".to_string(), "skill_view".to_string()];
        validate_managed_beta_tools(&tools).unwrap();
    }

    #[test]
    fn beta_policy_rejects_unsupported_tools() {
        let err = validate_managed_beta_tools(&[
            "read_file".to_string(),
            "terminal".to_string(),
            "browser".to_string(),
        ])
        .unwrap_err();

        assert!(err.to_string().contains("terminal"));
        assert!(err.to_string().contains("browser"));
    }
}
