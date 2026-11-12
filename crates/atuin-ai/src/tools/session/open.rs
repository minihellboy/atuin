//! Whether a thread is open in an agent on this machine now, and what the agent is doing.
//!
//! An agent with Atuin's lifecycle hooks installed reports each session it opens, what it is
//! doing in it, and when it closes it ([`presence`]). That answer is exact, and is taken first.
//!
//! Any other thread is checked in the same way that resuming a thread checks it
//! ([`is_live_among`]). Claude Code and Codex record which threads they have open, so their
//! answer is exact too. Others, such as pi and opencode, record nothing. A process of theirs that
//! runs where the thread works may have it open. That is reported as [`Open::Maybe`], not as
//! open.
//!
//! [`is_live_among`]: atuin_common::harnesstools::sync::SessionSync::is_live_among

use std::collections::HashMap;
use std::time::SystemTime;

use atuin_client::ai_session::{HarnessKind, HarnessSession, Session};
use atuin_client::settings::Settings;
use atuin_common::harnesstools::AnyHarness;
use atuin_common::harnesstools::presence::{self, Activity, Presence};
use atuin_common::harnesstools::sync::{Liveness, Processes, SessionSync as _};
use futures::StreamExt as _;

/// How long a thread can go without activity and still be checked, when its agent has no
/// lifecycle hooks.
///
/// A listing can hold hundreds of threads, and each check reads the agent's own records. An agent
/// without hooks that is left open but idle for longer than this is not reported.
pub const HORIZON: time::Duration = time::Duration::days(7);

/// How many threads are checked at once.
const CONCURRENT: usize = 16;

/// A thread that an agent on this machine has, or may have, open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Open {
    /// An agent has the thread open. It is process `pid`, when that is known. Its hooks say what
    /// it is doing, and since when, when they are installed.
    Now {
        pid: Option<u32>,
        activity: Option<(Activity, SystemTime)>,
    },
    /// A process of the thread's agent runs where the thread works, so it may have it open.
    Maybe,
}

impl Open {
    const fn of(liveness: Liveness) -> Option<Self> {
        match liveness {
            Liveness::Live { pid } => Some(Self::Now {
                pid,
                activity: None,
            }),
            Liveness::Unknown => Some(Self::Maybe),
            Liveness::NotLive => None,
        }
    }

    fn reported(presence: &Presence) -> Self {
        Self::Now {
            pid: Some(presence.pid),
            activity: Some((presence.activity, presence.since())),
        }
    }

    /// How the thread's state reads in a tool's output at `now`, such as
    /// `open now (pid 4242, idle for 3 hours)`.
    pub fn label_at(self, now: SystemTime) -> String {
        let (pid, activity) = match self {
            Self::Maybe => return "may be open".to_owned(),
            Self::Now { pid, activity } => (pid, activity),
        };
        let pid = pid.map(|pid| format!("pid {pid}"));
        let activity = activity.map(|(activity, since)| {
            let ago = now.duration_since(since).unwrap_or_default();
            // A turn is seldom long, so how long it has been working says little.
            if activity == Activity::Working || ago.as_secs() < 60 {
                activity.label().to_owned()
            } else {
                format!("{} for {}", activity.label(), span(ago))
            }
        });
        let details: Vec<String> = pid.into_iter().chain(activity).collect();
        if details.is_empty() {
            "open now".to_owned()
        } else {
            format!("open now ({})", details.join(", "))
        }
    }

    /// [`Self::label_at`] now.
    pub fn label(self) -> String {
        self.label_at(SystemTime::now())
    }
}

/// `duration` in its largest whole unit, such as `3 hours`.
fn span(duration: std::time::Duration) -> String {
    let minutes = duration.as_secs() / 60;
    let (n, unit) = match minutes {
        0..60 => (minutes, "minute"),
        60..1440 => (minutes / 60, "hour"),
        _ => (minutes / 1440, "day"),
    };
    format!(
        "{n} {unit}{}",
        if n == 1 {
            ""
        } else {
            "s"
        }
    )
}

/// The threads open on this machine.
pub type OpenThreads = HashMap<HarnessSession, Open>;

