use crate::pane::{self, Mouse};
use crate::proto::{
    C_DETACH, C_INPUT, C_RESIZE, S_DATA, S_EXIT, S_HISTORY, read_frame, write_frame,
};
use crate::rich::{self, Rich};
use anyhow::{Result, bail};
use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::{IsTerminal, Stdout, Write, stdout};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const PREVIEW_INTERVAL: Duration = Duration::from_millis(700);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const EVENT_POLL: Duration = Duration::from_millis(100);
const DETAIL_HEIGHT: u16 = 6;
/// Lines of history kept for scrolling back while the side panel is used as a terminal.
const PANE_SCROLLBACK: usize = 5000;
const MIN_LIST_WIDTH: u16 = 20;
const MIN_RIGHT_WIDTH: u16 = 30;
const MIN_PANE_HEIGHT: u16 = 3;

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
    /// Links and images, when this is a live view of the session rather than a copy of its screen.
    rich: Option<Rich>,
}

/// How the window is divided, as the user left it: the list's share of the width and the details pane's height.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, Deserialize)]
struct Sizes {
    list: f32,
    detail: u16,
    #[serde(default)]
    icons: Icons,
}

impl Default for Sizes {
    fn default() -> Self {
        Sizes {
            list: 0.4,
            detail: DETAIL_HEIGHT,
            icons: Icons::default(),
        }
    }
}

impl Sizes {
    fn file() -> std::path::PathBuf {
        crate::paths::home().join("browser.json")
    }

    fn load() -> Self {
        let mut sizes = std::fs::read(Self::file())
            .ok()
            .and_then(|b| serde_json::from_slice::<Sizes>(&b).ok())
            .filter(|s| s.list.is_finite())
            .unwrap_or_default();
        // DPTY_ICONS=nerd|plain decides for this run, whatever was saved.
        match std::env::var("DPTY_ICONS")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "nerd" => sizes.icons = Icons::Nerd,
            "plain" => sizes.icons = Icons::Plain,
            _ => {}
        }
        sizes
    }

    fn save(&self) {
        if let Ok(b) = serde_json::to_vec(self) {
            let _ = std::fs::write(Self::file(), b);
        }
    }

    fn list_width(&self, total: u16) -> u16 {
        let max = total.saturating_sub(MIN_RIGHT_WIDTH).max(MIN_LIST_WIDTH);
        ((total as f32 * self.list).round() as u16).clamp(MIN_LIST_WIDTH, max)
    }

    fn detail_height(&self, total: u16) -> u16 {
        let max = total.saturating_sub(MIN_PANE_HEIGHT).max(MIN_PANE_HEIGHT);
        self.detail.clamp(MIN_PANE_HEIGHT, max)
    }
}

