//! Automatic history search in a real Bash terminal with isolated history and configuration.

#![cfg(unix)]

mod common;
#[path = "common/pty.rs"]
#[allow(dead_code, reason = "the terminal harness is shared with other e2e test binaries")]
mod pty;
#[path = "common/shell.rs"]
#[allow(dead_code, reason = "the shell harness is shared with other e2e test binaries")]
mod shell;

use std::path::PathBuf;

use atuin_common::path::PathBufExt as _;
use common::{SESSION, marker};
use rstest::{fixture, rstest};
use shell::{PROMPT, Shell};

#[fixture]
fn auto_shell() -> Shell {
    start("enter_accept = false\n[search.auto_open]\nenabled = true\n")
}

fn start(settings: &str) -> Shell {
    Shell::start(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).joined("tests/shells/bash-preexec.toml"),
        Some(settings),
    )
    .expect("Bash is required for automatic history search tests")
}

#[rstest]
fn threshold_waits_for_the_configured_character_count() {
    let shell = start("[search.auto_open]\nenabled = true\nmin_chars = 3\n");
    shell.pty.send_str("pr");
    assert!(!shell.pty.screen().contains("Atuin auto"));
    shell.pty.send_str("i");
    shell.pty.wait_for("Atuin auto");
}

#[rstest]
fn automatic_search_is_disabled_by_default() {
    let shell = start("");
    shell.pty.send_str("printf 'ordinary\\n'");
    assert!(!shell.pty.screen().contains("Atuin auto"));
    shell.pty.send_enter();
    shell.pty.wait_for_line("ordinary");
}

#[rstest]
fn automatic_search_works_inside_the_output_capture_proxy() {
    let shell = start("[search.auto_open]\nenabled = true\n[pty_proxy]\nenabled = true\n");
    let output = marker();
    shell.pty.send_str(&format!("printf '{output}\\n'"));
    shell.pty.wait_for("Atuin auto");
    shell.pty.send_enter();
    shell.pty.wait_for_line(&output);
    shell.pty.wait_for_prompt();
}

#[rstest]
fn alt_word_motion_returns_to_readline_and_replays_the_key(auto_shell: Shell) {
    auto_shell.pty.send_str("echo ab cd");
    auto_shell.pty.send(b"\x1bb");
    auto_shell
        .pty
        .wait_for_screen("Alt+b returned to Bash", |screen| !screen.contains("Atuin auto"));
    auto_shell.pty.send_str("X");
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line("ab Xcd");
}

#[rstest]
fn tab_replays_a_custom_readline_binding(auto_shell: Shell) {
    auto_shell.pty.send_line("bind '\"\\C-i\": \"-tab\"'");
    auto_shell.pty.send_str("echo value");
    auto_shell.pty.send(b"\t");
    auto_shell.pty.wait_for_screen("custom Tab binding replayed", |screen| {
        !screen.contains("Atuin auto") && screen.contains("echo value-tab")
    });
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line("value-tab");
}

#[rstest]
fn multiline_paste_in_the_picker_returns_to_bash_without_execution(auto_shell: Shell) {
    let first = marker();
    let second = marker();
    auto_shell.pty.send_str("printf '");
    auto_shell.pty.send(format!("\x1b[200~{first}\n{second}\n\x1b[201~").as_bytes());
    auto_shell.pty.wait_for_screen("multiline input returned to Bash", |screen| {
        !screen.contains("Atuin auto")
    });
    assert!(!auto_shell.pty.screen().lines().any(|line| line.trim() == first));
    auto_shell.pty.send_str("'");
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line(&first);
    auto_shell.pty.wait_for_line(&second);
}

#[rstest]
fn resize_keeps_literal_submission_working(auto_shell: Shell) {
    auto_shell.pty.send_str("printf 'resize-ok\\n'");
    auto_shell.pty.resize(6, 45);
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line("resize-ok");
}

