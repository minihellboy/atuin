//! Recording which sessions an agent has open, and what it is doing in each, from its hook
//! events (see [`atuin_common::harnesstools::presence`]).
//!
//! Every hook event names its session. The lifecycle events say what the agent is doing:
//! `UserPromptSubmit` starts its work, `Stop` ends its turn, and `SessionEnd` closes the session.
//! A shell command's events say that it is working too. They only update a session that a
//! lifecycle event recorded, because an agent without the lifecycle hooks would never say that its
//! turn ended.

use std::path::PathBuf;
use std::time::SystemTime;

use atuin_common::harnesstools::presence::{self, Activity, Presence};
use atuin_common::harnesstools::sync::Processes;
use serde::Deserialize;

/// The fields of any hook event that say which session it is and what it marks.
#[derive(Debug, Deserialize)]
struct WireLifecycle {
    hook_event_name: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Why `SessionStart` ran: `startup`, `resume`, `clear` or `compact`.
    #[serde(default)]
    source: Option<String>,
    /// What Claude Code's `Notification` is about, such as `permission_prompt`.
    #[serde(default)]
    notification_type: Option<String>,
}

/// The agents that `atuin hook install` registers the lifecycle events for.
const LIFECYCLE_AGENTS: &[&str] = &["claude-code", "codex"];

/// What a hook event means for its session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// The agent is now doing this.
    To(Activity),
    /// The agent is working, if a lifecycle event recorded the session.
    StillWorking,
    /// The session goes on as it was. Compacting starts a session again in the middle of a turn
    /// as often as between turns.
    Same,
    /// The session has ended.
    Ended,
}

/// What hook event `event` means for its session, or `None` when it says nothing of it.
fn change(event: &WireLifecycle) -> Option<Change> {
    Some(match event.hook_event_name.as_str() {
        "SessionStart" if event.source.as_deref() == Some("compact") => Change::Same,
        "SessionStart" | "Stop" => Change::To(Activity::Idle),
        "UserPromptSubmit" => Change::To(Activity::Working),
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => Change::StillWorking,
        "PermissionRequest" => Change::To(Activity::NeedsInput),
        "Notification" => match event.notification_type.as_deref()? {
            "permission_prompt" | "elicitation_dialog" => Change::To(Activity::NeedsInput),
            "idle_prompt" => Change::To(Activity::Idle),
            _ => return None,
        },
        "SessionEnd" => Change::Ended,
        _ => return None,
    })
}

/// Record what hook event `input` from agent `harness` says of its session.
///
/// Errors are ignored: this is a side record, and the agent must never see the hook fail
/// because of it.
pub fn report(harness: &str, input: &str) {
    if !LIFECYCLE_AGENTS.contains(&harness) {
        return;
    }
    let Ok(event) = serde_json::from_str::<WireLifecycle>(input) else {
        return;
    };
    let Some(session) = event.session_id.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };
    let Some(change) = change(&event) else {
        return;
    };
    let activity = match change {
        Change::Ended => {
            let _ = presence::forget(harness, session);
            return;
        }
        Change::To(activity) => activity,
        Change::Same => presence::recorded_in(&presence::dir(), harness, session)
            .map_or(Activity::Idle, |last| last.activity),
        Change::StillWorking => {
            match presence::recorded_in(&presence::dir(), harness, session) {
                // Already known to be working: nothing to write on every command.
                Some(last) if last.activity == Activity::Working => return,
                Some(_) => Activity::Working,
                None => return,
            }
        }
    };
    let procs = Processes::here();
    let Some(pid) = procs.agent_above(std::process::id()) else {
        return;
    };
    let since_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let _ = presence::record(&Presence {
        harness: harness.to_owned(),
        session: session.to_owned(),
        pid,
        pid_start: presence::process_start(&procs, pid),
        activity,
        since_ms,
        cwd: event.cwd,
    });
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case::start(json!({"hook_event_name": "SessionStart", "source": "startup"}), Some(Change::To(Activity::Idle)))]
    #[case::compacted(json!({"hook_event_name": "SessionStart", "source": "compact"}), Some(Change::Same))]
    #[case::prompt(json!({"hook_event_name": "UserPromptSubmit"}), Some(Change::To(Activity::Working)))]
    #[case::command(json!({"hook_event_name": "PreToolUse"}), Some(Change::StillWorking))]
    #[case::turn_over(json!({"hook_event_name": "Stop"}), Some(Change::To(Activity::Idle)))]
    #[case::permission(json!({"hook_event_name": "PermissionRequest"}), Some(Change::To(Activity::NeedsInput)))]
    #[case::asks(json!({"hook_event_name": "Notification", "notification_type": "permission_prompt"}), Some(Change::To(Activity::NeedsInput)))]
    #[case::waits(json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"}), Some(Change::To(Activity::Idle)))]
    #[case::other_notice(json!({"hook_event_name": "Notification", "notification_type": "auth_success"}), None)]
    #[case::ended(json!({"hook_event_name": "SessionEnd", "reason": "exit"}), Some(Change::Ended))]
    #[case::subagent(json!({"hook_event_name": "SubagentStop"}), None)]
    fn an_event_changes_its_session(
        #[case] event: serde_json::Value,
        #[case] expected: Option<Change>,
    ) {
        let event: WireLifecycle = serde_json::from_value(event).unwrap();
        assert_eq!(change(&event), expected);
    }
}
