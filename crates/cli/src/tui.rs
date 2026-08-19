//! The terminal dashboard.
//!
//! ```text
//! ┌─ Bifrost ─────────────────────────────────────────────────────────────┐
//! │ alice · linux-laptop · 192.168.1.10      dashboard http://…:9000      │
//! ├─────────────────────────┬─────────────────────────────────────────────┤
//! │ This device             │ Chat                                        │
//! │  CPU  ███████░░░  42%   │  10:23:01 alice  hey                        │
//! │  RAM  ████████░░  12/16 │  10:23:05 You    hi                         │
//! ├─────────────────────────┤                                             │
//! │ Devices (3)             │                                             │
//! │  ● bob     192.168.1.11 │                                             │
//! ├─────────────────────────┴─────────────────────────────────────────────┤
//! │ > type a message, or /help                                            │
//! └───────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Every frame reads a fresh [`NodeSnapshot`], so what is drawn is always one
//! consistent view rather than several fields sampled at different moments.
//!
//! Two failure modes from the legacy dashboards are handled explicitly. The
//! terminal is restored on *any* exit path, including a panic — `Bifrost v3`
//! called `std::process::exit(0)` from inside `/quit` while in raw mode with
//! the alternate screen active, leaving the user's shell unusable. And input
//! never blocks the render loop.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bifrost_core::events::{NodeEvent, NoticeLevel};
use bifrost_core::node::{CommandOutcome, NodeSnapshot};
use bifrost_core::Node;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

/// Redraw interval. 20 fps looks live without burning a core.
const TICK: Duration = Duration::from_millis(50);

/// How often a fresh snapshot is pulled. Faster than this only re-renders
/// identical numbers, since telemetry itself only moves every 2 s.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(500);

/// Lines kept in the message pane.
const MAX_LINES: usize = 500;

/// Restores the terminal on drop, so it is repaired even on a panic.
///
/// A dashboard that leaves the shell in raw mode with the alternate screen
/// active forces the user to blind-type `reset`; a guard makes that
/// impossible to get wrong.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Terminal<CrosstermBackend<Stdout>>> {
        enable_raw_mode().context("cannot switch the terminal to raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("cannot open the alternate screen")?;
        Terminal::new(CrosstermBackend::new(stdout)).context("cannot create the terminal")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(crossterm::cursor::Show);
    }
}

/// A line in the message pane.
struct Entry {
    when: chrono::DateTime<chrono::Local>,
    who: String,
    text: String,
    level: NoticeLevel,
    own: bool,
}

struct App {
    node: Node,
    dashboard_url: Option<String>,
    snapshot: NodeSnapshot,
    entries: Vec<Entry>,
    input: String,
    cursor: usize,
    /// Lines scrolled back from the bottom; 0 pins to the newest.
    scroll: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    should_quit: bool,
}

impl App {
    fn push(
        &mut self,
        who: impl Into<String>,
        text: impl Into<String>,
        level: NoticeLevel,
        own: bool,
    ) {
        let who = who.into();
        for line in text.into().lines() {
            self.entries.push(Entry {
                when: chrono::Local::now(),
                who: who.clone(),
                text: line.to_string(),
                level,
                own,
            });
        }
        if self.entries.len() > MAX_LINES {
            self.entries.drain(..self.entries.len() - MAX_LINES);
        }
        // A new line pulls the view back to the bottom only if it was already
        // there, so reading history is not interrupted by arriving chat.
        if self.scroll == 0 {
            self.scroll = 0;
        }
    }
}

