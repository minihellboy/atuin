//! MCP prompts: ready-made requests that point an agent at Atuin.
//!
//! Claude Code offers each one as a slash command, such as `/mcp__atuin__continue`. A prompt
//! holds no data of its own. It tells the agent which tools to call, so the agent reads what it
//! needs when the prompt runs.

use rmcp::model::{
    ErrorData, GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, Role,
};

/// One prompt: its name, its description, its optional argument, and the request it makes.
struct Template {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    /// An optional argument, with its description.
    argument: Option<(&'static str, &'static str)>,
    /// The request, given the argument when the user passed one.
    text: fn(Option<&str>) -> String,
}

const TEMPLATES: &[Template] = &[
    Template {
        name: "continue",
        title: "Continue the last thread",
        description: "Pick up where the last agent thread in this project left off.",
        argument: Some(("topic", "What the thread was about, when it was not the last one.")),
        text: |topic| match topic {
            Some(topic) => format!(
                "Find the earlier agent thread in this project about {topic:?}: search with \
                 atuin_threads_search (cwd: '.'). Read how it ended with atuin_threads_read \
                 (start: -10). Tell me in a few lines what was done, what was decided and what \
                 was left, then carry on with what was left."
            ),
            None => "Find the last agent thread in this project with atuin_threads_list (cwd: \
                     '.', limit: 1). Read how it ended with atuin_threads_read (start: -10). Tell \
                     me in a few lines what was done, what was decided and what was left, then \
                     carry on with what was left."
                .to_owned(),
        },
    },
    Template {
        name: "why-failed",
        title: "Why did it fail?",
        description: "Explain the last command that failed here, from its captured output.",
        argument: Some(("command", "Words from the command, when it was not the last to fail.")),
        text: |command| {
            let which = command.map_or_else(
                || "the last command that failed in this project".to_owned(),
                |c| format!("the last failed command in this project matching {c:?}"),
            );
            format!(
                "Find {which} with atuin_history (only_failed: true, filter_modes: ['workspace'], \
                 limit: 1). Read its captured output with atuin_output, starting at the end. \
                 Explain why it failed and propose a fix. Don't run it again unless I ask."
            )
        },
    },
    Template {
        name: "agents",
        title: "What are my agents doing?",
        description: "List the agent threads open on this machine and what each is doing.",
        argument: None,
        text: |_| {
            "List the agent threads open on this machine now with atuin_threads_list (open: true). \
             For each, say in one line which agent it is, where it works, whether it is working, \
             idle or waiting for input, and what it is working on. Read a thread with \
             atuin_threads_read (start: -5) only when its title doesn't say what it is doing."
                .to_owned()
        },
    },
];

/// Every prompt, as `prompts/list` describes it.
pub fn list() -> Vec<Prompt> {
    TEMPLATES
        .iter()
        .map(|t| {
            let arguments = t.argument.map(|(name, description)| {
                vec![PromptArgument::new(name).with_description(description).with_required(false)]
            });
            Prompt::new(t.name, Some(t.description), arguments).with_title(t.title)
        })
        .collect()
}

/// Prompt `name`, filled in with `arguments`.
pub fn get(name: &str, arguments: Option<&JsonObject>) -> Result<GetPromptResult, ErrorData> {
    let template = TEMPLATES.iter().find(|t| t.name == name).ok_or_else(|| {
        ErrorData::invalid_params(format!("no Atuin prompt is called {name}"), None)
    })?;
    let argument = template.argument.and_then(|(arg, _)| {
        let value = arguments?.get(arg)?.as_str()?.trim();
        (!value.is_empty()).then_some(value)
    });
    let text = (template.text)(argument);
    Ok(GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
        .with_description(template.description))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    fn text(result: &GetPromptResult) -> String {
        serde_json::to_value(&result.messages[0]).unwrap()["content"]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[rstest]
    fn every_prompt_is_listed_and_can_be_got() {
        let names: Vec<String> = list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["continue", "why-failed", "agents"]);
        for name in &names {
            assert!(!text(&get(name, None).unwrap()).is_empty(), "{name}");
        }
    }

    #[rstest]
    #[case::given(Some(json!({"topic": "the flaky test"})), "about \"the flaky test\"")]
    #[case::blank(Some(json!({"topic": "  "})), "the last agent thread")]
    #[case::missing(None, "the last agent thread")]
    fn an_argument_narrows_the_request(
        #[case] args: Option<serde_json::Value>,
        #[case] says: &str,
    ) {
        let args = args.map(|a| a.as_object().unwrap().clone());
        let got = text(&get("continue", args.as_ref()).unwrap());
        assert!(got.contains(says), "{got}");
    }

    #[rstest]
    fn an_unknown_prompt_is_an_error() {
        assert!(get("nope", None).is_err());
    }

    /// Each prompt names only tools that the server has.
    #[rstest]
    fn prompts_name_real_tools() {
        let tools: Vec<String> = super::super::TOOLS.iter().map(|t| t.name.to_string()).collect();
        for name in ["continue", "why-failed", "agents"] {
            let got = text(&get(name, None).unwrap());
            for word in got.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                if word.starts_with("atuin_") {
                    assert!(tools.iter().any(|t| t == word), "{name} names {word}");
                }
            }
        }
    }
}
