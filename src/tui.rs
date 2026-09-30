// The operator roster (US3, constitution IV/V): read-only chrome over
// the board. This module renders; it never posts mail, never digests a
// seat, and never blocks on mail — data comes from the same read-only
// helpers the `seats` verb uses. The only writer here is the terminal.
use crate::board;
use crate::error::{SeatError, Result};
use crate::msg;
use crate::room;
use crate::seat::{self, Lifecycle, WaiterState};
use crate::verbs;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::tty::IsTty;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Padding, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Chrome geometry: the card never hugs the screen edge (4 cols of
/// margin split two-a-side keeps the frame breathing), and it stays at
/// or under a third of the terminal width so an equal-width popover
/// fits beside the centered card (one column of gap, glyphs never
/// collide). Mid-size terminals get a partial-width popover instead —
/// beside the card is the invariant that never breaks.
const CARD_MAX_WIDTH: u16 = 88;
const CARD_MARGIN: u16 = 4;
const CARD_GAP: u16 = 1;
/// Narrowest popover worth showing.
#[cfg(test)]
const POP_MIN_WIDTH: u16 = 24;
/// Roster table honest minimum: columns + spacing + borders + inner pad.
const CARD_MIN_WIDTH: u16 = 56;
/// Inner gap between the border and content. `Padding::proportional`
/// uses 2× this on the left/right (cells are taller than they are wide).
const INNER_PAD: u16 = 1;
/// Idle refresh from the filesystem (plan: roster on a ~1s cadence).
const TICK: Duration = Duration::from_millis(1000);
const RECENT_ROOM_POSTS: usize = 6;

const KEY_STYLE: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
const DIM_STYLE: Style = Style::new().fg(Color::DarkGray);
const TITLE_STYLE: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
const BORDER_STYLE: Style = Style::new().fg(Color::Gray);
const LIVE_STYLE: Style = Style::new().fg(Color::Cyan);
const UNREAD_STYLE: Style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
/// Low-chroma full-row band (Codex: no purple wash).
const BAND_STYLE: Style = Style::new().bg(Color::DarkGray).fg(Color::White);

/// Min on every column: leftover is shared (Ratatui flex-fit), so the
/// name column does not swallow the empty space.
const ROSTER_WIDTHS: [Constraint; 6] = [
    Constraint::Min(14), // fable-booster is 13
    Constraint::Min(6),
    Constraint::Min(4),
    Constraint::Min(7),
    Constraint::Min(5),
    Constraint::Min(5),
];
const ROOM_WIDTHS: [Constraint; 3] = [Constraint::Min(12), Constraint::Min(6), Constraint::Min(9)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Roster,
    Rooms,
    Stats,
    Index,
    Detail,
}

impl View {
    fn name(self) -> &'static str {
        match self {
            View::Roster => "roster",
            View::Rooms => "rooms",
            View::Stats => "stats",
            View::Index => "index",
            View::Detail => "detail",
        }
    }
}

#[derive(Debug, Clone)]
enum DetailKind {
    Seat(usize),
    Room(usize),
}

/// One roster row plus everything the popover and detail view explain.
#[derive(Debug, Clone)]
struct SeatInfo {
    name: String,
    unread: usize,
    last_seen: String,
    last_drained: String,
    wake: &'static str,
    state: &'static str,
    harness: String,
    model: Option<String>,
    lifecycle: Option<Lifecycle>,
    waiter: WaiterState,
    pid: Option<u32>,
    inbox: usize,
    archive: usize,
    quarantine: usize,
    follows: Vec<String>,
    corrupt: Option<String>,
}