/// Run the dashboard until the user quits.
pub async fn run(node: Node, dashboard_url: Option<String>) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    let _guard = TerminalGuard;

    let snapshot = node.snapshot().await;
    let mut app = App {
        node,
        dashboard_url,
        snapshot,
        entries: Vec::new(),
        input: String::new(),
        cursor: 0,
        scroll: 0,
        history: Vec::new(),
        history_index: None,
        should_quit: false,
    };

    // Seed the pane with restored history, so a restart is not a blank screen.
    let restored: Vec<_> = app
        .snapshot
        .chats
        .iter()
        .rev()
        .take(30)
        .rev()
        .cloned()
        .collect();
    let me = app.snapshot.identity.node_id.clone();
    for message in restored {
        let own = message.sender_id == me;
        let who = if own {
            "You".to_string()
        } else {
            message.sender_name.clone()
        };
        app.push(who, message.content, NoticeLevel::Info, own);
    }
    app.push(
        "",
        "Type a message, or /help for commands. Esc or /quit to exit.",
        NoticeLevel::Info,
        false,
    );

    let mut events = app.node.subscribe();
    let mut last_snapshot = Instant::now();

    while !app.should_quit {
        terminal.draw(|frame| draw(frame, &app))?;

        // Drain everything the node has published since the last frame.
        while let Ok(event) = events.try_recv() {
            absorb(&mut app, event);
        }

        if last_snapshot.elapsed() >= SNAPSHOT_INTERVAL {
            app.snapshot = app.node.snapshot().await;
            last_snapshot = Instant::now();
        }

        // Polling with a timeout is what keeps the clock and telemetry moving
        // while the user is not typing.
        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                // Windows reports both press and release; acting on both would
                // double every keystroke.
                if key.kind == KeyEventKind::Press {
                    handle_key(&mut app, key).await?;
                }
            }
        }
    }

    Ok(())
}

fn absorb(app: &mut App, event: NodeEvent) {
    match event {
        NodeEvent::Chat(message) => {
            let own = message.sender_id == app.snapshot.identity.node_id;
            // Our own messages were echoed when we sent them.
            if own {
                return;
            }
            let who = match &message.recipient_id {
                Some(_) => format!("{} →you", message.sender_name),
                None => message.sender_name.clone(),
            };
            app.push(who, message.content, NoticeLevel::Info, false);
        }
        NodeEvent::Notice { level, text } => app.push("", text, level, false),
        NodeEvent::PeerOnline(peer) => app.push(
            "",
            format!("{} joined", peer.display_name()),
            NoticeLevel::Success,
            false,
        ),
        NodeEvent::PeerOffline { display_name, .. } => app.push(
            "",
            format!("{display_name} left"),
            NoticeLevel::Warning,
            false,
        ),
        NodeEvent::Transfer(status) => {
            use bifrost_core::files::TransferState;
            match &status.state {
                TransferState::Completed => app.push(
                    "",
                    format!(
                        "Transferred \"{}\" with {}",
                        status.filename, status.peer_name
                    ),
                    NoticeLevel::Success,
                    false,
                ),
                TransferState::Failed(why) => app.push(
                    "",
                    format!("\"{}\" failed: {why}", status.filename),
                    NoticeLevel::Error,
                    false,
                ),
                // Progress is drawn in the transfers pane, not logged.
                _ => {}
            }
        }
        NodeEvent::Voice { .. } | NodeEvent::Telemetry(_) => {}
    }
}

async fn handle_key(app: &mut App, key: KeyEvent) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char('c') if ctrl => app.should_quit = true,
        KeyCode::Char('l') if ctrl => app.entries.clear(),
        KeyCode::Char('u') if ctrl => {
            app.input.clear();
            app.cursor = 0;
        }
        KeyCode::Esc => app.should_quit = true,

        KeyCode::Enter => {
            let line = app.input.trim().to_string();
            app.input.clear();
            app.cursor = 0;
            app.history_index = None;
            if line.is_empty() {
                return Ok(());
            }

            app.history.push(line.clone());
            // Echo the user's own message immediately.
            if !line.starts_with('/') {
                app.push("You", line.clone(), NoticeLevel::Info, true);
            }

            match app.node.execute_line(&line).await {
                Ok(Some(outcome)) => apply(app, outcome),
                Ok(None) => {}
                Err(e) => app.push("", format!("{e:#}"), NoticeLevel::Error, false),
            }
            app.snapshot = app.node.snapshot().await;
            app.scroll = 0;
        }

        KeyCode::Backspace if app.cursor > 0 => {
            // Step by character, not by byte: a multi-byte character must not
            // be split into invalid UTF-8.
            let previous = app.input[..app.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            app.input.replace_range(previous..app.cursor, "");
            app.cursor = previous;
        }
        KeyCode::Delete if app.cursor < app.input.len() => {
            let next = app.input[app.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| app.cursor + i)
                .unwrap_or(app.input.len());
            app.input.replace_range(app.cursor..next, "");
        }
        KeyCode::Left => {
            app.cursor = app.input[..app.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
        KeyCode::Right => {
            app.cursor = app.input[app.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| app.cursor + i)
                .unwrap_or(app.input.len());
        }
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),

        // Ctrl scrolls the message pane; plain arrows walk command history.
        KeyCode::Up if ctrl => {
            app.scroll = (app.scroll + 1).min(app.entries.len().saturating_sub(1));
        }
        KeyCode::Up => recall_history(app, -1),
        KeyCode::Down if ctrl => app.scroll = app.scroll.saturating_sub(1),
        KeyCode::Down => recall_history(app, 1),
        KeyCode::PageUp => app.scroll = (app.scroll + 10).min(app.entries.len().saturating_sub(1)),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),

        KeyCode::Char(c) => {
            app.input.insert(app.cursor, c);
            app.cursor += c.len_utf8();
        }
        _ => {}
    }

    Ok(())
}