/// Which of `sessions` an agent on this machine has open now.
///
/// A thread whose agent's hooks report it is taken as they say. Of the rest, only this machine's
/// threads that were active within [`HORIZON`] are checked. Amp keeps its threads on its servers,
/// so they are never checked.
pub async fn open_among<'a>(
    settings: &Settings,
    sessions: impl IntoIterator<Item = &'a Session>,
) -> OpenThreads {
    let sessions: Vec<&Session> = sessions.into_iter().collect();
    if sessions.is_empty() {
        return OpenThreads::new();
    }
    let procs = Processes::here().listed();
    let reported = reported(&procs).await;
    let here = settings.host_id().await.ok();
    let cutoff = time::OffsetDateTime::now_utc() - HORIZON;

    let mut open = OpenThreads::new();
    let mut checks = Vec::new();
    for s in sessions {
        if let Some(presence) = reported_for(&reported, &s.handle) {
            open.insert(s.handle.clone(), Open::reported(presence));
            continue;
        }
        // A thread with no recorded host was captured before hosts were, so it is this one's.
        let local = s.host.is_none() || s.host == here;
        if !local || s.updated_at < cutoff || s.handle.harness == HarnessKind::Amp {
            continue;
        }
        // Each check owns what it reads. A future that borrowed the session would not be `Send`
        // for every lifetime, which the MCP server's tool futures must be.
        let (handle, cwd, procs) = (s.handle.clone(), s.cwd.clone(), procs.clone());
        checks.push(async move {
            let harness = handle.harness.harness()?;
            let id = handle.session.as_ref();
            let liveness = harness.is_live_among(id, cwd.as_deref(), &procs).await;
            Some((handle, Open::of(liveness)?))
        });
    }
    let checked: Vec<_> = futures::stream::iter(checks)
        .buffer_unordered(CONCURRENT)
        .filter_map(std::future::ready)
        .collect()
        .await;
    open.extend(checked);
    open
}

/// What the hooks report for the thread `handle`.
///
/// Codex's hooks name a thread by its id. Capture names a thread kept in several rollouts by the
/// thread's id and the rollout's (`<thread>_<rollout>`), so that is looked up by the thread's.
fn reported_for<'a>(
    reported: &'a HashMap<HarnessSession, Presence>,
    handle: &HarnessSession,
) -> Option<&'a Presence> {
    reported.get(handle).or_else(|| {
        let (thread, _) = handle.session.as_ref().split_once('_')?;
        let thread = HarnessSession {
            harness: handle.harness,
            session: thread.to_owned().into(),
        };
        (handle.harness == HarnessKind::Codex).then(|| reported.get(&thread)).flatten()
    })
}

/// The sessions that agents' hooks report open, by thread.
pub async fn reported(procs: &Processes) -> HashMap<HarnessSession, Presence> {
    let procs = procs.clone();
    let live = tokio::task::spawn_blocking(move || presence::live(&procs)).await;
    live.unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            let harness = HarnessKind::from(&AnyHarness::from_name(&p.harness).ok()?);
            let handle = HarnessSession {
                harness,
                session: p.session.clone().into(),
            };
            Some((handle, p))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::exact(Liveness::Live { pid: Some(4242) }, Some("open now (pid 4242)"))]
    #[case::without_a_pid(Liveness::Live { pid: None }, Some("open now"))]
    #[case::unsure(Liveness::Unknown, Some("may be open"))]
    #[case::closed(Liveness::NotLive, None)]
    fn liveness_reads_as_a_label(#[case] liveness: Liveness, #[case] expected: Option<&str>) {
        assert_eq!(Open::of(liveness).map(Open::label).as_deref(), expected);
    }

    #[rstest]
    #[case::thread(HarnessKind::Codex, "t1", true)]
    #[case::rollout_of_the_thread(HarnessKind::Codex, "t1_r2", true)]
    #[case::other_thread(HarnessKind::Codex, "t2_r2", false)]
    #[case::not_codex(HarnessKind::ClaudeCode, "t1_r2", false)]
    fn a_codex_rollout_is_found_by_its_thread(
        #[case] harness: HarnessKind,
        #[case] id: &str,
        #[case] found: bool,
    ) {
        let handle = |harness, id: &str| HarnessSession {
            harness,
            session: id.to_owned().into(),
        };
        let presence = Presence {
            harness: "codex".to_owned(),
            session: "t1".to_owned(),
            pid: 7,
            pid_start: None,
            activity: Activity::Idle,
            since_ms: 0,
            cwd: None,
        };
        let reported = HashMap::from([(handle(HarnessKind::Codex, "t1"), presence)]);
        assert_eq!(reported_for(&reported, &handle(harness, id)).is_some(), found);
    }

    #[rstest]
    #[case::working(Activity::Working, 7200, "open now (pid 7, working)")]
    #[case::just_idle(Activity::Idle, 30, "open now (pid 7, idle)")]
    #[case::idle_for_hours(Activity::Idle, 3 * 3600 + 59, "open now (pid 7, idle for 3 hours)")]
    #[case::waiting_a_day(Activity::NeedsInput, 86_400, "open now (pid 7, needs input for 1 day)")]
    fn a_reported_thread_says_what_its_agent_is_doing(
        #[case] activity: Activity,
        #[case] ago: u64,
        #[case] expected: &str,
    ) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let open = Open::Now {
            pid: Some(7),
            activity: Some((activity, now - Duration::from_secs(ago))),
        };
        assert_eq!(open.label_at(now), expected);
    }
}