impl SeatInfo {
    fn lifecycle_text(&self) -> &str {
        self.lifecycle.map(|l| l.as_str()).unwrap_or("unset")
    }
    fn wake_explain(&self) -> &'static str {
        match self.lifecycle {
            Some(Lifecycle::ExitWake) => "exits 0 after one digest",
            Some(Lifecycle::Poller) => "stays up until SIGTERM",
            None => "no lifecycle recorded",
        }
    }
    fn waiter_text(&self) -> String {
        match (self.waiter, self.pid) {
            (WaiterState::Idle, _) => "none".to_string(),
            (WaiterState::Live, Some(p)) => format!("live (pid {p})"),
            (WaiterState::Live, None) => "live".to_string(),
            (WaiterState::Orphan, Some(p)) => format!("orphan (pid {p})"),
            (WaiterState::Orphan, None) => "orphan".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct RoomRow {
    name: String,
    posts: usize,
    followers: Vec<String>,
}

/// UI state. `card`/`pop` record where the last frame painted the card
/// and popover so mouse hit-testing maps rows without a re-layout.
struct Ui {
    root: PathBuf,
    view: View,
    seats: Vec<SeatInfo>,
    rooms: Vec<RoomRow>,
    room_preview: Vec<Line<'static>>,
    stats: Vec<String>,
    selected: usize,
    selected_room: usize,
    hover: Option<usize>,
    detail: DetailKind,
    status: Option<String>,
    quit: bool,
    card: Option<Rect>,
    pop: Option<Rect>,
}

impl Ui {
    fn new(root: &Path) -> Result<Self> {
        let mut ui = Ui {
            root: root.to_path_buf(),
            view: View::Roster,
            seats: Vec::new(),
            rooms: Vec::new(),
            room_preview: Vec::new(),
            stats: Vec::new(),
            selected: 0,
            selected_room: 0,
            hover: None,
            detail: DetailKind::Seat(0),
            status: None,
            quit: false,
            card: None,
            pop: None,
        };
        ui.refresh();
        Ok(ui)
    }

    /// Re-read the board for the current view. Errors land on the
    /// status line — the roster never panics on a bad board.
    fn refresh(&mut self) {
        self.status = None;
        match self.view {
            View::Roster | View::Index => self.load_seats(),
            View::Rooms => self.load_rooms(),
            View::Stats => self.load_stats(),
            View::Detail => match self.detail {
                DetailKind::Seat(_) => self.load_seats(),
                DetailKind::Room(_) => self.load_rooms(),
            },
        }
    }

    fn load_seats(&mut self) {
        let names = match verbs::sorted_seat_names(&self.root) {
            Ok(n) => n,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let mut seats = Vec::with_capacity(names.len());
        for name in names {
            let dir = seat::seat_dir(&self.root, &name);
            let (meta, corrupt) = match seat::read_meta(&dir) {
                Ok(m) => (Some(m), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let unread = verbs::unread_count(&self.root, &dir).unwrap_or(0);
            let lifecycle = meta.as_ref().and_then(|m| m.lifecycle);
            let waiter = seat::waiter_state(&dir);
            let pid = seat::read_wait_pid(&dir);
            seats.push(SeatInfo {
                wake: wake_word(lifecycle),
                state: state_word(corrupt.is_some(), waiter),
                unread,
                last_seen: meta
                    .as_ref()
                    .and_then(|m| m.last_seen.clone())
                    .unwrap_or_else(|| "-".to_string()),
                last_drained: meta
                    .as_ref()
                    .and_then(|m| m.last_drained.clone())
                    .unwrap_or_else(|| "-".to_string()),
                name,
                harness: meta.as_ref().map(|m| m.harness.clone()).unwrap_or_default(),
                model: meta.as_ref().and_then(|m| m.model.clone()),
                lifecycle,
                waiter,
                pid,
                inbox: msg::list_msg_dir(&dir.join("inbox"))
                    .map(|v| v.len())
                    .unwrap_or(0),
                archive: msg::list_msg_dir(&dir.join("archive"))
                    .map(|v| v.len())
                    .unwrap_or(0),
                quarantine: msg::list_msg_dir(&dir.join("quarantine"))
                    .map(|v| v.len())
                    .unwrap_or(0),
                follows: followed_rooms(&dir),
                corrupt,
            });
        }
        self.seats = seats;
        if self.seats.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.seats.len() - 1);
        }
    }

    fn load_rooms(&mut self) {
        let names = match room_names(&self.root) {
            Ok(n) => n,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let seats = verbs::sorted_seat_names(&self.root).unwrap_or_default();
        let mut rooms = Vec::with_capacity(names.len());
        for name in names {
            let posts = room::posts(&self.root, &name).map(|p| p.len()).unwrap_or(0);
            let mut followers = Vec::new();
            for seat_name in &seats {
                if seat::seat_dir(&self.root, seat_name)
                    .join("cursors")
                    .join(&name)
                    .exists()
                {
                    followers.push(seat_name.clone());
                }
            }
            rooms.push(RoomRow {
                name,
                posts,
                followers,
            });
        }
        self.rooms = rooms;
        if self.rooms.is_empty() {
            self.selected_room = 0;
        } else {
            self.selected_room = self.selected_room.min(self.rooms.len() - 1);
        }
    }

    fn load_stats(&mut self) {
        // Board at a glance, read from the board itself (the roster is
        // read-only chrome; there is no event log to count).
        let seats = verbs::sorted_seat_names(&self.root).unwrap_or_default();
        let mut armed = 0usize;
        let mut unread = 0usize;
        let mut archived = 0usize;
        let mut quarantined = 0usize;
        for name in &seats {
            let dir = seat::seat_dir(&self.root, name);
            unread += verbs::unread_count(&self.root, &dir).unwrap_or(0);
            match seat::waiter_state(&dir) {
                WaiterState::Idle => {}
                WaiterState::Live | WaiterState::Orphan => armed += 1,
            }
            archived += msg::list_msg_dir(&dir.join("archive"))
                .map(|v| v.len())
                .unwrap_or(0);
            quarantined += msg::list_msg_dir(&dir.join("quarantine"))
                .map(|v| v.len())
                .unwrap_or(0);
        }
        let rooms = room_names(&self.root).unwrap_or_default();
        let mut posts = 0usize;
        for name in &rooms {
            posts += room::posts(&self.root, name).map(|p| p.len()).unwrap_or(0);
        }
        self.stats = vec![
            format!("seats {}", seats.len()),
            format!("armed {armed}"),
            format!("unread {unread}"),
            format!("rooms {}", rooms.len()),
            format!("posts {posts}"),
            format!("archived {archived}"),
            format!("quarantined {quarantined}"),
        ];
    }

    fn go(&mut self, view: View) {
        self.view = view;
        self.hover = None;
        self.refresh();
    }

    fn open_detail(&mut self) {
        match self.view {
            View::Roster if self.selected < self.seats.len() => {
                self.detail = DetailKind::Seat(self.selected);
                self.go(View::Detail);
            }
            View::Rooms if self.selected_room < self.rooms.len() => {
                self.detail = DetailKind::Room(self.selected_room);
                self.room_preview = room_preview(&self.root, &self.rooms[self.selected_room]);
                self.go(View::Detail);
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.go(View::Index),
            KeyCode::Esc => {
                if self.view != View::Roster {
                    self.go(View::Roster);
                }
            }
            // j/k move the selection and always drop the hover (the
            // keyboard owns the highlight now).
            KeyCode::Char('j') | KeyCode::Down => {
                self.hover = None;
                self.move_selection(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.hover = None;
                self.move_selection(-1);
            }
            KeyCode::Enter => self.open_detail(),
            KeyCode::Char('r') if self.view == View::Roster => self.go(View::Rooms),
            KeyCode::Char('s') if self.view == View::Roster => self.go(View::Stats),
            _ => {}
        }
    }

    fn move_selection(&mut self, dir: i32) {
        let (sel, len) = match self.view {
            View::Roster => (&mut self.selected, self.seats.len()),
            View::Rooms => (&mut self.selected_room, self.rooms.len()),
            _ => return,
        };
        if len == 0 {
            return;
        }
        *sel = ((*sel as i32) + dir).rem_euclid(len as i32) as usize;
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::Moved => {
                if self.view == View::Roster {
                    if let Some(i) = self.row_at(m.column, m.row) {
                        self.selected = i;
                        self.hover = Some(i);
                    } else {
                        self.hover = None;
                    }
                } else {
                    self.hover = None;
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.view == View::Roster => {
                if let Some(i) = self.row_at(m.column, m.row) {
                    self.selected = i;
                    self.hover = Some(i);
                }
            }
            _ => {}
        }
    }

    /// Map a screen cell to a roster row. Header, footer, borders, and
    /// inner padding are not rows. `card` is where the last frame
    /// painted the card, so the band can never be hit outside it.
    fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        let card = self.card?;
        let inner = card_inner(card);
        if inner.width == 0 || inner.height < 2 {
            return None;
        }
        if x < inner.x || x >= inner.x + inner.width {
            return None;
        }
        // inner.y is the table header; the last inner row is the footer.
        if y <= inner.y || y >= inner.y + inner.height - 1 {
            return None;
        }
        let i = (y - inner.y - 1) as usize;
        (i < self.seats.len()).then_some(i)
    }

    fn draw(&mut self, f: &mut Frame) {
        let frame = f.area();
        self.draw_card(f, frame);
        if let Some(s) = &self.status {
            f.render_widget(
                Paragraph::new(Line::styled(s.clone(), Style::new().fg(Color::Red))),
                Rect::new(frame.x, frame.y, frame.width, 1),
            );
        }
    }

    fn draw_card(&mut self, f: &mut Frame, avail: Rect) {
        self.pop = None;
        let rows_needed = match self.view {
            View::Roster => self.seats.len().max(1),
            View::Rooms => self.rooms.len().max(1),
            View::Stats => self.stats.len() + 1,
            View::Index => 9,
            View::Detail => match self.detail {
                DetailKind::Seat(i) => self.seats.get(i).map(|s| seat_detail_lines(s).len() + 1),
                DetailKind::Room(i) => self
                    .rooms
                    .get(i)
                    .map(|r| room_detail_lines(r, &self.room_preview).len() + 1),
            }
            .unwrap_or(1)
            .max(1),
        };
        let want_panel = self.view == View::Roster;
        let (card, pop_slot) = pair_rects_ex(avail, rows_needed, want_panel);
        let block = Block::bordered()
            .title(format!(" seat — {} ", self.view.name()))
            .title_style(TITLE_STYLE)
            .border_style(BORDER_STYLE)
            .padding(Padding::proportional(INNER_PAD));
        let inner = block.inner(card);
        let split = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let (body, foot) = (split[0], split[1]);
        f.render_widget(block, card);
        match self.view {
            View::Roster => self.draw_roster_table(f, body),
            View::Rooms => self.draw_rooms_table(f, body),
            View::Stats => self.draw_stats(f, body),
            View::Index => f.render_widget(Paragraph::new(index_lines()), body),
            View::Detail => self.draw_detail(f, body),
        }
        draw_footer(f, foot, self.view);
        self.card = Some(card);
        self.draw_hover(f, pop_slot);
    }

    fn draw_roster_table(&mut self, f: &mut Frame, inner: Rect) {
        if self.seats.is_empty() {
            f.render_widget(
                Paragraph::new(Line::styled(
                    "(no seats — register one: seat register --seat <name> --harness <h>)",
                    DIM_STYLE,
                )),
                inner,
            );
            return;
        }
        let header = Row::new([
            Cell::from("seat"),
            Cell::from(Text::from("unread").right_aligned()),
            Cell::from("seen"),
            Cell::from("drained"),
            Cell::from(Text::from("wake").right_aligned()),
            Cell::from("state"),
        ])
        .style(DIM_STYLE.add_modifier(Modifier::BOLD));
        let rows = self.seats.iter().map(|s| {
            let unread = if s.unread > 0 {
                UNREAD_STYLE
            } else {
                DIM_STYLE
            };
            let state = match s.state {
                "live" => LIVE_STYLE,
                "orphan" | "corrupt" => Style::new().fg(Color::Red),
                _ => DIM_STYLE,
            };
            Row::new([
                Cell::from(s.name.clone()),
                Cell::from(Text::from(s.unread.to_string()).right_aligned()).style(unread),
                Cell::from(short_rel(&s.last_seen)),
                Cell::from(short_rel(&s.last_drained)),
                Cell::from(Text::from(s.wake).right_aligned()),
                Cell::from(s.state).style(state),
            ])
        });
        let table = Table::new(rows, ROSTER_WIDTHS)
            .header(header)
            .row_highlight_style(BAND_STYLE)
            .column_spacing(2)
            .flex(Flex::SpaceBetween);
        let mut state = TableState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(table, inner, &mut state);
    }

    fn draw_rooms_table(&mut self, f: &mut Frame, inner: Rect) {
        if self.rooms.is_empty() {
            f.render_widget(
                Paragraph::new(Line::styled(
                    "(no rooms — create one: seat room create <name>)",
                    DIM_STYLE,
                )),
                inner,
            );
            return;
        }
        let header = Row::new([
            Cell::from("room"),
            Cell::from(Text::from("posts").right_aligned()),
            Cell::from(Text::from("follows").right_aligned()),
        ])
        .style(DIM_STYLE.add_modifier(Modifier::BOLD));
        let rows = self.rooms.iter().map(|r| {
            Row::new([
                Cell::from(r.name.clone()),
                Cell::from(Text::from(r.posts.to_string()).right_aligned()),
                Cell::from(Text::from(r.followers.len().to_string()).right_aligned()),
            ])
        });
        let table = Table::new(rows, ROOM_WIDTHS)
            .header(header)
            .row_highlight_style(BAND_STYLE)
            .column_spacing(2)
            .flex(Flex::SpaceBetween);
        let mut state = TableState::default();
        state.select(Some(self.selected_room));
        f.render_stateful_widget(table, inner, &mut state);
    }

    fn draw_stats(&mut self, f: &mut Frame, inner: Rect) {
        let mut lines = vec![Line::styled("board at a glance".to_string(), DIM_STYLE)];
        lines.extend(self.stats.iter().map(|l| {
            let mut parts = l.split_whitespace();
            Line::from(vec![
                Span::styled(parts.next().unwrap_or("").to_string(), KEY_STYLE),
                Span::styled(format!("  {}", parts.next().unwrap_or("")), DIM_STYLE),
            ])
        }));
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_detail(&mut self, f: &mut Frame, inner: Rect) {
        let lines = match self.detail {
            DetailKind::Seat(i) => match self.seats.get(i) {
                Some(s) => seat_detail_lines(s),
                None => vec![Line::styled("(seat vanished)".to_string(), DIM_STYLE)],
            },
            DetailKind::Room(i) => match self.rooms.get(i) {
                Some(r) => room_detail_lines(r, &self.room_preview),
                None => vec![Line::styled("(room vanished)".to_string(), DIM_STYLE)],
            },
        };
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The hover popover: same width as the card, beside it (never over
    /// the rows), with the title painted once by the block.
    fn draw_hover(&mut self, f: &mut Frame, pop_slot: Option<Rect>) {
        if self.view != View::Roster {
            return;
        }
        // Always explain the highlighted row (keyboard or mouse). A
        // leftover hover on a different seat was painting the wrong panel.
        let Some(info) = self.seats.get(self.selected) else {
            return;
        };
        let Some(mut pop) = pop_slot else {
            return;
        };
        let lines = popover_lines(info);
        pop.height = (lines.len() as u16)
            .saturating_add(2)
            .saturating_add(INNER_PAD.saturating_mul(2))
            .min(pop.height)
            .max(2);
        let block = Block::bordered()
            .title(format!(" {} ", info.name))
            .title_style(TITLE_STYLE)
            .border_style(BORDER_STYLE)
            .padding(Padding::proportional(INNER_PAD));
        let inner = block.inner(pop);
        f.render_widget(block, pop);
        f.render_widget(Paragraph::new(lines), inner);
        self.pop = Some(pop);
    }
}

// ---- read-only board helpers (local to this module) -----------------

fn room_names(root: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(board::rooms_dir(root))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

fn followed_rooms(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("cursors"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Recent posts of a room, newest last, one line each (for detail).
fn room_preview(root: &Path, r: &RoomRow) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Ok(posts) = room::posts(root, &r.name) {
        for p in posts.iter().rev().take(RECENT_ROOM_POSTS) {
            if let Ok((header, body)) = msg::read_msg(p) {
                let first = body.lines().next().unwrap_or("").to_string();
                out.push(Line::from(vec![
                    Span::styled(short_ts(&header.ts), DIM_STYLE),
                    Span::raw(format!(" {} ", header.from)),
                    Span::styled(first, Style::new()),
                ]));
            }
        }
    }
    out
}

// ---- pure chrome helpers (unit-tested without a terminal) -----------

fn wake_word(lifecycle: Option<Lifecycle>) -> &'static str {
    match lifecycle {
        Some(Lifecycle::ExitWake) => "exits",
        Some(Lifecycle::Poller) => "stays",
        None => "-",
    }
}

fn state_word(corrupt: bool, waiter: WaiterState) -> &'static str {
    match (corrupt, waiter) {
        (true, _) => "corrupt",
        (false, WaiterState::Idle) => "idle",
        (false, WaiterState::Live) => "live",
        (false, WaiterState::Orphan) => "orphan",
    }
}

/// `2026-09-23T07:14:12.123456+00:00` → `09-23 07:14` for popover and
/// detail; unparseable values pass through truncated.
fn short_ts(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => t.format("%m-%d %H:%M").to_string(),
        Err(_) => ts.chars().take(11).collect(),
    }
}

/// Compact age for the roster's seen / drained columns: `now`, `4m`,
/// `3h`, `2d`, then `MM-DD`. Six columns of table have to fit in half
/// a terminal so the popover still fits beside the card.
fn short_rel(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return ts.chars().take(5).collect();
    };
    let t = t.with_timezone(&chrono::Utc);
    let mins = chrono::Utc::now().signed_duration_since(t).num_minutes();
    if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if mins < 60 * 24 {
        format!("{}h", mins / 60)
    } else if mins < 60 * 24 * 30 {
        format!("{}d", mins / (60 * 24))
    } else {
        t.format("%m-%d").to_string()
    }
}

fn footer_pairs(view: View) -> Vec<(&'static str, &'static str)> {
    match view {
        View::Roster => vec![
            ("j/k", "move"),
            ("r", "rooms"),
            ("enter", "open"),
            ("q", "quit"),
        ],
        View::Rooms => vec![
            ("j/k", "move"),
            ("esc", "roster"),
            ("enter", "detail"),
            ("q", "quit"),
        ],
        _ => vec![("esc", "roster"), ("q", "quit")],
    }
}

/// The footer grammar (FR-006): `key:action | key:action`. Used by the
/// unit tests to pin the exact grammar; the live footer is draw_footer.
#[cfg_attr(not(test), allow(dead_code))]
fn footer_text(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, a)| format!("{k}:{a}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Spread key:action pairs across the card so they are not packed left.
fn draw_footer(f: &mut Frame, area: Rect, view: View) {
    let pairs = footer_pairs(view);
    if pairs.is_empty() || area.width == 0 {
        return;
    }
    let widths: Vec<Constraint> = pairs
        .iter()
        .map(|(k, a)| Constraint::Length((k.len() + 1 + a.len()) as u16))
        .collect();
    let cols = Layout::horizontal(widths)
        .flex(Flex::SpaceBetween)
        .split(area);
    for ((k, a), rect) in pairs.iter().zip(cols.iter()) {
        f.render_widget(
            Line::from(vec![
                Span::styled((*k).to_string(), KEY_STYLE),
                Span::styled(format!(":{a}"), DIM_STYLE),
            ]),
            *rect,
        );
    }
}

fn kv_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<13}"), DIM_STYLE),
        Span::raw(value.to_string()),
    ])
}

fn list_text(names: &[String]) -> String {
    if names.is_empty() {
        "-".to_string()
    } else {
        names.join(", ")
    }
}

fn popover_lines(s: &SeatInfo) -> Vec<Line<'static>> {
    vec![
        kv_line(
            "harness",
            &match &s.model {
                Some(m) => format!("{} ({m})", s.harness),
                None => s.harness.clone(),
            },
        ),
        kv_line("wake", s.wake_explain()),
        kv_line("waiter", &s.waiter_text()),
        kv_line("unread", &format!("{} (inbox {})", s.unread, s.inbox)),
        kv_line("last seen", &short_ts(&s.last_seen)),
        kv_line("last drained", &short_ts(&s.last_drained)),
        kv_line("follows", &list_text(&s.follows)),
    ]
}

fn seat_detail_lines(s: &SeatInfo) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv_line("seat", &s.name),
        kv_line("harness", &s.harness),
        kv_line("model", s.model.as_deref().unwrap_or("-")),
        kv_line(
            "lifecycle",
            &format!("{} — {}", s.lifecycle_text(), s.wake_explain()),
        ),
        kv_line("waiter", &s.waiter_text()),
        kv_line(
            "unread",
            &format!(
                "{} (inbox {}, rooms {})",
                s.unread,
                s.inbox,
                s.unread.saturating_sub(s.inbox)
            ),
        ),
        kv_line("archive", &s.archive.to_string()),
        kv_line("quarantine", &s.quarantine.to_string()),
        kv_line("follows", &list_text(&s.follows)),
        kv_line("last seen", &s.last_seen),
        kv_line("last drained", &s.last_drained),
    ];
    if let Some(why) = &s.corrupt {
        lines.push(Line::styled(
            format!("meta: {why}"),
            Style::new().fg(Color::Red),
        ));
    }
    lines
}

fn room_detail_lines(r: &RoomRow, preview: &[Line<'static>]) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv_line("room", &r.name),
        kv_line("posts", &r.posts.to_string()),
        kv_line("followers", &list_text(&r.followers)),
        Line::styled("recent posts".to_string(), DIM_STYLE),
    ];
    if preview.is_empty() {
        lines.push(Line::styled("(no posts yet)".to_string(), DIM_STYLE));
    } else {
        lines.extend(preview.iter().cloned());
    }
    lines
}

fn index_lines() -> Vec<Line<'static>> {
    vec![
        kv_line("?", "this index"),
        kv_line("r", "rooms"),
        kv_line("s", "stats (board at a glance)"),
        kv_line("enter", "detail for the selection"),
        kv_line("j / k", "move the selection (clears hover)"),
        kv_line("hover", "rest the pointer on a row to explain it"),
        kv_line("esc", "back to the roster"),
        kv_line("q", "quit"),
        Line::styled("the roster is read-only chrome".to_string(), DIM_STYLE),
    ]
}