fn recall_history(app: &mut App, direction: i32) {
    if app.history.is_empty() {
        return;
    }

    let next = match (app.history_index, direction) {
        (None, -1) => Some(app.history.len() - 1),
        (Some(0), -1) => Some(0),
        (Some(i), -1) => Some(i - 1),
        (Some(i), 1) if i + 1 < app.history.len() => Some(i + 1),
        (Some(_), 1) => None, // stepping past the newest clears the line
        (None, _) => None,
        _ => app.history_index,
    };

    app.history_index = next;
    app.input = next.map(|i| app.history[i].clone()).unwrap_or_default();
    app.cursor = app.input.len();
}

fn apply(app: &mut App, outcome: CommandOutcome) {
    match outcome {
        CommandOutcome::Message { level, text } => app.push("", text, level, false),
        CommandOutcome::Peers { peers } => {
            if peers.is_empty() {
                app.push(
                    "",
                    "No devices discovered yet.",
                    NoticeLevel::Warning,
                    false,
                );
            } else {
                for peer in peers {
                    app.push(
                        "",
                        format!(
                            "{} {:<18} {:<15} {:<14} CPU {:>5.1}%  {:>6.1} GB free",
                            if peer.online { "●" } else { "○" },
                            peer.display_name(),
                            peer.ip,
                            peer.device_type,
                            peer.telemetry.cpu_usage,
                            peer.telemetry.ram_available_mb() as f64 / 1024.0,
                        ),
                        NoticeLevel::Info,
                        false,
                    );
                }
            }
        }
        CommandOutcome::Status { snapshot } => {
            app.push(
                "",
                format!("Node      {}", snapshot.identity.username),
                NoticeLevel::Info,
                false,
            );
            app.push(
                "",
                format!("Address   {}", snapshot.ip),
                NoticeLevel::Info,
                false,
            );
            app.push(
                "",
                format!("Dashboard {}", snapshot.dashboard_url),
                NoticeLevel::Info,
                false,
            );
            app.push(
                "",
                format!("Devices   {} online", snapshot.online_peers),
                NoticeLevel::Info,
                false,
            );
            app.push(
                "",
                format!("Downloads {}", snapshot.download_dir),
                NoticeLevel::Info,
                false,
            );
        }
        CommandOutcome::Clear => {
            app.entries.clear();
            app.scroll = 0;
        }
        CommandOutcome::Quit => app.should_quit = true,
    }
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.size());

    draw_header(frame, app, rows[0]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(38), Constraint::Min(30)])
        .split(rows[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(4),
            Constraint::Length(6),
        ])
        .split(columns[0]);

    draw_self(frame, app, left[0]);
    draw_peers(frame, app, left[1]);
    draw_transfers(frame, app, left[2]);
    draw_messages(frame, app, columns[1]);
    draw_input(frame, app, rows[2]);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let s = &app.snapshot;
    let left = format!(
        "  {} · {} · {}",
        s.identity.username, s.identity.device_type, s.ip
    );
    let right = match &app.dashboard_url {
        Some(url) => format!("dashboard {url}  "),
        None => format!("{} device(s) online  ", s.online_peers),
    };

    let padding =
        (area.width as usize).saturating_sub(left.chars().count() + right.chars().count() + 2);

    let line = Line::from(vec![
        Span::styled(left, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(padding)),
        Span::styled(right, Style::default().fg(Color::Cyan)),
    ]);

    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(Span::styled(
            " ⬡ Bifrost ",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        area,
    );
}

