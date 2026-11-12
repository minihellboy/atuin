//! Which agent sessions are open on this machine, and what each is doing, as the agents' own
//! lifecycle hooks report it.
//!
//! `atuin hook <agent>` records each session it hears from in a file of its own, named for the
//! agent and the session ([`record`]). The file names the agent's process and when that process
//! started, so a file left behind by an agent that crashed is told apart from a live one when it
//! is read ([`live`]). The session's `SessionEnd` removes it ([`forget`]).
//!
//! This is exact for every agent with lifecycle hooks, where the checks in
//! [`sync::liveness`](super::sync::liveness) can only guess for some. It also says whether the
//! agent is working or waiting for the user, which nothing else on disk says reliably.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::sync::liveness::{Processes, Start};
use crate::path::PathBufExt as _;

/// What an agent is doing in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Working on a prompt: between the prompt and the end of the agent's turn.
    Working,
    /// Its turn is over, so it is waiting for the user's next prompt.
    Idle,
    /// It asked the user something, such as permission to run a tool, and is waiting for the
    /// answer.
    NeedsInput,
}

impl Activity {
    /// How the activity reads in a tool's output.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::NeedsInput => "needs input",
        }
    }
}

/// One open session, as its agent's hooks last reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    /// The agent, as `atuin hook` names it (`claude-code`, `codex`).
    pub harness: String,
    /// The agent's own id for the session.
    pub session: String,
    /// The agent's process.
    pub pid: u32,
    /// When that process started, so that a reused pid is not taken for it.
    pub pid_start: Option<Start>,
    pub activity: Activity,
    /// When the activity last changed, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// Where the session works, when the hook said so.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

impl Presence {
    /// When the activity last changed.
    #[must_use]
    pub fn since(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(self.since_ms)
    }
}

/// Where the session files are: `$XDG_RUNTIME_DIR/atuin/agents`, else a directory of this user's
/// under the temporary directory.
///
/// They describe running processes, so they belong where the machine clears them on reboot.
#[must_use]
pub fn dir() -> PathBuf {
    match crate::env::var_abspath("XDG_RUNTIME_DIR") {
        Some(runtime) => runtime.into_path_buf().joined("atuin").joined("agents"),
        None => {
            let user = crate::env::var_nonempty("USER")
                .or_else(|| crate::env::var_nonempty("USERNAME"))
                .map_or_else(|| "user".to_owned(), |u| u.to_string_lossy().into_owned());
            std::env::temp_dir().joined(format!("atuin-agents-{user}"))
        }
    }
}

/// The file that holds session `session` of agent `harness` in `dir`.
///
/// Both names come from the agent. Any character that could leave the directory is replaced.
fn file_in(dir: &Path, harness: &str, session: &str) -> PathBuf {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    dir.join(format!("{}--{}.json", safe(harness), safe(session).trim_start_matches('.')))
}

/// Record `presence` in `dir`, replacing what was recorded for its session.
///
/// The file is written beside its place and renamed over it, so a reader never sees half of one.
/// The directory is made private to this user.
pub fn record_in(dir: &Path, presence: &Presence) -> io::Result<()> {
    make_private_dir(dir)?;
    let path = file_in(dir, &presence.harness, &presence.session);
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_vec(presence)?)?;
    std::fs::rename(&temp, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// [`record_in`] the shared [`dir`].
pub fn record(presence: &Presence) -> io::Result<()> {
    record_in(&dir(), presence)
}

/// Forget session `session` of agent `harness` in `dir`: it has ended.
pub fn forget_in(dir: &Path, harness: &str, session: &str) -> io::Result<()> {
    match std::fs::remove_file(file_in(dir, harness, session)) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// [`forget_in`] the shared [`dir`].
pub fn forget(harness: &str, session: &str) -> io::Result<()> {
    forget_in(&dir(), harness, session)
}

/// What was last recorded for session `session` of agent `harness` in `dir`, if anything.
#[must_use]
pub fn recorded_in(dir: &Path, harness: &str, session: &str) -> Option<Presence> {
    read(&file_in(dir, harness, session))
}

fn read(path: &Path) -> Option<Presence> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Every session recorded in `dir` whose agent process still runs, as `procs` tells it.
///
/// A file whose process has exited, or whose pid now belongs to a process started at another
/// time, was left by an agent that crashed or was killed before its `SessionEnd`. It is removed.
/// A process that can't be told running or not is kept.
#[must_use]
pub fn live_in(dir: &Path, procs: &Processes) -> Vec<Presence> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut live = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(presence) = read(&path) else {
            continue;
        };
        match procs.running(presence.pid) {
            Ok(Some(running)) => {
                let reused = presence.pid_start.as_ref().zip(running.start.as_ref());
                if reused.is_some_and(|(recorded, now)| recorded != now) {
                    let _ = std::fs::remove_file(&path);
                } else {
                    live.push(presence);
                }
            }
            Ok(None) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => live.push(presence),
        }
    }
    live
}

