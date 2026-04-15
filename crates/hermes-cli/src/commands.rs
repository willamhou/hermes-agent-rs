//! CLI slash command registry and resolver.

pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: "help",
        aliases: &["h", "?"],
        description: "Show commands",
        usage: "/help",
    },
    CommandDef {
        name: "quit",
        aliases: &["q", "exit"],
        description: "Exit",
        usage: "/quit",
    },
    CommandDef {
        name: "new",
        aliases: &["reset"],
        description: "New conversation",
        usage: "/new",
    },
    CommandDef {
        name: "clear",
        aliases: &[],
        description: "Clear screen",
        usage: "/clear",
    },
    CommandDef {
        name: "model",
        aliases: &["m"],
        description: "Show current model",
        usage: "/model",
    },
    CommandDef {
        name: "tools",
        aliases: &["t"],
        description: "List tools",
        usage: "/tools",
    },
    CommandDef {
        name: "status",
        aliases: &[],
        description: "Session info",
        usage: "/status",
    },
    CommandDef {
        name: "retry",
        aliases: &[],
        description: "Re-run last message",
        usage: "/retry",
    },
    CommandDef {
        name: "undo",
        aliases: &[],
        description: "Remove last turn",
        usage: "/undo",
    },
    CommandDef {
        name: "compress",
        aliases: &[],
        description: "Compress context",
        usage: "/compress",
    },
    CommandDef {
        name: "skills",
        aliases: &[],
        description: "List/reload skills",
        usage: "/skills [reload]",
    },
    CommandDef {
        name: "save",
        aliases: &[],
        description: "Save to file",
        usage: "/save [path]",
    },
    CommandDef {
        name: "cron",
        aliases: &[],
        description: "Scheduled jobs",
        usage: "/cron",
    },
];

/// Resolve a slash command from user input.
///
/// Matches by exact name, alias, or unambiguous prefix. Returns `None` if the
/// input does not start with `/`, nothing follows the slash, or the prefix is
/// ambiguous / unknown.
pub fn resolve_command(input: &str) -> Option<&'static CommandDef> {
    let word = input.split_whitespace().next()?.strip_prefix('/')?;
    let word_lower = word.to_lowercase();

    // Exact match on name or alias
    if let Some(cmd) = COMMANDS
        .iter()
        .find(|c| c.name == word_lower || c.aliases.contains(&word_lower.as_str()))
    {
        return Some(cmd);
    }

    // Unambiguous prefix match
    let matches: Vec<_> = COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(&word_lower as &str))
        .collect();
    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_by_exact_name() {
        let cmd = resolve_command("/help").expect("should resolve /help");
        assert_eq!(cmd.name, "help");
    }

    #[test]
    fn resolve_by_alias() {
        let cmd = resolve_command("/q").expect("should resolve /q alias");
        assert_eq!(cmd.name, "quit");

        let cmd = resolve_command("/exit").expect("should resolve /exit alias");
        assert_eq!(cmd.name, "quit");

        let cmd = resolve_command("/?").expect("should resolve /? alias");
        assert_eq!(cmd.name, "help");
    }

    #[test]
    fn resolve_by_unambiguous_prefix() {
        // "cr" matches only "cron"
        let cmd = resolve_command("/cr").expect("should resolve /cr prefix to cron");
        assert_eq!(cmd.name, "cron");

        // "sk" matches only "skills"
        let cmd = resolve_command("/sk").expect("should resolve /sk prefix to skills");
        assert_eq!(cmd.name, "skills");
    }

    #[test]
    fn ambiguous_prefix_returns_none() {
        // "s" matches "status", "save", "skills" — ambiguous
        assert!(resolve_command("/s").is_none());

        // "c" matches "clear", "compress", "cron" — ambiguous
        assert!(resolve_command("/c").is_none());
    }

    #[test]
    fn unknown_command_returns_none() {
        assert!(resolve_command("/foobar").is_none());
        assert!(resolve_command("/xyz123").is_none());
        // No slash prefix at all — not a command
        assert!(resolve_command("help").is_none());
    }
}