fn draw_self(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.snapshot.telemetry;
    let ram_pct = ratio(t.ram_used_mb as f64, t.ram_total_mb as f64);
    let disk_pct = ratio(t.disk_used_gb, t.disk_total_gb);

    let width = (area.width as usize).saturating_sub(24).clamp(6, 14);
    let lines = vec![
        gauge_line(
            "CPU ",
            t.cpu_usage as f64,
            format!("{:>5.1}%", t.cpu_usage),
            width,
        ),
        gauge_line(
            "RAM ",
            ram_pct,
            format!(
                "{:.0}/{:.0}G",
                t.ram_used_mb as f64 / 1024.0,
                t.ram_total_mb as f64 / 1024.0
            ),
            width,
        ),
        gauge_line(
            "Disk",
            disk_pct,
            format!("{:.0}/{:.0}G", t.disk_used_gb, t.disk_total_gb),
            width,
        ),
        Line::from(Span::styled(
            format!(
                " net ↓{:.0} ↑{:.0} KB/s · {} cores",
                t.net_rx_kbps, t.net_tx_kbps, t.cpu_cores
            ),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            format!(
                " pool {} node(s) · {} cores · {:.0} GB free",
                app.snapshot.pool.nodes,
                app.snapshot.pool.cpu_cores,
                app.snapshot.pool.ram_available_mb as f64 / 1024.0
            ),
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" This device "),
        ),
        area,
    );
}

fn ratio(used: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        (used / total * 100.0).clamp(0.0, 100.0)
    }
}

/// A labelled bar. The numeric value always sits beside it, so colour is
/// never the only signal — the same rule the web dashboard follows.
fn gauge_line(label: &str, pct: f64, value: String, width: usize) -> Line<'static> {
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);

    let colour = if pct >= 90.0 {
        Color::Red
    } else if pct >= 75.0 {
        Color::Yellow
    } else {
        Color::Cyan
    };

    Line::from(vec![
        Span::raw(format!(" {label} ")),
        Span::styled("█".repeat(filled), Style::default().fg(colour)),
        Span::styled(
            "░".repeat(width - filled),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(format!(" {value}")),
    ])
}

