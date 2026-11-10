use anyhow::{Context, Result, bail};
use base64::Engine;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{IsTerminal, Stdout, stdout};
use std::process::Stdio;
use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(700);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const EVENT_POLL: Duration = Duration::from_millis(100);
const DETAIL_HEIGHT: u16 = 10;

#[derive(Clone, Debug, Deserialize)]
struct Session {
    id: String,
    name: String,
    status: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    live_cwd: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    created_ms: i64,
    #[serde(default)]
    last_active_ms: i64,
    #[serde(default)]
    attached: u64,
    /// The session this browser is running inside.
    #[serde(default)]
    here: bool,
}

impl Session {
    fn cwd(&self) -> &str {
        self.live_cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or(&self.cwd)
    }

    fn heading(&self) -> &str {
        self.title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or(&self.name)
    }

    fn running(&self) -> bool {
        self.status == "running"
    }

    fn matches(&self, query: &str) -> bool {
        [
            &self.id,
            &self.name,
            &self.status,
            &self.kind,
            self.cwd(),
            self.heading(),
            self.agent_session_id.as_deref().unwrap_or(""),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(query))
    }
}

/// One place the search text was found inside a session's history.
#[derive(Clone, Debug, Deserialize)]
struct Hit {
    id: String,
    source: String,
    text: String,
}

/// A scrollable page of text: saved output, or the last screen of a session that has ended.
struct TextView {
    id: String,
    name: String,
    /// "logs" or "screen": which RPC refills it.
    source: &'static str,
    text: String,
    wrapped: Vec<String>,
    width: u16,
    scroll: usize,
    /// Stay at the end as new output arrives.
    follow: bool,
}

impl TextView {
    fn new(id: String, name: String, source: &'static str, text: String) -> Self {
        Self {
            id,
            name,
            source,
            text,
            wrapped: vec![],
            width: 0,
            scroll: 0,
            follow: true,
        }
    }

    fn max_scroll(&self, height: u16) -> usize {
        self.wrapped.len().saturating_sub(height as usize)
    }

    /// Wraps for the given width and keeps the scroll position inside the text.
    fn layout(&mut self, width: u16, height: u16) {
        if self.width != width || self.wrapped.is_empty() {
            self.wrapped = wrap(&self.text, width as usize);
            self.width = width;
        }
        let max = self.max_scroll(height);
        if self.follow || self.scroll > max {
            self.scroll = max;
        }
    }

    fn set_text(&mut self, text: String) {
        if text != self.text {
            self.text = text;
            self.wrapped.clear();
        }
    }
}

