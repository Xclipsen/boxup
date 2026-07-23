use std::collections::HashSet;
use std::io::{self, stdout};
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::domain::{ArchiveItem, FileType, Snapshot};
use crate::index::Index;

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
            message: "Tab pane | Enter open | Space select | / search | r reload | R restore | M mount | D diff | Q quit".into(),
            config_path: config_path.display().to_string(),
        };
        app.reload_files(index)?;
        Ok(app)
    }

    fn handle_key(&mut self, index: &Index, code: KeyCode) -> Result<bool> {
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
                    let results = index.search_snapshot(&query, &snapshot)?;
                    self.directory.clear();
                    self.files = results
                        .into_iter()
                        .map(|result| ArchiveItem {
                            path: result.path,
                            kind: result.kind,
                            size: result.size,
                            mtime: result.mtime,
                            mode: None,
                            uid: None,
                            gid: None,
                            user: None,
                            group: None,
                            link_target: None,
                            health: None,
                        })
                        .collect();
                    self.file_index = 0;
                    self.search_result_query = Some(query.clone());
                    self.message = format!(
                        "Search '{query}': {} matches in current snapshot",
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
            KeyCode::Up => self.move_selection(index, -1)?,
            KeyCode::Down => self.move_selection(index, 1)?,
            KeyCode::Enter => self.enter(index)?,
            KeyCode::Backspace if self.focus == Focus::Files => {
                self.directory = self
                    .directory
                    .rsplit_once('/')
                    .map_or("", |(parent, _)| parent)
                    .to_owned();
                self.reload_files(index)?;
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
                self.message = if paths.is_empty() {
                    "Restore: select at least one file".into()
                } else {
                    let snapshot = self.current_snapshot().expect("files require a snapshot");
                    format!(
                        "Restore hint: boxup --config {} restore --to NEW_PATH {} -- {}",
                        shell_quote(&self.config_path),
                        shell_quote(&snapshot.name),
                        paths
                            .iter()
                            .map(|path| shell_quote(path))
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                };
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
            let display_path = if app.search_result_query.is_some() {
                item.path.as_str()
            } else {
                item.path.rsplit('/').next().unwrap_or(&item.path)
            };
            ListItem::new(format!("{marker} {}{suffix}", display_path))
        })
        .collect();
    let title = if let Some(query) = &app.search_result_query {
        format!("Search {query:?}")
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
    };
    frame.render_widget(
        Paragraph::new(status).block(Block::default().title("Command").borders(Borders::ALL)),
        vertical[1],
    );
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
    }

    #[test]
    fn command_hints_quote_real_values() {
        assert_eq!(shell_quote("a b'c"), "'a b'\"'\"'c'");
    }

    #[test]
    fn restore_hint_places_options_before_literal_paths() {
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
        };
        app.handle_key(&index, KeyCode::Char('R')).unwrap();
        assert!(
            app.message
                .contains("restore --to NEW_PATH 'host-archive' -- '-literal'")
        );
    }

    #[test]
    fn search_mode_tracks_results_and_reload_is_lowercase() {
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
        };

        app.handle_key(&index, KeyCode::Enter).unwrap();
        assert_eq!(app.search_result_query.as_deref(), Some("needle"));
        assert!(app.directory.is_empty());
        app.handle_key(&index, KeyCode::Char('r')).unwrap();
        assert!(app.message.starts_with("Reloaded "));
        assert!(app.search_result_query.is_none());
    }
}
