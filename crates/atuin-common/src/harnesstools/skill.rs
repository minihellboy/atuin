//! Atuin's agent skill: a `SKILL.md` in the Agent Skills format (agentskills.io), which tells an
//! agent when Atuin's MCP tools answer a question.
//!
//! The MCP server's instructions say the same, but many agents shorten or drop server
//! instructions. An agent reads a skill's description up front and its body when the task fits,
//! so the skill reaches agents that the instructions miss. `atuin mcp install` writes it beside
//! the agent's other skills ([`install_in`]).

use std::io;
use std::path::{Path, PathBuf};

/// The skill's directory name, which the Agent Skills format requires to match its `name`.
pub const NAME: &str = "atuin";

/// The line that marks a `SKILL.md` as the one atuin writes. A skill of the same name without it
/// is the user's own, which atuin leaves alone.
const MARKER: &str = "<!-- Written by `atuin mcp install`, which replaces this file. -->";

/// The skill.
pub const SKILL: &str = r#"---
name: atuin
description: Look up what actually happened in the user's terminal and in earlier AI agent sessions. Use when a task depends on past commands (how something was run, whether it succeeded, why it failed, what it printed), on what another agent did or decided ('last time', 'continue', 'again'), or on which agents are running now.
---

<!-- Written by `atuin mcp install`, which replaces this file. -->

# Atuin

Atuin records every shell command with its directory, exit code and duration, including the
commands that agents run. With output capture on, it keeps what each command printed. It also
keeps the transcripts of past agent sessions, called threads. Its MCP server, `atuin`, reads all
of this. The tools are read-only.

## When to use it

- **How was this run before?** Search `atuin_history` before guessing flags or asking the user.
  Pass `cwd: "."` to stay in this project, and `since` to narrow by time.
- **Why did it fail?** Search `atuin_history` with `only_failed: true`. Then read the error with
  `atuin_output` and the history id. Don't run an expensive command again just to see its output.
- **Where did I see this?** When you know what was printed but not by which command, search the
  output itself with `atuin_output_search`.
- **What happened last time?** For "continue", "again", "still" or "what did we decide", list
  this project's threads with `atuin_threads_list` (`cwd: "."`), or search them with
  `atuin_threads_search`. Read one with `atuin_threads_read`. `start: -10` shows how it ended.
- **What else is running?** `atuin_threads_list` with `open: true` lists the threads that agents
  on this machine have open now, and says whether each agent is working, idle or waiting for
  input.

## Notes

- Don't read `~/.bash_history` or `~/.zsh_history`. They are stale in non-interactive shells
  and have no exit codes. Atuin is the reliable record.
- A thread that is open now may still change. Read it again for the latest.
- Results can include commands and threads from other machines when the user syncs them.
"#;

/// Write the skill into the skills directory `skills`, as `<skills>/atuin/SKILL.md`.
///
/// A `SKILL.md` there that atuin did not write is left as it is. Returns where the skill is, or
/// `None` when the user's own skill holds the name.
pub fn install_in(skills: &Path) -> io::Result<Option<PathBuf>> {
    let dir = skills.join(NAME);
    let path = dir.join("SKILL.md");
    match std::fs::read_to_string(&path) {
        Ok(existing) if !existing.contains(MARKER) => return Ok(None),
        Ok(existing) if existing == SKILL => return Ok(Some(path)),
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, SKILL)?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn the_skill_is_written_and_rewritten_but_never_over_the_users_own() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atuin").join("SKILL.md");

        assert_eq!(install_in(dir.path()).unwrap(), Some(path.clone()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);

        // An older copy of atuin's own is replaced.
        std::fs::write(&path, format!("old\n{MARKER}\n")).unwrap();
        install_in(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL);

        // The user's own skill of the same name is kept.
        std::fs::write(&path, "mine").unwrap();
        assert_eq!(install_in(dir.path()).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine");
    }

    /// The Agent Skills format: front matter with a `name` that matches the directory and a
    /// `description` of at most 1024 characters.
    #[rstest]
    fn the_skill_follows_the_agent_skills_format() {
        let front = SKILL.strip_prefix("---\n").and_then(|s| s.split_once("\n---\n")).unwrap().0;
        let field = |key: &str| {
            front.lines().find_map(|l| l.strip_prefix(&format!("{key}: "))).unwrap().to_owned()
        };
        assert_eq!(field("name"), NAME);
        let description = field("description");
        assert!(!description.is_empty() && description.len() <= 1024, "{}", description.len());
        assert!(SKILL.contains(MARKER));
    }
}