/// Splits text into rows no wider than `width`, so scrolling can count real rows.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for line in safe_text(text).replace('\t', "    ").lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
        }
        for chunk in chars.chunks(width) {
            out.push(chunk.iter().collect());
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ConfirmKind {
    Stop,
    Delete,
}

struct Confirm {
    kind: ConfirmKind,
    id: String,
    label: String,
}

enum PromptKind {
    New,
    Rename(String),
}

struct Prompt {
    kind: PromptKind,
    text: String,
}

/// The selected session's screen, kept as a terminal so it can be drawn with its colours.
struct Preview {
    id: String,
    parser: vt100::Parser,
}

#[derive(Default)]
struct Browser {
    sessions: Vec<Session>,
    visible: Vec<usize>,
    selection: ListState,
    query: String,
    searching: bool,
    /// Content matches for `hits_for`, by session id.
    hits: HashMap<String, Vec<Hit>>,
    hits_for: String,
    view: Option<TextView>,
    confirm: Option<Confirm>,
    prompt: Option<Prompt>,
    notice: Option<String>,
    preview: Option<Preview>,
    /// Rows available to the text view, from the last draw.
    view_height: u16,
}

impl Browser {
    fn selected(&self) -> Option<&Session> {
        self.selection
            .selected()
            .and_then(|i| self.visible.get(i))
            .and_then(|&i| self.sessions.get(i))
    }

    fn rebuild(&mut self, previous_id: Option<&str>, previous_index: usize) {
        let query = self.query.to_lowercase();
        let hits = (self.hits_for == self.query).then_some(&self.hits);
        self.visible = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.matches(&query) || hits.is_some_and(|h| h.contains_key(&s.id)))
            .map(|(i, _)| i)
            .collect();
        let selected = previous_id
            .and_then(|id| self.visible.iter().position(|&i| self.sessions[i].id == id))
            .or_else(|| {
                (!self.visible.is_empty()).then(|| previous_index.min(self.visible.len() - 1))
            });
        self.selection.select(selected);
    }

    /// Running sessions first, then newest first. Activity does not reorder the list, so rows stay where they are.
    fn replace_sessions(&mut self, mut sessions: Vec<Session>) {
        let previous_id = self.selected().map(|s| s.id.clone());
        let previous_index = self.selection.selected().unwrap_or(0);
        sessions.sort_by(|a, b| {
            b.running()
                .cmp(&a.running())
                .then_with(|| b.created_ms.cmp(&a.created_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        self.sessions = sessions;
        self.rebuild(previous_id.as_deref(), previous_index);
    }

    fn update_filter(&mut self) {
        let previous_id = self.selected().map(|s| s.id.clone());
        self.rebuild(previous_id.as_deref(), 0);
    }

    fn set_hits(&mut self, query: String, hits: Vec<Hit>) {
        self.hits.clear();
        for hit in hits {
            self.hits.entry(hit.id.clone()).or_default().push(hit);
        }
        self.hits_for = query;
        self.update_filter();
    }

    fn selected_hits(&self) -> &[Hit] {
        if self.query.is_empty() || self.hits_for != self.query {
            return &[];
        }
        self.selected()
            .and_then(|s| self.hits.get(&s.id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn move_selection(&mut self, delta: i32) {
        if self.visible.is_empty() {
            self.selection.select(None);
            return;
        }
        let current = self.selection.selected().unwrap_or(0) as i64;
        let next = (current + delta as i64).clamp(0, self.visible.len() as i64 - 1);
        self.selection.select(Some(next as usize));
    }

    fn text_input(text: &mut String, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.push(c)
            }
            _ => return false,
        }
        true
    }

    fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if let Some(confirm) = self.confirm.take() {
            return match (key.code, confirm.kind) {
                (KeyCode::Char('y'), ConfirmKind::Stop) => Action::Stop(confirm.id),
                (KeyCode::Char('y'), ConfirmKind::Delete) => Action::Delete(confirm.id),
                _ => Action::None,
            };
        }
        if let Some(prompt) = &mut self.prompt {
            match key.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Enter => {
                    let prompt = self.prompt.take().expect("prompt is open");
                    return match prompt.kind {
                        PromptKind::New => Action::New(prompt.text),
                        PromptKind::Rename(id) => Action::Rename(id, prompt.text.trim().to_string()),
                    };
                }
                _ => {
                    Self::text_input(&mut prompt.text, key);
                }
            }
            return Action::None;
        }
        if let Some(view) = &mut self.view {
            let page = self.view_height.max(2) as usize - 1;
            let max = view.max_scroll(self.view_height);
            let to = match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => {
                    self.view = None;
                    return Action::None;
                }
                KeyCode::Up | KeyCode::Char('k') => view.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => view.scroll + 1,
                KeyCode::PageUp => view.scroll.saturating_sub(page),
                KeyCode::PageDown | KeyCode::Char(' ') => view.scroll + page,
                KeyCode::Home | KeyCode::Char('g') => 0,
                KeyCode::End | KeyCode::Char('G') => max,
                _ => return Action::None,
            };
            view.scroll = to.min(max);
            view.follow = view.scroll == max;
            return Action::None;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.searching = false;
                    self.update_filter();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Down => self.move_selection(1),
                _ => {
                    if Self::text_input(&mut self.query, key) {
                        self.update_filter();
                    }
                }
            }
            return Action::None;
        }
        let selected = self.selected().cloned();
        match (key.code, selected) {
            (KeyCode::Char('q'), _) => return Action::Quit,
            (KeyCode::Char('/'), _) => self.searching = true,
            (KeyCode::Esc, _) => {
                self.query.clear();
                self.update_filter();
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => self.move_selection(-1),
            (KeyCode::Down | KeyCode::Char('j'), _) => self.move_selection(1),
            (KeyCode::PageUp, _) => self.move_selection(-10),
            (KeyCode::PageDown, _) => self.move_selection(10),
            (KeyCode::Char('g') | KeyCode::Home, _) => self.move_selection(i32::MIN / 2),
            (KeyCode::Char('G') | KeyCode::End, _) => self.move_selection(i32::MAX / 2),
            (KeyCode::Char('n'), _) => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::New,
                    text: String::new(),
                })
            }
            (KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('s') | KeyCode::Char('d'), Some(s)) if s.here => {
                return Action::Notice("This is the session you are in. It cannot be entered, stopped, restarted or deleted from inside itself.".into());
            }
            (KeyCode::Char('l'), Some(s)) => {
                return Action::View(s.id.clone(), s.heading().to_string(), "logs");
            }
            (KeyCode::Enter, Some(s)) => {
                return if s.running() {
                    Action::Attach(s.id)
                } else {
                    // A session that has ended: its last screen is the readable record (raw output of a full-screen
                    // program is not).
                    Action::View(s.id.clone(), s.heading().to_string(), "screen")
                };
            }
            (KeyCode::Char('r'), Some(s)) => {
                return if s.running() {
                    Action::Notice("Session is already running. Press Enter to attach.".into())
                } else {
                    Action::Restart(s.id)
                };
            }
            (KeyCode::Char('R'), Some(s)) => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Rename(s.id),
                    text: s.name,
                })
            }
            (KeyCode::Char('s'), Some(s)) => {
                if !s.running() {
                    return Action::Notice("Session is already stopped.".into());
                }
                self.confirm = Some(Confirm {
                    kind: ConfirmKind::Stop,
                    label: s.heading().to_string(),
                    id: s.id,
                });
            }
            (KeyCode::Char('d'), Some(s)) => {
                self.confirm = Some(Confirm {
                    kind: ConfirmKind::Delete,
                    label: s.heading().to_string(),
                    id: s.id,
                });
            }
            _ => {}
        }
        Action::None
    }
}