// ---- geometry (pure; the band-inside-the-card contract lives here) ---

/// Centered card: sized so an equal-width side panel still fits
/// (`2*w + gap <= screen`). Floored at the table minimum. Never full bleed.
#[cfg(test)]
fn card_rect(avail: Rect, data_rows: usize) -> Rect {
    pair_rects_ex(avail, data_rows, true).0
}

/// Area inside the card's border and proportional padding — the same
/// rectangle Block::inner produces when we paint.
fn card_inner(card: Rect) -> Rect {
    Block::bordered()
        .padding(Padding::proportional(INNER_PAD))
        .inner(card)
}

/// Card + optional equal-width panel, centered as a pair.
fn pair_rects_ex(avail: Rect, data_rows: usize, want_panel: bool) -> (Rect, Option<Rect>) {
    let cap = CARD_MAX_WIDTH.min(avail.width.saturating_sub(CARD_MARGIN));
    // borders(2) + header(1) + footer(1) + vertical pad (2 * INNER_PAD)
    let h = (data_rows as u16 + 4 + 2 * INNER_PAD)
        .min(avail.height)
        .max(1);
    let y = avail.y + avail.height.saturating_sub(h) / 2;
    let pair_budget = avail.width.saturating_sub(CARD_MARGIN.max(CARD_GAP));
    let pair_fits =
        want_panel && pair_budget >= CARD_MIN_WIDTH.saturating_mul(2).saturating_add(CARD_GAP);
    if pair_fits {
        let w = (pair_budget.saturating_sub(CARD_GAP) / 2).clamp(CARD_MIN_WIDTH.min(cap), cap);
        let pair_w = w.saturating_mul(2).saturating_add(CARD_GAP);
        let x0 = avail.x + avail.width.saturating_sub(pair_w) / 2;
        let card = Rect::new(x0, y, w, h);
        let pop = Rect::new(card.x + card.width + CARD_GAP, y, w, h);
        (card, Some(pop))
    } else {
        // Sparse views (rooms/stats) stay content-sized, not half the terminal.
        let compact = cap.min(52);
        let w = compact.clamp(CARD_MIN_WIDTH.min(compact), compact);
        let x0 = avail.x + avail.width.saturating_sub(w) / 2;
        (
            Rect::new(x0, y, w.min(avail.width.saturating_sub(x0 - avail.x)), h),
            None,
        )
    }
}

