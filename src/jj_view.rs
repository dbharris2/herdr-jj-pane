//! Minimal read-only Jujutsu log / diff-stat pane.

use notify::{RecursiveMode, Watcher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::term::{Modes, TermGuard};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Log,
    DiffStat,
}

impl View {
    fn title(self) -> &'static str {
        match self {
            Self::Log => "jj log",
            Self::DiffStat => "jj diff --stat",
        }
    }

    fn command(self, repo: &Path) -> Result<String> {
        let mut cmd = Command::new("jj");
        cmd.current_dir(repo).args(["--no-pager", "--color=always"]);
        match self {
            Self::Log => {
                cmd.arg("log");
            }
            Self::DiffStat => {
                cmd.args(["diff", "--stat"]);
            }
        }
        let output = cmd.output().context("spawning jj")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("jj {} failed: {stderr}", self.title());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

pub fn run() -> Result<()> {
    crate::logx::init_panic_hook();
    let repo = resolve_repo()?;
    let mut view = View::Log;
    let mut content = String::new();
    let mut error = None;
    let mut scroll = 0_u16;
    let (events_tx, events_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = events_tx.send(event);
    })?;
    watcher.watch(&repo, RecursiveMode::Recursive)?;
    let (result_tx, result_rx) = mpsc::channel();
    let mut refreshed = Instant::now() - REFRESH_INTERVAL;
    let mut pending = None;
    let mut running = false;
    let mut generation = 0_u64;

    let mut terminal = ratatui::init();
    let _term = TermGuard::enter(Modes {
        mouse: true,
        ..Default::default()
    });
    let result = (|| -> Result<()> {
        loop {
            while let Ok(event) = events_rx.try_recv() {
                if let Ok(event) = event {
                    // jj may update its own metadata while reading. Keep worktree
                    // changes during a run without feeding those writes back.
                    let worktree_change = event.paths.iter().any(|path| {
                        path.strip_prefix(&repo).is_ok_and(|relative| {
                            !matches!(relative.components().next(), Some(std::path::Component::Normal(name)) if name == ".jj" || name == ".git")
                        })
                    });
                    if !running || worktree_change {
                        pending = Some(Instant::now());
                    }
                }
            }
            while let Ok((result_generation, result)) = result_rx.try_recv() {
                running = false;
                if result_generation == generation {
                    match result {
                        Ok(output) => {
                            content = output;
                            error = None;
                        }
                        Err(err) => error = Some(err),
                    }
                } else {
                    pending = Some(Instant::now() - DEBOUNCE);
                }
                refreshed = Instant::now();
            }
            if !running
                && (refreshed.elapsed() >= REFRESH_INTERVAL
                    || pending.is_some_and(|at: Instant| at.elapsed() >= DEBOUNCE))
            {
                pending = None;
                running = true;
                let repo = repo.clone();
                let tx = result_tx.clone();
                let current_view = view;
                let current_generation = generation;
                std::thread::spawn(move || {
                    let result = current_view.command(&repo).map_err(|err| err.to_string());
                    let _ = tx.send((current_generation, result));
                });
            }

            terminal.draw(|frame| {
                let areas = Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
                    .split(frame.area());
                let header = format!(
                    " {}  ·  Tab switch  ·  ↑↓/wheel scroll  ·  PgUp/PgDn page  ·  q close  ·  live ",
                    view.title()
                );
                frame.render_widget(
                    Paragraph::new(header).style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    areas[0],
                );

                let body = if let Some(error) = &error {
                    Text::from(error.clone()).style(Style::default().fg(Color::LightRed))
                } else {
                    ansi_text(&content)
                };
                let viewport_height = areas[1].height.saturating_sub(1);
                let max_scroll = max_scroll(
                    &body,
                    areas[1].width.saturating_sub(2),
                    viewport_height,
                );
                scroll = scroll.min(max_scroll);
                frame.render_widget(
                    Paragraph::new(body)
                        .block(
                            Block::default()
                                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM),
                        )
                        .wrap(Wrap { trim: false })
                        .scroll((scroll, 0)),
                    areas[1],
                );
            })?;

            if event::poll(Duration::from_millis(100))? {
                let viewport_height = terminal.size()?.height.saturating_sub(2).max(1);
                let page = viewport_height.saturating_sub(1).max(1);
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Up => scroll = scroll.saturating_sub(1),
                        KeyCode::Down => scroll = scroll.saturating_add(1),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(page),
                        KeyCode::PageDown => scroll = scroll.saturating_add(page),
                        KeyCode::Home => scroll = 0,
                        KeyCode::End => scroll = u16::MAX,
                        KeyCode::Char('l') => {
                            view = View::Log;
                            scroll = 0;
                            generation += 1;
                            pending = Some(Instant::now() - DEBOUNCE);
                        }
                        KeyCode::Char('d') => {
                            view = View::DiffStat;
                            scroll = 0;
                            generation += 1;
                            pending = Some(Instant::now() - DEBOUNCE);
                        }
                        KeyCode::Tab => {
                            view = match view {
                                View::Log => View::DiffStat,
                                View::DiffStat => View::Log,
                            };
                            scroll = 0;
                            generation += 1;
                            pending = Some(Instant::now() - DEBOUNCE);
                        }
                        _ => {}
                    },
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                        MouseEventKind::ScrollDown => scroll = scroll.saturating_add(3),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    drop(_term);
    ratatui::restore();
    if let Some(state) = std::env::var_os("JJVIEW_STATE_PATH") {
        let _ = std::fs::remove_file(state);
    }
    result
}

pub fn resolve_repo() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(repo) = std::env::var_os("JJVIEW_REPO") {
        candidates.push(PathBuf::from(repo));
    }
    if let Ok(raw) = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        && let Ok(ctx) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        candidates.extend(
            [
                "/worktree/checkout_path",
                "/workspace_cwd",
                "/focused_pane_cwd",
                "/worktree/repo_root",
            ]
            .into_iter()
            .filter_map(|key| ctx.pointer(key)?.as_str())
            .map(PathBuf::from),
        );
    }
    candidates.push(std::env::current_dir().context("reading current directory")?);

    for dir in candidates {
        let output = Command::new("jj")
            .arg("-R")
            .arg(&dir)
            .arg("root")
            .output()
            .context("spawning jj")?;
        if output.status.success() {
            let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !root.is_empty() {
                return Ok(PathBuf::from(root));
            }
        }
    }
    Err(io::Error::other("not inside a Jujutsu repository").into())
}