/// Where the panes were drawn last, for finding what the mouse is on.
#[derive(Clone, Copy, Default)]
struct Areas {
    body: Rect,
    list: Rect,
    right: Rect,
    detail: Rect,
    /// Inside the preview pane's border: where the session's screen is drawn.
    screen: Rect,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Divider {
    /// Between the list and the right-hand side.
    Columns,
    /// Between details and the preview.
    Rows,
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
    sizes: Sizes,
    areas: Areas,
    dragging: Option<Divider>,
    /// Height in rows of each listed session, from the last draw.
    item_heights: Vec<u16>,
    /// Cells drawn this frame that belong to a link: column, row, link.
    link_cells: Vec<(u16, u16, u32)>,
    /// Cells written to the terminal as links by the last frame.
    linked_before: Vec<(u16, u16)>,
    /// Where the session's cursor was drawn this frame, if it is showing.
    cursor: Option<(u16, u16)>,
    /// A session has the keyboard: the browser is a terminal on it.
    focused: bool,
    /// That session fills the window under a one-line header, instead of sitting in the side panel.
    zoom: bool,
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

    /// Asks to use the side panel as a terminal on the selected session.
    fn focus_request(&self) -> Action {
        match self.selected() {
            None => Action::None,
            Some(s) if s.here => Action::Notice("This is the session you are in. It cannot be entered from inside itself.".into()),
            Some(s) if !s.running() => Action::Notice("Session has ended. Press Enter for its last screen, or r to restart it.".into()),
            Some(_) if self.areas.screen.width < 10 || self.areas.screen.height < 2 => {
                Action::Notice("The side panel is too small to use as a terminal. Widen the window, or press Enter for the full window.".into())
            }
            Some(s) => Action::Focus(s.id.clone()),
        }
    }

    fn divider_at(&self, m: &Mouse) -> Option<Divider> {
        let a = &self.areas;
        if a.right.width == 0 || !m.within(a.body.x, a.body.y, a.body.width, a.body.height) {
            return None;
        }
        // Each divider is two border lines thick: the edge of the pane on either side.
        if m.x + 1 == a.right.x || m.x == a.right.x {
            return Some(Divider::Columns);
        }
        let edge = a.detail.y + a.detail.height;
        (m.x > a.right.x && (m.y + 1 == edge || m.y == edge)).then_some(Divider::Rows)
    }

    fn drag_to(&mut self, divider: Divider, m: &Mouse) {
        let a = self.areas;
        match divider {
            Divider::Columns if a.body.width > 0 => {
                let width = (m.x.saturating_sub(a.body.x) + 1).clamp(
                    MIN_LIST_WIDTH,
                    a.body
                        .width
                        .saturating_sub(MIN_RIGHT_WIDTH)
                        .max(MIN_LIST_WIDTH),
                );
                self.sizes.list = width as f32 / a.body.width as f32;
            }
            Divider::Rows => {
                let height = m.y.saturating_sub(a.right.y) + 1;
                self.sizes.detail = height.clamp(
                    MIN_PANE_HEIGHT,
                    a.right
                        .height
                        .saturating_sub(MIN_PANE_HEIGHT)
                        .max(MIN_PANE_HEIGHT),
                );
            }
            _ => {}
        }
    }

    /// The listed session drawn on screen row `y`.
    fn item_at(&self, y: u16) -> Option<usize> {
        let top = self.areas.list.y + 1;
        let bottom = self.areas.list.y + self.areas.list.height.saturating_sub(1);
        if y < top || y >= bottom {
            return None;
        }
        let mut row = top;
        for i in self.selection.offset()..self.visible.len() {
            row += self.item_heights.get(i).copied().unwrap_or(2);
            if y < row {
                return Some(i);
            }
        }
        None
    }

    /// Mouse: drag the lines between panes to resize them, click a session to select it, click the preview to use it
    /// as a terminal, wheel to move through the list or scroll a text view.
    fn mouse(&mut self, m: Mouse) -> Action {
        if let Some(view) = &mut self.view {
            if let Some(step) = m.wheel() {
                let max = view.max_scroll(self.view_height);
                view.scroll = (view.scroll as i64 + step as i64 * 3).clamp(0, max as i64) as usize;
                view.follow = view.scroll == max;
            }
            return Action::None;
        }
        if self.confirm.is_some() || self.prompt.is_some() {
            return Action::None;
        }
        if let Some(divider) = self.dragging {
            if m.release {
                self.dragging = None;
                self.sizes.save();
            } else {
                self.drag_to(divider, &m);
            }
            return Action::None;
        }
        let a = self.areas;
        if let Some(step) = m.wheel() {
            if m.within(a.list.x, a.list.y, a.list.width, a.list.height) {
                self.move_selection(step);
            }
            return Action::None;
        }
        if !m.left_press() {
            return Action::None;
        }
        if let Some(divider) = self.divider_at(&m) {
            self.dragging = Some(divider);
            return Action::None;
        }
        if m.within(a.list.x, a.list.y, a.list.width, a.list.height) {
            if let Some(i) = self.item_at(m.y) {
                self.selection.select(Some(i));
            }
            return Action::None;
        }
        if m.within(a.screen.x, a.screen.y, a.screen.width, a.screen.height) {
            return self.focus_request();
        }
        Action::None
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
                        PromptKind::Rename(id) => {
                            Action::Rename(id, prompt.text.trim().to_string())
                        }
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
            (KeyCode::Tab, _) => return self.focus_request(),
            (KeyCode::Char('i'), _) => {
                self.sizes.icons = self.sizes.icons.other();
                self.sizes.save();
                return Action::Notice(match self.sizes.icons {
                    Icons::Nerd => "Icons on. They need a Nerd Font in your terminal; press i again if you see boxes.".into(),
                    Icons::Plain => "Icons off.".into(),
                });
            }
            (
                KeyCode::Char('l')
                | KeyCode::Enter
                | KeyCode::Char('r')
                | KeyCode::Char('s')
                | KeyCode::Char('d'),
                Some(s),
            ) if s.here => {
                return Action::Notice("This is the session you are in. It cannot be entered, stopped, restarted or deleted from inside itself.".into());
            }
            (KeyCode::Char('l'), Some(s)) => {
                return Action::View(s.id.clone(), s.heading().to_string(), "logs");
            }
            (KeyCode::Enter, Some(s)) => {
                return if s.running() {
                    Action::Zoom(s.id)
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
    /// Open the session in the whole window, under the dpty header.
    Zoom(String),
    Restart(String),
    Stop(String),
    Delete(String),
    New(String),
    Rename(String, String),
    View(String, String, &'static str),
    Notice(String),
    /// Use the side panel as a terminal on this session.
    Focus(String),
}

/// A mouse event from the terminal library, in the form the rest of the browser uses.
fn mouse_from(e: MouseEvent) -> Mouse {
    let button = |b: MouseButton| match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (code, release) = match e.kind {
        MouseEventKind::Down(b) => (button(b), false),
        MouseEventKind::Up(b) => (button(b), true),
        MouseEventKind::Drag(b) => (32 + button(b), false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    Mouse {
        code,
        x: e.column,
        y: e.row,
        release,
    }
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
            EnableMouseCapture,
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
            DisableMouseCapture,
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
fn draw_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    screen: &vt100::Screen,
    links: &mut Vec<(u16, u16, u32)>,
) {
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
            let (text, link) = rich::link_of(cell.contents());
            target.set_symbol(if text.is_empty() { " " } else { &text });
            target.set_style(style);
            if let Some(link) = link {
                links.push((area.x + x, area.y + y, link));
            }
        }
    }
}

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
/// Behind the selected session. Its text is set light explicitly, so it reads on light and dark terminals alike.
const SELECTED_BG: Color = Color::Indexed(237);

/// The symbols the browser draws with. `Nerd` needs a Nerd Font in the terminal (https://www.nerdfonts.com); `Plain`
/// uses characters every font has.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Icons {
    #[default]
    Plain,
    Nerd,
}

impl Icons {
    fn other(self) -> Self {
        match self {
            Icons::Plain => Icons::Nerd,
            Icons::Nerd => Icons::Plain,
        }
    }

    fn pick(self, plain: &'static str, nerd: &'static str) -> &'static str {
        match self {
            Icons::Plain => plain,
            Icons::Nerd => nerd,
        }
    }

    /// The session's state as one coloured symbol, in place of the word.
    fn status(self, session: &Session) -> (&'static str, Color) {
        match (session.status.as_str(), session.exit_code) {
            ("running", _) => ("●", Color::Green),
            ("lost", _) => (self.pick("!", "\u{f071}"), Color::Yellow),
            ("hibernated", _) => (self.pick("z", "\u{f186}"), Color::Cyan),
            (_, Some(0) | None) => (self.pick("○", "\u{f00c}"), DIM),
            _ => (self.pick("✕", "\u{f00d}"), Color::Red),
        }
    }

    /// What the session runs. Without icons this is the word.
    fn kind(self, kind: &str) -> String {
        match self {
            Icons::Plain => kind.to_string(),
            Icons::Nerd => match kind {
                "shell" => "\u{f489}",
                "claude" | "codex" | "grok" => "\u{f06a9}",
                _ => "\u{f013}",
            }
            .to_string(),
        }
    }

    fn folder(self) -> &'static str {
        self.pick("", "\u{f07b} ")
    }

    fn attached(self) -> &'static str {
        self.pick("attached", "\u{f0c1}")
    }

    fn here(self) -> &'static str {
        self.pick("you are here", "\u{f041} here")
    }

    fn search(self) -> &'static str {
        self.pick("Search: ", "\u{f002} Search: ")
    }
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

/// The state in words, for where there is room: "running", "lost", "exited", "exit 3".
fn status_text(session: &Session) -> String {
    match (session.status.as_str(), session.exit_code) {
        ("running" | "lost" | "hibernated", _) | (_, Some(0) | None) => session.status.clone(),
        (_, Some(code)) => format!("exit {code}"),
    }
}

fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// At most `max` columns of `text`, keeping the start.
fn clip(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in text.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

/// At most `max` columns of `text`, keeping the end: the end of a path says more than its start.
fn clip_start(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    let mut kept: Vec<char> = vec![];
    let mut used = 1;
    for c in text.chars().rev() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        kept.push(c);
    }
    let tail: String = kept.into_iter().rev().collect();
    if max > 0 { format!("…{tail}") } else { tail }
}

/// A path with the home directory written as `~`.
fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_string(),
    }
}

fn panel<'a>(title: impl Into<Line<'a>>, border: Style) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title.into())
}

