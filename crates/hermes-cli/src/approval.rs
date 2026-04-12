use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal as _, Write as _},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use hermes_config::config::hermes_home;
use hermes_core::tool::{ApprovalDecision, ApprovalRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Default, Serialize, Deserialize)]
struct ApprovalFile {
    #[serde(default)]
    allow_always: Vec<String>,
}

#[derive(Clone)]
pub struct ApprovalManager {
    file_path: PathBuf,
    session_allow: Arc<Mutex<HashSet<String>>>,
    persistent_allow: Arc<Mutex<HashSet<String>>>,
}

impl ApprovalManager {
    pub fn load_or_default() -> Self {
        let path = hermes_home().join("approvals.json");
        match Self::load(path.clone()) {
            Ok(manager) => manager,
            Err(err) => {
                tracing::warn!(path = %path.display(), "failed to load approval memory: {err}");
                Self::empty(path)
            }
        }
    }

    pub fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::empty(path));
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read approval file '{}'", path.display()))?;
        let parsed: ApprovalFile = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse approval file '{}'", path.display()))?;

        Ok(Self {
            file_path: path,
            session_allow: Arc::new(Mutex::new(HashSet::new())),
            persistent_allow: Arc::new(Mutex::new(parsed.allow_always.into_iter().collect())),
        })
    }

    pub fn remembered_decision(&self, memory_key: &str) -> Option<ApprovalDecision> {
        if self
            .persistent_allow
            .lock()
            .ok()
            .is_some_and(|set| set.contains(memory_key))
        {
            return Some(ApprovalDecision::AllowAlways);
        }

        if self
            .session_allow
            .lock()
            .ok()
            .is_some_and(|set| set.contains(memory_key))
        {
            return Some(ApprovalDecision::AllowSession);
        }

        None
    }

    pub fn remember(&self, memory_key: &str, decision: ApprovalDecision) -> Result<()> {
        match decision {
            ApprovalDecision::AllowSession => {
                self.session_allow
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session approval memory lock poisoned"))?
                    .insert(memory_key.to_string());
            }
            ApprovalDecision::AllowAlways => {
                self.session_allow
                    .lock()
                    .map_err(|_| anyhow::anyhow!("session approval memory lock poisoned"))?
                    .insert(memory_key.to_string());
                self.persistent_allow
                    .lock()
                    .map_err(|_| anyhow::anyhow!("persistent approval memory lock poisoned"))?
                    .insert(memory_key.to_string());
                self.save_persistent()?;
            }
            ApprovalDecision::Allow | ApprovalDecision::Deny => {}
        }

        Ok(())
    }

    pub fn spawn_handler(
        self,
        mut approval_rx: mpsc::Receiver<ApprovalRequest>,
        interactive: bool,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(req) = approval_rx.recv().await {
                let memory_key = req.memory_key.clone();
                let decision = if let Some(remembered) = self.remembered_decision(&memory_key) {
                    tracing::info!(
                        tool = %req.tool_name,
                        "reusing remembered approval decision"
                    );
                    remembered
                } else if !interactive {
                    tracing::warn!(
                        tool = %req.tool_name,
                        "denying approval request in non-interactive mode"
                    );
                    ApprovalDecision::Deny
                } else {
                    let tool_name = req.tool_name.clone();
                    let command = req.command.clone();
                    let reason = req.reason.clone();
                    match tokio::task::spawn_blocking(move || {
                        prompt_for_approval(&tool_name, &command, &reason)
                    })
                    .await
                    {
                        Ok(decision) => decision,
                        Err(err) => {
                            tracing::warn!(tool = %req.tool_name, "approval prompt failed: {err}");
                            ApprovalDecision::Deny
                        }
                    }
                };

                if let Err(err) = self.remember(&memory_key, decision.clone()) {
                    tracing::warn!(
                        tool = %req.tool_name,
                        "failed to store approval memory: {err}"
                    );
                }

                let _ = req.response_tx.send(decision);
            }
        })
    }

    fn save_persistent(&self) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create approval memory directory '{}'",
                    parent.display()
                )
            })?;
        }

        let mut allow_always = self
            .persistent_allow
            .lock()
            .map_err(|_| anyhow::anyhow!("persistent approval memory lock poisoned"))?
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        allow_always.sort();

        let payload = serde_json::to_string_pretty(&ApprovalFile { allow_always })
            .context("failed to serialize approval memory")?;
        fs::write(&self.file_path, payload).with_context(|| {
            format!(
                "failed to write approval memory file '{}'",
                self.file_path.display()
            )
        })?;

        Ok(())
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            file_path: path,
            session_allow: Arc::new(Mutex::new(HashSet::new())),
            persistent_allow: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

pub fn is_interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

fn prompt_for_approval(tool_name: &str, command: &str, reason: &str) -> ApprovalDecision {
    eprintln!("\nApproval required");
    eprintln!("Tool: {tool_name}");
    if !reason.trim().is_empty() {
        eprintln!("Reason: {reason}");
    }
    eprintln!("Command: {command}");

    loop {
        eprint!("Approve? [y] once / [s] session / [a] always / [n] deny: ");
        let _ = io::stderr().flush();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            eprintln!("Failed to read approval choice, denying.");
            return ApprovalDecision::Deny;
        }

        match parse_approval_choice(&input) {
            Some(decision) => return decision,
            None => eprintln!("Please enter y, s, a, or n."),
        }
    }
}

fn parse_approval_choice(input: &str) -> Option<ApprovalDecision> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalDecision::Allow),
        "s" | "session" => Some(ApprovalDecision::AllowSession),
        "a" | "always" => Some(ApprovalDecision::AllowAlways),
        "" | "n" | "no" | "deny" => Some(ApprovalDecision::Deny),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_session_is_not_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("approvals.json");

        let manager = ApprovalManager::load(path.clone()).unwrap();
        manager
            .remember("terminal:rm -rf /", ApprovalDecision::AllowSession)
            .unwrap();

        assert_eq!(
            manager.remembered_decision("terminal:rm -rf /"),
            Some(ApprovalDecision::AllowSession)
        );

        let reloaded = ApprovalManager::load(path).unwrap();
        assert_eq!(reloaded.remembered_decision("terminal:rm -rf /"), None);
    }

    #[test]
    fn allow_always_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("approvals.json");

        let manager = ApprovalManager::load(path.clone()).unwrap();
        manager
            .remember("terminal:git push --force", ApprovalDecision::AllowAlways)
            .unwrap();

        let reloaded = ApprovalManager::load(path).unwrap();
        assert_eq!(
            reloaded.remembered_decision("terminal:git push --force"),
            Some(ApprovalDecision::AllowAlways)
        );
    }

    #[test]
    fn parse_choices_maps_expected_inputs() {
        assert_eq!(parse_approval_choice("y"), Some(ApprovalDecision::Allow));
        assert_eq!(
            parse_approval_choice("session"),
            Some(ApprovalDecision::AllowSession)
        );
        assert_eq!(
            parse_approval_choice("always"),
            Some(ApprovalDecision::AllowAlways)
        );
        assert_eq!(parse_approval_choice(""), Some(ApprovalDecision::Deny));
        assert_eq!(parse_approval_choice("weird"), None);
    }
}