#[rstest]
fn typing_opens_global_history_and_selection_returns_for_editing(auto_shell: Shell) {
    let saved = marker();
    let command = format!("printf '{saved}\\n'");
    let elsewhere = auto_shell.env.home().join("other-repository");
    std::fs::create_dir(&elsewhere).unwrap();
    auto_shell.env.record(&command, SESSION, &elsewhere);
    auto_shell.pty.send_str("printf");
    auto_shell.pty.wait_for("Atuin auto");
    auto_shell.pty.wait_for(&command);
    auto_shell.pty.send(b"\x1b[B\r");
    auto_shell.pty.wait_for_screen("selection returned to Bash", |screen| {
        !screen.contains("Atuin auto") && screen.contains(&format!("{PROMPT} {command}"))
    });
    assert!(!auto_shell.pty.screen().lines().any(|line| line.trim() == saved));
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line(&saved);
}

#[rstest]
#[case::paced(false)]
#[case::queued(true)]
fn enter_runs_literal_input_instead_of_a_history_match(auto_shell: Shell, #[case] queued: bool) {
    let old = marker();
    let new = marker();
    auto_shell.env.record(&format!("printf '{old}\\n'"), SESSION, auto_shell.env.home());
    let command = format!("printf '{new}\\n'");
    if queued {
        auto_shell.pty.send(format!("{command}\r").as_bytes());
    } else {
        auto_shell.pty.send_str(&command);
        auto_shell.pty.wait_for("Atuin auto");
        auto_shell.pty.send_enter();
    }
    auto_shell.pty.wait_for_line(&new);
    auto_shell.pty.wait_for_prompt();
    assert!(!auto_shell.pty.screen().lines().any(|line| line.trim() == old));
    common::wait_until("literal command recorded once", || {
        let mut list = auto_shell.env.atuin(&["history", "list", "--format", "{command}"]);
        list.env("ATUIN_SESSION", SESSION);
        common::output(list).lines().filter(|line| *line == command).count() == 1
    });
}

#[rstest]
fn escape_keeps_unicode_and_dismisses_until_the_next_prompt(auto_shell: Shell) {
    let output = format!("café-🦀-{}", marker());
    auto_shell.pty.send_str(&format!("printf '{output}"));
    auto_shell.pty.send(b"\x1b");
    auto_shell.pty.wait_for_screen("Escape returned the input", |screen| {
        !screen.contains("Atuin auto") && screen.contains(&format!("{PROMPT} printf '{output}"))
    });
    auto_shell.pty.send_str("\\n'");
    assert!(!auto_shell.pty.screen().contains("Atuin auto"));
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line(&output);
    auto_shell.pty.wait_for_prompt();
    auto_shell.pty.send_str("p");
    auto_shell.pty.wait_for("Atuin auto");
}

#[rstest]
fn tab_hands_the_current_line_to_readline_completion(auto_shell: Shell) {
    std::fs::write(auto_shell.env.home().join("auto-complete-target"), "data").unwrap();
    auto_shell.pty.send_str("cat auto-complete-tar");
    auto_shell.pty.send(b"\t");
    auto_shell.pty.wait_for_screen("Bash completed the filename", |screen| {
        !screen.contains("Atuin auto") && screen.contains("cat auto-complete-target")
    });
}

#[rstest]
#[case::private_command(" printf 'private\\n'", "private")]
#[case::bracketed_paste("\x1b[200~printf 'pasted\\n'\x1b[201~", "pasted")]
fn native_input_bypasses_the_picker(
    auto_shell: Shell,
    #[case] input: &str,
    #[case] expected: &str,
) {
    auto_shell.pty.send(input.as_bytes());
    auto_shell.pty.wait_for(expected);
    assert!(!auto_shell.pty.screen().contains("Atuin auto"));
    auto_shell.pty.send_enter();
    auto_shell.pty.wait_for_line(expected);
}
