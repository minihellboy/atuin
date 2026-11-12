//! MCP resources: past threads, the threads open now, and the captured output of a command, by
//! URI.
//!
//! An agent can read a resource in without calling a tool. Claude Code lists them when the user
//! types `@`, so the user can mention a past thread in a prompt (`@atuin:atuin://thread/<id>`).
//! Each resource reads as the matching tool's output does.

use atuin_client::settings::Settings;
use rmcp::model::{ErrorData, Resource, ResourceTemplate};
use serde_json::json;

use crate::tools::session::caller::{Caller, is_own};
use crate::tools::session::list::AtuinThreadsListToolCall;
use crate::tools::session::read::AtuinThreadsReadToolCall;
use crate::tools::session::{connect, is_subagent, label, list_sessions};
use crate::tools::{AtuinOutputToolCall, ToolOutcome};

/// A thread, by its atuin id or a unique prefix of it.
const THREAD: &str = "atuin://thread/";
/// The captured output of a command, by its history id.
const OUTPUT: &str = "atuin://output/";
/// The threads that agents on this machine have open now.
const OPEN: &str = "atuin://threads/open";

/// How many recent threads the resource list names.
const LISTED_THREADS: usize = 10;

/// How many messages, back from the end, a thread resource reads.
const THREAD_TAIL: i64 = 40;

const TEXT: &str = "text/plain";

/// The resource templates: any thread, and any command's output.
pub fn templates() -> Vec<ResourceTemplate> {
    vec![
        ResourceTemplate::new(format!("{THREAD}{{id}}"), "thread")
            .with_title("Agent thread")
            .with_description(format!(
                "The last {THREAD_TAIL} messages of a captured agent thread, by its atuin id or a \
                 unique prefix of it, or 'latest' for the last thread before this one."
            ))
            .with_mime_type(TEXT),
        ResourceTemplate::new(format!("{OUTPUT}{{history_id}}"), "output")
            .with_title("Command output")
            .with_description(
                "The captured terminal output of a command, by the history id that atuin_history \
                 gives it.",
            )
            .with_mime_type(TEXT),
    ]
}

/// The resources worth naming up front: the open threads, and the most recent threads.
///
/// Errors leave the recent threads out. The list is a convenience, and a daemon that is not
/// running must not fail it.
pub async fn list(settings: &Settings, caller: &Caller<'_>) -> Vec<Resource> {
    let mut resources = vec![
        Resource::new(OPEN, "open-threads")
            .with_title("Threads open now")
            .with_description("The threads that agents on this machine have open now.")
            .with_mime_type(TEXT),
    ];
    let Ok(mut client) = connect(settings).await else {
        return resources;
    };
    let Ok(sessions) = list_sessions(&mut client, None, None).await else {
        return resources;
    };
    let own = caller.own_in(&sessions);
    let recent = sessions
        .iter()
        .filter(|s| !is_own(s, own.as_ref()) && !is_subagent(s))
        .take(LISTED_THREADS);
    resources.extend(recent.map(|s| {
        let title = label(s);
        let mut resource = Resource::new(format!("{THREAD}{}", s.atuin_id), s.atuin_id.to_string())
            .with_mime_type(TEXT);
        if !title.is_empty() {
            resource = resource.with_title(title);
        }
        match &s.cwd {
            Some(cwd) => resource.with_description(format!("A thread in {}", cwd.display())),
            None => resource,
        }
    }));
    resources
}

/// The text of the resource at `uri`.
pub async fn read(
    uri: &str,
    settings: &Settings,
    caller: &Caller<'_>,
) -> Result<String, ErrorData> {
    let outcome = if uri == OPEN {
        let call: AtuinThreadsListToolCall = parse(json!({ "open": true, "limit": 50 }))?;
        call.execute(settings, caller).await
    } else if let Some(id) = uri.strip_prefix(THREAD).filter(|id| !id.is_empty()) {
        let call: AtuinThreadsReadToolCall =
            parse(json!({ "session_id": id, "start": -THREAD_TAIL, "limit": THREAD_TAIL }))?;
        call.execute(settings, caller).await
    } else if let Some(id) = uri.strip_prefix(OUTPUT).filter(|id| !id.is_empty()) {
        let call = AtuinOutputToolCall::try_from(&json!({ "history_id": id }))
            .map_err(|e| ErrorData::resource_not_found(e.to_string(), None))?;
        call.execute().await
    } else {
        return Err(ErrorData::resource_not_found(format!("no Atuin resource is {uri}"), None));
    };
    match outcome {
        ToolOutcome::Success(text) => Ok(text),
        ToolOutcome::Error(text) => Err(ErrorData::resource_not_found(text, None)),
        outcome @ ToolOutcome::Structured { .. } => Ok(outcome.format_for_llm(None)),
    }
}

fn parse<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, ErrorData> {
    serde_json::from_value(args).map_err(|e| ErrorData::internal_error(e.to_string(), None))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Every template names a URI that [`read`] knows.
    #[rstest]
    fn templates_name_the_uris_that_are_read() {
        let templates: Vec<String> = templates().into_iter().map(|t| t.uri_template).collect();
        assert_eq!(templates, ["atuin://thread/{id}", "atuin://output/{history_id}"]);
    }

    /// The arguments each resource passes its tool are ones the tool takes.
    #[rstest]
    fn each_resource_passes_its_tool_valid_arguments() {
        parse::<AtuinThreadsListToolCall>(json!({ "open": true, "limit": 50 })).unwrap();
        let read = json!({ "session_id": "abc", "start": -THREAD_TAIL, "limit": THREAD_TAIL });
        parse::<AtuinThreadsReadToolCall>(read).unwrap();
        AtuinOutputToolCall::try_from(&json!({ "history_id": "018f0000000070008000000000000000" }))
            .unwrap();
    }
}