fn title<'a>(text: impl Into<String>, active: bool) -> Line<'a> {
    let style = if active {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(Span::styled(format!(" {} ", text.into()), style))
}

/// A row of key hints: the key in the accent colour, what it does beside it.
fn hints<'a>(pairs: &[(&'a str, &'a str)]) -> Line<'a> {
    let mut spans = vec![Span::raw(" ")];
    for (key, does) in pairs {
        spans.push(Span::styled(
            *key,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {does}   "),
            Style::default().fg(DIM),
        ));
    }
    Line::from(spans)
}

/// One session in the list: its state, title and how long since it did anything, then what it is and where.
fn session_item<'a>(
    session: &Session,
    icons: Icons,
    selected: bool,
    width_available: usize,
    hit: Option<&Hit>,
) -> ListItem<'a> {
    let dim = Style::default().fg(if selected { Color::Gray } else { DIM });
    let bar = if selected {
        Span::styled("▌ ", Style::default().fg(ACCENT))
    } else {
        Span::raw("  ")
    };
    let (symbol, color) = icons.status(session);
    let age = ago(session.last_active_ms);
    let room = width_available.saturating_sub(2 + 2 + 1 + width(&age));
    let heading = clip(&safe_text(session.heading()), room);
    let gap = width_available.saturating_sub(2 + 2 + width(&heading) + width(&age));
    let mut name = Style::default().add_modifier(Modifier::BOLD);
    if selected {
        name = name.fg(Color::White);
    } else if !session.running() {
        name = name.fg(Color::Gray);
    }
    let first = Line::from(vec![
        bar.clone(),
        Span::styled(format!("{symbol} "), Style::default().fg(color)),
        Span::styled(heading, name),
        Span::raw(" ".repeat(gap)),
        Span::styled(age, dim),
    ]);

    let mut second = vec![bar.clone(), Span::raw("  ")];
    let mut used = 4;
    let mut push = |spans: &mut Vec<Span<'a>>, text: String, style: Style| {
        used += width(&text) + 2;
        spans.push(Span::styled(text, style));
        spans.push(Span::raw("  "));
    };
    if !session.running() {
        push(
            &mut second,
            status_text(session),
            Style::default().fg(color),
        );
    }
    push(&mut second, icons.kind(&safe_text(&session.kind)), dim);
    if session.here {
        push(
            &mut second,
            icons.here().to_string(),
            Style::default().fg(ACCENT),
        );
    } else if session.attached > 0 {
        push(&mut second, icons.attached().to_string(), dim);
    }
    second.push(Span::styled(
        clip_start(
            &tilde(&safe_text(session.cwd())),
            width_available.saturating_sub(used),
        ),
        dim,
    ));

    let mut lines = vec![first, Line::from(second)];
    if let Some(hit) = hit {
        let text = format!(
            "{}: {}",
            safe_text(&hit.source),
            safe_text(&hit.text).replace('\n', " ")
        );
        lines.push(Line::from(vec![
            bar,
            Span::raw("  "),
            Span::styled(
                clip(&text, width_available.saturating_sub(4)),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }
    ListItem::new(lines)
}

/// The colours and attributes of a drawn cell, as the terminal sequence that sets them from scratch.
fn sgr(cell: &ratatui::buffer::Cell) -> String {
    let mut codes = vec!["0".to_string()];
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if cell.modifier.contains(modifier) {
            codes.push(code.into());
        }
    }
    let named = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    for (color, base) in [(cell.fg, 30), (cell.bg, 40)] {
        match color {
            Color::Reset => {}
            Color::Indexed(i) => codes.push(format!("{};5;{i}", base + 8)),
            Color::Rgb(r, g, b) => codes.push(format!("{};2;{r};{g};{b}", base + 8)),
            other => {
                if let Some(n) = named.iter().position(|c| *c == other) {
                    codes.push(if n < 8 {
                        (base + n).to_string()
                    } else {
                        (base + 60 + n - 8).to_string()
                    });
                }
            }
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// Writes the frame's link cells to the terminal again as real hyperlinks (the drawing library has no notion of
/// them), and rewrites cells that were links last frame and are not now, so nothing stays clickable by mistake.
/// The cells' text and colours are exactly what was just drawn.
fn link_overlay<'a>(
    buffer: &ratatui::buffer::Buffer,
    now: &[(u16, u16, u32)],
    before: &mut Vec<(u16, u16)>,
    uri: impl Fn(u32) -> Option<&'a str>,
    cursor: Option<(u16, u16)>,
) -> Vec<u8> {
    let mut out = String::new();
    let put = |out: &mut String, x: u16, y: u16| -> u16 {
        match buffer.cell((x, y)) {
            Some(cell) => {
                out.push_str(&sgr(cell));
                out.push_str(cell.symbol());
                width(cell.symbol()).max(1) as u16
            }
            None => 1,
        }
    };
    for &(x, y) in before
        .iter()
        .filter(|at| !now.iter().any(|n| (n.0, n.1) == **at))
    {
        out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
        put(&mut out, x, y);
    }
    let mut i = 0;
    while i < now.len() {
        let (x, y, link) = now[i];
        let mut end = i + 1;
        while end < now.len()
            && now[end].1 == y
            && now[end].2 == link
            && now[end].0 > now[end - 1].0
            && now[end].0 - now[end - 1].0 <= 2
        {
            end += 1;
        }
        if let Some(uri) = uri(link) {
            out.push_str(&format!("\x1b[{};{}H\x1b]8;;{uri}\x1b\\", y + 1, x + 1));
            let mut at = x;
            for cell in &now[i..end] {
                if cell.0 != at {
                    out.push_str(&format!("\x1b[{};{}H", y + 1, cell.0 + 1));
                }
                at = cell.0 + put(&mut out, cell.0, y);
            }
            out.push_str("\x1b]8;;\x1b\\");
        }
        i = end;
    }
    *before = now.iter().map(|n| (n.0, n.1)).collect();
    if !out.is_empty() {
        out.push_str("\x1b[0m");
        if let Some((x, y)) = cursor {
            out.push_str(&format!("\x1b[{};{}H", y + 1, x + 1));
        }
    }
    out.into_bytes()
}

/// Draws a frame, then lays the hyperlinks over it.
fn render(terminal: &mut TerminalGuard, browser: &mut Browser) -> Result<()> {
    let done = terminal.terminal.draw(|frame| draw(frame, browser))?;
    let Browser {
        link_cells,
        linked_before,
        preview,
        cursor,
        ..
    } = browser;
    if link_cells.is_empty() && linked_before.is_empty() {
        return Ok(());
    }
    let table = preview.as_ref().and_then(|p| p.rich.as_ref());
    let bytes = link_overlay(
        done.buffer,
        link_cells,
        linked_before,
        |link| table.and_then(|r| r.uri(link)),
        *cursor,
    );
    let mut out = stdout();
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(())
}

/// The size of one character cell in pixels, as the terminal driver reports it (0 when it does not say).
fn cell_pixels() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } != 0 || ws.ws_col == 0 || ws.ws_row == 0
    {
        return (0, 0);
    }
    (ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row)
}

/// A session in the whole window: one header line saying this is dpty and how to get back, then the session's screen.
fn draw_zoom(frame: &mut Frame<'_>, browser: &mut Browser) {
    let area = frame.area();
    let icons = browser.sizes.icons;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let bar = Style::default().bg(SELECTED_BG);
    let mut header = vec![Span::styled(
        format!(" {} dpty ", icons.pick("←", "\u{f060}")),
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    )];
    let mut hint = "click here or Ctrl-] for the session list ".to_string();
    if let Some(p) = &browser.preview {
        let screen = p.parser.screen();
        let (r, c) = screen.size();
        if screen.scrollback() > 0 {
            hint = format!("scrolled back {} lines   {hint}", screen.scrollback());
        }
        match browser.sessions.iter().find(|s| s.id == p.id) {
            Some(session) => {
                let (symbol, color) = icons.status(session);
                header.push(Span::styled(format!("  {symbol} "), bar.fg(color)));
                header.push(Span::styled(
                    safe_text(session.heading()),
                    bar.fg(Color::White).add_modifier(Modifier::BOLD),
                ));
            }
            None => header.push(Span::styled(
                format!("  {}", safe_text(&p.id)),
                bar.fg(Color::White),
            )),
        }
        header.push(Span::styled(format!("   {c}x{r}"), bar.fg(Color::Gray)));
    }
    let used: usize = header.iter().map(|s| s.width()).sum();
    let hint = clip(&hint, (area.width as usize).saturating_sub(used + 2));
    header.push(Span::styled(
        " ".repeat((area.width as usize).saturating_sub(used + width(&hint))),
        bar,
    ));
    header.push(Span::styled(hint, bar.fg(Color::Gray)));
    frame.render_widget(Paragraph::new(Line::from(header)), rows[0]);
    browser.areas = Areas {
        body: rows[1],
        screen: rows[1],
        ..Areas::default()
    };
    let mut links = vec![];
    if let Some(p) = &browser.preview {
        let screen = p.parser.screen();
        draw_preview(frame, rows[1], screen, &mut links);
        let (row, col) = screen.cursor_position();
        let row = row.saturating_sub(preview_origin(screen, rows[1].height));
        if !screen.hide_cursor()
            && screen.scrollback() == 0
            && col < rows[1].width
            && row < rows[1].height
        {
            browser.cursor = Some((rows[1].x + col, rows[1].y + row));
            frame.set_cursor_position((rows[1].x + col, rows[1].y + row));
        }
    }
    browser.link_cells = links;
}

fn draw(frame: &mut Frame<'_>, browser: &mut Browser) {
    browser.link_cells.clear();
    browser.cursor = None;
    if browser.focused && browser.zoom {
        return draw_zoom(frame, browser);
    }
    let area = frame.area();
    let icons = browser.sizes.icons;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let dim = Style::default().fg(DIM);
    let mut header = vec![
        Span::styled(
            " dpty ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if browser.searching {
        header.push(Span::styled(icons.search(), Style::default().fg(ACCENT)));
        header.push(Span::raw(format!("{}▏", safe_text(&browser.query))));
        header.push(Span::styled(
            "   names, titles, directories, output, commands, transcripts",
            dim,
        ));
    } else if browser.query.is_empty() {
        let running = browser.sessions.iter().filter(|s| s.running()).count();
        header.push(Span::styled(
            format!("{} sessions", browser.visible.len()),
            Style::default().fg(Color::Gray),
        ));
        header.push(Span::styled(format!("  ·  {running} running"), dim));
    } else {
        header.push(Span::styled(icons.search(), Style::default().fg(ACCENT)));
        header.push(Span::raw(safe_text(&browser.query)));
        header.push(Span::styled(
            format!("   {} found  ·  Esc clear", browser.visible.len()),
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
        let position = format!(
            "{}-{} of {}{}",
            (view.scroll + 1).min(view.wrapped.len()),
            (view.scroll + browser.view_height as usize).min(view.wrapped.len()),
            view.wrapped.len(),
            if view.follow { "  following" } else { "" }
        );
        let block = panel(
            title(format!("{label}: {}", safe_text(&view.name)), true),
            Style::default().fg(ACCENT),
        )
        .title_top(Line::from(Span::styled(format!(" {position} "), dim)).right_aligned());
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
        frame.render_widget(
            Paragraph::new(hints(&[
                ("↑↓", "scroll"),
                ("PgUp/PgDn", "page"),
                ("g/G", "start/end"),
                ("Esc", "back"),
            ])),
            rows[2],
        );
        return;
    }

    // Below this width there is only the list.
    let wide = area.width >= MIN_LIST_WIDTH + MIN_RIGHT_WIDTH + 10;
    let list_width = if wide {
        browser.sizes.list_width(rows[1].width)
    } else {
        rows[1].width
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(list_width), Constraint::Min(0)])
        .split(rows[1]);
    browser.areas = Areas {
        body: rows[1],
        list: columns[0],
        right: columns[1],
        ..Areas::default()
    };
    // The pane that has the keyboard is drawn in the accent colour; a line being dragged in yellow.
    let border = |active: bool, divider: Option<Divider>| {
        if divider.is_some() && browser.dragging == divider {
            Style::default().fg(Color::Yellow)
        } else if active {
            Style::default().fg(ACCENT)
        } else {
            Style::default().fg(DIM)
        }
    };
    let list_border = border(!browser.focused, Some(Divider::Columns));
    let detail_border = border(false, Some(Divider::Rows));
    let show_hits = !browser.query.is_empty() && browser.hits_for == browser.query;
    let selected_index = browser.selection.selected();
    let item_width = columns[0].width.saturating_sub(2) as usize;
    let items: Vec<ListItem<'_>> = browser
        .visible
        .iter()
        .enumerate()
        .map(|(row, &i)| {
            let session = &browser.sessions[i];
            let hit = browser
                .hits
                .get(&session.id)
                .filter(|_| show_hits)
                .and_then(|h| h.first());
            session_item(session, icons, selected_index == Some(row), item_width, hit)
        })
        .collect();
    browser.item_heights = items.iter().map(|i| i.height() as u16).collect();
    let list = List::new(items)
        .block(panel(title("Sessions", !browser.focused), list_border))
        .highlight_style(Style::default().bg(SELECTED_BG));
    frame.render_stateful_widget(list, columns[0], &mut browser.selection);

    if columns[1].width > 0 {
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(browser.sizes.detail_height(columns[1].height)),
                Constraint::Min(0),
            ])
            .split(columns[1]);
        browser.areas.detail = right[0];
        let hits = browser.selected_hits();
        let label = |text: &'static str| Span::styled(text, dim);
        let (heading, detail): (String, Vec<Line<'_>>) = match browser.selected() {
            Some(session) if !hits.is_empty() => {
                let mut lines: Vec<Line<'_>> = hits
                    .iter()
                    .take(right[0].height.saturating_sub(2) as usize)
                    .map(|h| {
                        Line::from(vec![
                            Span::styled(
                                format!("{}  ", safe_text(&h.source)),
                                Style::default().fg(Color::Yellow),
                            ),
                            Span::raw(safe_text(&h.text).replace('\n', " ")),
                        ])
                    })
                    .collect();
                if session.matches(&browser.query.to_lowercase()) {
                    lines.push(Line::from(label(
                        "also matches by name, title or directory",
                    )));
                }
                (format!("Matches ({})", hits.len()), lines)
            }
            Some(session) => {
                let (symbol, color) = icons.status(session);
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(format!("{symbol} "), Style::default().fg(color)),
                        Span::styled(
                            safe_text(session.heading()),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("   {}", status_text(session)),
                            Style::default().fg(color),
                        ),
                    ]),
                    Line::from(vec![
                        label("id "),
                        Span::raw(safe_text(&session.id)),
                        label("   name "),
                        Span::raw(safe_text(&session.name)),
                        label("   kind "),
                        Span::raw(safe_text(&session.kind)),
                        label("   attached "),
                        Span::raw(session.attached.to_string()),
                        label("   active "),
                        Span::raw(format!("{} ago", ago(session.last_active_ms))),
                    ]),
                    Line::from(vec![
                        label(icons.pick("in ", icons.folder())),
                        Span::raw(clip_start(
                            &tilde(&safe_text(session.cwd())),
                            right[0].width.saturating_sub(5) as usize,
                        )),
                    ]),
                ];
                if let Some(agent) = session.agent_session_id.as_deref() {
                    lines.push(Line::from(vec![
                        label("agent "),
                        Span::raw(safe_text(agent)),
                    ]));
                }
                ("Details".to_string(), lines)
            }
            None => (
                "Details".to_string(),
                vec![Line::from(label(if browser.sessions.is_empty() {
                    "No sessions yet. Press n to start one."
                } else {
                    "No sessions match your search."
                }))],
            ),
        };
        frame.render_widget(
            Paragraph::new(detail)
                .block(panel(title(heading, false), detail_border))
                .wrap(Wrap { trim: false }),
            right[0],
        );

        let selected_id = browser.selected().map(|s| s.id.clone());
        let preview = browser
            .preview
            .as_ref()
            .filter(|p| Some(&p.id) == selected_id.as_ref());
        let focused = browser.focused && preview.is_some();
        let (name, note) = match preview {
            Some(p) => {
                let (r, c) = p.parser.screen().size();
                let back = p.parser.screen().scrollback();
                match (focused, back) {
                    (true, 0) => (
                        format!("Terminal  {c}x{r}"),
                        "Ctrl-] or click outside to leave".to_string(),
                    ),
                    (true, n) => (
                        format!("Terminal  {c}x{r}"),
                        format!("scrolled back {n} lines"),
                    ),
                    _ => (format!("Preview  {c}x{r}"), "Tab to use".to_string()),
                }
            }
            None if browser.selected().is_some_and(|s| s.here) => (
                "Preview".to_string(),
                "this is the session you are in".to_string(),
            ),
            None => ("Preview".to_string(), String::new()),
        };
        let mut block = panel(title(name, focused), border(focused, Some(Divider::Rows)));
        if !note.is_empty() {
            block =
                block.title_top(Line::from(Span::styled(format!(" {note} "), dim)).right_aligned());
        }
        let inner = block.inner(right[1]);
        frame.render_widget(block, right[1]);
        let mut links = vec![];
        let mut cursor = None;
        if let Some(p) = preview {
            let screen = p.parser.screen();
            draw_preview(frame, inner, screen, &mut links);
            // The program's own cursor, when the panel is in use and showing the live screen.
            let (row, col) = screen.cursor_position();
            let row = row.saturating_sub(preview_origin(screen, inner.height));
            if focused
                && !screen.hide_cursor()
                && screen.scrollback() == 0
                && col < inner.width
                && row < inner.height
            {
                cursor = Some((inner.x + col, inner.y + row));
                frame.set_cursor_position((inner.x + col, inner.y + row));
            }
        }
        browser.link_cells = links;
        browser.cursor = cursor;
        browser.areas.screen = inner;
    }

    let warn = Style::default().fg(Color::Yellow);
    let footer: Line<'_> = if let Some(confirm) = &browser.confirm {
        Line::from(Span::styled(
            match confirm.kind {
                ConfirmKind::Stop => format!(
                    " [y/N] Stop \"{}\"? The program ends; its history is kept.",
                    safe_text(&confirm.label)
                ),
                ConfirmKind::Delete => format!(
                    " [y/N] Delete \"{}\"? This ends the program and removes its history for good.",
                    safe_text(&confirm.label)
                ),
            },
            warn,
        ))
    } else if let Some(prompt) = &browser.prompt {
        let (ask, done) = match prompt.kind {
            PromptKind::New => ("New session, command (empty = shell): ", "start"),
            PromptKind::Rename(_) => ("Rename to: ", "save"),
        };
        Line::from(vec![
            Span::styled(format!(" {ask}"), Style::default().fg(ACCENT)),
            Span::raw(format!("{}▏", safe_text(&prompt.text))),
            Span::styled(format!("   Enter {done}  Esc cancel"), dim),
        ])
    } else if browser.focused {
        hints(&[
            ("Ctrl-]", "back to the list"),
            ("Shift-PgUp/PgDn", "scroll back"),
            ("click outside", "leave"),
        ])
    } else if let Some(notice) = &browser.notice {
        Line::from(Span::styled(format!(" {}", safe_text(notice)), warn))
    } else {
        hints(&[
            ("⏎", "open"),
            ("Tab", "use panel"),
            ("/", "search"),
            ("n", "new"),
            ("l", "output"),
            ("s", "stop"),
            ("r", "restart"),
            ("R", "rename"),
            ("d", "delete"),
            ("i", "icons"),
            ("q", "quit"),
        ])
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
    browser.preview = Some(Preview {
        id,
        parser,
        rich: None,
    });
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

/// Reads the keyboard and mouse as raw bytes while the side panel is in use, so they reach the program exactly as
/// typed. `stop` hands the terminal's input back to the browser at once: nothing typed after it is read here.
struct RawInput {
    wake: libc::c_int,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RawInput {
    fn start(tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) -> Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let (wait, wake) = (fds[0], fds[1]);
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let mut p = [
                    libc::pollfd {
                        fd: wait,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                let ready = unsafe { libc::poll(p.as_mut_ptr(), 2, -1) };
                if ready < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                // Asked to stop, or the terminal went away.
                if ready < 0 || p[0].revents != 0 || p[1].revents & libc::POLLIN == 0 {
                    break;
                }
                let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 || tx.send(buf[..n as usize].to_vec()).is_err() {
                    break;
                }
            }
            unsafe { libc::close(wait) };
        });
        Ok(RawInput {
            wake,
            thread: Some(thread),
        })
    }

    async fn stop(mut self) {
        unsafe { libc::write(self.wake, [0u8].as_ptr() as *const libc::c_void, 1) };
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        unsafe { libc::close(self.wake) };
    }
}

