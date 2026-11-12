//! What the messages between an editor and an ACP agent mean for Atuin.
//!
//! The Agent Client Protocol (agentclientprotocol.com) is JSON-RPC over the agent's stdio, one
//! message per line. [`Observer`] reads each line that passes through the proxy and returns what
//! it means ([`Effect`]):
//!
//! - **Commands.** An agent runs a command in the editor's terminal with `terminal/create`. Its
//!   exit comes in the reply to `terminal/wait_for_exit`, or to a `terminal/output` that finds it
//!   finished. Each such command is a history entry, as a hook records a command that an agent
//!   runs itself.
//! - **Sessions.** The editor opens a session with `session/new`, `session/load` or
//!   `session/resume`, and sends each prompt with `session/prompt`, whose reply ends the turn. An
//!   agent asks the user with `session/request_permission`. These say what the agent is doing, as
//!   the lifecycle hooks of other agents do.
//!
//! A line that is not JSON-RPC, or a message that is none of these, means nothing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use atuin_common::harnesstools::presence::Activity;
use serde_json::Value;

/// Who sent a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The editor, which the protocol calls the client.
    Editor,
    Agent,
}

impl Side {
    const fn other(self) -> Self {
        match self {
            Self::Editor => Self::Agent,
            Self::Agent => Self::Editor,
        }
    }
}

/// What a message means for Atuin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The agent started `command` in a terminal of the editor's, which the agent calls
    /// `terminal`.
    CommandStarted {
        terminal: String,
        command: String,
        cwd: Option<PathBuf>,
    },
    /// The command in `terminal` exited with `exit`.
    CommandEnded {
        terminal: String,
        exit: i64,
    },
    /// The terminal was let go before its command's exit was known.
    CommandDropped {
        terminal: String,
    },
    /// The agent is now doing `activity` in `session`.
    Session {
        session: String,
        activity: Activity,
    },
}

/// A request waiting for its reply, and what the reply will mean.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    TerminalCreate {
        command: String,
        cwd: Option<PathBuf>,
    },
    TerminalExit {
        terminal: String,
    },
    TerminalRelease {
        terminal: String,
    },
    /// `session/new`, whose reply names the session.
    NewSession,
    /// `session/load` or `session/resume` of `session`.
    OpenSession {
        session: String,
    },
    Prompt {
        session: String,
    },
    Permission {
        session: String,
    },
}

/// Reads the lines of one proxied connection. See the module docs.
#[derive(Debug, Default)]
pub struct Observer {
    /// The requests each side sent that wait for the other side's reply, by sender and id.
    pending: HashMap<(Side, String), Pending>,
    /// The terminals whose command has started, and has not yet ended or been dropped.
    running: HashSet<String>,
}