/// [`live_in`] the shared [`dir`].
#[must_use]
pub fn live(procs: &Processes) -> Vec<Presence> {
    live_in(&dir(), procs)
}

/// The process that `pid` names, with when it started: what a [`Presence`] records of it.
#[must_use]
pub fn process_start(procs: &Processes, pid: u32) -> Option<Start> {
    procs.running(pid).ok()??.start
}

#[cfg(unix)]
fn make_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use rstest::{fixture, rstest};
    use tempfile::TempDir;

    use super::*;
    use crate::harnesstools::sync::liveness::TableEntry;

    #[fixture]
    fn dir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn presence(session: &str, pid: u32, start: u64) -> Presence {
        Presence {
            harness: "codex".to_owned(),
            session: session.to_owned(),
            pid,
            pid_start: Some(Start::Epoch(start)),
            activity: Activity::Working,
            since_ms: 1,
            cwd: None,
        }
    }

    /// A process table holding process `pid`, started at `start`.
    fn running(pid: u32, start: u64) -> Processes {
        Processes::Table(vec![TableEntry {
            pid,
            name: "codex".into(),
            start_time: start,
            ..TableEntry::default()
        }])
    }

    #[rstest]
    fn a_recorded_session_is_live_while_its_process_runs(dir: TempDir) {
        record_in(dir.path(), &presence("a", 10, 5)).unwrap();

        assert_eq!(live_in(dir.path(), &running(10, 5)), [presence("a", 10, 5)]);
    }

    #[rstest]
    #[case::exited(running(11, 5))]
    #[case::pid_reused(running(10, 6))]
    fn a_session_whose_process_is_gone_is_forgotten(dir: TempDir, #[case] procs: Processes) {
        record_in(dir.path(), &presence("a", 10, 5)).unwrap();

        assert_eq!(live_in(dir.path(), &procs), []);
        assert_eq!(recorded_in(dir.path(), "codex", "a"), None, "the stale file is removed");
    }

    #[rstest]
    fn a_new_record_replaces_the_last_and_forgetting_removes_it(dir: TempDir) {
        record_in(dir.path(), &presence("a", 10, 5)).unwrap();
        let idle = Presence {
            activity: Activity::Idle,
            ..presence("a", 10, 5)
        };
        record_in(dir.path(), &idle).unwrap();
        assert_eq!(recorded_in(dir.path(), "codex", "a"), Some(idle));

        forget_in(dir.path(), "codex", "a").unwrap();
        forget_in(dir.path(), "codex", "a").unwrap();
        assert_eq!(recorded_in(dir.path(), "codex", "a"), None);
    }

    #[rstest]
    #[case::plain("codex", "01a1", "codex--01a1.json")]
    #[case::traversal("codex", "../../etc/x", "codex--_.._etc_x.json")]
    fn a_session_file_never_leaves_its_directory(
        #[case] harness: &str,
        #[case] session: &str,
        #[case] name: &str,
    ) {
        let file = file_in(Path::new("/r"), harness, session);
        assert_eq!(file, Path::new("/r").join(name));
    }
}