/// Convert the SGR color/style codes emitted by jj into ratatui spans.
fn ansi_text(input: &str) -> Text<'static> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut buffer = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            for code in chars.by_ref() {
                if code == 'm' {
                    break;
                }
                params.push(code);
            }
            flush_span(&mut spans, &mut buffer, style);
            apply_sgr(&mut style, &params);
        } else if ch == '\n' {
            flush_span(&mut spans, &mut buffer, style);
            lines.push(Line::from(std::mem::take(&mut spans)));
        } else if ch != '\r' {
            buffer.push(ch);
        }
    }
    flush_span(&mut spans, &mut buffer, style);
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

fn flush_span(spans: &mut Vec<Span<'static>>, buffer: &mut String, style: Style) {
    if !buffer.is_empty() {
        spans.push(Span::styled(std::mem::take(buffer), style));
    }
}

fn apply_sgr(style: &mut Style, params: &str) {
    let codes: Vec<u16> = if params.is_empty() {
        vec![0]
    } else {
        params
            .split(';')
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    let mut i = 0;
    while i < codes.len() {
        let code = codes[i];
        match code {
            0 => *style = Style::default(),
            1 => style.add_modifier |= Modifier::BOLD,
            3 => style.add_modifier |= Modifier::ITALIC,
            4 => style.add_modifier |= Modifier::UNDERLINED,
            22 => *style = (*style).remove_modifier(Modifier::BOLD),
            23 => *style = (*style).remove_modifier(Modifier::ITALIC),
            24 => *style = (*style).remove_modifier(Modifier::UNDERLINED),
            30..=37 => style.fg = Some(ansi_basic_color(code - 30, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi_basic_color(code - 40, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi_basic_color(code - 90, true)),
            100..=107 => style.bg = Some(ansi_basic_color(code - 100, true)),
            38 | 48 if i + 2 < codes.len() && codes[i + 1] == 5 => {
                let color = Color::Indexed(codes[i + 2] as u8);
                if code == 38 {
                    style.fg = Some(color);
                } else {
                    style.bg = Some(color);
                }
                i += 2;
            }
            38 | 48 if i + 4 < codes.len() && codes[i + 1] == 2 => {
                let color = Color::Rgb(codes[i + 2] as u8, codes[i + 3] as u8, codes[i + 4] as u8);
                if code == 38 {
                    style.fg = Some(color);
                } else {
                    style.bg = Some(color);
                }
                i += 4;
            }
            _ => {}
        }
        i += 1;
    }
}

fn ansi_basic_color(index: u16, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

fn max_scroll(text: &Text<'_>, width: u16, height: u16) -> u16 {
    let width = usize::from(width.max(1));
    let rows = text
        .lines
        .iter()
        .map(|line| {
            let cells = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            cells.max(1).div_ceil(width)
        })
        .sum::<usize>();
    rows.saturating_sub(usize::from(height))
        .min(usize::from(u16::MAX)) as u16
}
