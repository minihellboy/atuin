//! `atuin acp -- <agent> [args...]`: run an Agent Client Protocol agent through Atuin.
//!
//! An editor, such as Zed, starts the agent as this command instead of directly.
//! Atuin starts the agent and passes every line between the two unchanged. Atuin also reads each
//! line ([`observer`]):
//!
//! - It records each command the agent runs in the editor's terminal as a history entry, authored
//!   by the agent, as `atuin hook` does for a command an agent runs itself.
//! - It records each session the agent has open and what the agent is doing in it, so the
//!   threads tools can say so ([`atuin_common::harnesstools::presence`]).
//!
//! Recording happens on its own task, so a slow write never holds up a message. Whatever goes
//! wrong in it is logged and passed over: the agent must work as if it were started directly.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::SystemTime;

use atuin_client::history::{AuthorKind, HistoryId};
use atuin_client::settings::Settings;
use atuin_common::harnesstools::presence::{self, Activity, Presence};
use atuin_common::harnesstools::sync::Processes;
use atuin_common::harnesstools::sync::liveness::Start;
use clap::Parser;
use eyre::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;

use self::observer::{Effect, Observer, Side};
use super::history;

mod observer;

/// Run an ACP agent, recording the commands it runs and the sessions it has open.
#[derive(Parser, Debug)]
pub struct Cmd {
    /// The agent's name in history and in thread listings, such as `claude-code`. By default it
    /// is taken from the agent's command.
    #[arg(long)]
    name: Option<String>,

    /// The agent's command and its arguments.
    #[arg(required = true, last = true, value_name = "AGENT")]
    agent: Vec<String>,
}

impl Cmd {
    pub async fn run(self, settings: &Arc<Settings>) -> Result<()> {
        let Some((program, args)) = self.agent.split_first() else {
            bail!("expected the agent's command after `--`");
        };
        let name = self.name.unwrap_or_else(|| agent_name(&self.agent).to_owned());

        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("could not start {program}"))?;
        let pid = child.id();
        let (Some(to_agent), Some(from_agent)) = (child.stdin.take(), child.stdout.take()) else {
            bail!("could not connect to {program}'s stdio");
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let recorder = tokio::spawn(record(Arc::clone(settings), name, pid, rx));
        let editor_to_agent = relay(tokio::io::stdin(), to_agent, Side::Editor, tx.clone());
        let agent_to_editor = relay(from_agent, tokio::io::stdout(), Side::Agent, tx);

        // The agent's output ending means it is done. The editor closing its side only closes
        // the agent's input, which an agent takes as the sign to exit.
        let (_, out) = tokio::join!(editor_to_agent, agent_to_editor);
        let status = child.wait().await?;
        let _ = recorder.await;
        out?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        Ok(())
    }
}

/// One line that passed, by who sent it.
type Line = (Side, Vec<u8>);

/// Copy every line from `from` to `to` as it is, and send a copy to the recorder.
async fn relay(
    from: impl AsyncRead + Unpin,
    mut to: impl AsyncWrite + Unpin,
    side: Side,
    lines: mpsc::UnboundedSender<Line>,
) -> Result<()> {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    loop {
        line.clear();
        if from.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        to.write_all(&line).await?;
        to.flush().await?;
        let _ = lines.send((side, line.clone()));
    }
    to.shutdown().await?;
    Ok(())
}

/// Read the lines that passed, and record what they mean, until both relays have stopped.
async fn record(
    settings: Arc<Settings>,
    name: String,
    pid: Option<u32>,
    mut lines: mpsc::UnboundedReceiver<Line>,
) {
    let mut observer = Observer::default();
    let mut entries: HashMap<String, HistoryId> = HashMap::new();
    let mut sessions: Vec<String> = Vec::new();
    let procs = Processes::here();
    let pid_start = pid.and_then(|pid| presence::process_start(&procs, pid));

    while let Some((side, line)) = lines.recv().await {
        for effect in observer.observe(side, &line) {
            match effect {
                Effect::CommandStarted {
                    terminal,
                    command,
                    cwd,
                } => {
                    let started = history::start_history_entry(
                        &settings,
                        &command,
                        Some(&name),
                        Some(AuthorKind::Agent),
                        None,
                        cwd.as_deref(),
                        None,
                    )
                    .await;
                    match started {
                        Ok(Some(id)) => {
                            entries.insert(terminal, id);
                        }
                        Ok(None) => {}
                        Err(err) => tracing::warn!(%err, "could not record an ACP command"),
                    }
                }
                Effect::CommandEnded { terminal, exit } => {
                    if let Some(id) = entries.remove(&terminal) {
                        let _ = history::end_history_entry(&settings, id, exit, None).await;
                    }
                }
                Effect::CommandDropped { terminal } => {
                    if let Some(id) = entries.remove(&terminal) {
                        let _ = history::cancel_history_entry(&settings, id).await;
                    }
                }
                Effect::Session { session, activity } => {
                    let Some(pid) = pid else {
                        continue;
                    };
                    if !sessions.contains(&session) {
                        sessions.push(session.clone());
                    }
                    report(&name, &session, pid, pid_start.clone(), activity);
                }
            }
        }
    }

    // The agent has exited: its commands will never end, and its sessions are closed.
    for id in entries.into_values() {
        let _ = history::cancel_history_entry(&settings, id).await;
    }
    for session in sessions {
        let _ = presence::forget(&name, &session);
    }
}

fn report(name: &str, session: &str, pid: u32, pid_start: Option<Start>, activity: Activity) {
    let since_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let _ = presence::record(&Presence {
        harness: name.to_owned(),
        session: session.to_owned(),
        pid,
        pid_start,
        activity,
        since_ms,
        cwd: None,
    });
}

/// The agent's name, from its command: the agent Atuin knows that the command names, such as
/// `claude-code` for `npx @zed-industries/claude-code-acp`, else the program's own name.
fn agent_name(command: &[String]) -> &str {
    const KNOWN: &[(&str, &str)] = &[
        ("claude", "claude-code"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("kiro", "kiro"),
        ("grok", "grok"),
        ("copilot", "copilot"),
        ("hermes", "hermes"),
    ];
    let words = command.iter().map(|word| word.rsplit(['/', '\\']).next().unwrap_or(word));
    for word in words {
        if let Some((_, name)) = KNOWN.iter().find(|(part, _)| word.contains(part)) {
            return name;
        }
    }
    command.first().map_or("acp", |program| program.rsplit(['/', '\\']).next().unwrap_or(program))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::adapter(&["npx", "@zed-industries/claude-code-acp"], "claude-code")]
    #[case::codex(&["/usr/local/bin/codex-acp"], "codex")]
    #[case::other(&["/opt/gemini", "--experimental-acp"], "gemini")]
    fn the_agent_is_named_from_its_command(#[case] command: &[&str], #[case] expected: &str) {
        let command: Vec<String> = command.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(agent_name(&command), expected);
    }

    #[rstest]
    fn a_command_is_parsed_after_the_separator() {
        let cmd = Cmd::try_parse_from(["acp", "--name", "x", "--", "agent", "--flag"]).unwrap();
        assert_eq!(cmd.name.as_deref(), Some("x"));
        assert_eq!(cmd.agent, ["agent", "--flag"]);
    }
}
