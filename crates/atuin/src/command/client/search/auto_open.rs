//! The automatic Bash picker keeps literal command submission separate from history selection.

use std::sync::Arc;
use std::time::Duration;

use atuin_client::database::{Sqlite, current_context};
use atuin_client::settings::Settings;
use atuin_client::tui::cursor::Cursor;
use atuin_common::string::EscapeNonPrintablePosixExt as _;
use eyre::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::UnicodeWidthStr as _;

use super::engines::{SearchEngine as _, SearchState, engine};
use super::interactive::Stdout;

#[derive(Debug, PartialEq, Eq, derive_more::Display)]
enum Action {
    #[display("edit")]
    Edit,
    #[display("submit")]
    Submit,
    #[display("complete")]
    Complete,
    #[display("search")]
    Search,
    #[display("eof")]
    Eof,
    #[display("key-{_0:03o}")]
    Key(u8),
    #[display("alt-{_0:03o}")]
    Alt(u8),
    #[display("backtab")]
    BackTab,
}

/// A final sentinel keeps command substitution from removing trailing newlines from the input.
fn frame(action: &Action, input: &Cursor) -> String {
    format!("{action}\n{}\n{}\u{1f}", input.position(), input.as_str())
}

struct Editor {
    input: Cursor,
    selected: Option<usize>,
}