fn draw_peers(frame: &mut Frame, app: &App, area: Rect) {
    let peers = &app.snapshot.peers;

    let items: Vec<ListItem> = if peers.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            " searching…",
            Style::default().fg(Color::DarkGray),
        )))]
    } else {
        peers
            .iter()
            .map(|peer| {
                let (marker, colour) = if peer.online {
                    ("●", Color::Green)
                } else {
                    ("○", Color::DarkGray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {marker} "), Style::default().fg(colour)),
                    Span::styled(
                        format!("{:<14}", truncate(peer.display_name(), 14)),
                        Style::default().add_modifier(if peer.online {
                            Modifier::empty()
                        } else {
                            Modifier::DIM
                        }),
                    ),
                    Span::styled(
                        format!("{:>3.0}% ", peer.telemetry.cpu_usage),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(truncate(&peer.ip, 15), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect()
    };

    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Devices ({}) ", app.snapshot.online_peers)),
        ),
        area,
    );
}

fn draw_transfers(frame: &mut Frame, app: &App, area: Rect) {
    use bifrost_core::files::TransferState;

    let lines: Vec<Line> = if app.snapshot.transfers.is_empty() {
        vec![Line::from(Span::styled(
            " none",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.snapshot
            .transfers
            .iter()
            .take(4)
            .map(|t| {
                let arrow = match t.direction {
                    bifrost_core::files::transfer::Direction::Incoming => "↓",
                    bifrost_core::files::transfer::Direction::Outgoing => "↑",
                };
                let (mark, colour) = match &t.state {
                    TransferState::Completed => ("✓", Color::Green),
                    TransferState::Failed(_) | TransferState::Rejected(_) => ("✕", Color::Red),
                    _ => ("·", Color::Cyan),
                };
                Line::from(vec![
                    Span::styled(format!(" {arrow}{mark} "), Style::default().fg(colour)),
                    Span::raw(format!("{:<16}", truncate(&t.filename, 16))),
                    Span::styled(
                        format!("{:>3.0}%", t.progress_pct()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            })
            .collect()
    };

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Transfers ")),
        area,
    );
}

fn draw_messages(frame: &mut Frame, app: &App, area: Rect) {
    let visible = (area.height as usize).saturating_sub(2);
    let total = app.entries.len();

    // `scroll` counts lines back from the newest, so a growing log does not
    // shift what the reader is looking at.
    let end = total.saturating_sub(app.scroll);
    let start = end.saturating_sub(visible);

    let lines: Vec<Line> = app.entries[start..end]
        .iter()
        .map(|entry| {
            let mut spans = vec![Span::styled(
                format!(" {} ", entry.when.format("%H:%M:%S")),
                Style::default().fg(Color::DarkGray),
            )];

            if !entry.who.is_empty() {
                spans.push(Span::styled(
                    format!("{:<12} ", truncate(&entry.who, 12)),
                    Style::default().fg(if entry.own {
                        Color::Cyan
                    } else {
                        Color::Magenta
                    }),
                ));
            } else {
                spans.push(Span::raw(" ".repeat(13)));
            }

            spans.push(Span::styled(
                entry.text.clone(),
                match entry.level {
                    NoticeLevel::Success => Style::default().fg(Color::Green),
                    NoticeLevel::Warning => Style::default().fg(Color::Yellow),
                    NoticeLevel::Error => Style::default().fg(Color::Red),
                    NoticeLevel::Info => Style::default(),
                },
            ));

            Line::from(spans)
        })
        .collect();

    let title = if app.scroll > 0 {
        format!(
            " Messages (scrolled back {} lines — Ctrl-↓ to return) ",
            app.scroll
        )
    } else {
        " Messages ".to_string()
    };

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " > ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(app.input.as_str()),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title_bottom(Line::from(Span::styled(
                    " Enter send · ↑↓ history · Ctrl-↑↓ scroll · Esc quit ",
                    Style::default().fg(Color::DarkGray),
                )))
                .title_alignment(Alignment::Left),
        ),
        area,
    );

    // Place the real cursor where the caret is, counting display columns so a
    // multi-byte character does not push it off by a byte.
    let column = app.input[..app.cursor].chars().count() as u16;
    frame.set_cursor(area.x + 3 + column, area.y + 1);
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gauge_fills_proportionally() {
        for (pct, expected_filled) in [(0.0, 0), (50.0, 5), (100.0, 10)] {
            let line = gauge_line("CPU ", pct, "x".into(), 10);
            let filled = line
                .spans
                .iter()
                .filter(|s| s.content.starts_with('█'))
                .map(|s| s.content.chars().count())
                .sum::<usize>();
            assert_eq!(filled, expected_filled, "at {pct}%");
        }
    }

    #[test]
    fn a_gauge_never_overflows_its_width() {
        // A momentary over-100% CPU reading must not panic on the repeat count.
        let line = gauge_line("CPU ", 250.0, "x".into(), 10);
        let total: usize = line
            .spans
            .iter()
            .filter(|s| s.content.starts_with('█') || s.content.starts_with('░'))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(total, 10);
    }

    #[test]
    fn ratio_handles_a_zero_denominator() {
        assert_eq!(
            ratio(5.0, 0.0),
            0.0,
            "a machine reporting no disk must not divide by zero"
        );
        assert_eq!(ratio(50.0, 100.0), 50.0);
        assert_eq!(ratio(150.0, 100.0), 100.0, "clamped");
    }

    #[test]
    fn truncate_respects_characters_not_bytes() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-name", 8), "a-very-…");
        // Multi-byte input must not be split mid-character.
        let truncated = truncate("日本語のホスト名", 4);
        assert_eq!(truncated.chars().count(), 4);
        assert!(truncated.ends_with('…'));
    }
}