enum Action {
    None,
    Quit,
    Attach(String),
    Restart(String),
    Stop(String),
    Delete(String),
    New(String),
    Rename(String, String),
    View(String, String, &'static str),
    Notice(String),
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("the session browser needs an interactive terminal; use 'dpty ls' for a list");
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
        let mut guard = Self {
            terminal,
            active: false,
        };
        guard.enter()?;
        Ok(guard)
    }

    fn enter(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            crossterm::cursor::Hide
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        self.active = true;
        self.terminal.clear()?;
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let raw = terminal::disable_raw_mode();
        let screen = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        raw?;
        screen?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

fn safe_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn ago(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let s = (crate::util::now_ms() - ms).max(0) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn status_style(session: &Session) -> (String, Color) {
    match session.status.as_str() {
        "running" => ("running".into(), Color::Green),
        "lost" => ("lost".into(), Color::Red),
        _ => match session.exit_code {
            Some(0) | None => (session.status.clone(), Color::DarkGray),
            Some(code) => (format!("exit {code}"), Color::Yellow),
        },
    }
}

fn term_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The first screen row to show when the screen is taller than the pane: keep the cursor and the last text in view.
fn preview_origin(screen: &vt100::Screen, height: u16) -> u16 {
    let (rows, cols) = screen.size();
    let last_text = (0..rows)
        .rev()
        .find(|&r| (0..cols).any(|c| screen.cell(r, c).is_some_and(|cell| cell.has_contents())))
        .unwrap_or(0);
    let bottom = last_text.max(screen.cursor_position().0);
    (bottom + 1).saturating_sub(height)
}

/// Draws a terminal screen into the pane, cell by cell, clipped to what fits.
fn draw_preview(frame: &mut Frame<'_>, area: Rect, screen: &vt100::Screen) {
    let (rows, cols) = screen.size();
    let top = preview_origin(screen, area.height);
    let buffer = frame.buffer_mut();
    for y in 0..area.height.min(rows.saturating_sub(top)) {
        for x in 0..area.width.min(cols) {
            let Some(cell) = screen.cell(top + y, x) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(target) = buffer.cell_mut((area.x + x, area.y + y)) else {
                continue;
            };
            let mut style = Style::default()
                .fg(term_color(cell.fgcolor()))
                .bg(term_color(cell.bgcolor()));
            for (on, modifier) in [
                (cell.bold(), Modifier::BOLD),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    style = style.add_modifier(modifier);
                }
            }
            let text = cell.contents();
            target.set_symbol(if text.is_empty() { " " } else { text });
            target.set_style(style);
        }
    }
}

fn draw(frame: &mut Frame<'_>, browser: &mut Browser) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let dim = Style::default().fg(Color::DarkGray);
    let mut header = vec![
        Span::styled(
            "dpty",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if browser.searching {
        header.push(Span::raw(format!("Search: {}_", safe_text(&browser.query))));
        header.push(Span::styled(
            "   names, titles, directories, output, commands, transcripts",
            dim,
        ));
    } else if browser.query.is_empty() {
        header.push(Span::styled(
            format!("{} sessions", browser.visible.len()),
            dim,
        ));
    } else {
        header.push(Span::raw(format!("Search: {}", safe_text(&browser.query))));
        header.push(Span::styled(
            format!("   {} found  |  Esc clear", browser.visible.len()),
            dim,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(header)), rows[0]);

    if let Some(view) = &mut browser.view {
        let label = if view.source == "screen" {
            "Last screen"
        } else {
            "Output"
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {label}: {} ", safe_text(&view.name)));
        let inner = block.inner(rows[1]);
        view.layout(inner.width, inner.height);
        browser.view_height = inner.height;
        let lines: Vec<Line<'_>> = view
            .wrapped
            .iter()
            .skip(view.scroll)
            .take(inner.height as usize)
            .map(|l| Line::raw(l.as_str()))
            .collect();
        frame.render_widget(Paragraph::new(lines).block(block), rows[1]);
        let position = format!(
            "{}-{} of {}",
            (view.scroll + 1).min(view.wrapped.len()),
            (view.scroll + inner.height as usize).min(view.wrapped.len()),
            view.wrapped.len()
        );
        frame.render_widget(
            Paragraph::new(format!(
                "Up/Down scroll  PgUp/PgDn page  g/G start/end  Esc back    {position}{}",
                if view.follow { "  (following)" } else { "" }
            )),
            rows[2],
        );
        return;
    }

    let wide = area.width >= 90;
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if wide {
            [Constraint::Percentage(40), Constraint::Percentage(60)]
        } else {
            [Constraint::Percentage(100), Constraint::Percentage(0)]
        })
        .split(rows[1]);
    let show_hits = !browser.query.is_empty() && browser.hits_for == browser.query;
    let items: Vec<ListItem<'_>> = browser
        .visible
        .iter()
        .map(|&i| {
            let session = &browser.sessions[i];
            let (status, color) = status_style(session);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{status:<8} "), Style::default().fg(color)),
                    Span::styled(
                        safe_text(session.heading()),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(Span::styled(
                    format!(
                        "{}  {}  {}{}  {}",
                        safe_text(&session.id),
                        safe_text(&session.kind),
                        ago(session.last_active_ms),
                        if session.here {
                            "  you are here"
                        } else if session.attached > 0 {
                            "  attached"
                        } else {
                            ""
                        },
                        safe_text(session.cwd())
                    ),
                    dim,
                )),
            ];
            if let Some(hit) = browser
                .hits
                .get(&session.id)
                .filter(|_| show_hits)
                .and_then(|h| h.first())
            {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}: {}",
                        safe_text(&hit.source),
                        safe_text(&hit.text).replace('\n', " ")
                    ),
                    Style::default().fg(Color::Yellow),
                )));
            }
            ListItem::new(lines)
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Sessions "))
        .highlight_symbol("> ")
        .highlight_style(Style::default().fg(Color::Cyan));
    frame.render_stateful_widget(list, columns[0], &mut browser.selection);

    if columns[1].width > 0 {
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(DETAIL_HEIGHT), Constraint::Min(3)])
            .split(columns[1]);
        let hits = browser.selected_hits();
        let (title, detail) = match browser.selected() {
            Some(session) if !hits.is_empty() => (
                format!(" Matches ({}) ", hits.len()),
                hits.iter()
                    .take(DETAIL_HEIGHT as usize - 2)
                    .map(|h| {
                        format!(
                            "{}: {}",
                            safe_text(&h.source),
                            safe_text(&h.text).replace('\n', " ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + if session.matches(&browser.query.to_lowercase()) {
                        "\n(also matches by name, title or directory)"
                    } else {
                        ""
                    },
            ),
            Some(session) => (
                " Details ".to_string(),
                format!(
                    "{}\nID        {}    Name  {}\nStatus    {}    Kind  {}    Attached  {}\nActive    {} ago\nDirectory {}\nAgent     {}",
                    safe_text(session.heading()),
                    safe_text(&session.id),
                    safe_text(&session.name),
                    status_style(session).0,
                    safe_text(&session.kind),
                    session.attached,
                    ago(session.last_active_ms),
                    safe_text(session.cwd()),
                    safe_text(session.agent_session_id.as_deref().unwrap_or("-"))
                ),
            ),
            None => (
                " Details ".to_string(),
                if browser.sessions.is_empty() {
                    "No sessions yet.\n\nPress n to start one.".into()
                } else {
                    "No sessions match your search.".into()
                },
            ),
        };
        frame.render_widget(
            Paragraph::new(detail)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false }),
            right[0],
        );

        let selected_id = browser.selected().map(|s| s.id.clone());
        let preview = browser
            .preview
            .as_ref()
            .filter(|p| Some(&p.id) == selected_id.as_ref());
        let title = match preview {
            Some(p) => {
                let (r, c) = p.parser.screen().size();
                format!(" Preview  {c}x{r} ")
            }
            None if browser.selected().is_some_and(|s| s.here) => " Preview (this is the session you are in) ".to_string(),
            None => " Preview ".to_string(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(right[1]);
        frame.render_widget(block, right[1]);
        if let Some(p) = preview {
            draw_preview(frame, inner, p.parser.screen());
        }
    }

    let footer = if let Some(confirm) = &browser.confirm {
        match confirm.kind {
            ConfirmKind::Stop => format!(
                "[y/N] Stop \"{}\"? The program ends; its history is kept.",
                safe_text(&confirm.label)
            ),
            ConfirmKind::Delete => format!(
                "[y/N] Delete \"{}\"? This ends the program and removes its history for good.",
                safe_text(&confirm.label)
            ),
        }
    } else if let Some(prompt) = &browser.prompt {
        match prompt.kind {
            PromptKind::New => format!(
                "New session, command (empty = shell): {}_   Enter start  Esc cancel",
                safe_text(&prompt.text)
            ),
            PromptKind::Rename(_) => format!(
                "Rename to: {}_   Enter save  Esc cancel",
                safe_text(&prompt.text)
            ),
        }
    } else if let Some(notice) = &browser.notice {
        safe_text(notice)
    } else {
        "Enter open  / search  n new  l output  s stop  r restart  R rename  d delete  q quit".into()
    };
    frame.render_widget(Paragraph::new(footer), rows[2]);
}

async fn refresh(browser: &mut Browser) -> Result<()> {
    let response = crate::call("list", json!({})).await?;
    let sessions = serde_json::from_value(
        response
            .get("sessions")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )?;
    browser.replace_sessions(sessions);
    Ok(())
}

/// Fetches the selected session's screen. It reads a copy: nothing is attached and the session is not resized.
async fn refresh_preview(browser: &mut Browser) {
    // The browser's own session is not previewed: it would be a picture of this screen.
    let Some(id) = browser.selected().filter(|s| !s.here).map(|s| s.id.clone()) else {
        browser.preview = None;
        return;
    };
    let Ok(snap) = crate::call("snapshot", json!({"id": id})).await else {
        return;
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(snap["b64"].as_str().unwrap_or(""))
        .unwrap_or_default();
    let cols = snap["cols"].as_u64().unwrap_or(120).clamp(2, 1000) as u16;
    let rows = snap["rows"].as_u64().unwrap_or(32).clamp(2, 1000) as u16;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&bytes);
    browser.preview = Some(Preview { id, parser });
}

async fn run_search(browser: &mut Browser) {
    let query = browser.query.clone();
    let hits = match crate::call("search", json!({"query": query, "limit": 200})).await {
        Ok(result) => serde_json::from_value(result["hits"].clone()).unwrap_or_default(),
        Err(error) => {
            browser.notice = Some(format!("Search failed: {error}"));
            vec![]
        }
    };
    browser.set_hits(query, hits);
}

async fn fetch_text(id: &str, source: &str) -> Result<String> {
    let result = crate::call(source, json!({"id": id, "bytes": 262144})).await?;
    Ok(result["text"].as_str().unwrap_or("").to_string())
}

async fn attach(terminal: &mut TerminalGuard, id: &str) -> Result<()> {
    terminal.leave()?;
    let status = tokio::process::Command::new(std::env::current_exe()?)
        .arg("attach")
        .arg("--no-history")
        .arg(id)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await;
    terminal.enter()?;
    let status = status.context("starting attach")?;
    if !status.success() {
        bail!("attach exited with {status}");
    }
    Ok(())
}

async fn restart_and_attach(terminal: &mut TerminalGuard, id: &str) -> Result<()> {
    let current = crate::call("get", json!({"id": id})).await?;
    if current["running"].as_bool() != Some(true) {
        crate::call("restart", json!({"id": id})).await?;
    }
    attach(terminal, id).await
}

async fn new_session(terminal: &mut TerminalGuard, command_line: &str) -> Result<()> {
    let mut parts = crate::agents::shell_split(command_line);
    let cmd = (!parts.is_empty()).then(|| parts.remove(0));
    let (cols, rows) = crate::term_size();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let created = crate::call(
        "new",
        json!({"cmd": cmd, "args": parts, "cwd": cwd, "env": crate::capture_env(false), "cols": cols, "rows": rows}),
    )
    .await?;
    let id = created["id"].as_str().unwrap_or("").to_string();
    attach(terminal, &id).await
}

pub async fn run() -> Result<()> {
    let mut terminal = TerminalGuard::new()?;
    let mut browser = Browser::default();
    let mut refreshed = Instant::now() - REFRESH_INTERVAL;
    let mut previewed = Instant::now() - PREVIEW_INTERVAL;
    let mut typed = Instant::now();
    let mut needs_draw = true;
    loop {
        if refreshed.elapsed() >= REFRESH_INTERVAL {
            if let Err(error) = refresh(&mut browser).await {
                browser.notice = Some(format!("Refresh failed: {error}"));
            }
            if let Some(view) = browser.view.as_mut().filter(|v| v.follow)
                && let Ok(text) = fetch_text(&view.id, view.source).await
            {
                view.set_text(text);
            }
            refreshed = Instant::now();
            needs_draw = true;
        }
        // Content search waits for a pause in typing; the name/title filter is instant.
        if browser.query.chars().count() >= 2
            && browser.hits_for != browser.query
            && typed.elapsed() >= SEARCH_DEBOUNCE
        {
            run_search(&mut browser).await;
            needs_draw = true;
        }
        let selected_id = browser.selected().map(|s| s.id.clone());
        let preview_stale = browser.preview.as_ref().map(|p| &p.id) != selected_id.as_ref();
        if browser.view.is_none() && (preview_stale || previewed.elapsed() >= PREVIEW_INTERVAL) {
            refresh_preview(&mut browser).await;
            previewed = Instant::now();
            needs_draw = true;
        }
        if needs_draw {
            terminal.terminal.draw(|frame| draw(frame, &mut browser))?;
            needs_draw = false;
        }
        if !event::poll(EVENT_POLL)? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                needs_draw = true;
                continue;
            }
            _ => continue,
        };
        needs_draw = true;
        typed = Instant::now();
        browser.notice = None;
        let action = browser.key(key);
        let result = match action {
            Action::None => continue,
            Action::Quit => break,
            Action::Notice(message) => {
                browser.notice = Some(message);
                continue;
            }
            Action::Attach(id) => attach(&mut terminal, &id).await,
            Action::Restart(id) => restart_and_attach(&mut terminal, &id).await,
            Action::New(command_line) => new_session(&mut terminal, &command_line).await,
            Action::Stop(id) => crate::call("kill", json!({"id": id})).await.map(|_| {
                browser.notice = Some("Stop requested; its history is kept.".into());
            }),
            Action::Delete(id) => crate::call("rm", json!({"id": id})).await.map(|_| {
                browser.notice = Some("Session deleted.".into());
            }),
            Action::Rename(id, name) => crate::call("set", json!({"id": id, "name": name}))
                .await
                .map(|_| ()),
            Action::View(id, name, source) => fetch_text(&id, source).await.map(|text| {
                browser.view = Some(TextView::new(id, name, source, text));
            }),
        };
        if let Err(error) = result {
            browser.notice = Some(error.to_string());
        }
        refreshed = Instant::now() - REFRESH_INTERVAL;
    }
    terminal.leave()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, title: &str, cwd: &str, status: &str, created_ms: i64) -> Session {
        Session {
            id: id.into(),
            name: id.into(),
            status: status.into(),
            title: Some(title.into()),
            kind: "shell".into(),
            cwd: cwd.into(),
            live_cwd: None,
            agent_session_id: None,
            exit_code: None,
            created_ms,
            last_active_ms: created_ms,
            attached: 0,
            here: false,
        }
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn search_matches_title_and_directory_without_case_sensitivity() {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![
            session("one", "Fix the bug", "/tmp/alpha", "running", 1),
            session("two", "Other", "/tmp/Beta", "running", 2),
        ]);
        browser.query = "BUG".into();
        browser.update_filter();
        assert_eq!(browser.selected().map(|s| s.id.as_str()), Some("one"));
        browser.query = "beta".into();
        browser.update_filter();
        assert_eq!(browser.selected().map(|s| s.id.as_str()), Some("two"));
        browser.query = "absent".into();
        browser.update_filter();
        assert!(browser.selected().is_none());
    }

