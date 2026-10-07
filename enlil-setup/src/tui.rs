//! Interactive [`Prompter`](super::Prompter) built on ratatui.
//!
//! Every wizard question gets a fullscreen screen: an editable line for text
//! and numbers, a navigable list for multi-choice, and a yes/no toggle for
//! confirmations. `Esc` aborts the wizard from any screen; the terminal is
//! restored when the prompter is dropped.

use std::io;

use anyhow::{Result, anyhow, bail};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{Prompter, parse_bounded_u64};

/// Fullscreen interactive wizard prompter.
///
/// Takes over the terminal on creation and restores it on drop — wrap the
/// wizard run in a block so summaries print on a clean terminal afterwards.
pub struct TuiPrompter {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TuiPrompter {
    /// Take over the terminal for the interactive wizard.
    ///
    /// # Errors
    ///
    /// Returns an error if raw mode or the alternate screen cannot be enabled.
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }

    /// Draw one prompt screen: a titled header, the body, and a help footer
    /// with an optional validation error line.
    fn draw(
        &mut self,
        prompt: &str,
        body: Vec<Line<'static>>,
        footer: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.terminal.draw(|frame| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(4),
                ])
                .split(frame.area());
            let header = Paragraph::new(prompt).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Enlil setup wizard "),
            );
            frame.render_widget(header, rows[0]);
            let content = Paragraph::new(body).wrap(Wrap { trim: true });
            frame.render_widget(content, rows[1]);
            let mut footer_lines = vec![Line::from(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            ))];
            if let Some(message) = error {
                footer_lines.push(Line::from(Span::styled(
                    message,
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
            }
            frame.render_widget(Paragraph::new(footer_lines), rows[2]);
        })?;
        Ok(())
    }

    /// Draw a fullscreen stateful list with a help footer.
    fn draw_list(&mut self, list: List<'_>, state: &mut ListState, footer: &str) -> Result<()> {
        self.terminal.draw(|frame| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(4), Constraint::Length(3)])
                .split(frame.area());
            frame.render_stateful_widget(list, rows[0], state);
            let help = Paragraph::new(Line::from(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(help, rows[1]);
        })?;
        Ok(())
    }

    /// Editable single-line text field with live validation.
    fn prompt_text(
        &mut self,
        prompt: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String> {
        let mut field = TextField::with_default(default);
        let mut error: Option<String> = None;
        let header = format!("{prompt}\n(default: {default})");
        loop {
            self.draw(
                &header,
                vec![field.render()],
                "Type to edit \u{2022} arrows move \u{2022} Enter confirms \u{2022} Esc cancels",
                error.as_deref(),
            )?;
            match read_key()? {
                KeyCode::Enter => {
                    let value = field.value_or(default);
                    match validate(&value) {
                        Ok(()) => return Ok(value),
                        Err(message) => error = Some(message),
                    }
                }
                KeyCode::Esc => bail!("wizard cancelled"),
                KeyCode::Char(c) => {
                    field.insert(c);
                    error = None;
                }
                KeyCode::Backspace => {
                    field.backspace();
                    error = None;
                }
                KeyCode::Delete => {
                    field.delete();
                    error = None;
                }
                KeyCode::Left => field.move_left(),
                KeyCode::Right => field.move_right(),
                KeyCode::Home => field.move_to_start(),
                KeyCode::End => field.move_to_end(),
                _ => {}
            }
        }
    }

    /// Number entry with range validation, on top of [`Self::prompt_text`].
    fn prompt_number(&mut self, prompt: &str, default: u64, min: u64, max: u64) -> Result<u64> {
        let ranged = format!("{prompt} [{min}\u{2013}{max}]");
        let text = self.prompt_text(&ranged, &default.to_string(), &|input| {
            parse_bounded_u64(input, min, max).map(|_| ())
        })?;
        // Already validated by the prompt above; a failure here is a bug.
        parse_bounded_u64(text.trim(), min, max).map_err(|message| anyhow!(message))
    }

    /// Single-choice list; returns the selected index.
    fn prompt_choice(&mut self, prompt: &str, options: &[String], default: usize) -> Result<usize> {
        if options.is_empty() {
            bail!("no options to choose from");
        }
        let mut state = ListState::default();
        state.select(Some(default.min(options.len() - 1)));
        loop {
            let items: Vec<ListItem> = options
                .iter()
                .map(|option| ListItem::new(Line::from(option.as_str())))
                .collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(prompt))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("\u{25b6} ");
            self.draw_list(
                list,
                &mut state,
                "\u{2191}/\u{2193} select \u{2022} Enter confirms \u{2022} Esc cancels",
            )?;
            match read_key()? {
                KeyCode::Enter => {
                    if let Some(index) = state.selected() {
                        return Ok(index);
                    }
                }
                KeyCode::Esc => bail!("wizard cancelled"),
                KeyCode::Up | KeyCode::Char('k') => state.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => state.select_next(),
                _ => {}
            }
        }
    }

    /// Multi-choice list; returns the selected indices.
    fn prompt_multi_choice(&mut self, prompt: &str, options: &[String]) -> Result<Vec<usize>> {
        if options.is_empty() {
            bail!("no options to choose from");
        }
        let mut checked = vec![false; options.len()];
        let mut state = ListState::default();
        state.select(Some(0));
        loop {
            let items: Vec<ListItem> = options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    let mark = if checked[index] { "[x]" } else { "[ ]" };
                    ListItem::new(Line::from(format!("{mark} {option}")))
                })
                .collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(prompt))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("\u{25b6} ");
            self.draw_list(
                list,
                &mut state,
                "\u{2191}/\u{2193} move \u{2022} Space toggles \u{2022} Enter confirms \u{2022} Esc cancels",
            )?;
            match read_key()? {
                KeyCode::Enter => {
                    let mut chosen = Vec::new();
                    for (index, &is_checked) in checked.iter().enumerate() {
                        if is_checked {
                            chosen.push(index);
                        }
                    }
                    return Ok(chosen);
                }
                KeyCode::Esc => bail!("wizard cancelled"),
                KeyCode::Char(' ') => {
                    if let Some(index) = state.selected() {
                        checked[index] = !checked[index];
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => state.select_previous(),
                KeyCode::Down | KeyCode::Char('j') => state.select_next(),
                _ => {}
            }
        }
    }

    /// Yes/no confirmation, on top of [`Self::prompt_choice`].
    fn prompt_confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let options = [String::from("Yes"), String::from("No")];
        let chosen = self.prompt_choice(prompt, &options, usize::from(!default))?;
        Ok(chosen == 0)
    }

    /// Informational screen; any of Enter/Esc continues.
    fn prompt_info(&mut self, title: &str, lines: &[String]) -> Result<()> {
        let body: Vec<Line> = lines.iter().cloned().map(Line::from).collect();
        self.draw(title, body, "Enter to continue", None)?;
        loop {
            match read_key()? {
                KeyCode::Enter | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}

impl Drop for TuiPrompter {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

impl Prompter for TuiPrompter {
    fn info(&mut self, title: &str, lines: &[String]) -> Result<()> {
        self.prompt_info(title, lines)
    }

    fn text(
        &mut self,
        prompt: &str,
        default: &str,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String> {
        self.prompt_text(prompt, default, validate)
    }

    fn number(&mut self, prompt: &str, default: u64, min: u64, max: u64) -> Result<u64> {
        self.prompt_number(prompt, default, min, max)
    }

    fn multi_choice(&mut self, prompt: &str, options: &[String]) -> Result<Vec<usize>> {
        self.prompt_multi_choice(prompt, options)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        self.prompt_confirm(prompt, default)
    }
}

/// Block until a key press arrives, ignoring releases and non-key events.
fn read_key() -> Result<KeyCode> {
    loop {
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            return Ok(key.code);
        }
    }
}

/// Editable single-line input with a character cursor.
struct TextField {
    chars: Vec<char>,
    cursor: usize,
}

impl TextField {
    fn with_default(default: &str) -> Self {
        let chars: Vec<char> = default.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
        }
    }

    /// Current value, falling back to `default` when the field is blank —
    /// clearing the line is how the user accepts the default.
    fn value_or(&self, default: &str) -> String {
        let value: String = self.chars.iter().collect();
        if value.trim().is_empty() {
            default.to_string()
        } else {
            value
        }
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    const fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    const fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    const fn move_to_end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Render the line with the character under the cursor in reverse video.
    fn render(&self) -> Line<'static> {
        let mut spans = Vec::with_capacity(self.chars.len() + 1);
        for (index, c) in self.chars.iter().enumerate() {
            let style = if index == self.cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        if self.cursor == self.chars.len() {
            spans.push(Span::styled(
                " ".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
        Line::from(spans)
    }
}
