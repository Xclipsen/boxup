use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
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

use crate::config::validate_id;
use crate::domain::{
    ArchiveItem, BackupPhase, BackupProgress, BackupProtocolEvent, BackupProtocolEventKind,
    FileType, JobRecord, JobState, Snapshot, utc_now,
};
use crate::index::{Index, IndexStatus};
use crate::jobs::LocalLock;
use crate::restore::{RestorePhase, RestoreProgress};

pub struct TuiContext {
    pub config_path: PathBuf,
    pub host: String,
    pub repository: String,
    pub state_dir: PathBuf,
    pub due_hours: u64,
    pub automatic_backups: Option<bool>,
    pub background_indexing: Option<bool>,
}

pub fn run(index: &Index, context: TuiContext) -> Result<()> {
    enable_raw_mode()?;
    let mut output = stdout();
    if let Err(error) = execute!(output, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let guard = TerminalGuard;
    let mut terminal = Terminal::new(CrosstermBackend::new(output))?;
    let result = run_loop(&mut terminal, index, context);
    drop(terminal);
    drop(guard);
    result
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), LeaveAlternateScreen, Show);
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    index: &Index,
    context: TuiContext,
) -> Result<()> {
    let mut app = App::new(index, context)?;
    let result = loop {
        app.poll_worker(index);
        app.refresh_dashboard_if_due(index);
        if let Err(error) = terminal.draw(|frame| draw(frame, &app)) {
            break Err(error.into());
        }
        match event::poll(Duration::from_millis(250)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                        && app.mode == Mode::BackingUp
                    {
                        app.request_backup_cancel();
                        continue;
                    }
                    match app.handle_key(index, key.code) {
                        Ok(true) => break Ok(()),
                        Ok(false) => {}
                        Err(error) => break Err(error),
                    }
                }
                Ok(_) => {}
                Err(error) => break Err(error.into()),
            },
            Ok(false) => {}
            Err(error) => break Err(error.into()),
        }
    };
    if app.worker.is_some() {
        restore_terminal();
        app.shutdown_worker();
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Snapshots,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreChoice {
    SafeCopy,
    OriginalPaths,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Search(String),
    ConfirmBackup,
    BackingUp,
    ChooseRestore {
        paths: Vec<String>,
        choice: RestoreChoice,
    },
    ConfirmRestore {
        paths: Vec<String>,
        input: String,
    },
    Restoring,
}

enum WorkerEvent {
    RestoreProgress(RestoreProgress),
    RestoreFinished(std::result::Result<RestoreOutcome, String>),
    BackupProgress(BackupProgress),
    BackupFinished(std::result::Result<Snapshot, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RestoreRequest {
    SafeCopy { destination: PathBuf },
    OriginalPaths,
}

enum RestoreOutcome {
    SafeCopy(PathBuf),
    OriginalPaths,
}

#[derive(serde::Deserialize)]
struct SafeRestoreResult {
    destination: PathBuf,
}

#[derive(Clone, Copy)]
enum WorkerKind {
    Backup,
    Restore,
}

struct Worker {
    receiver: Receiver<WorkerEvent>,
    cancel: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    kind: WorkerKind,
}

struct Dashboard {
    index: IndexStatus,
    index_usable: bool,
    last_backup: Option<JobRecord>,
    latest_backup_attempt: Option<JobRecord>,
    running_jobs: Vec<JobRecord>,
    jobs: Vec<JobRecord>,
    estimated_seconds: Option<u64>,
    operation_active: bool,
    refreshed: Instant,
}

struct App {
    snapshots: Vec<Snapshot>,
    snapshot_index: usize,
    files: Vec<ArchiveItem>,
    file_index: usize,
    directory: String,
    selected: HashSet<String>,
    diff: Vec<String>,
    view: View,
    show_details: bool,
    focus: Focus,
    mode: Mode,
    search_result_query: Option<String>,
    message: String,
    config_path: String,
    host: String,
    repository: String,
    state_dir: PathBuf,
    due_hours: u64,
    automatic_backups: Option<bool>,
    background_indexing: Option<bool>,
    dashboard: Dashboard,
    backup_progress: Option<BackupProgress>,
    restore_progress: Option<RestoreProgress>,
    worker: Option<Worker>,
}

impl App {
    fn new(index: &Index, context: TuiContext) -> Result<Self> {
        let operation_active = LocalLock::is_held(&context.state_dir)?;
        let snapshots = index.snapshots().unwrap_or_default();
        let dashboard = load_dashboard(index, &context.repository, &context.state_dir)
            .unwrap_or_else(|_| unavailable_dashboard(operation_active));
        let message = if dashboard.index.schema_version == 0 && operation_active {
            "Another Boxup operation is active; status will refresh automatically".into()
        } else {
            "Ready".into()
        };
        let mut app = Self {
            snapshots,
            snapshot_index: 0,
            files: Vec::new(),
            file_index: 0,
            directory: String::new(),
            selected: HashSet::new(),
            diff: Vec::new(),
            view: View::Dashboard,
            show_details: false,
            focus: Focus::Snapshots,
            mode: Mode::Normal,
            search_result_query: None,
            message,
            config_path: context.config_path.display().to_string(),
            host: context.host,
            repository: context.repository,
            state_dir: context.state_dir,
            due_hours: context.due_hours,
            automatic_backups: context.automatic_backups,
            background_indexing: context.background_indexing,
            dashboard,
            backup_progress: None,
            restore_progress: None,
            worker: None,
        };
        app.reload_files(index)?;
        Ok(app)
    }

    fn handle_key(&mut self, index: &Index, code: KeyCode) -> Result<bool> {
        if matches!(self.mode, Mode::Restoring | Mode::BackingUp) {
            return Ok(false);
        }
        if self.mode == Mode::ConfirmBackup {
            match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => self.start_backup()?,
                _ => {}
            }
            return Ok(false);
        }
        if let Mode::ChooseRestore { paths, choice } = &mut self.mode {
            let mut next = None;
            match code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') | KeyCode::Tab => {
                    *choice = if *choice == RestoreChoice::SafeCopy {
                        RestoreChoice::OriginalPaths
                    } else {
                        RestoreChoice::SafeCopy
                    };
                }
                KeyCode::Char('1') => *choice = RestoreChoice::SafeCopy,
                KeyCode::Char('2') => *choice = RestoreChoice::OriginalPaths,
                KeyCode::Enter => {
                    next = Some(selected_restore_request(
                        paths.clone(),
                        *choice,
                        &self.host,
                        utc_now(),
                    )?)
                }
                _ => {}
            }
            if let Some((paths, RestoreRequest::SafeCopy { destination })) = next {
                if Path::new(&self.config_path).parent() != Some(Path::new("/etc/boxup")) {
                    self.mode = Mode::Normal;
                    self.message =
                        "Safe-copy restore requires a system profile under /etc/boxup".into();
                } else {
                    self.start_restore_worker(paths, RestoreRequest::SafeCopy { destination })?;
                }
            } else if let Some((paths, RestoreRequest::OriginalPaths)) = next {
                if Path::new(&self.config_path).parent() != Some(Path::new("/etc/boxup")) {
                    self.mode = Mode::Normal;
                    self.message =
                        "Replacing original files requires a system profile under /etc/boxup"
                            .into();
                } else {
                    self.mode = Mode::ConfirmRestore {
                        paths,
                        input: String::new(),
                    };
                }
            }
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

        if self.view == View::Dashboard {
            match code {
                KeyCode::Char('q' | 'Q') => return Ok(true),
                KeyCode::Char('1' | 'B') => {
                    if Path::new(&self.config_path).parent() != Some(Path::new("/etc/boxup")) {
                        self.message =
                            "TUI backup requires a system profile under /etc/boxup".into();
                    } else if self.dashboard.operation_active {
                        self.message = "Another Boxup operation is already active".into();
                    } else {
                        self.mode = Mode::ConfirmBackup;
                    }
                }
                KeyCode::Char('2') => self.open_browser(true),
                KeyCode::Char('3' | 'b') => self.open_browser(false),
                KeyCode::Char('4') => {
                    self.show_details = !self.show_details;
                    self.message = if self.show_details {
                        "Technical details shown; press 4 to return to the simple view".into()
                    } else {
                        "Simple backup overview shown".into()
                    };
                }
                KeyCode::Char('5') => self.toggle_automation()?,
                KeyCode::Char('r') => {
                    self.refresh_dashboard(index)?;
                    self.message = "Backup status refreshed".into();
                }
                _ => {}
            }
            return Ok(false);
        }

        match code {
            KeyCode::Char('q' | 'Q') => return Ok(true),
            KeyCode::Char('g' | 'G') => {
                self.view = View::Dashboard;
                self.message = "Backup overview".into();
            }
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
                } else {
                    self.mode = Mode::ChooseRestore {
                        paths,
                        choice: RestoreChoice::SafeCopy,
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

    fn open_browser(&mut self, restore_intent: bool) {
        if self.dashboard.index_usable {
            self.view = View::Browser;
            self.message = if restore_intent {
                "Choose files with Space, then press R to restore them. Press G for the dashboard."
                    .into()
            } else {
                "Browse with arrows or H/J/K/L. Space selects files; R restores; G returns.".into()
            };
        } else {
            self.message =
                "File browsing is unavailable because the local backup list needs updating".into();
        }
    }

    fn start_restore(&mut self, paths: Vec<String>) -> Result<()> {
        self.start_restore_worker(paths, RestoreRequest::OriginalPaths)
    }

    fn start_restore_worker(&mut self, paths: Vec<String>, request: RestoreRequest) -> Result<()> {
        let snapshot = self
            .current_snapshot()
            .context("restore requires a snapshot")?
            .name
            .clone();
        let config = self.config_path.clone();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_request = request.clone();
        let thread = std::thread::spawn(move || {
            let mut command = restore_command(
                nix::unistd::Uid::effective().is_root(),
                &config,
                &snapshot,
                &paths,
                &worker_request,
            );
            command
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
                    let mut safe_result = None;
                    if let Some(output) = child.stdout.take() {
                        for line in BufReader::new(output).lines().map_while(Result::ok) {
                            match &worker_request {
                                RestoreRequest::OriginalPaths => {
                                    if let Ok(progress) =
                                        serde_json::from_str::<RestoreProgress>(&line)
                                    {
                                        let _ = sender.send(WorkerEvent::RestoreProgress(progress));
                                    }
                                }
                                RestoreRequest::SafeCopy { .. } => {
                                    if let Ok(value) =
                                        serde_json::from_str::<SafeRestoreResult>(&line)
                                    {
                                        safe_result = Some(value.destination);
                                    }
                                }
                            }
                        }
                    }
                    let status = child.wait();
                    let error = stderr
                        .and_then(|task| task.join().ok())
                        .flatten()
                        .map(sanitize_worker_error);
                    match status {
                        Ok(status) if status.success() => match &worker_request {
                            RestoreRequest::OriginalPaths => Ok(RestoreOutcome::OriginalPaths),
                            RestoreRequest::SafeCopy { destination }
                                if safe_result.as_ref() == Some(destination) =>
                            {
                                Ok(RestoreOutcome::SafeCopy(destination.clone()))
                            }
                            RestoreRequest::SafeCopy { .. } => {
                                Err("privileged helper returned an invalid safe restore result"
                                    .into())
                            }
                        },
                        Ok(_) => Err(error.unwrap_or_else(|| "privileged helper failed".into())),
                        Err(error) => Err(format!("failed to wait for helper: {error}")),
                    }
                }
                Err(error) => Err(format!("failed to start privileged helper: {error}")),
            };
            let _ = sender.send(WorkerEvent::RestoreFinished(result));
        });
        self.restore_progress =
            matches!(request, RestoreRequest::OriginalPaths).then_some(RestoreProgress {
                phase: RestorePhase::Validating,
                current: 0,
                total: 0,
                files: 0,
                bytes: 0,
            });
        self.worker = Some(Worker {
            receiver,
            cancel,
            thread: Some(thread),
            kind: WorkerKind::Restore,
        });
        self.mode = Mode::Restoring;
        Ok(())
    }

    fn start_backup(&mut self) -> Result<()> {
        let config = self.config_path.clone();
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let thread = std::thread::spawn(move || {
            let mut command = if nix::unistd::Uid::effective().is_root() {
                Command::new("/usr/lib/boxup/boxup-root")
            } else {
                let mut command = Command::new("/usr/bin/sudo");
                command.arg("-n");
                command.arg("/usr/lib/boxup/boxup-root");
                command
            };
            command
                .arg("--config")
                .arg(config)
                .arg("backup")
                .arg("--progress-json")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let result = match command.spawn() {
                Ok(mut child) => {
                    let mut control = child.stdin.take();
                    let stderr = child.stderr.take().map(|error_output| {
                        std::thread::spawn(move || {
                            BufReader::new(error_output)
                                .lines()
                                .map_while(Result::ok)
                                .filter(|line| !line.trim().is_empty())
                                .last()
                        })
                    });
                    let mut snapshot = None;
                    let mut protocol_error = None;
                    if let Some(output) = child.stdout.take() {
                        if let Err(error) = nix::fcntl::fcntl(
                            &output,
                            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
                        ) {
                            protocol_error = Some(format!(
                                "failed to make backup progress interruptible: {error}"
                            ));
                        }
                        let mut output = BufReader::new(output);
                        let mut line = String::new();
                        while protocol_error.is_none() {
                            if worker_cancel.load(Ordering::Acquire) {
                                drop(control.take());
                                break;
                            }
                            match output.read_line(&mut line) {
                                Ok(0) => break,
                                Ok(_) => {
                                    if let Ok(event) =
                                        serde_json::from_str::<BackupProtocolEvent>(&line)
                                    {
                                        if event.version == 1 {
                                            match event.event {
                                                BackupProtocolEventKind::Progress { progress } => {
                                                    let _ = sender.send(
                                                        WorkerEvent::BackupProgress(progress),
                                                    );
                                                }
                                                BackupProtocolEventKind::Result {
                                                    snapshot: result,
                                                } => snapshot = Some(result),
                                            }
                                        }
                                    }
                                    line.clear();
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    drop(control.take());
                    let status = child.wait();
                    let error = stderr
                        .and_then(|task| task.join().ok())
                        .flatten()
                        .map(sanitize_worker_error);
                    match status {
                        Ok(status) if status.success() => match protocol_error {
                            Some(error) => Err(error),
                            None => snapshot.ok_or_else(|| {
                                "privileged helper returned no backup result".into()
                            }),
                        },
                        Ok(_) => Err(error.unwrap_or_else(|| "privileged helper failed".into())),
                        Err(error) => Err(format!("failed to wait for helper: {error}")),
                    }
                }
                Err(error) => Err(format!("failed to start privileged helper: {error}")),
            };
            let _ = sender.send(WorkerEvent::BackupFinished(result));
        });
        self.backup_progress = Some(BackupProgress {
            phase: BackupPhase::Preparing,
            elapsed_seconds: 0,
            estimated_total_seconds: self.dashboard.estimated_seconds,
            files: 0,
            original_bytes: 0,
            compressed_bytes: 0,
            deduplicated_bytes: 0,
        });
        self.worker = Some(Worker {
            receiver,
            cancel,
            thread: Some(thread),
            kind: WorkerKind::Backup,
        });
        self.mode = Mode::BackingUp;
        Ok(())
    }

    fn poll_worker(&mut self, index: &Index) {
        let Some(worker) = &self.worker else {
            return;
        };
        let events: Vec<_> = worker.receiver.try_iter().collect();
        for event in events {
            match event {
                WorkerEvent::RestoreProgress(progress) => self.restore_progress = Some(progress),
                WorkerEvent::RestoreFinished(result) => {
                    self.mode = Mode::Normal;
                    self.finish_worker();
                    match result {
                        Ok(RestoreOutcome::OriginalPaths) => {
                            self.selected.clear();
                            self.message = "Restore to original path completed".into();
                        }
                        Ok(RestoreOutcome::SafeCopy(destination)) => {
                            self.selected.clear();
                            self.message =
                                format!("Safe copy restored to {}", destination.display());
                        }
                        Err(error) => {
                            self.message = format!("Restore failed: {error}");
                        }
                    }
                }
                WorkerEvent::BackupProgress(progress) => {
                    self.backup_progress = Some(progress);
                }
                WorkerEvent::BackupFinished(result) => {
                    self.mode = Mode::Normal;
                    self.finish_worker();
                    match result {
                        Ok(snapshot) => {
                            let _ = self.reload_snapshots(index);
                            let _ = self.refresh_dashboard(index);
                            self.message = format!("Backup completed: {}", snapshot.name);
                        }
                        Err(error) => {
                            self.message = format!("Backup failed: {error}");
                            let _ = self.refresh_dashboard(index);
                        }
                    }
                }
            }
        }
    }

    fn finish_worker(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn request_backup_cancel(&mut self) {
        if let Some(worker) = &self.worker {
            if matches!(worker.kind, WorkerKind::Backup) {
                worker.cancel.store(true, Ordering::Release);
                self.message =
                    "Cancellation requested; waiting until application data is safely resumed"
                        .into();
            }
        }
    }

    fn shutdown_worker(&mut self) {
        let Some(mut worker) = self.worker.take() else {
            return;
        };
        if matches!(worker.kind, WorkerKind::Backup) {
            worker.cancel.store(true, Ordering::Release);
        }
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
    }

    fn refresh_dashboard_if_due(&mut self, index: &Index) {
        if self.dashboard.refreshed.elapsed() >= Duration::from_secs(1) {
            let _ = self.refresh_dashboard(index);
        }
    }

    fn refresh_dashboard(&mut self, index: &Index) -> Result<()> {
        let reload_snapshots = !self.dashboard.index_usable && self.snapshots.is_empty();
        self.dashboard = load_dashboard(index, &self.repository, &self.state_dir)?;
        if reload_snapshots && self.dashboard.index_usable {
            let _ = self.reload_snapshots(index);
        }
        Ok(())
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

    fn toggle_automation(&mut self) -> Result<()> {
        ensure!(
            Path::new(&self.config_path).parent() == Some(Path::new("/etc/boxup")),
            "automation requires an installed system profile"
        );
        let enable = self.automatic_backups != Some(true) || self.background_indexing != Some(true);
        let action = if enable { "enable" } else { "disable" };
        let mut command = if nix::unistd::Uid::effective().is_root() {
            Command::new("/usr/lib/boxup/setup-automation")
        } else {
            let mut command = Command::new("/usr/bin/boxup");
            if let Some(home) = std::env::var_os("HOME") {
                command.arg("--browse-config").arg(
                    PathBuf::from(home).join(format!(".config/boxup/{}-browse.toml", self.host)),
                );
            }
            command.arg("automation");
            command
        };
        if nix::unistd::Uid::effective().is_root() {
            command.arg(&self.host);
        }
        let status = command
            .arg(action)
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .context("failed to start the automation helper")?;
        ensure!(status.success(), "could not change automatic backups");
        self.automatic_backups = Some(enable);
        self.background_indexing = Some(enable);
        self.message = if enable {
            "Daily backups, background indexing, and desktop notifications are on".into()
        } else {
            "Automatic backups and background indexing are off".into()
        };
        Ok(())
    }
}

fn selected_restore_request(
    paths: Vec<String>,
    choice: RestoreChoice,
    host: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Vec<String>, RestoreRequest)> {
    let request = match choice {
        RestoreChoice::SafeCopy => {
            validate_id("host", host)?;
            let destination = Path::new("/var/lib/boxup-recovery")
                .join(host)
                .join(format!("restore-{}", now.format("%Y%m%dT%H%M%SZ")));
            RestoreRequest::SafeCopy { destination }
        }
        RestoreChoice::OriginalPaths => RestoreRequest::OriginalPaths,
    };
    Ok((paths, request))
}

fn restore_command(
    root: bool,
    config: &str,
    snapshot: &str,
    paths: &[String],
    request: &RestoreRequest,
) -> Command {
    let mut command = if root {
        Command::new("/usr/lib/boxup/boxup-root")
    } else {
        let mut command = Command::new("/usr/bin/pkexec");
        command.arg("/usr/lib/boxup/boxup-root");
        command
    };
    command.arg("--config").arg(config);
    match request {
        RestoreRequest::SafeCopy { destination } => {
            command
                .arg("restore-safe")
                .arg("--to")
                .arg(destination)
                .arg(snapshot);
        }
        RestoreRequest::OriginalPaths => {
            command
                .arg("restore-original")
                .arg("--confirm")
                .arg("RESTORE")
                .arg(snapshot);
        }
    }
    command.arg("--").args(paths);
    command
}

fn load_dashboard(index: &Index, repository: &str, state_dir: &Path) -> Result<Dashboard> {
    Ok(Dashboard {
        index: index.status()?,
        index_usable: index.is_usable(repository, Duration::from_secs(24 * 60 * 60))?,
        last_backup: index.last_success_job("backup")?,
        latest_backup_attempt: index.latest_job("backup")?,
        running_jobs: index.running_jobs()?,
        jobs: index.recent_jobs(10)?,
        estimated_seconds: index.estimated_backup_seconds(5)?,
        operation_active: LocalLock::is_held(state_dir)?,
        refreshed: Instant::now(),
    })
}

fn unavailable_dashboard(operation_active: bool) -> Dashboard {
    Dashboard {
        index: IndexStatus::default(),
        index_usable: false,
        last_backup: None,
        latest_backup_attempt: None,
        running_jobs: Vec::new(),
        jobs: Vec::new(),
        estimated_seconds: None,
        operation_active,
        refreshed: Instant::now(),
    }
}

struct Headline {
    label: &'static str,
    note: Option<String>,
}

fn next_due(last: Option<&JobRecord>, due_hours: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    last.and_then(|job| job.finished_at).and_then(|finished| {
        i64::try_from(due_hours)
            .ok()
            .map(|hours| finished + chrono::Duration::hours(hours))
    })
}

fn backup_headline(app: &App) -> Headline {
    let latest = app.dashboard.latest_backup_attempt.as_ref();
    let backing_up = app.mode == Mode::BackingUp
        || (app.dashboard.operation_active
            && latest.is_some_and(|job| job.state == JobState::Running));
    let due = next_due(app.dashboard.last_backup.as_ref(), app.due_hours)
        .is_none_or(|next| utc_now() >= next);
    let label = if backing_up {
        "Backing up"
    } else if latest.is_some_and(|job| job.state == JobState::Running) {
        "Needs attention"
    } else if latest.is_some_and(|job| job.state == JobState::Failed) {
        "Backup failed"
    } else if app.dashboard.operation_active {
        "Needs attention"
    } else if app.automatic_backups == Some(false) {
        "Automatic backups off"
    } else if app.dashboard.last_backup.is_none() {
        "No backup yet"
    } else if due {
        "Needs attention"
    } else {
        "Protected"
    };
    let note = (label == "Protected")
        .then(|| {
            app.dashboard
                .last_backup
                .as_ref()?
                .message
                .as_deref()?
                .strip_prefix("Completed with note:")
                .or_else(|| {
                    app.dashboard
                        .last_backup
                        .as_ref()?
                        .message
                        .as_deref()?
                        .strip_prefix("Completed with notes:")
                })
                .map(str::trim)
                .filter(|note| !note.is_empty())
                .map(str::to_owned)
        })
        .flatten();
    Headline { label, note }
}

fn friendly_time(value: chrono::DateTime<chrono::Utc>) -> String {
    let now = utc_now();
    if value.date_naive() == now.date_naive() {
        format!("Today at {} UTC", value.format("%H:%M"))
    } else if value.date_naive() == (now - chrono::Duration::days(1)).date_naive() {
        format!("Yesterday at {} UTC", value.format("%H:%M"))
    } else {
        value.format("%b %-d, %Y at %H:%M UTC").to_string()
    }
}

fn friendly_age(value: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = utc_now().signed_duration_since(value).num_seconds().max(0) as u64;
    if seconds < 5 {
        "just now".into()
    } else if seconds < 60 {
        format!("{seconds} seconds ago")
    } else {
        format!("{} minutes ago", seconds / 60)
    }
}

fn friendly_job_phase(phase: Option<&str>) -> &'static str {
    match phase {
        Some("preparing") => "Preparing",
        Some("auditing") => "Checking applications",
        Some("staging") => "Preparing application data",
        Some("creating_archive") => "Reading and storing files",
        Some("refreshing_index") => "Updating the file browser",
        Some("finalizing") => "Finishing",
        Some("complete") => "Complete",
        Some("failed") => "Failed",
        _ => "Starting",
    }
}

fn friendly_next_due(value: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let Some(value) = value else {
        return "Due now".into();
    };
    let remaining = value.signed_duration_since(utc_now()).num_seconds();
    if remaining <= 0 {
        "Due now".into()
    } else if remaining < 60 * 60 {
        format!("in {} minutes", (remaining + 59) / 60)
    } else if remaining < 48 * 60 * 60 {
        format!("in {} hours", (remaining + 3599) / 3600)
    } else {
        friendly_time(value)
    }
}

fn setting_state(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "On",
        Some(false) => "Off",
        None => "Not reported",
    }
}

fn job_duration(job: &JobRecord) -> Option<u64> {
    job.finished_at.map(|finished| {
        finished
            .signed_duration_since(job.started_at)
            .num_seconds()
            .max(0) as u64
    })
}

fn backup_phase_label(phase: BackupPhase) -> &'static str {
    match phase {
        BackupPhase::Preparing => "Preparing",
        BackupPhase::Auditing => "Auditing containers and services",
        BackupPhase::Staging => "Staging application data",
        BackupPhase::CreatingArchive => "Creating backup archive",
        BackupPhase::RefreshingIndex => "Updating file browser",
        BackupPhase::Finalizing => "Finishing backup",
        BackupPhase::Complete => "Complete",
        BackupPhase::Failed => "Failed",
    }
}

fn human_duration_tui(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn human_bytes_tui(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
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
    if app.view == View::Dashboard {
        draw_dashboard(frame, app);
        draw_backup_overlay(frame, app);
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
        Mode::ConfirmBackup => Line::from("Confirm backup"),
        Mode::BackingUp => Line::from("Backup in progress"),
        Mode::ChooseRestore { .. } => Line::from("Choose how to restore"),
        Mode::ConfirmRestore { .. } => Line::from("Confirm original-path restore"),
        Mode::Restoring => Line::from("Restore in progress"),
    };
    frame.render_widget(
        Paragraph::new(status).block(Block::default().title("Command").borders(Borders::ALL)),
        vertical[1],
    );

    match &app.mode {
        Mode::ChooseRestore { choice, .. } => {
            let area = centered_rect(70, 10, frame.area());
            frame.render_widget(Clear, area);
            let safe = if *choice == RestoreChoice::SafeCopy {
                "> 1 Restore a safe copy (recommended)"
            } else {
                "  1 Restore a safe copy (recommended)"
            };
            let original = if *choice == RestoreChoice::OriginalPaths {
                "> 2 Replace original files"
            } else {
                "  2 Replace original files"
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "How should Boxup restore your selection?\n\n{safe}\n{original}\n\nUse arrows or 1/2, then Enter. Esc cancels."
                ))
                .block(
                    Block::default()
                        .title("Restore Files")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                )
                .wrap(Wrap { trim: false }),
                area,
            );
        }
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

fn draw_dashboard(frame: &mut ratatui::Frame<'_>, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(frame.area());
    let headline = backup_headline(app);
    let headline_color = match headline.label {
        "Protected" => Color::Green,
        "Backing up" => Color::Cyan,
        "Backup failed" => Color::Red,
        _ => Color::Yellow,
    };
    let headline_text = headline.note.as_deref().map_or_else(
        || headline.label.to_owned(),
        |note| format!("{}  Note: {note}", headline.label),
    );
    frame.render_widget(
        Paragraph::new(headline_text)
            .style(
                Style::default()
                    .fg(headline_color)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .title("Boxup")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(headline_color)),
            ),
        vertical[0],
    );
    let body = if app.show_details {
        let recent = app
            .dashboard
            .jobs
            .iter()
            .take(5)
            .map(|job| {
                let duration = job_duration(job)
                    .map(human_duration_tui)
                    .unwrap_or_else(|| job.phase.clone().unwrap_or_else(|| "running".into()));
                format!(
                    "{}  {:?}  {}  {}{}",
                    job.started_at.format("%m-%d %H:%M"),
                    job.state,
                    job.kind,
                    duration,
                    job.message
                        .as_deref()
                        .map(|message| format!("  | {message}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Host: {}\nRepository: {}\nIndex cache: {}; browser {}\nIndex refreshed: {}\nOperation lock: {}; running jobs: {}\n\nRecent jobs\n{}",
            app.host,
            app.repository,
            if app.dashboard.index.complete {
                "complete"
            } else {
                "incomplete"
            },
            if app.dashboard.index_usable {
                "available"
            } else {
                "unavailable"
            },
            app.dashboard
                .index
                .refreshed_at
                .map(friendly_time)
                .unwrap_or_else(|| "never".into()),
            if app.dashboard.operation_active {
                "active"
            } else {
                "clear"
            },
            app.dashboard.running_jobs.len(),
            if recent.is_empty() { "None" } else { &recent },
        )
    } else {
        let last = app.dashboard.last_backup.as_ref();
        let last_backup = last
            .and_then(|job| {
                let finished = job.finished_at?;
                let detail = job_duration(job)
                    .map(human_duration_tui)
                    .map(|duration| format!(" ({duration})"))
                    .unwrap_or_default();
                Some(format!("{}{detail}", friendly_time(finished)))
            })
            .unwrap_or_else(|| "No successful backup yet".into());
        if let Some(running) = app
            .dashboard
            .latest_backup_attempt
            .as_ref()
            .filter(|job| job.state == JobState::Running && app.dashboard.operation_active)
        {
            let spinner = ["|", "/", "-", "\\"][utc_now().timestamp().unsigned_abs() as usize % 4];
            format!(
                "{spinner} Backup is running   {}   elapsed {}\n{} files   {} processed   {} new\nLast progress update: {}",
                friendly_job_phase(running.phase.as_deref()),
                human_duration_tui(
                    utc_now()
                        .signed_duration_since(running.started_at)
                        .num_seconds()
                        .max(0) as u64
                ),
                running.files,
                human_bytes_tui(running.original_bytes),
                human_bytes_tui(running.deduplicated_bytes),
                running
                    .updated_at
                    .map(friendly_age)
                    .unwrap_or_else(|| "not reported".into()),
            )
        } else {
            let latest_result = app
                .dashboard
                .latest_backup_attempt
                .as_ref()
                .filter(|job| job.state == JobState::Failed)
                .map(|job| {
                    format!(
                        "Latest attempt failed: {}\nReason: {}",
                        job.finished_at
                            .map(friendly_time)
                            .unwrap_or_else(|| friendly_time(job.started_at)),
                        job.message.as_deref().unwrap_or("Open technical details")
                    )
                })
                .unwrap_or_else(|| app.message.clone());
            format!(
                "Last backup: {last_backup}\nNext backup: {}\nAutomatic backups: {}   Background indexing: {}\n\n{}",
                friendly_next_due(next_due(last, app.due_hours)),
                setting_state(app.automatic_backups),
                setting_state(app.background_indexing),
                latest_result,
            )
        }
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .title(if app.show_details {
                        "Settings and details"
                    } else {
                        "Backup overview"
                    })
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        vertical[1],
    );
    frame.render_widget(
        Paragraph::new(
            "1 Back up now   2 Restore files   3 Browse backups\n4 Settings/details   5 Toggle automation   Q Quit",
        )
        .block(Block::default().title("Menu").borders(Borders::ALL)),
        vertical[2],
    );
}

fn draw_backup_overlay(frame: &mut ratatui::Frame<'_>, app: &App) {
    match app.mode {
        Mode::Normal if app.message.starts_with("Backup failed:") => {
            let area = centered_rect(90, 12, frame.area());
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(format!(
                    "Review the message below.\n\n{}\n\nPress r to dismiss.",
                    app.message
                ))
                .block(
                    Block::default()
                        .title("Backup Failed")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Red)),
                )
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: false }),
                area,
            );
        }
        Mode::ConfirmBackup => {
            let area = centered_rect(68, 9, frame.area());
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(
                    "Start a backup now?\n\nConfigured containers or services may be briefly quiesced.\nPress Enter to start or Esc to cancel.",
                )
                .block(
                    Block::default()
                        .title("Start Backup")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                )
                .wrap(Wrap { trim: true }),
                area,
            );
        }
        Mode::BackingUp => {
            let area = centered_rect(76, 11, frame.area());
            frame.render_widget(Clear, area);
            let progress = app.backup_progress;
            let phase = progress.map_or("Starting", |value| backup_phase_label(value.phase));
            let ratio = progress
                .and_then(|value| value.estimated_total_seconds.map(|total| (value, total)))
                .filter(|(_, total)| *total > 0)
                .map_or(0.0, |(value, total)| {
                    (value.elapsed_seconds as f64 / total as f64).min(0.95)
                });
            let details = progress.map_or_else(
                || "Waiting for administrator authorization".into(),
                |value| {
                    format!(
                        "{} files  {} processed  {} new\nElapsed: {}  {}",
                        value.files,
                        human_bytes_tui(value.original_bytes),
                        human_bytes_tui(value.deduplicated_bytes),
                        human_duration_tui(value.elapsed_seconds),
                        value.estimated_total_seconds.map_or_else(
                            || "ETA unavailable".into(),
                            |total| if value.elapsed_seconds < total {
                                format!(
                                    "ETA ~{}",
                                    human_duration_tui(total - value.elapsed_seconds)
                                )
                            } else {
                                "running longer than estimate".into()
                            }
                        )
                    )
                },
            );
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(3),
                ])
                .split(area);
            frame.render_widget(
                Paragraph::new(phase).block(Block::default().title("Backup").borders(Borders::ALL)),
                chunks[0],
            );
            frame.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL))
                    .gauge_style(Style::default().fg(Color::Cyan))
                    .label(
                        if progress.is_some_and(|value| value.estimated_total_seconds.is_some()) {
                            format!("~{:.0}%", ratio * 100.0)
                        } else {
                            "Estimating...".into()
                        },
                    )
                    .ratio(ratio),
                chunks[1],
            );
            frame.render_widget(
                Paragraph::new(details)
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: false }),
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
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/tmp/config.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();
        app.view = View::Browser;
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
    fn dashboard_remains_available_when_browser_index_is_unusable() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/tmp/config.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();

        assert_eq!(app.view, View::Dashboard);
        assert!(!app.dashboard.index_usable);
        app.handle_key(&index, KeyCode::Char('b')).unwrap();
        assert_eq!(app.view, View::Dashboard);
        assert!(app.message.contains("browsing is unavailable"));
    }

    #[test]
    fn dashboard_starts_while_the_index_is_locked_for_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite3");
        let _writable = Index::open(&path).unwrap();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let index = Index::open_read_only(&path).unwrap();

        let app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(true),
            },
        )
        .unwrap();

        assert_eq!(app.view, View::Dashboard);
        assert!(app.snapshots.is_empty());
        assert!(!app.dashboard.index_usable);
    }

    #[test]
    fn dashboard_number_keys_follow_the_visible_menu() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();
        app.dashboard.index_usable = true;

        app.handle_key(&index, KeyCode::Char('1')).unwrap();
        assert_eq!(app.mode, Mode::ConfirmBackup);
        app.handle_key(&index, KeyCode::Esc).unwrap();

        app.handle_key(&index, KeyCode::Char('2')).unwrap();
        assert_eq!(app.view, View::Browser);
        assert!(app.message.contains("press R to restore"));

        app.view = View::Dashboard;
        app.handle_key(&index, KeyCode::Char('3')).unwrap();
        assert_eq!(app.view, View::Browser);

        app.view = View::Dashboard;
        app.handle_key(&index, KeyCode::Char('4')).unwrap();
        assert!(app.show_details);
        assert!(app.handle_key(&index, KeyCode::Char('Q')).unwrap());
    }

    #[test]
    fn successful_backup_note_keeps_protected_headline() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();
        let finished = utc_now() - chrono::Duration::hours(1);
        let job = JobRecord {
            id: 7,
            kind: "backup".into(),
            state: JobState::Succeeded,
            started_at: finished - chrono::Duration::minutes(2),
            finished_at: Some(finished),
            message: Some("Completed with note: one file changed while being read".into()),
            phase: None,
            updated_at: Some(finished),
            files: 12,
            original_bytes: 1024,
            compressed_bytes: 512,
            deduplicated_bytes: 128,
            archive_name: Some("test-archive".into()),
            archive_id: Some("a".repeat(64)),
            stats_recorded: true,
        };
        app.dashboard.last_backup = Some(job.clone());
        app.dashboard.latest_backup_attempt = Some(job);

        let headline = backup_headline(&app);
        assert_eq!(headline.label, "Protected");
        assert_eq!(
            headline.note.as_deref(),
            Some("one file changed while being read")
        );
    }

    #[test]
    fn running_backup_dashboard_shows_live_progress() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(true),
            },
        )
        .unwrap();
        let started = utc_now() - chrono::Duration::minutes(2);
        let job = JobRecord {
            id: 8,
            kind: "backup".into(),
            state: JobState::Running,
            started_at: started,
            finished_at: None,
            message: None,
            phase: Some("creating_archive".into()),
            updated_at: Some(utc_now()),
            files: 305_808,
            original_bytes: 13 * 1024 * 1024 * 1024,
            compressed_bytes: 7 * 1024 * 1024 * 1024,
            deduplicated_bytes: 310_189,
            archive_name: None,
            archive_id: None,
            stats_recorded: false,
        };
        app.dashboard.latest_backup_attempt = Some(job.clone());
        app.dashboard.running_jobs = vec![job];
        app.dashboard.operation_active = true;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Backup is running"));
        assert!(rendered.contains("Reading and storing files"));
        assert!(rendered.contains("305808 files"));
        assert!(rendered.contains("13.0 GiB processed"));
    }

    #[test]
    fn tui_backup_cancellation_sets_the_worker_control_flag() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();
        let (_sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        app.worker = Some(Worker {
            receiver,
            cancel: Arc::clone(&cancel),
            thread: None,
            kind: WorkerKind::Backup,
        });

        app.request_backup_cancel();
        assert!(cancel.load(Ordering::Acquire));
        assert!(app.message.contains("Cancellation requested"));
    }

    #[test]
    fn dashboard_wraps_the_complete_backup_failure() {
        let temp = tempfile::tempdir().unwrap();
        let index = Index::open(temp.path().join("index.sqlite3")).unwrap();
        let mut app = App::new(
            &index,
            TuiContext {
                config_path: PathBuf::from("/etc/boxup/test.toml"),
                host: "test".into(),
                repository: "/test".into(),
                state_dir: temp.path().join("state"),
                due_hours: 24,
                automatic_backups: Some(true),
                background_indexing: Some(false),
            },
        )
        .unwrap();
        app.message = format!(
            "Backup failed: Borg completed with warnings: {}: permission denied",
            "long/path/".repeat(12)
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("permission denied"));
        assert!(rendered.contains("Review the message below"));
        assert!(!rendered.contains("Borg warning"));
    }

    #[test]
    fn restore_choice_starts_safe_flow_and_original_keeps_exact_confirmation() {
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
            view: View::Browser,
            show_details: false,
            focus: Focus::Files,
            mode: Mode::Normal,
            search_result_query: None,
            message: String::new(),
            config_path: "/etc/boxup/host.toml".into(),
            host: "host".into(),
            repository: "/test".into(),
            state_dir: temp.path().join("state"),
            due_hours: 24,
            automatic_backups: Some(true),
            background_indexing: Some(false),
            dashboard: Dashboard {
                index: IndexStatus::default(),
                index_usable: true,
                last_backup: None,
                latest_backup_attempt: None,
                running_jobs: Vec::new(),
                jobs: Vec::new(),
                estimated_seconds: None,
                operation_active: false,
                refreshed: Instant::now(),
            },
            backup_progress: None,
            restore_progress: None,
            worker: None,
        };
        app.handle_key(&index, KeyCode::Char('R')).unwrap();
        assert_eq!(
            app.mode,
            Mode::ChooseRestore {
                paths: vec!["-literal".into()],
                choice: RestoreChoice::SafeCopy,
            }
        );
        let (paths, request) = selected_restore_request(
            vec!["-literal".into()],
            RestoreChoice::SafeCopy,
            "host",
            Utc.with_ymd_and_hms(2026, 7, 28, 14, 23, 5).unwrap(),
        )
        .unwrap();
        assert_eq!(
            request,
            RestoreRequest::SafeCopy {
                destination: PathBuf::from("/var/lib/boxup-recovery/host/restore-20260728T142305Z"),
            }
        );
        let command = restore_command(false, &app.config_path, "host-archive", &paths, &request);
        assert_eq!(command.get_program(), "/usr/bin/pkexec");
        assert_eq!(
            command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "/usr/lib/boxup/boxup-root",
                "--config",
                "/etc/boxup/host.toml",
                "restore-safe",
                "--to",
                "/var/lib/boxup-recovery/host/restore-20260728T142305Z",
                "host-archive",
                "--",
                "-literal",
            ]
        );

        app.handle_key(&index, KeyCode::Char('2')).unwrap();
        app.handle_key(&index, KeyCode::Enter).unwrap();
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
            view: View::Browser,
            show_details: false,
            focus: Focus::Files,
            mode: Mode::Search("needle".into()),
            search_result_query: None,
            message: String::new(),
            config_path: "/etc/boxup/host.toml".into(),
            host: "host".into(),
            repository: "/test".into(),
            state_dir: temp.path().join("state"),
            due_hours: 24,
            automatic_backups: Some(true),
            background_indexing: Some(false),
            dashboard: Dashboard {
                index: IndexStatus::default(),
                index_usable: true,
                last_backup: None,
                latest_backup_attempt: None,
                running_jobs: Vec::new(),
                jobs: Vec::new(),
                estimated_seconds: None,
                operation_active: false,
                refreshed: Instant::now(),
            },
            backup_progress: None,
            restore_progress: None,
            worker: None,
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