impl Observer {
    /// What the line `line`, sent by `from`, means.
    pub fn observe(&mut self, from: Side, line: &[u8]) -> Vec<Effect> {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            return Vec::new();
        };
        let id = message.get("id").filter(|id| !id.is_null()).map(Value::to_string);
        match (message.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => self.request(from, id, method, &message["params"]),
            (None, Some(id)) => self.reply(from, &id, &message),
            _ => Vec::new(),
        }
    }

    fn request(&mut self, from: Side, id: String, method: &str, params: &Value) -> Vec<Effect> {
        let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
        let pending = match (from, method) {
            (Side::Agent, "terminal/create") => {
                let Some(program) = text("command") else {
                    return Vec::new();
                };
                let args = params.get("args").and_then(Value::as_array);
                let args = args.into_iter().flatten().filter_map(Value::as_str);
                Pending::TerminalCreate {
                    command: command_line(&program, args),
                    cwd: text("cwd").map(PathBuf::from),
                }
            }
            (Side::Agent, "terminal/wait_for_exit" | "terminal/output") => {
                let Some(terminal) = text("terminalId") else {
                    return Vec::new();
                };
                Pending::TerminalExit { terminal }
            }
            (Side::Agent, "terminal/release") => {
                let Some(terminal) = text("terminalId") else {
                    return Vec::new();
                };
                Pending::TerminalRelease { terminal }
            }
            (Side::Agent, "session/request_permission") => {
                let Some(session) = text("sessionId") else {
                    return Vec::new();
                };
                self.pending.insert((from, id), Pending::Permission {
                    session: session.clone(),
                });
                return vec![Effect::Session {
                    session,
                    activity: Activity::NeedsInput,
                }];
            }
            (Side::Editor, "session/new") => Pending::NewSession,
            (Side::Editor, "session/load" | "session/resume") => {
                let Some(session) = text("sessionId") else {
                    return Vec::new();
                };
                Pending::OpenSession { session }
            }
            (Side::Editor, "session/prompt") => {
                let Some(session) = text("sessionId") else {
                    return Vec::new();
                };
                self.pending.insert((from, id), Pending::Prompt {
                    session: session.clone(),
                });
                return vec![Effect::Session {
                    session,
                    activity: Activity::Working,
                }];
            }
            _ => return Vec::new(),
        };
        self.pending.insert((from, id), pending);
        Vec::new()
    }

    /// A reply from `from`, to the request the other side sent as `id`.
    fn reply(&mut self, from: Side, id: &str, message: &Value) -> Vec<Effect> {
        let Some(pending) = self.pending.remove(&(from.other(), id.to_owned())) else {
            return Vec::new();
        };
        let result = message.get("result").filter(|_| message.get("error").is_none());
        let session = |session: String, activity| Effect::Session { session, activity };
        let effect = match (pending, result) {
            (Pending::TerminalCreate { command, cwd }, Some(result)) => {
                let Some(terminal) = result.get("terminalId").and_then(Value::as_str) else {
                    return Vec::new();
                };
                self.running.insert(terminal.to_owned());
                Effect::CommandStarted {
                    terminal: terminal.to_owned(),
                    command,
                    cwd,
                }
            }
            (Pending::TerminalExit { terminal }, Some(result)) => {
                // `terminal/output` holds the exit under `exitStatus`, once there is one.
                let status = result.get("exitStatus").unwrap_or(result);
                let Some(exit) = exit_code(status) else {
                    return Vec::new();
                };
                if !self.running.remove(&terminal) {
                    return Vec::new();
                }
                Effect::CommandEnded { terminal, exit }
            }
            (Pending::TerminalRelease { terminal }, _) => {
                if !self.running.remove(&terminal) {
                    return Vec::new();
                }
                Effect::CommandDropped { terminal }
            }
            (Pending::NewSession, Some(result)) => {
                let Some(id) = result.get("sessionId").and_then(Value::as_str) else {
                    return Vec::new();
                };
                session(id.to_owned(), Activity::Idle)
            }
            // A session that opens waits for a prompt. A turn ends with its reply, whether it
            // ended well or not.
            (Pending::OpenSession { session: id }, Some(_))
            | (Pending::Prompt { session: id }, _) => session(id, Activity::Idle),
            (Pending::Permission { session: id }, _) => session(id, Activity::Working),
            _ => return Vec::new(),
        };
        vec![effect]
    }
}

/// The exit code that a terminal exit status reports: its `exitCode`, else the shell's code for
/// the signal that ended it. `None` while the command still runs.
fn exit_code(status: &Value) -> Option<i64> {
    if let Some(code) = status.get("exitCode").and_then(Value::as_i64) {
        return Some(code);
    }
    let signal = status.get("signal").and_then(Value::as_str)?;
    let number = match signal.trim_start_matches("SIG") {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "TERM" => 15,
        _ => return Some(128),
    };
    Some(128 + number)
}