    #[test]
    fn a_session_whose_output_matches_is_listed_with_its_matches() {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![
            session("one", "One", "/", "running", 1),
            session("two", "Two", "/", "running", 2),
        ]);
        browser.query = "needle".into();
        browser.update_filter();
        assert!(browser.selected().is_none(), "no name or title matches");
        let hit = Hit {
            id: "one".into(),
            source: "output".into(),
            text: "found the needle here".into(),
        };
        browser.set_hits("needle".into(), vec![hit]);
        assert_eq!(browser.selected().map(|s| s.id.as_str()), Some("one"));
        assert_eq!(browser.selected_hits().len(), 1);
        // Hits for an older query are not shown against a newer one.
        browser.query = "needles".into();
        browser.update_filter();
        assert!(browser.selected().is_none());
    }

    #[test]
    fn activity_does_not_reorder_the_list() {
        let mut browser = Browser::default();
        let mut older = session("one", "One", "/", "running", 1);
        let newer = session("two", "Two", "/", "running", 2);
        browser.replace_sessions(vec![older.clone(), newer.clone()]);
        let order = |b: &Browser| b.sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(order(&browser), ["two", "one"]);
        browser.move_selection(1);
        older.last_active_ms = 99;
        browser.replace_sessions(vec![older, newer]);
        assert_eq!(order(&browser), ["two", "one"]);
        assert_eq!(browser.selected().map(|s| s.id.as_str()), Some("one"));
    }

    #[test]
    fn control_sequences_are_not_rendered() {
        assert_eq!(safe_text("hi\x1b[2J\nthere"), "hi[2J\nthere");
    }

    #[test]
    fn stopping_and_deleting_require_confirmation() {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![session("one", "Work", "/", "running", 1)]);
        assert!(matches!(browser.key(key('s')), Action::None));
        assert!(matches!(browser.key(key('n')), Action::None));
        assert!(browser.confirm.is_none() && browser.prompt.is_none());
        assert!(matches!(browser.key(key('s')), Action::None));
        assert!(matches!(browser.key(key('y')), Action::Stop(id) if id == "one"));
        assert!(matches!(browser.key(key('d')), Action::None));
        assert_eq!(browser.confirm.as_ref().map(|c| c.kind), Some(ConfirmKind::Delete));
        assert!(matches!(browser.key(key('y')), Action::Delete(id) if id == "one"));
    }

    #[test]
    fn the_session_the_browser_runs_in_cannot_be_acted_on() {
        let mut own = session("me", "browser", "/tmp", "running", 1);
        own.here = true;
        let mut browser = Browser::default();
        browser.replace_sessions(vec![own]);
        for k in ['s', 'd', 'r', 'l'] {
            assert!(matches!(browser.key(key(k)), Action::Notice(_)), "{k}");
            assert!(browser.confirm.is_none(), "{k}");
        }
        assert!(matches!(browser.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::Notice(_)));
    }

    #[test]
    fn rename_prompt_starts_with_the_current_name() {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![session("one", "Work", "/", "running", 1)]);
        browser.key(key('R'));
        browser.key(key('x'));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(browser.key(enter), Action::Rename(id, name) if id == "one" && name == "onex"));
    }

    #[test]
    fn text_view_scroll_counts_wrapped_rows_and_stops_at_the_end() {
        let mut browser = Browser::default();
        let text = format!("{}\nshort\n", "x".repeat(25));
        let mut view = TextView::new("one".into(), "One".into(), "logs", text);
        view.layout(10, 2);
        assert_eq!(view.wrapped.len(), 4, "25 chars wrap to 3 rows of 10, plus one");
        assert_eq!(view.scroll, 2, "opens at the end");
        browser.view = Some(view);
        browser.view_height = 2;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        browser.key(down);
        assert_eq!(browser.view.as_ref().unwrap().scroll, 2, "cannot scroll past the end");
        browser.key(key('g'));
        let view = browser.view.as_ref().unwrap();
        assert!(view.scroll == 0 && !view.follow);
    }

    #[test]
    fn preview_shows_the_bottom_of_a_screen_taller_than_the_pane() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"top\r\n");
        assert_eq!(preview_origin(parser.screen(), 10), 0);
        parser.process(b"\x1b[20;1Hprompt> ");
        assert_eq!(preview_origin(parser.screen(), 10), 10);
    }
}