/// A box beside the card — never over the rows. Equal width to the
/// card when a side has room (right preferred, then left); otherwise
/// the wider margin hosts a partial-width box; None when neither side
/// is worth painting.
#[cfg(test)]
fn popover_rect(card: Rect, avail: Rect, need_h: u16) -> Option<Rect> {
    let space = avail.height.saturating_sub(card.y);
    if space < 2 {
        return None;
    }
    let h = need_h.clamp(2, space);
    let right = avail.width.saturating_sub(card.x + card.width + CARD_GAP);
    let left = card.x.saturating_sub(CARD_GAP);
    let (x, w) = if right >= card.width {
        (card.x + card.width + CARD_GAP, card.width)
    } else if left >= card.width {
        (card.x - card.width - CARD_GAP, card.width)
    } else if right >= left && right >= POP_MIN_WIDTH {
        (card.x + card.width + CARD_GAP, right)
    } else if left > right && left >= POP_MIN_WIDTH {
        (card.x - left - CARD_GAP, left)
    } else {
        return None;
    };
    Some(Rect::new(x, card.y, w, h))
}

// ---- entry, terminal lifecycle ---------------------------------------

/// T033/T038: decide what bare `seat` does. TERM=dumb (or unset) is one
/// line on stderr and exit 1; a missing tty is the command-required
/// refusal (FR-007); only then does the roster open.
pub fn launch(root: &Path) -> Result<()> {
    match std::env::var("TERM") {
        Ok(t) if !t.is_empty() && t != "dumb" => {}
        _ => {
            return Err(SeatError::Tui(
                "TERM is dumb or unset; the roster needs a real terminal (verbs still work)"
                    .to_string(),
            ))
        }
    }
    if !std::io::stdin().is_tty() || !std::io::stdout().is_tty() {
        return Err(SeatError::Tui(format!(
            "a verb is required when stdin/stdout are not a tty; verbs: {}",
            crate::KEEP_LIST
        )));
    }
    board::ensure_board(root)?;
    let mut terminal = setup_terminal()?;
    // If anything panics under the hood, put the terminal back first.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_globals();
        original_hook(info);
    }));
    let result = run(&mut terminal, root);
    restore_terminal(&mut terminal);
    drop(std::panic::take_hook()); // our hook's job is done
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) {
    let _ = terminal.show_cursor();
    restore_terminal_globals();
}