impl Editor {
    fn new(query: String) -> Self {
        let mut input = Cursor::from(query);
        input.end();
        Self {
            input,
            selected: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle(&mut self, event: Event, results: &[String], settings: &Settings) -> Option<Action> {
        let previous = self.input.as_str().to_owned();
        let action = match event {
            Event::Paste(text) => {
                for c in text.chars() {
                    self.input.insert(c);
                }
                // Bash handles pasted multiline commands and their continuation prompts.
                text.contains('\n').then_some(Action::Edit)
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Enter => {
                        if let Some(command) = self.selected.and_then(|i| results.get(i)) {
                            self.input = Cursor::from(command.clone());
                            self.input.end();
                            Some(if settings.enter_accept {
                                Action::Submit
                            } else {
                                Action::Edit
                            })
                        } else {
                            Some(Action::Submit)
                        }
                    }
                    KeyCode::Tab => Some(Action::Complete),
                    KeyCode::BackTab => Some(Action::BackTab),
                    KeyCode::Char('r') if control => Some(Action::Search),
                    KeyCode::Char('c' | 'g') if control => {
                        self.input.clear();
                        Some(Action::Edit)
                    }
                    KeyCode::Char('d') if control && self.input.as_str().is_empty() => {
                        Some(Action::Eof)
                    }
                    KeyCode::Up | KeyCode::Down => {
                        if !results.is_empty() {
                            self.selected = Some(match (self.selected, key.code) {
                                (None, _) => 0,
                                (Some(i), KeyCode::Up) => i.saturating_sub(1),
                                (Some(i), _) => (i + 1).min(results.len() - 1),
                            });
                        }
                        None
                    }
                    KeyCode::Left => {
                        self.input.left();
                        None
                    }
                    KeyCode::Right => {
                        self.input.right();
                        None
                    }
                    KeyCode::Home => {
                        self.input.start();
                        None
                    }
                    KeyCode::End => {
                        self.input.end();
                        None
                    }
                    KeyCode::Backspace => {
                        self.input.back();
                        None
                    }
                    KeyCode::Delete => {
                        self.input.remove();
                        None
                    }
                    KeyCode::Char('a') if control => {
                        self.input.start();
                        None
                    }
                    KeyCode::Char('e') if control => {
                        self.input.end();
                        None
                    }
                    KeyCode::Char('b') if control => {
                        self.input.left();
                        None
                    }
                    KeyCode::Char('f') if control => {
                        self.input.right();
                        None
                    }
                    KeyCode::Char('u') if control => {
                        self.input.clear_to_start();
                        None
                    }
                    KeyCode::Char('k') if control => {
                        self.input.clear_to_end();
                        None
                    }
                    KeyCode::Char('w') if control => {
                        self.input.remove_prev_word(&settings.word_chars, settings.word_jump_mode);
                        None
                    }
                    KeyCode::Char('d') if control => {
                        self.input.remove();
                        None
                    }
                    KeyCode::Char(c) if control && c.is_ascii() => {
                        Some(Action::Key(u8::try_from(c).unwrap() & 0x1f))
                    }
                    KeyCode::Char(c)
                        if key.modifiers.contains(KeyModifiers::ALT) && c.is_ascii() =>
                    {
                        Some(Action::Alt(u8::try_from(c).unwrap()))
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) =>
                    {
                        self.input.insert(c);
                        None
                    }
                    // Unhandled editing shortcuts return control to Bash instead of guessing.
                    _ => Some(Action::Edit),
                }
            }
            _ => None,
        };
        if self.input.as_str() != previous {
            self.selected = None;
        }
        action.or_else(|| self.input.as_str().is_empty().then_some(Action::Edit))
    }
}

pub(super) async fn history(
    query: String,
    settings: &Arc<Settings>,
    mut db: Sqlite,
) -> Result<String> {
    let options = &settings.search.auto_open;
    let height = options.max_results.get().min(20) + 2;
    let stdout = Stdout::new(true, true)?;
    let mut terminal = Terminal::with_options(CrosstermBackend::new(stdout), TerminalOptions {
        viewport: Viewport::Inline(height),
    })?;
    let result = async {
        let mut editor = Editor::new(query);
        let mut backend = engine(options.search_mode.into(), settings);
        let context = current_context(settings).await?;
        let mut results = Vec::<String>::new();
        let mut previous_query = None;
        loop {
            terminal.draw(|f| {
                let chunks = Layout::vertical([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(f.area());
                let prefix = editor.input.substring().escape_non_printable();
                let col = u16::try_from(prefix.width().saturating_add(2)).unwrap_or(u16::MAX);
                let scroll = col.saturating_sub(chunks[0].width.saturating_sub(1));
                f.render_widget(
                    Paragraph::new(Line::from(format!(
                        "> {}",
                        editor.input.as_str().escape_non_printable()
                    )))
                    .scroll((0, scroll)),
                    chunks[0],
                );
                let items =
                    results.iter().map(|command| ListItem::new(command.escape_non_printable()));
                let list = List::new(items)
                    .highlight_symbol("> ")
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
                let mut selection = ListState::default().with_selected(editor.selected);
                if results.is_empty() {
                    let message = if previous_query.is_none() {
                        "Searching history…"
                    } else {
                        "No matching history. Keep typing your command."
                    };
                    f.render_widget(Paragraph::new(message), chunks[1]);
                } else {
                    f.render_stateful_widget(list, chunks[1], &mut selection);
                }
                let enter = match (editor.selected, settings.enter_accept) {
                    (Some(_), true) => "run selected command",
                    (Some(_), false) => "insert selection",
                    (None, _) => "run typed command",
                };
                f.render_widget(
                    Paragraph::new(format!(
                        "Atuin auto | Enter: {enter} | arrows: select | Tab: complete | Esc: edit"
                    )),
                    chunks[2],
                );
                f.set_cursor_position((chunks[0].x + col.saturating_sub(scroll), chunks[0].y));
            })?;
            if previous_query.as_deref() != Some(editor.input.as_str()) {
                let state = SearchState {
                    input: Cursor::from(editor.input.as_str().to_owned()),
                    filter_mode: options.filter_mode,
                    context: context.clone(),
                    custom_context: None,
                    shells: settings.search.shells.clone(),
                };
                results = match tokio::time::timeout(
                    Duration::from_millis(250),
                    backend.query(&state, &mut db),
                )
                .await
                {
                    Ok(Ok(history)) => history
                        .into_iter()
                        .take(usize::from(options.max_results.get().min(20)))
                        .map(|entry| entry.command)
                        .collect(),
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "automatic history query failed");
                        Vec::new()
                    }
                    Err(_) => Vec::new(),
                };
                previous_query = Some(editor.input.as_str().to_owned());
                continue;
            }
            if let Some(action) = editor.handle(event::read()?, &results, settings) {
                break Ok::<_, eyre::Report>(frame(&action, &editor.input));
            }
        }
    }
    .await;
    // Clear the panel on both successful handoff and errors before restoring terminal modes.
    terminal.clear()?;
    let origin = terminal.get_frame().area().as_position();
    terminal.set_cursor_position(origin)?;
    result
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ratatui::crossterm::event::KeyEvent;
    use rstest::{fixture, rstest};

    use super::*;

    #[fixture]
    fn settings() -> Settings {
        Settings::utc()
    }

    #[fixture]
    fn results() -> Vec<String> {
        vec!["ssh old-host".into(), "ssh other-host".into()]
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[rstest]
    #[case(false)]
    #[case(true)]
    fn enter_submits_literal_input_without_implicit_selection(
        mut settings: Settings,
        results: Vec<String>,
        #[case] accept: bool,
    ) {
        settings.enter_accept = accept;
        let mut editor = Editor::new("ssh new-host".into());
        assert_eq!(editor.handle(key(KeyCode::Enter), &results, &settings), Some(Action::Submit));
        assert_eq!(editor.input.as_str(), "ssh new-host");
    }

    #[rstest]
    #[case(false, Action::Edit)]
    #[case(true, Action::Submit)]
    fn selection_respects_enter_accept(
        mut settings: Settings,
        results: Vec<String>,
        #[case] accept: bool,
        #[case] expected: Action,
    ) {
        settings.enter_accept = accept;
        let mut editor = Editor::new("ssh".into());
        assert_eq!(editor.handle(key(KeyCode::Down), &results, &settings), None);
        assert_eq!(editor.handle(key(KeyCode::Enter), &results, &settings), Some(expected));
        assert_eq!(editor.input.as_str(), "ssh old-host");
    }

    #[rstest]
    fn editing_clears_selection(settings: Settings, results: Vec<String>) {
        let mut editor = Editor::new("ssh".into());
        editor.handle(key(KeyCode::Up), &results, &settings);
        editor.handle(key(KeyCode::Char('x')), &results, &settings);
        assert_eq!(editor.selected, None);
        assert_eq!(editor.handle(key(KeyCode::Enter), &results, &settings), Some(Action::Submit));
        assert_eq!(editor.input.as_str(), "sshx");
    }

    #[rstest]
    #[case(KeyCode::Esc, Action::Edit)]
    #[case(KeyCode::Tab, Action::Complete)]
    fn handoff_keeps_input(settings: Settings, #[case] code: KeyCode, #[case] action: Action) {
        let mut editor = Editor::new("echo café".into());
        assert_eq!(editor.handle(key(code), &[], &settings), Some(action));
        assert_eq!(editor.input.as_str(), "echo café");
        assert_eq!(editor.input.position(), "echo café".len());
    }

    #[rstest]
    #[case("two words", None)]
    #[case("café 🦀", None)]
    #[case("one\ntwo\n", Some(Action::Edit))]
    fn paste_preserves_text(
        settings: Settings,
        #[case] text: &str,
        #[case] action: Option<Action>,
    ) {
        let mut editor = Editor::new("echo ".into());
        assert_eq!(editor.handle(Event::Paste(text.into()), &[], &settings), action);
        assert_eq!(editor.input.as_str(), format!("echo {text}"));
    }

    #[rstest]
    fn frame_preserves_arbitrary_text_and_byte_cursor() {
        proptest!(|(text in any::<String>())| {
            let editor = Editor::new(text.clone());
            let encoded = frame(&Action::Edit, &editor.input);
            let fields: Vec<_> = encoded.strip_suffix('\u{1f}').unwrap().splitn(3, '\n').collect();
            prop_assert_eq!(fields[0], "edit");
            prop_assert_eq!(fields[1].parse::<usize>().unwrap(), text.len());
            prop_assert_eq!(fields[2], text);
        });
    }
}
