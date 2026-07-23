use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, stdout};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};

use crate::domain::{ArchiveItem, FileType, Snapshot};
use crate::index::Index;
use crate::restore::{RestorePhase, RestoreProgress};

pub fn run(index: &Index, config_path: &Path) -> Result<()> {
    enable_raw_mode()?;
    let mut output = stdout();
    if let Err(error) = execute!(output, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(output)) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(stdout(), LeaveAlternateScreen);
            return Err(error.into());
        }
    };
    let result = run_loop(&mut terminal, index, config_path);
    let raw_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();
    result?;
    raw_result?;
    screen_result?;
    cursor_result?;
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    index: &Index,
    config_path: &Path,
) -> Result<()> {
    let mut app = App::new(index, config_path)?;
    loop {
        app.poll_restore();
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && app.handle_key(index, key.code)? {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Snapshots,
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Search(String),
    ConfirmRestore { paths: Vec<String>, input: String },
    Restoring,
}

enum WorkerEvent {
    Progress(RestoreProgress),
    Finished(std::result::Result<(), String>),
}

struct App {
    snapshots: Vec<Snapshot>,
    snapshot_index: usize,
    files: Vec<ArchiveItem>,
    file_index: usize,
    directory: String,
    selected: HashSet<String>,
    diff: Vec<String>,
    focus: Focus,
    mode: Mode,
    search_result_query: Option<String>,
    message: String,
    config_path: String,
    restore_progress: Option<RestoreProgress>,
    restore_worker: Option<Receiver<WorkerEvent>>,
}

impl App {
    fn new(index: &Index, config_path: &Path) -> Result<Self> {
        let snapshots = index.snapshots()?;
        let mut app = Self {
            snapshots,
            snapshot_index: 0,
            files: Vec::new(),
            file_index: 0,
            directory: String::new(),
            selected: HashSet::new(),
            diff: Vec::new(),
            focus: Focus::Snapshots,
            mode: Mode::Normal,
            search_result_query: None,
            message: "h/j/k/l navigate | Space select | / search | r reload | R restore | M mount | D diff | Q quit".into(),
            config_path: config_path.display().to_string(),
            restore_progress: None,
            restore_worker: None,
        };
        app.reload_files(index)?;
        Ok(app)
    }

    fn handle_key(&mut self, index: &Index, code: KeyCode) -> Result<bool> {
        if self.mode == Mode::Restoring {
            return Ok(false);
        }
        if let Mode::ConfirmRestore { paths, input } = &mut self.mode {
            let mut start = None;
            match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(character) => input.push(character),
                KeyCode::Enter if input == "RESTORE" => start = Some(paths.clone()),
                KeyCode::Enter => {
                    self.message = "Confirmation must be exactly RESTORE".into();
                }
                _ => {}
            }
            if let Some(paths) = start {
                self.start_restore(paths)?;
            }
            return Ok(false);
        }
        if let Mode::Search(query) = &mut self.mode {
            match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(character) => query.push(character),
                KeyCode::Enter => {
                    let query = query.clone();
                    self.mode = Mode::Normal;
                    if query.trim().is_empty() {
                        self.message = "Search query cannot be empty".into();
                        return Ok(false);
                    }
                    let Some(snapshot) = self.current_snapshot().map(|value| value.name.clone())
                    else {
                        return Ok(false);
                    };
                    let query_lower = query.to_lowercase();
                    self.files = index
                        .directory_entries(&snapshot, &self.directory)?
                        .into_iter()
                        .filter(|item| {
                            item.path
                                .rsplit('/')
                                .next()
                                .unwrap_or(&item.path)
                                .to_lowercase()
                                .contains(&query_lower)
                        })
                        .collect();
                    self.file_index = 0;
                    self.search_result_query = Some(query.clone());
                    self.message = format!(
                        "Filter '{query}': {} matches in current directory",
                        self.files.len()
                    );
                }
                _ => {}
            }
            return Ok(false);
        }

        match code {
            KeyCode::Char('q' | 'Q') => return Ok(true),
            KeyCode::Char('r') => self.reload_snapshots(index)?,
            KeyCode::Tab => {
                self.focus = if self.focus == Focus::Snapshots {
                    Focus::Files
                } else {
                    Focus::Snapshots
                };
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(index, -1)?,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(index, 1)?,
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(index)?,
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.leave(index)?,
            KeyCode::Home => self.move_to_edge(index, false)?,
            KeyCode::End => self.move_to_edge(index, true)?,
            KeyCode::Esc if self.search_result_query.is_some() => {
                self.reload_files(index)?;
                self.message = "Directory filter cleared".into();
            }
            KeyCode::Char(' ') if self.focus == Focus::Files => {
                if let Some(path) = self.current_file().map(|item| item.path.clone()) {
                    if !self.selected.remove(&path) {
                        self.selected.insert(path);
                    }
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Search(String::new()),
            KeyCode::Char('R') => {
                let mut paths: Vec<_> = if self.selected.is_empty() {
                    self.current_file()
                        .map(|item| item.path.clone())
                        .into_iter()
                        .collect()
                } else {
                    self.selected.iter().cloned().collect()
                };
                paths.sort();
                if paths.is_empty() {
                    self.message = "Restore: select at least one file".into();
                } else if Path::new(&self.config_path).parent() != Some(Path::new("/etc/boxup")) {
                    self.message =
                        "Original-path restore requires a system profile under /etc/boxup".into();
                } else {
                    self.mode = Mode::ConfirmRestore {
                        paths,
                        input: String::new(),
                    };
                }
            }
            KeyCode::Char('M') => {
                self.message = self.current_snapshot().map_or_else(
                    || "Mount: no snapshot selected".into(),
                    |snapshot| {
                        format!(
                            "Mount hint: boxup --config {} mount {} EMPTY_TARGET",
                            shell_quote(&self.config_path),
                            shell_quote(&snapshot.name)
                        )
                    },
                );
            }
            KeyCode::Char('D') => {
                if let Some(name) = self
                    .current_snapshot()
                    .map(|snapshot| snapshot.name.clone())
                {
                    if !self.diff.contains(&name) {
                        self.diff.push(name);
                        if self.diff.len() > 2 {
                            self.diff.remove(0);
                        }
                    }
                }
                self.message = if self.diff.len() == 2 {
                    format!(
                        "Diff hint: boxup --config {} diff {} {}",
                        shell_quote(&self.config_path),
                        shell_quote(&self.diff[0]),
                        shell_quote(&self.diff[1])
                    )
                } else {
                    "Diff: select one more snapshot with D".into()
                };
            }
            _ => {}
        }
        Ok(false)
    }

    fn start_restore(&mut self, paths: Vec<String>) -> Result<()> {
        let snapshot = self
            .current_snapshot()
            .context("restore requires a snapshot")?
            .name
            .clone();
        let config = self.config_path.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut command = if nix::unistd::Uid::effective().is_root() {
                Command::new("/usr/lib/boxup/boxup-root")
            } else {
                let mut command = Command::new("/usr/bin/pkexec");
                command.arg("/usr/lib/boxup/boxup-root");
                command
            };
            command
                .arg("--config")
                .arg(config)
                .arg("restore-original")
                .arg("--confirm")
                .arg("RESTORE")
                .arg(snapshot)
                .arg("--")
                .args(paths)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let result = match command.spawn() {
                Ok(mut child) => {
                    let stderr = child.stderr.take().map(|error_output| {
                        std::thread::spawn(move || {
                            BufReader::new(error_output)
                                .lines()
                                .map_while(Result::ok)
                                .filter(|line| !line.trim().is_empty())
                                .last()
                        })
                    });
                    if let Some(output) = child.stdout.take() {
                        for line in BufReader::new(output).lines().map_while(Result::ok) {
                            if let Ok(progress) = serde_json::from_str::<RestoreProgress>(&line) {
                                let _ = sender.send(WorkerEvent::Progress(progress));
                            }
                        }
                    }
                    let status = child.wait();
                    let error = stderr
                        .and_then(|task| task.join().ok())
                        .flatten()
                        .map(sanitize_worker_error);
                    match status {
                        Ok(status) if status.success() => Ok(()),
                        Ok(_) => Err(error.unwrap_or_else(|| "privileged helper failed".into())),
                        Err(error) => Err(format!("failed to wait for helper: {error}")),
                    }
                }
                Err(error) => Err(format!("failed to start privileged helper: {error}")),
            };
            let _ = sender.send(WorkerEvent::Finished(result));
        });
        self.restore_progress = Some(RestoreProgress {
            phase: RestorePhase::Validating,
            current: 0,
            total: 0,
            files: 0,
            bytes: 0,
        });
        self.restore_worker = Some(receiver);
        self.mode = Mode::Restoring;
        Ok(())
    }

    fn poll_restore(&mut self) {
        let Some(receiver) = &self.restore_worker else {
            return;
        };
        let events: Vec<_> = receiver.try_iter().collect();
        for event in events {
            match event {
                WorkerEvent::Progress(progress) => self.restore_progress = Some(progress),
                WorkerEvent::Finished(result) => {
                    self.mode = Mode::Normal;
                    self.restore_worker = None;
                    match result {
                        Ok(()) => {
                            self.selected.clear();
                            self.message = "Restore to original path completed".into();
                        }
                        Err(error) => {
                            self.message = format!("Restore failed: {error}");
                        }
                    }
                }
            }
        }
    }

    fn move_selection(&mut self, index: &Index, delta: isize) -> Result<()> {
        let (selection, length) = match self.focus {
            Focus::Snapshots => (&mut self.snapshot_index, self.snapshots.len()),
            Focus::Files => (&mut self.file_index, self.files.len()),
        };
        let previous = *selection;
        if length == 0 {
            *selection = 0;
        } else {
            *selection = ((*selection as isize + delta).clamp(0, length as isize - 1)) as usize;
        }
        if self.focus == Focus::Snapshots && *selection != previous {
            self.directory.clear();
            self.selected.clear();
            self.reload_files(index)?;
        }
        Ok(())
    }

    fn move_to_edge(&mut self, index: &Index, end: bool) -> Result<()> {
        match self.focus {
            Focus::Snapshots => {
                let next = if end {
                    self.snapshots.len().saturating_sub(1)
                } else {
                    0
                };
                if self.snapshot_index != next {
                    self.snapshot_index = next;
                    self.directory.clear();
                    self.selected.clear();
                    self.reload_files(index)?;
                }
            }
            Focus::Files => {
                self.file_index = if end {
                    self.files.len().saturating_sub(1)
                } else {
                    0
                };
            }
        }
        Ok(())
    }

    fn leave(&mut self, index: &Index) -> Result<()> {
        if self.focus != Focus::Files {
            return Ok(());
        }
        if self.directory.is_empty() {
            self.focus = Focus::Snapshots;
            return Ok(());
        }
        self.directory = self
            .directory
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_owned();
        self.reload_files(index)
    }

    fn enter(&mut self, index: &Index) -> Result<()> {
        match self.focus {
            Focus::Snapshots => {
                self.directory.clear();
                self.selected.clear();
                self.reload_files(index)?;
                self.focus = Focus::Files;
            }
            Focus::Files => {
                if let Some(item) = self.current_file() {
                    if item.kind == FileType::Directory {
                        self.directory = item.path.clone();
                        self.reload_files(index)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn reload_files(&mut self, index: &Index) -> Result<()> {
        self.files = match self.current_snapshot() {
            Some(snapshot) => index.directory_entries(&snapshot.name, &self.directory)?,
            None => Vec::new(),
        };
        self.file_index = 0;
        self.search_result_query = None;
        Ok(())
    }

    fn reload_snapshots(&mut self, index: &Index) -> Result<()> {
        let current = self
            .current_snapshot()
            .map(|snapshot| snapshot.name.clone());
        self.snapshots = index.snapshots()?;
        self.snapshot_index = current
            .as_deref()
            .and_then(|name| {
                self.snapshots
                    .iter()
                    .position(|snapshot| snapshot.name == name)
            })
            .unwrap_or(0);
        self.directory.clear();
        self.selected.clear();
        self.diff
            .retain(|name| self.snapshots.iter().any(|snapshot| &snapshot.name == name));
        self.reload_files(index)?;
        self.message = format!("Reloaded {} snapshots from the index", self.snapshots.len());
        Ok(())
    }

    fn current_snapshot(&self) -> Option<&Snapshot> {
        self.snapshots.get(self.snapshot_index)
    }

    fn current_file(&self) -> Option<&ArchiveItem> {
        self.files.get(self.file_index)
    }
}

fn sanitize_worker_error(error: String) -> String {
    error
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .take(500)
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 70 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "Boxup needs a terminal at least 70x12. Resize the terminal, or press Q to quit.",
            )
            .block(Block::default().title("Boxup").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(7), Constraint::Length(3)])
        .split(area);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(42),
            Constraint::Percentage(30),
        ])
        .split(vertical[0]);

    let snapshot_items: Vec<_> = app
        .snapshots
        .iter()
        .map(|snapshot| {
            ListItem::new(format!(
                "{}  {}",
                snapshot.start.format("%Y-%m-%d"),
                snapshot.name
            ))
        })
        .collect();
    let mut snapshot_state = ListState::default()
        .with_selected((!app.snapshots.is_empty()).then_some(app.snapshot_index));
    frame.render_stateful_widget(
        List::new(snapshot_items)
            .block(pane_block("Snapshots", app.focus == Focus::Snapshots))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        panes[0],
        &mut snapshot_state,
    );

    let file_items: Vec<_> = app
        .files
        .iter()
        .map(|item| {
            let marker = if app.selected.contains(&item.path) {
                "[x]"
            } else {
                "[ ]"
            };
            let suffix = if item.kind == FileType::Directory {
                "/"
            } else {
                ""
            };
            let display_path = item.path.rsplit('/').next().unwrap_or(&item.path);
            ListItem::new(format!("{marker} {}{suffix}", display_path))
        })
        .collect();
    let title = if let Some(query) = &app.search_result_query {
        format!("Filter {query:?}")
    } else if app.directory.is_empty() {
        "Files /".into()
    } else {
        format!("Files /{}", app.directory)
    };
    let mut file_state =
        ListState::default().with_selected((!app.files.is_empty()).then_some(app.file_index));
    frame.render_stateful_widget(
        List::new(file_items)
            .block(pane_block(&title, app.focus == Focus::Files))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        panes[1],
        &mut file_state,
    );

    let info = app.current_file().map_or_else(
        || "No file selected".into(),
        |item| {
            format!(
                "Path: {}\nType: {:?}\nSize: {} bytes\nModified: {}\nOwner: {}:{}\nHealth: {}",
                item.path,
                item.kind,
                item.size,
                item.mtime
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_else(|| "unknown".into()),
                item.user.as_deref().unwrap_or("?"),
                item.group.as_deref().unwrap_or("?"),
                item.health.as_deref().unwrap_or("unknown")
            )
        },
    );
    frame.render_widget(
        Paragraph::new(info)
            .block(Block::default().title("Info").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        panes[2],
    );

    let status = match &app.mode {
        Mode::Normal => Line::from(app.message.clone()),
        Mode::Search(query) => Line::from(vec![
            Span::styled(
                "Search: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(query),
        ]),
        Mode::ConfirmRestore { .. } => Line::from("Confirm original-path restore"),
        Mode::Restoring => Line::from("Restore in progress"),
    };
    frame.render_widget(
        Paragraph::new(status).block(Block::default().title("Command").borders(Borders::ALL)),
        vertical[1],
    );

    match &app.mode {
        Mode::ConfirmRestore { paths, input } => {
            let area = centered_rect(76, 12, frame.area());
            frame.render_widget(Clear, area);
            let targets = paths
                .iter()
                .take(4)
                .map(|path| format!("/{path}"))
                .collect::<Vec<_>>()
                .join("\n");
            let extra = paths.len().saturating_sub(4);
            let text = format!(
                "The selected snapshot data will exactly replace the current path.\nFiles that exist only in the current path will be removed.\n\n{targets}{}\n\nType RESTORE and press Enter:\n{input}",
                if extra > 0 {
                    format!("\n... and {extra} more")
                } else {
                    String::new()
                }
            );
            frame.render_widget(
                Paragraph::new(text)
                    .block(
                        Block::default()
                            .title("Restore Original Paths")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(Color::Yellow)),
                    )
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
        Mode::Restoring => {
            let area = centered_rect(70, 9, frame.area());
            frame.render_widget(Clear, area);
            let progress = app.restore_progress.as_ref();
            let phase = progress.map_or_else(
                || "Starting".into(),
                |value| match value.phase {
                    RestorePhase::Validating if value.current > 0 => {
                        format!(
                            "Validating live snapshot: {} entries scanned",
                            value.current
                        )
                    }
                    RestorePhase::Validating => {
                        "Validating live snapshot (reading archive metadata)".into()
                    }
                    RestorePhase::Extracting => "Extracting".into(),
                    RestorePhase::Verifying => "Verifying extracted data".into(),
                    RestorePhase::Publishing => "Publishing original paths".into(),
                    RestorePhase::Complete => "Complete".into(),
                },
            );
            let ratio = progress
                .filter(|value| value.total > 0)
                .map_or(0.0, |value| {
                    (value.current.min(value.total) as f64 / value.total as f64).clamp(0.0, 1.0)
                });
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ])
                .split(area);
            frame.render_widget(
                Paragraph::new(phase)
                    .block(Block::default().title("Restore").borders(Borders::ALL)),
                chunks[0],
            );
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL))
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .label(
                        if progress.is_some_and(|value| {
                            value.phase == RestorePhase::Validating && value.total == 0
                        }) {
                            String::from("Scanning...")
                        } else {
                            format!("{:.0}%", ratio * 100.0)
                        },
                    )
                    .ratio(ratio),
                chunks[1],
            );
            frame.render_widget(
                Paragraph::new("Do not power off while paths are being published.")
                    .style(Style::default().fg(Color::Yellow)),
                chunks[2],
            );
        }
        _ => {}
    }
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let vertical_margin = area.height.saturating_sub(height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_margin),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    let horizontal_margin = area.width.saturating_mul(100 - width_percent) / 200;
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(horizontal_margin),
            Constraint::Min(1),
            Constraint::Length(horizontal_margin),
        ])
        .split(vertical[1])[1]
}