fn restore_terminal_globals() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, root: &Path) -> Result<()> {
    let mut ui = Ui::new(root)?;
    loop {
        terminal.draw(|f| ui.draw(f))?;
        if ui.quit {
            break;
        }
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(k) => ui.on_key(k),
                Event::Mouse(m) => ui.on_mouse(m),
                Event::Resize(_, _) => {}
                _ => {}
            }
        } else {
            ui.refresh(); // idle tick: ~1s board re-read
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board;
    use crate::seat::Lifecycle;
    use crate::testutil::temp_root;
    use crate::verbs;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn board_with_seats(tag: &str) -> PathBuf {
        let root = temp_root(tag);
        board::init(&root).unwrap();
        verbs::cmd_register(
            &root,
            "alpha",
            "sb",
            Some("glm".into()),
            Some(Lifecycle::ExitWake),
            None,
            Vec::new(),
        )
        .unwrap();
        verbs::cmd_register(
            &root,
            "beta",
            "sb",
            None,
            Some(Lifecycle::Poller),
            None,
            Vec::new(),
        )
        .unwrap();
        root
    }

    /// Drop one unread message into a seat's inbox (test fixture; the
    /// roster module itself only ever reads).
    fn stash_mail(root: &Path, to: &str) {
        let token = verbs::sender_token(root, "alpha").unwrap();
        let dir = seat::seat_dir(root, to);
        msg::write_msg(&dir.join("inbox"), to, "alpha", &token, "unread mail").unwrap();
    }

    fn buffer_lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        let mut lines = Vec::new();
        for y in 0..buf.area.height {
            let mut s = String::new();
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            lines.push(s);
        }
        lines
    }

    #[test]
    fn wake_words_and_state_words() {
        assert_eq!(wake_word(Some(Lifecycle::ExitWake)), "exits");
        assert_eq!(wake_word(Some(Lifecycle::Poller)), "stays");
        assert_eq!(wake_word(None), "-");
        assert_eq!(state_word(false, WaiterState::Idle), "idle");
        assert_eq!(state_word(false, WaiterState::Live), "live");
        assert_eq!(state_word(false, WaiterState::Orphan), "orphan");
        assert_eq!(state_word(true, WaiterState::Live), "corrupt");
    }

    #[test]
    fn short_ts_compacts_known_stamps() {
        assert_eq!(short_ts("2026-09-23T07:14:12.123456+00:00"), "09-23 07:14");
        assert_eq!(short_ts("-"), "-");
        assert_eq!(short_ts("garbage-in"), "garbage-in");
        let rel = short_rel("2026-09-23T07:14:12.123456+00:00");
        assert!(rel.chars().count() <= 5, "{rel}");
        assert_eq!(short_rel("-"), "-");
    }

    #[test]
    fn footer_grammar_and_esc_roster_off_roster_only() {
        assert_eq!(
            footer_text(&footer_pairs(View::Roster)),
            "j/k:move | r:rooms | enter:open | q:quit"
        );
        for view in [View::Rooms, View::Stats, View::Index, View::Detail] {
            let text = footer_text(&footer_pairs(view));
            assert!(text.contains("esc:roster"), "{text}");
            assert!(text.ends_with("q:quit"), "{text}");
        }
        assert!(!footer_text(&footer_pairs(View::Roster)).contains("esc:roster"));
    }

    #[test]
    fn popover_is_beside_the_card_equal_width_when_possible() {
        let term = Rect::new(0, 0, 100, 30);
        let card = Rect::new(10, 8, 40, 14);
        let pop = popover_rect(card, term, 9).unwrap();
        assert_eq!(pop.width, card.width, "equal width");
        assert_eq!(pop.x, card.x + card.width + 1, "right side, one-col gap");
        assert!(pop.x >= card.x + card.width, "never over the rows");
        assert_eq!(pop.height, 9);

        // Right side does not fit → mirror to the left, still equal.
        let card_r = Rect::new(70, 8, 40, 14);
        let pop2 = popover_rect(card_r, term, 9).unwrap();
        assert_eq!(pop2.x + pop2.width + 1, card_r.x);
        assert_eq!(pop2.width, card_r.width);

        // Both margins too small for equal width → the wider margin
        // hosts a partial-width box (still beside, never over rows).
        let mid = Rect::new(0, 0, 100, 30);
        let card_m = Rect::new(40, 8, 40, 14);
        let pop3 = popover_rect(card_m, mid, 9).unwrap();
        assert!(pop3.width < card_m.width, "partial width");
        assert!(pop3.width >= POP_MIN_WIDTH);
        assert!(pop3.x >= card_m.x + card_m.width || pop3.x + pop3.width < card_m.x);

        // Neither side worth painting → no popover at all.
        let tight = Rect::new(0, 0, 50, 30);
        assert!(popover_rect(Rect::new(5, 8, 40, 14), tight, 9).is_none());
        // Height clamps to the screen space under the card top.
        let low = Rect::new(0, 0, 100, 30);
        assert_eq!(
            popover_rect(Rect::new(10, 28, 20, 2), low, 9)
                .unwrap()
                .height,
            2
        );
    }

    #[test]
    fn card_is_centered_never_full_width_and_popover_fits_beside() {
        let term = Rect::new(0, 0, 120, 40);
        let card = card_rect(term, 10);
        assert!(card.width < term.width);
        assert!(card.height < term.height);
        assert!(card.x >= 2, "left margin");
        assert!(card.x + card.width <= term.width - 2, "right margin");
        assert_eq!(
            card.height, 16,
            "rows + header + borders + footer + vertical pad"
        );
        // Beside the card, something always fits on this size.
        let pop = pair_rects_ex(term, 10, true).1.expect("side panel");
        assert!(pop.x >= card.x + card.width, "never over the rows");
        assert_eq!(pop.width, card.width, "equal-width pair");

        // A wide screen grows the card to the cap and the popover is
        // equal-width beside it.
        let wide_area = Rect::new(0, 0, 240, 40);
        let (wide, wpop) = pair_rects_ex(wide_area, 10, true);
        assert_eq!(wide.width, CARD_MAX_WIDTH);
        assert_eq!(
            wpop.unwrap().width,
            wide.width,
            "equal width on a wide screen"
        );

        // ...and a small screen caps it to the margin, full height.
        let small = card_rect(Rect::new(0, 0, 30, 10), 40);
        assert_eq!(small.width, 26);
        assert_eq!(small.height, 10, "capped to the screen");
    }

    #[test]
    fn row_at_maps_only_rows_inside_the_card() {
        let root = board_with_seats("tui-hover");
        let mut ui = Ui::new(&root).unwrap();
        ui.card = Some(Rect::new(30, 5, 40, 8 + 2 * INNER_PAD));
        let card = ui.card.unwrap();
        let inner = card_inner(card);
        let x = inner.x + 1;
        let first = inner.y + 1;
        assert_eq!(ui.row_at(x, first), Some(0), "first row under the header");
        assert_eq!(ui.row_at(x, first + 1), Some(1));
        assert_eq!(ui.row_at(x, inner.y), None, "header row does not hover");
        assert_eq!(ui.row_at(x, card.y), None, "top border");
        assert_eq!(ui.row_at(card.x, first), None, "left of the card");
        assert_eq!(
            ui.row_at(x, card.y.saturating_sub(1)),
            None,
            "above the card"
        );
        let last = card.y + card.height - 1;
        assert_eq!(ui.row_at(x, last), None, "bottom border");
        assert_eq!(ui.row_at(x, last + 1), None, "below the card");
        ui.card = None;
        assert_eq!(ui.row_at(x, first), None, "no frame yet");
    }

    #[test]
    fn keys_drive_views_and_jk_clears_hover() {
        let root = board_with_seats("tui-keys");
        room::create(&root, "ops").unwrap();
        let mut ui = Ui::new(&root).unwrap();
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);

        ui.hover = Some(0);
        ui.on_key(key(KeyCode::Char('j')));
        assert_eq!(ui.hover, None, "j clears hover");
        assert_eq!(ui.selected, 1);
        ui.on_key(key(KeyCode::Char('k')));
        assert_eq!(ui.selected, 0);
        ui.on_key(key(KeyCode::Char('k')));
        assert_eq!(ui.selected, 1, "selection wraps");

        ui.on_key(key(KeyCode::Char('r')));
        assert_eq!(ui.view, View::Rooms);
        assert!(!ui.rooms.is_empty());
        ui.on_key(key(KeyCode::Esc));
        assert_eq!(ui.view, View::Roster);
        ui.on_key(key(KeyCode::Esc));
        assert_eq!(ui.view, View::Roster, "esc on roster is a no-op");

        ui.on_key(key(KeyCode::Char('s')));
        assert_eq!(ui.view, View::Stats);
        assert!(!ui.stats.is_empty(), "stats lines loaded");
        ui.on_key(key(KeyCode::Esc));

        ui.on_key(key(KeyCode::Char('?')));
        assert_eq!(ui.view, View::Index);
        ui.on_key(key(KeyCode::Esc));
        assert_eq!(ui.view, View::Roster);

        ui.on_key(key(KeyCode::Enter));
        assert_eq!(ui.view, View::Detail);
        ui.on_key(key(KeyCode::Esc));
        assert_eq!(ui.view, View::Roster);

        ui.on_key(key(KeyCode::Char('q')));
        assert!(ui.quit);
    }

    #[test]
    fn launch_refuses_dumb_term_and_non_tty_cleanly() {
        // cargo test is not a tty, so both refusals are exercisable
        // without ever touching a terminal.
        if std::io::stdin().is_tty() {
            return; // run under `cargo test -- --nocapture` on a tty: skip
        }
        let root = board_with_seats("tui-launch");
        let saved = std::env::var("TERM").ok();
        std::env::set_var("TERM", "dumb");
        let err = launch(&root).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("term"), "{err}");
        assert!(!err.to_string().contains('\n'), "one line");

        std::env::set_var("TERM", "xterm-256color");
        let err2 = launch(&root).unwrap_err();
        assert!(err2.to_string().contains("a verb is required"), "{err2}");
        assert!(err2.to_string().contains("init register"), "names verbs");
        match saved {
            Some(t) => std::env::set_var("TERM", t),
            None => std::env::remove_var("TERM"),
        }
    }

    #[test]
    fn roster_loads_columns_read_only() {
        let root = board_with_seats("tui-load");
        stash_mail(&root, "beta");
        let mut ui = Ui::new(&root).unwrap();
        assert_eq!(ui.seats.len(), 2);
        let alpha = ui.seats.iter().find(|s| s.name == "alpha").unwrap();
        let beta = ui.seats.iter().find(|s| s.name == "beta").unwrap();
        assert_eq!(alpha.wake, "exits");
        assert_eq!(beta.wake, "stays");
        assert_eq!(beta.unread, 1);
        assert_eq!(beta.inbox, 1);
        assert_eq!(alpha.unread, 0);
        assert_eq!(alpha.state, "idle");
        assert_eq!(beta.state, "idle");

        // Rooms view counts posts and followers.
        room::create(&root, "ops").unwrap();
        room::cmd_post(&root, "ops", "alpha", "bulletin").unwrap();
        room::cmd_follow(&root, "ops", "beta").unwrap();
        ui.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert_eq!(ui.view, View::Rooms);
        let ops = ui.rooms.iter().find(|r| r.name == "ops").unwrap();
        assert_eq!(ops.posts, 1);
        assert_eq!(ops.followers, vec!["beta".to_string()]);
    }

    #[test]
    fn selection_band_stays_inside_the_card() {
        let root = board_with_seats("tui-band");
        let mut ui = Ui::new(&root).unwrap();
        ui.selected = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();

        let card = ui.card.expect("card recorded");
        assert!(card.width < 100, "card is not full width");
        let buf = terminal.backend().buffer();
        let mut band_xs: Vec<u16> = Vec::new();
        let mut band_y: Option<u16> = None;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf[(x, y)].style().bg == Some(Color::DarkGray) {
                    band_xs.push(x);
                    band_y = Some(y);
                }
            }
        }
        assert!(!band_xs.is_empty(), "selection band is painted");
        let min_x = *band_xs.iter().min().unwrap();
        let max_x = *band_xs.iter().max().unwrap();
        assert!(min_x > 0 && max_x < 99, "band is not full terminal width");
        let inner = card_inner(card);
        assert!(min_x >= inner.x, "band starts inside the padded card");
        assert!(
            max_x < inner.x + inner.width,
            "band ends inside the padded card"
        );
        let y = band_y.unwrap();
        assert!(
            y > inner.y && y < inner.y + inner.height - 1,
            "band is a data row, not header/footer/border"
        );

        let foot_y = inner.y + inner.height - 1;
        let mut footer_xs: Vec<u16> = Vec::new();
        for x in inner.x..inner.x + inner.width {
            if buf[(x, foot_y)].symbol() != " " {
                footer_xs.push(x);
            }
        }
        let lines = buffer_lines(&terminal);
        let joined = lines.join("\n");
        assert!(joined.contains("j/k:move"), "{joined}");
        assert!(joined.contains("r:rooms"), "{joined}");
        assert!(joined.contains("q:quit"), "{joined}");

        // One blank row between the top border and the table header.
        let pad_row = &lines[(card.y + 1) as usize];
        let header_row = &lines[inner.y as usize];
        assert!(
            !pad_row.contains("seat"),
            "padding between border and header: {pad_row:?}"
        );
        assert!(header_row.contains("seat"), "{header_row:?}");

        assert!(!footer_xs.is_empty(), "footer has glyphs on the inner row");
        assert!(
            *footer_xs.first().unwrap() >= inner.x,
            "footer inset from the left border"
        );
        assert!(
            *footer_xs.last().unwrap() > inner.x + inner.width / 2,
            "footer pairs are spread, not packed left: {:?}",
            &lines[foot_y as usize]
        );
    }

    #[test]
    fn hover_popover_beside_card_title_once_and_clears() {
        let root = board_with_seats("tui-pop");
        let mut ui = Ui::new(&root).unwrap();
        ui.selected = 1;
        ui.hover = Some(0); // stale hover must not win
        let mut terminal = Terminal::new(TestBackend::new(180, 24)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();

        let card = ui.card.unwrap();
        let pop = ui.pop.expect("side panel for the highlighted row");
        assert_eq!(pop.width, card.width, "equal-width box beside");
        assert!(pop.x >= card.x + card.width, "beside, never over rows");
        assert!(pop.x + pop.width <= 180, "fits the terminal");
        let lines = buffer_lines(&terminal);
        let top = &lines[pop.y as usize];
        assert_eq!(top.matches("beta").count(), 1, "title painted exactly once");
        assert!(!top.contains("alpha"), "stale hover does not win");

        ui.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(ui.selected, 0);
        terminal.draw(|f| ui.draw(f)).unwrap();
        let pop2 = ui.pop.expect("panel follows the new selection");
        let top2 = &buffer_lines(&terminal)[pop2.y as usize];
        assert!(top2.contains("alpha"), "{top2}");

        ui.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(ui.hover, None, "pointer leaving the card clears hover");
        terminal.draw(|f| ui.draw(f)).unwrap();
        assert!(ui.pop.is_some(), "panel stays on the selected row");
    }

    #[test]
    fn hover_popover_partial_width_on_mid_size_terminal() {
        let root = board_with_seats("tui-popmid");
        let mut ui = Ui::new(&root).unwrap();
        ui.selected = 0;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();
        assert!(ui.pop.is_none(), "no side panel on a narrow terminal");
        let card = ui.card.unwrap();
        assert!(card.width < 100, "not full bleed");
    }

    #[test]
    fn rooms_stats_and_index_views_render_in_a_card() {
        let root = board_with_seats("tui-views");
        room::create(&root, "ops").unwrap();
        room::cmd_post(&root, "ops", "alpha", "bulletin one").unwrap();
        room::cmd_follow(&root, "ops", "alpha").unwrap();
        let mut ui = Ui::new(&root).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        for (key, needle, footer_needle) in [
            (KeyCode::Char('r'), "ops", "esc:roster"),
            (KeyCode::Char('s'), "seats", "esc:roster"),
            (KeyCode::Char('?'), "move the selection", "esc:roster"),
        ] {
            ui.on_key(KeyEvent::new(key, KeyModifiers::NONE));
            terminal.draw(|f| ui.draw(f)).unwrap();
            let lines = buffer_lines(&terminal);
            let body = lines.join("\n");
            assert!(
                body.contains(needle),
                "view {:?} shows {needle}",
                ui.view.name()
            );
            assert!(
                body.contains(footer_needle),
                "footer {footer_needle} in {:?}",
                ui.view.name()
            );
            ui.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }

        // Enter on a roster row opens the seat detail.
        ui.selected = 0;
        ui.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(ui.detail, DetailKind::Seat(0)));
        terminal.draw(|f| ui.draw(f)).unwrap();
        let body = buffer_lines(&terminal).join("\n");
        assert!(body.contains("lifecycle"));
        assert!(body.contains("exit-wake"));

        // Enter on a room row opens the room detail with recent posts.
        ui.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        ui.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        ui.selected_room = 0;
        ui.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(ui.detail, DetailKind::Room(0)));
        terminal.draw(|f| ui.draw(f)).unwrap();
        let body = buffer_lines(&terminal).join("\n");
        assert!(body.contains("bulletin one"));
    }
}