/// `program` and `args` as one shell command line, each quoted where it needs to be.
fn command_line<'a>(program: &'a str, args: impl Iterator<Item = &'a str>) -> String {
    let mut words = vec![program];
    words.extend(args);
    shlex::try_join(words.iter().copied()).unwrap_or_else(|_| words.join(" "))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn send(observer: &mut Observer, from: Side, message: &Value) -> Vec<Effect> {
        observer.observe(from, message.to_string().as_bytes())
    }

    #[rstest]
    fn a_terminal_command_is_started_and_ended() {
        let mut o = Observer::default();
        let create = json!({"jsonrpc": "2.0", "id": 7, "method": "terminal/create", "params": {
            "sessionId": "s", "command": "cargo", "args": ["test", "--all features"], "cwd": "/p"
        }});
        assert_eq!(send(&mut o, Side::Agent, &create), []);
        let created = json!({"jsonrpc": "2.0", "id": 7, "result": {"terminalId": "t1"}});
        assert_eq!(send(&mut o, Side::Editor, &created), [Effect::CommandStarted {
            terminal: "t1".to_owned(),
            command: "cargo test '--all features'".to_owned(),
            cwd: Some(PathBuf::from("/p")),
        }]);

        let wait = json!({"jsonrpc": "2.0", "id": "w", "method": "terminal/wait_for_exit",
            "params": {"sessionId": "s", "terminalId": "t1"}});
        send(&mut o, Side::Agent, &wait);
        let exited = json!({"jsonrpc": "2.0", "id": "w", "result": {"exitCode": 101}});
        assert_eq!(send(&mut o, Side::Editor, &exited), [Effect::CommandEnded {
            terminal: "t1".to_owned(),
            exit: 101,
        }]);

        // Releasing it after its exit drops nothing.
        let release = json!({"jsonrpc": "2.0", "id": 9, "method": "terminal/release",
            "params": {"sessionId": "s", "terminalId": "t1"}});
        send(&mut o, Side::Agent, &release);
        assert_eq!(
            send(&mut o, Side::Editor, &json!({"jsonrpc": "2.0", "id": 9, "result": {}})),
            []
        );
    }

    #[rstest]
    #[case::still_running(json!({"output": "…", "truncated": false}), None)]
    #[case::finished(json!({"output": "", "exitStatus": {"exitCode": 0, "signal": null}}), Some(0))]
    #[case::killed(json!({"output": "", "exitStatus": {"exitCode": null, "signal": "SIGKILL"}}), Some(137))]
    fn terminal_output_ends_the_command_once_it_has_exited(
        #[case] result: Value,
        #[case] exit: Option<i64>,
    ) {
        let mut o = Observer::default();
        send(
            &mut o,
            Side::Agent,
            &json!({"id": 1, "method": "terminal/create",
            "params": {"command": "ls"}}),
        );
        send(&mut o, Side::Editor, &json!({"id": 1, "result": {"terminalId": "t"}}));
        send(
            &mut o,
            Side::Agent,
            &json!({"id": 2, "method": "terminal/output",
            "params": {"terminalId": "t"}}),
        );
        let effects = send(&mut o, Side::Editor, &json!({"id": 2, "result": result}));
        let expected: Vec<Effect> = exit
            .map(|exit| Effect::CommandEnded {
                terminal: "t".to_owned(),
                exit,
            })
            .into_iter()
            .collect();
        assert_eq!(effects, expected);
    }

    #[rstest]
    fn a_terminal_released_while_running_is_dropped() {
        let mut o = Observer::default();
        send(
            &mut o,
            Side::Agent,
            &json!({"id": 1, "method": "terminal/create",
            "params": {"command": "sleep", "args": ["100"]}}),
        );
        send(&mut o, Side::Editor, &json!({"id": 1, "result": {"terminalId": "t"}}));
        send(
            &mut o,
            Side::Agent,
            &json!({"id": 2, "method": "terminal/release",
            "params": {"terminalId": "t"}}),
        );
        assert_eq!(send(&mut o, Side::Editor, &json!({"id": 2, "result": null})), [
            Effect::CommandDropped {
                terminal: "t".to_owned()
            }
        ]);
    }

    #[rstest]
    fn a_session_goes_from_idle_to_working_to_needing_input_and_back() {
        let mut o = Observer::default();
        let state = |session: &str, activity| Effect::Session {
            session: session.to_owned(),
            activity,
        };
        send(
            &mut o,
            Side::Editor,
            &json!({"id": 1, "method": "session/new", "params": {"cwd": "/p"}}),
        );
        assert_eq!(send(&mut o, Side::Agent, &json!({"id": 1, "result": {"sessionId": "s"}})), [
            state("s", Activity::Idle)
        ]);
        assert_eq!(
            send(
                &mut o,
                Side::Editor,
                &json!({"id": 2, "method": "session/prompt",
                "params": {"sessionId": "s", "prompt": []}})
            ),
            [state("s", Activity::Working)]
        );
        // The agent's request and the editor's request share an id: each side numbers its own.
        assert_eq!(
            send(
                &mut o,
                Side::Agent,
                &json!({"id": 2, "method": "session/request_permission",
                "params": {"sessionId": "s"}})
            ),
            [state("s", Activity::NeedsInput)]
        );
        assert_eq!(send(&mut o, Side::Editor, &json!({"id": 2, "result": {"outcome": {}}})), [
            state("s", Activity::Working)
        ]);
        assert_eq!(
            send(&mut o, Side::Agent, &json!({"id": 2, "result": {"stopReason": "end_turn"}})),
            [state("s", Activity::Idle)]
        );
    }

    #[rstest]
    #[case::not_json(b"not json" as &[u8])]
    #[case::notification(br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#)]
    #[case::unknown_reply(br#"{"jsonrpc":"2.0","id":99,"result":{}}"#)]
    fn other_lines_mean_nothing(#[case] line: &[u8]) {
        assert_eq!(Observer::default().observe(Side::Agent, line), []);
    }
}