fn pane_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn changing_snapshot_clears_files_and_restore_selection() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let connection = rusqlite::Connection::open(index.path()).unwrap();
        connection
            .execute(
                "INSERT INTO archives(borg_id, name, start) VALUES (?1, 'first', ?2)",
                rusqlite::params![
                    "a".repeat(64),
                    Utc.timestamp_opt(2, 0).unwrap().to_rfc3339()
                ],
            )
            .unwrap();
        let first_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO archives(borg_id, name, start) VALUES (?1, 'second', ?2)",
                rusqlite::params![
                    "b".repeat(64),
                    Utc.timestamp_opt(1, 0).unwrap().to_rfc3339()
                ],
            )
            .unwrap();
        let second_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO files(archive_id, path, parent, name, type, size)
                 VALUES (?1, 'first-file', '', 'first-file', 'file', 1)",
                [first_id],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO files(archive_id, path, parent, name, type, size)
                 VALUES (?1, 'second-file', '', 'second-file', 'file', 1)",
                [second_id],
            )
            .unwrap();
        drop(connection);
        let mut app = App::new(&index, Path::new("/tmp/config.toml")).unwrap();
        app.selected.insert("first-file".into());

        app.move_selection(&index, 1).unwrap();
        assert_eq!(app.snapshot_index, 1);
        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].path, "second-file");
        assert!(app.selected.is_empty());
        assert!(app.directory.is_empty());

        app.handle_key(&index, KeyCode::Char('l')).unwrap();
        assert_eq!(app.focus, Focus::Files);
        app.handle_key(&index, KeyCode::Char('h')).unwrap();
        assert_eq!(app.focus, Focus::Snapshots);
        app.handle_key(&index, KeyCode::Char('k')).unwrap();
        assert_eq!(app.snapshot_index, 0);
        app.handle_key(&index, KeyCode::Char('j')).unwrap();
        assert_eq!(app.snapshot_index, 1);
    }

    #[test]
    fn command_hints_quote_real_values() {
        assert_eq!(shell_quote("a b'c"), "'a b'\"'\"'c'");
    }

    #[test]
    fn restore_opens_original_path_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App {
            snapshots: vec![Snapshot {
                id: "a".repeat(64),
                name: "host-archive".into(),
                start: Utc.timestamp_opt(0, 0).unwrap(),
                end: None,
                hostname: None,
                username: None,
            }],
            snapshot_index: 0,
            files: vec![ArchiveItem {
                path: "-literal".into(),
                kind: FileType::File,
                size: 1,
                mtime: None,
                mode: None,
                uid: None,
                gid: None,
                user: None,
                group: None,
                link_target: None,
                health: None,
            }],
            file_index: 0,
            directory: String::new(),
            selected: HashSet::new(),
            diff: Vec::new(),
            focus: Focus::Files,
            mode: Mode::Normal,
            search_result_query: None,
            message: String::new(),
            config_path: "/etc/boxup/host.toml".into(),
            restore_progress: None,
            restore_worker: None,
        };
        app.handle_key(&index, KeyCode::Char('R')).unwrap();
        assert_eq!(
            app.mode,
            Mode::ConfirmRestore {
                paths: vec!["-literal".into()],
                input: String::new(),
            }
        );
    }

    #[test]
    fn search_mode_tracks_results_and_reload_is_lowercase() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let connection = rusqlite::Connection::open(index.path()).unwrap();
        connection
            .execute(
                "INSERT INTO archives(borg_id, name, start) VALUES (?1, 'host-archive', ?2)",
                rusqlite::params![
                    "a".repeat(64),
                    Utc.timestamp_opt(0, 0).unwrap().to_rfc3339()
                ],
            )
            .unwrap();
        let archive_id = connection.last_insert_rowid();
        for (path, parent) in [("old/needle", "old"), ("other/needle", "other")] {
            connection
                .execute(
                    "INSERT INTO files(archive_id, path, parent, name, type, size)
                     VALUES (?1, ?2, ?3, 'needle', 'file', 1)",
                    rusqlite::params![archive_id, path, parent],
                )
                .unwrap();
        }
        drop(connection);
        let mut app = App {
            snapshots: vec![Snapshot {
                id: "a".repeat(64),
                name: "host-archive".into(),
                start: Utc.timestamp_opt(0, 0).unwrap(),
                end: None,
                hostname: None,
                username: None,
            }],
            snapshot_index: 0,
            files: Vec::new(),
            file_index: 0,
            directory: "old".into(),
            selected: HashSet::new(),
            diff: Vec::new(),
            focus: Focus::Files,
            mode: Mode::Search("needle".into()),
            search_result_query: None,
            message: String::new(),
            config_path: "/etc/boxup/host.toml".into(),
            restore_progress: None,
            restore_worker: None,
        };

        app.handle_key(&index, KeyCode::Enter).unwrap();
        assert_eq!(app.search_result_query.as_deref(), Some("needle"));
        assert_eq!(app.directory, "old");
        assert!(app.message.contains("current directory"));
        assert_eq!(app.files.len(), 1);
        assert_eq!(app.files[0].path, "old/needle");
        app.handle_key(&index, KeyCode::Char('r')).unwrap();
        assert!(app.message.starts_with("Reloaded "));
        assert!(app.search_result_query.is_none());
    }
}