/// Uses the side panel, or the whole window under a header line, as a terminal on a session until Ctrl-], a click elsewhere, or the program ending. The session
/// is resized to the panel, exactly as it is resized to the window by a full attach. Returns the click that left the
/// panel, if that is how it ended.
async fn use_pane(
    terminal: &mut TerminalGuard,
    browser: &mut Browser,
    id: &str,
) -> Result<Option<Mouse>> {
    let zoom = browser.zoom;
    let area = browser.areas.screen;
    let mut size = (area.width, area.height);
    if zoom {
        // The whole window but the header line.
        let (cols, rows) = terminal::size().unwrap_or((120, 32));
        size = (cols.max(2), rows.saturating_sub(1).max(1));
    }
    let mut client = crate::client::Client::connect().await?;
    client
        .call(
            "attach",
            json!({"id": id, "cols": size.0, "rows": size.1, "history": "raw"}),
        )
        .await?;
    let (mut reader, mut writer) = (client.reader, client.writer);
    let mut rich = Rich::default();
    rich.cell = cell_pixels();
    browser.preview = Some(Preview {
        id: id.to_string(),
        parser: vt100::Parser::new(size.1, size.0, PANE_SCROLLBACK),
        rich: Some(rich),
    });
    browser.focused = true;
    {
        // Ask the real terminal how big a cell is and whether it draws images. The answers arrive with the typing.
        let mut out = stdout();
        out.write_all(b"\x1b[16t\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")?;
        out.flush()?;
    }
    // What arrives first is the past: optionally the recent raw output, then the screen as the daemon has it.
    let mut replayed = false;
    let mut screen_seen = false;

    let (ftx, mut frx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(256);
    let frames = tokio::spawn(async move {
        while let Ok(f) = read_frame(&mut reader).await {
            if ftx.send(f).await.is_err() {
                break;
            }
        }
    });
    let (itx, mut irx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let input = RawInput::start(itx)?;

    let mut splitter = pane::Splitter::default();
    let mut modes = pane::Modes::default();
    let mut tick = tokio::time::interval(EVENT_POLL);
    let mut window = terminal::size().unwrap_or_default();
    let mut refreshed = Instant::now();
    let mut leftover = None;
    let mut dirty = true;
    let result: Result<()> = async {
        'pane: while let Some(Preview { parser, rich: Some(rich), .. }) = browser.preview.as_mut() {
            if dirty {
                dirty = false;
                let want = pane::Modes::of(parser.screen());
                if want != modes {
                    let mut out = stdout();
                    out.write_all(&modes.switch(want))?;
                    out.flush()?;
                    modes = want;
                }
                render(terminal, browser)?;
                // The panel changed size (window resized): the session follows it.
                let now = (browser.areas.screen.width, browser.areas.screen.height);
                if now != size && now.0 >= 2 && now.1 >= 1 {
                    size = now;
                    if let Some(p) = browser.preview.as_mut() {
                        p.parser.screen_mut().set_size(size.1, size.0);
                    }
                    let mut payload = size.0.to_be_bytes().to_vec();
                    payload.extend(size.1.to_be_bytes());
                    write_frame(&mut writer, C_RESIZE, &payload).await?;
                }
                continue;
            }
            tokio::select! {
                frame = frx.recv() => match frame {
                    Some((S_HISTORY, data)) => {
                        // Recent output as the program wrote it: brings back links and images, into the scrollback too.
                        rich.replaying = true;
                        let fx = rich.feed(parser, &data);
                        rich.replaying = false;
                        stdout().write_all(&fx.terminal)?;
                        replayed = !data.is_empty();
                        dirty = true;
                    }
                    Some((S_DATA, data)) if !screen_seen => {
                        screen_seen = true;
                        // The daemon's picture of the screen is the reference. If replaying the raw output arrived at
                        // the same picture, keep ours (it has the links) and take only modes and cursor from the
                        // daemon's; otherwise show the daemon's.
                        let mut reference = vt100::Parser::new(size.1, size.0, 0);
                        reference.process(&data);
                        if replayed && rich::without_marks(&parser.screen().contents()) == reference.screen().contents() {
                            parser.process(&reference.screen().input_mode_formatted());
                            parser.process(&reference.screen().cursor_state_formatted());
                        } else {
                            parser.process(&data);
                        }
                        dirty = true;
                    }
                    Some((S_DATA, data)) => {
                        let mut fx = rich.feed(parser, &data);
                        while let Ok((S_DATA, more)) = frx.try_recv() {
                            let next = rich.feed(parser, &more);
                            fx.terminal.extend(next.terminal);
                            fx.session.extend(next.session);
                        }
                        if !fx.terminal.is_empty() {
                            let mut out = stdout();
                            out.write_all(&fx.terminal)?;
                            out.flush()?;
                        }
                        if !fx.session.is_empty() {
                            write_frame(&mut writer, C_INPUT, &fx.session).await?;
                        }
                        dirty = true;
                    }
                    Some((S_EXIT, _)) => {
                        browser.notice = Some("The program ended.".into());
                        break 'pane;
                    }
                    _ => {
                        browser.notice = Some("Lost the connection to the session.".into());
                        break 'pane;
                    }
                },
                bytes = irx.recv() => {
                    let Some(bytes) = bytes else { break 'pane };
                    for item in splitter.feed(&bytes) {
                        let screen = parser.screen_mut();
                        match item {
                            pane::Input::Leave => break 'pane,
                            pane::Input::CellSize(w, h) => rich.cell = (w, h),
                            pane::Input::Graphics(ok) => rich.graphics = ok,
                            pane::Input::Bytes(typed) => {
                                screen.set_scrollback(0);
                                write_frame(&mut writer, C_INPUT, &typed).await?;
                            }
                            pane::Input::Page(direction) => {
                                let page = size.1.max(2) as i64 - 1;
                                screen.set_scrollback((screen.scrollback() as i64 - direction as i64 * page).max(0) as usize);
                            }
                            pane::Input::Mouse(m) => {
                                let area = browser.areas.screen;
                                if !m.within(area.x, area.y, area.width, area.height) {
                                    // A click outside leaves: on the header line, or anywhere off the side panel.
                                    if m.left_press() {
                                        leftover = Some(m);
                                        break 'pane;
                                    }
                                    continue;
                                }
                                let local = Mouse { x: m.x - area.x, y: m.y - area.y, ..m };
                                if pane::wants(screen.mouse_protocol_mode(), &local) {
                                    if let Some(report) = pane::encode(&local, screen.mouse_protocol_encoding()) {
                                        write_frame(&mut writer, C_INPUT, &report).await?;
                                    }
                                } else if let Some(step) = m.wheel() {
                                    if screen.alternate_screen() {
                                        // A full-screen program that takes no mouse: the wheel moves its cursor,
                                        // as terminals do.
                                        let key: &[u8] = match (step < 0, screen.application_cursor()) {
                                            (true, false) => b"\x1b[A",
                                            (true, true) => b"\x1bOA",
                                            (false, false) => b"\x1b[B",
                                            (false, true) => b"\x1bOB",
                                        };
                                        write_frame(&mut writer, C_INPUT, &key.repeat(3)).await?;
                                    } else {
                                        screen.set_scrollback((screen.scrollback() as i64 - step as i64 * 3).max(0) as usize);
                                    }
                                }
                            }
                        }
                    }
                    dirty = true;
                }
                _ = tick.tick() => {
                    let now = terminal::size().unwrap_or_default();
                    if now != window {
                        window = now;
                        dirty = true;
                    }
                    if refreshed.elapsed() >= REFRESH_INTERVAL {
                        refreshed = Instant::now();
                        let _ = refresh(browser).await;
                        let still = if zoom { browser.sessions.iter().any(|s| s.id == id) } else { browser.selected().map(|s| s.id.as_str()) == Some(id) };
                        if !still {
                            break 'pane;
                        }
                        dirty = true;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    // The keyboard goes back to the browser before anything shows that the panel was left.
    input.stop().await;
    let _ = write_frame(&mut writer, C_DETACH, &[]).await;
    frames.abort();
    let mut out = stdout();
    let _ = out.write_all(&modes.switch(pane::Modes::default()));
    // Images shown by this view are freed on the real terminal; the copy of the screen that follows has none.
    if let Some(rich) = browser.preview.as_mut().and_then(|p| p.rich.as_mut()) {
        let _ = out.write_all(&rich.cleanup());
    }
    let _ = out.flush();
    browser.focused = false;
    result.map(|_| leftover)
}

/// Makes sure the session is running, restarting it if it has ended.
async fn restart(id: &str) -> Result<()> {
    let current = crate::call("get", json!({"id": id})).await?;
    if current["running"].as_bool() != Some(true) {
        crate::call("restart", json!({"id": id})).await?;
    }
    Ok(())
}

/// Starts a session from a command line typed in the browser and returns its id.
async fn new_session(command_line: &str) -> Result<String> {
    let mut parts = crate::agents::shell_split(command_line);
    let cmd = (!parts.is_empty()).then(|| parts.remove(0));
    let (cols, rows) = crate::term_size();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    let created = crate::call("new", json!({"cmd": cmd, "args": parts, "cwd": cwd, "env": crate::capture_env(false), "cols": cols, "rows": rows})).await?;
    Ok(created["id"].as_str().unwrap_or("").to_string())
}

/// Opens a session in the browser, in the side panel or (`zoom`) the whole window, and tidies up when it is left.
async fn open(
    terminal: &mut TerminalGuard,
    browser: &mut Browser,
    id: &str,
    zoom: bool,
) -> Result<()> {
    browser.zoom = zoom;
    let outcome = use_pane(terminal, browser, id).await;
    browser.focused = false;
    browser.zoom = false;
    let _ = refresh(browser).await;
    // A click outside the side panel both leaves it and does what a click there does. The click that leaves the whole
    // window view lands on its header, which is not there any more.
    if let Some(Action::Notice(message)) = outcome?.filter(|_| !zoom).map(|m| browser.mouse(m)) {
        browser.notice = Some(message);
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    let mut terminal = TerminalGuard::new()?;
    let mut browser = Browser {
        sizes: Sizes::load(),
        ..Browser::default()
    };
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
            render(&mut terminal, &mut browser)?;
            needs_draw = false;
        }
        if !event::poll(EVENT_POLL)? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) => {
                typed = Instant::now();
                browser.notice = None;
                browser.key(key)
            }
            Event::Mouse(e) => {
                let m = mouse_from(e);
                // The pointer merely passing over is not worth a redraw.
                if m.motion() && browser.dragging.is_none() {
                    continue;
                }
                if m.left_press() {
                    browser.notice = None;
                }
                browser.mouse(m)
            }
            Event::Resize(_, _) => {
                needs_draw = true;
                continue;
            }
            _ => continue,
        };
        needs_draw = true;
        let result = match action {
            Action::None => continue,
            Action::Quit => break,
            Action::Notice(message) => {
                browser.notice = Some(message);
                continue;
            }
            Action::Focus(id) => open(&mut terminal, &mut browser, &id, false).await,
            Action::Zoom(id) => open(&mut terminal, &mut browser, &id, true).await,
            Action::Restart(id) => match restart(&id).await {
                Ok(()) => open(&mut terminal, &mut browser, &id, true).await,
                Err(error) => Err(error),
            },
            Action::New(command_line) => match new_session(&command_line).await {
                Ok(id) => open(&mut terminal, &mut browser, &id, true).await,
                Err(error) => Err(error),
            },
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
        assert_eq!(
            browser.confirm.as_ref().map(|c| c.kind),
            Some(ConfirmKind::Delete)
        );
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
        assert!(matches!(
            browser.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Notice(_)
        ));
    }

    fn laid_out() -> Browser {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![
            session("a", "one", "/tmp", "running", 3),
            session("b", "two", "/tmp", "running", 2),
            session("c", "three", "/tmp", "exited", 1),
        ]);
        browser.areas = Areas {
            body: Rect::new(0, 1, 100, 30),
            list: Rect::new(0, 1, 40, 30),
            right: Rect::new(40, 1, 60, 30),
            detail: Rect::new(40, 1, 60, 10),
            screen: Rect::new(41, 12, 58, 18),
        };
        browser.item_heights = vec![2, 2, 2];
        browser
    }

    fn press(x: u16, y: u16) -> Mouse {
        Mouse {
            code: 0,
            x,
            y,
            release: false,
        }
    }

    #[test]
    fn dragging_the_line_between_list_and_preview_resizes_them() {
        let mut browser = laid_out();
        assert!(matches!(browser.mouse(press(39, 5)), Action::None));
        assert_eq!(browser.dragging, Some(Divider::Columns));
        browser.mouse(Mouse {
            code: 32,
            x: 59,
            y: 5,
            release: false,
        });
        assert_eq!(browser.sizes.list_width(100), 60);
        browser.mouse(Mouse {
            code: 32,
            x: 99,
            y: 5,
            release: false,
        });
        assert_eq!(
            browser.sizes.list_width(100),
            100 - MIN_RIGHT_WIDTH,
            "the right side keeps a usable width"
        );
        browser.mouse(Mouse {
            code: 32,
            x: 2,
            y: 5,
            release: false,
        });
        assert_eq!(
            browser.sizes.list_width(100),
            MIN_LIST_WIDTH,
            "so does the list"
        );
    }

    #[test]
    fn dragging_the_line_between_details_and_preview_resizes_them() {
        let mut browser = laid_out();
        browser.mouse(press(60, 10));
        assert_eq!(browser.dragging, Some(Divider::Rows));
        browser.mouse(Mouse {
            code: 32,
            x: 60,
            y: 15,
            release: false,
        });
        assert_eq!(browser.sizes.detail_height(30), 15);
        browser.mouse(Mouse {
            code: 32,
            x: 60,
            y: 200,
            release: false,
        });
        assert_eq!(browser.sizes.detail_height(30), 30 - MIN_PANE_HEIGHT);
    }

    #[test]
    fn clicking_and_the_wheel_move_through_the_list() {
        let mut browser = laid_out();
        browser.mouse(press(5, 4));
        assert_eq!(
            browser.selected().unwrap().id,
            "b",
            "second session: rows 4-5"
        );
        browser.mouse(Mouse {
            code: 65,
            x: 5,
            y: 4,
            release: false,
        });
        assert_eq!(browser.selected().unwrap().id, "c");
        browser.mouse(Mouse {
            code: 64,
            x: 5,
            y: 4,
            release: false,
        });
        browser.mouse(Mouse {
            code: 64,
            x: 5,
            y: 4,
            release: false,
        });
        assert_eq!(browser.selected().unwrap().id, "a");
        browser.mouse(Mouse {
            code: 65,
            x: 70,
            y: 20,
            release: false,
        });
        assert_eq!(
            browser.selected().unwrap().id,
            "a",
            "the wheel over the preview leaves the list alone"
        );
    }

    #[test]
    fn the_panel_becomes_a_terminal_only_for_a_running_session_that_is_not_this_one() {
        let mut browser = laid_out();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(browser.key(tab), Action::Focus(id) if id == "a"));
        assert!(
            matches!(browser.mouse(press(50, 20)), Action::Focus(id) if id == "a"),
            "clicking the preview does the same"
        );
        browser.selection.select(Some(2));
        assert!(
            matches!(browser.key(tab), Action::Notice(_)),
            "ended session"
        );
        browser.sessions[0].here = true;
        browser.selection.select(Some(0));
        assert!(matches!(browser.key(tab), Action::Notice(_)), "own session");
        assert!(matches!(browser.mouse(press(50, 20)), Action::Notice(_)));
    }

    #[test]
    fn long_text_is_cut_to_fit_and_paths_keep_their_end() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("a long session title", 10), "a long se…");
        assert_eq!(
            clip_start("/Users/someone/Documents/GitHub/project", 16),
            "…/GitHub/project"
        );
        assert_eq!(
            width(&clip("宽宽宽宽宽宽", 7)),
            7,
            "wide characters count for two columns"
        );
        // SAFETY: tests in this binary do not read HOME concurrently in a way that depends on its value.
        unsafe { std::env::set_var("HOME", "/Users/someone") };
        assert_eq!(tilde("/Users/someone/code"), "~/code");
        assert_eq!(
            tilde("/Users/someone-else/code"),
            "/Users/someone-else/code"
        );
    }

    #[test]
    fn a_session_row_shows_state_as_a_symbol_and_fits_the_list() {
        let mut running = session(
            "a",
            "A fairly long session title that will not fit",
            "/tmp/somewhere/deep/inside",
            "running",
            1,
        );
        running.attached = 1;
        for icons in [Icons::Plain, Icons::Nerd] {
            let item = session_item(&running, icons, false, 30, None);
            assert_eq!(item.height(), 2);
            assert!(item.width() <= 30, "{icons:?}: {}", item.width());
        }
        assert_eq!(Icons::Plain.status(&running).0, "●");
        let mut failed = session("b", "x", "/tmp", "exited", 1);
        failed.exit_code = Some(3);
        assert_eq!(status_text(&failed), "exit 3");
        assert_eq!(Icons::Plain.status(&failed), ("✕", Color::Red));
        assert_ne!(
            Icons::Nerd.status(&failed).0,
            Icons::Plain.status(&failed).0
        );
    }

    #[test]
    fn the_icon_key_switches_sets() {
        let mut browser = Browser::default();
        assert_eq!(browser.sizes.icons, Icons::Plain);
        assert_eq!(Icons::Plain.other(), Icons::Nerd);
        assert_eq!(Icons::Nerd.other().other(), Icons::Nerd);
        browser.sizes.icons = Icons::Nerd;
        assert_eq!(
            serde_json::from_str::<Sizes>(&serde_json::to_string(&browser.sizes).unwrap())
                .unwrap()
                .icons,
            Icons::Nerd
        );
        assert_eq!(
            serde_json::from_str::<Sizes>(r#"{"list":0.3,"detail":6}"#)
                .unwrap()
                .icons,
            Icons::Plain,
            "older saved layouts still load"
        );
    }

    #[test]
    fn rename_prompt_starts_with_the_current_name() {
        let mut browser = Browser::default();
        browser.replace_sessions(vec![session("one", "Work", "/", "running", 1)]);
        browser.key(key('R'));
        browser.key(key('x'));
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(browser.key(enter), Action::Rename(id, name) if id == "one" && name == "onex")
        );
    }

    #[test]
    fn text_view_scroll_counts_wrapped_rows_and_stops_at_the_end() {
        let mut browser = Browser::default();
        let text = format!("{}\nshort\n", "x".repeat(25));
        let mut view = TextView::new("one".into(), "One".into(), "logs", text);
        view.layout(10, 2);
        assert_eq!(
            view.wrapped.len(),
            4,
            "25 chars wrap to 3 rows of 10, plus one"
        );
        assert_eq!(view.scroll, 2, "opens at the end");
        browser.view = Some(view);
        browser.view_height = 2;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        browser.key(down);
        assert_eq!(
            browser.view.as_ref().unwrap().scroll,
            2,
            "cannot scroll past the end"
        );
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
