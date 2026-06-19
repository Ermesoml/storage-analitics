#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("This tool currently targets Windows only.");
}

#[cfg(target_os = "windows")]
mod windows_app {
    use std::{
        collections::VecDeque,
        ffi::OsStr,
        fs, io,
        os::windows::{ffi::OsStrExt, fs::MetadataExt},
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc::{self, Receiver, Sender},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    };
    use ratatui::{
        Frame, Terminal,
        backend::CrosstermBackend,
        layout::{Alignment, Constraint, Layout},
        style::{Modifier, Style},
        text::Line,
        widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
    };
    use rusqlite::{Connection, OptionalExtension, params};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
        GetVolumeInformationW,
    };

    type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

    const SPINNER: [&str; 8] = ["|", "/", "-", "\\", "|", "/", "-", "\\"];
    const DRIVE_UNKNOWN: u32 = 0;
    const DRIVE_NO_ROOT_DIR: u32 = 1;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;
    const MAX_SCAN_WORKERS: usize = 4;
    const UPDATE_BYTES_THRESHOLD: u64 = 32 * 1024 * 1024;
    const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
    const CACHE_DB_FILE: &str = "storage_analytics_cache.sqlite3";

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum SizeState {
        Pending,
        Cached,
        Partial,
        Complete,
    }

    impl SizeState {
        fn is_complete(self) -> bool {
            matches!(self, SizeState::Complete)
        }
    }

    #[derive(Clone, Debug)]
    struct DriveInfo {
        root: PathBuf,
        name: String,
        label: Option<String>,
        drive_type: &'static str,
        total_bytes: u64,
        free_bytes: u64,
    }

    impl DriveInfo {
        fn used_bytes(&self) -> u64 {
            self.total_bytes.saturating_sub(self.free_bytes)
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum EntryKind {
        Directory,
        File,
        Link,
        Other,
    }

    impl EntryKind {
        fn label(&self) -> &'static str {
            match self {
                EntryKind::Directory => "dir",
                EntryKind::File => "file",
                EntryKind::Link => "link",
                EntryKind::Other => "other",
            }
        }

        fn can_enter(&self) -> bool {
            matches!(self, EntryKind::Directory)
        }
    }

    #[derive(Clone, Debug)]
    struct EntryInfo {
        path: PathBuf,
        name: String,
        kind: EntryKind,
        size_bytes: u64,
        size_state: SizeState,
    }

    #[derive(Clone, Debug)]
    struct DirectoryTask {
        path: PathBuf,
        cached_hint: bool,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum View {
        Drives,
        Directory,
    }

    #[derive(Debug)]
    enum ScanEvent {
        Started {
            request_id: u64,
            path: PathBuf,
            total_entries: usize,
        },
        Entry {
            request_id: u64,
            entry: EntryInfo,
        },
        SizeUpdated {
            request_id: u64,
            path: PathBuf,
            size: u64,
            state: SizeState,
        },
        Finished {
            request_id: u64,
            path: PathBuf,
            error_count: usize,
        },
        Failed {
            request_id: u64,
            path: PathBuf,
            error: String,
        },
    }

    #[derive(Debug)]
    enum DeletionResult {
        Success(PathBuf),
        Failure(PathBuf, String),
    }

    #[derive(Debug, Default)]
    struct ScanProgress {
        request_id: u64,
        total_entries: usize,
        resolved_entries: usize,
        error_count: usize,
        started: bool,
        scanning: bool,
    }

    struct App {
        view: View,
        drives: Vec<DriveInfo>,
        drives_state: TableState,
        entries: Vec<EntryInfo>,
        entries_state: TableState,
        current_root: Option<PathBuf>,
        current_path: Option<PathBuf>,
        scan_progress: ScanProgress,
        scanner_tx: Sender<ScanEvent>,
        scanner_rx: Receiver<ScanEvent>,
        scan_generation: Arc<AtomicU64>,
        cache_store: CacheStore,
        status: String,
        spinner_index: usize,
        last_content_height: u16,
        user_moved_selection: bool,
        pending_delete: Option<PathBuf>,
        is_deleting: bool,
        deletion_tx: Sender<DeletionResult>,
        deletion_rx: Receiver<DeletionResult>,
    }

    #[derive(Clone)]
    struct CacheStore {
        db_path: Arc<PathBuf>,
    }

    #[derive(Clone, Debug)]
    struct CachedDirectory {
        listing_fingerprint: String,
        recursive_size: u64,
    }

    #[derive(Clone, Debug)]
    struct ListingEntry {
        name: String,
        kind: EntryKind,
        size_bytes: u64,
        last_write_time: u64,
        creation_time: u64,
        file_attributes: u32,
    }

    #[derive(Debug)]
    struct DirectoryListing {
        fingerprint: String,
        direct_non_dir_bytes: u64,
        child_dirs: Vec<PathBuf>,
        error_count: usize,
        cacheable: bool,
    }

    #[derive(Debug)]
    struct DirectoryScanResult {
        size_bytes: u64,
        error_count: usize,
    }

    struct DirectoryProgress {
        total_bytes: u64,
        last_reported_bytes: u64,
        last_reported_at: Instant,
    }

    impl DirectoryProgress {
        fn new() -> Self {
            Self {
                total_bytes: 0,
                last_reported_bytes: 0,
                last_reported_at: Instant::now(),
            }
        }

        fn add_bytes(&mut self, bytes: u64) {
            self.total_bytes = self.total_bytes.saturating_add(bytes);
        }

        fn emit(
            &mut self,
            sender: &Sender<ScanEvent>,
            request_id: u64,
            path: &Path,
            state: SizeState,
        ) {
            let now = Instant::now();
            let bytes_delta = self.total_bytes.saturating_sub(self.last_reported_bytes);
            let time_elapsed = now.duration_since(self.last_reported_at);

            if state != SizeState::Complete
                && (self.total_bytes == self.last_reported_bytes
                    || (bytes_delta < UPDATE_BYTES_THRESHOLD && time_elapsed < UPDATE_INTERVAL))
            {
                return;
            }

            let _ = sender.send(ScanEvent::SizeUpdated {
                request_id,
                path: path.to_path_buf(),
                size: self.total_bytes,
                state,
            });

            self.last_reported_bytes = self.total_bytes;
            self.last_reported_at = now;
        }
    }

    impl CacheStore {
        fn new(base_dir: &Path) -> AppResult<Self> {
            let store = Self {
                db_path: Arc::new(base_dir.join(CACHE_DB_FILE)),
            };
            store.initialize()?;
            Ok(store)
        }

        fn initialize(&self) -> AppResult<()> {
            let conn = self.open_connection()?;
            conn.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS directory_cache (
                    path_key TEXT PRIMARY KEY,
                    display_path TEXT NOT NULL,
                    listing_fingerprint TEXT NOT NULL,
                    recursive_size INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );
                ",
            )?;
            Ok(())
        }

        fn open_connection(&self) -> io::Result<Connection> {
            let conn = Connection::open(&*self.db_path).map_err(sqlite_err_to_io)?;
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(sqlite_err_to_io)?;
            conn.execute_batch(
                "
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                ",
            )
            .map_err(sqlite_err_to_io)?;
            Ok(conn)
        }
    }

    impl App {
        fn new() -> AppResult<Self> {
            let base_dir = std::env::current_dir()?;
            let cache_store = CacheStore::new(&base_dir)?;
            let drives = discover_drives()?;
            let drives_state = select_first(TableState::default(), drives.len());
            let (scanner_tx, scanner_rx) = mpsc::channel();
            let (deletion_tx, deletion_rx) = mpsc::channel();
            let status = if drives.is_empty() {
                "No accessible drives were found.".to_string()
            } else {
                "Select a drive and press Enter.".to_string()
            };

            Ok(Self {
                view: View::Drives,
                drives,
                drives_state,
                entries: Vec::new(),
                entries_state: TableState::default(),
                current_root: None,
                current_path: None,
                scan_progress: ScanProgress::default(),
                scanner_tx,
                scanner_rx,
                scan_generation: Arc::new(AtomicU64::new(0)),
                cache_store,
                status,
                spinner_index: 0,
                last_content_height: 10,
                user_moved_selection: false,
                pending_delete: None,
                is_deleting: false,
                deletion_tx,
                deletion_rx,
            })
        }

        fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> AppResult<()> {
            loop {
                self.process_scan_events();
                self.process_deletion_events();
                terminal.draw(|frame| self.draw(frame))?;

                if event::poll(Duration::from_millis(150))? {
                    if let Event::Key(key) = event::read()? {
                        if key.kind == KeyEventKind::Press && !self.handle_key(key.code)? {
                            return Ok(());
                        }
                    }
                } else if self.scan_progress.scanning || self.is_deleting {
                    self.spinner_index = (self.spinner_index + 1) % SPINNER.len();
                }
            }
        }

        fn draw(&mut self, frame: &mut Frame<'_>) {
            let areas = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(frame.area());

            self.last_content_height = areas[1].height;

            let title = match (&self.view, &self.current_path) {
                (View::Drives, _) => "Storage Analytics - Drives".to_string(),
                (View::Directory, Some(path)) => {
                    format!("Storage Analytics - {}", path.display())
                }
                (View::Directory, None) => "Storage Analytics - Directory".to_string(),
            };

            let title_block = Block::default()
                .borders(Borders::ALL)
                .title(Line::from(title));
            let summary = Paragraph::new(self.summary_line()).block(title_block);
            frame.render_widget(summary, areas[0]);

            match self.view {
                View::Drives => self.draw_drives(frame, areas[1]),
                View::Directory => self.draw_entries(frame, areas[1]),
            }

            let footer = Paragraph::new(self.footer_line())
                .block(Block::default().borders(Borders::ALL).title("Help"));
            frame.render_widget(footer, areas[2]);

            if self.pending_delete.is_some() {
                self.draw_delete_modal(frame);
            }
        }

        fn draw_drives(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
            let rows = self.drives.iter().map(|drive| {
                let label = drive.label.as_deref().unwrap_or("-");
                Row::new([
                    Cell::from(drive.name.clone()),
                    Cell::from(label.to_string()),
                    Cell::from(drive.drive_type.to_string()),
                    Cell::from(human_bytes(drive.used_bytes())),
                    Cell::from(human_bytes(drive.free_bytes)),
                    Cell::from(human_bytes(drive.total_bytes)),
                ])
            });

            let table = Table::new(
                rows,
                [
                    Constraint::Length(8),
                    Constraint::Percentage(28),
                    Constraint::Length(10),
                    Constraint::Length(12),
                    Constraint::Length(12),
                    Constraint::Length(12),
                ],
            )
            .header(
                Row::new(["Drive", "Label", "Type", "Used", "Free", "Total"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Connected Drives"),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

            let mut state = self.drives_state.clone();
            frame.render_stateful_widget(table, area, &mut state);
        }

        fn draw_entries(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
            let total_bytes: u64 = self.entries.iter().map(|e| e.size_bytes).sum();

            let rows = self.entries.iter().map(|entry| {
                let size = format_entry_size(entry);
                let pct = format_entry_percentage(entry.size_bytes, total_bytes);

                Row::new([
                    Cell::from(entry.kind.label().to_string()),
                    Cell::from(entry.name.clone()),
                    Cell::from(size),
                    Cell::from(pct),
                ])
            });

            let table = Table::new(
                rows,
                [
                    Constraint::Length(8),
                    Constraint::Percentage(55),
                    Constraint::Length(16),
                    Constraint::Length(10),
                ],
            )
            .header(
                Row::new(["Type", "Name", "Size", "%"])
                    .style(Style::default().add_modifier(Modifier::BOLD)),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Folder Contents"),
            )
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

            let mut state = self.entries_state.clone();
            frame.render_stateful_widget(table, area, &mut state);
        }

        fn draw_delete_modal(&self, frame: &mut Frame<'_>) {
            let area = frame.area();
            let popup_area = centered_rect(60, 25, area);

            frame.render_widget(Clear, popup_area);

            let name = self
                .pending_delete
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            let (title, text, hint_text) = if self.is_deleting {
                let spinner = SPINNER[self.spinner_index % SPINNER.len()];
                (
                    " Deleting ",
                    format!("Deleting {}... {}", name, spinner),
                    "",
                )
            } else {
                let is_dir = self
                    .pending_delete
                    .as_ref()
                    .map(|p| p.is_dir())
                    .unwrap_or(false);
                let item_type = if is_dir { "folder" } else { "file" };
                (
                    " Confirm Deletion ",
                    format!("Are you sure you want to delete this {}?\n\n{}", item_type, name),
                    "Enter to confirm | Esc to cancel",
                )
            };

            let block = Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().add_modifier(Modifier::BOLD));

            let paragraph = Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center);

            frame.render_widget(paragraph, popup_area);

            if !hint_text.is_empty() {
                let hint_area = ratatui::layout::Rect {
                    x: popup_area.x + 2,
                    y: popup_area.y + popup_area.height.saturating_sub(2),
                    width: popup_area.width.saturating_sub(4),
                    height: 1,
                };
                let hint = Paragraph::new(hint_text)
                    .alignment(Alignment::Center);
                frame.render_widget(hint, hint_area);
            }
        }

        fn summary_line(&self) -> Line<'static> {
            match self.view {
                View::Drives => {
                    let drive_count = self.drives.len();
                    let total_bytes: u64 = self.drives.iter().map(|drive| drive.total_bytes).sum();
                    let used_bytes: u64 = self.drives.iter().map(DriveInfo::used_bytes).sum();
                    Line::from(format!(
                        "{} drive(s) visible | Used {} of {}",
                        drive_count,
                        human_bytes(used_bytes),
                        human_bytes(total_bytes)
                    ))
                }
                View::Directory => {
                    let shown_bytes: u64 = self.entries.iter().map(|entry| entry.size_bytes).sum();
                    let pending = self
                        .entries
                        .iter()
                        .filter(|entry| !entry.size_state.is_complete())
                        .count();
                    let progress = self.scan_progress_label();

                    let spinner = if self.scan_progress.scanning {
                        format!(" {} scanning", SPINNER[self.spinner_index])
                    } else {
                        String::new()
                    };

                    Line::from(format!(
                        "{} item(s) | Shown size {} | Progress {} | Pending {}{}",
                        self.entries.len(),
                        human_bytes(shown_bytes),
                        progress,
                        pending,
                        spinner
                    ))
                }
            }
        }

        fn footer_line(&self) -> Line<'static> {
            let selected_hint = match self.view {
                View::Drives => "Arrows/jk/Home/End/PgUp/PgDown move | Enter opens drive | r refresh drives | q quits",
                View::Directory => {
                    "Arrows/jk/Home/End/PgUp/PgDown move | Enter opens folder | Del deletes | Backspace/Esc goes up | r rescans | d returns to drives | q quits"
                }
            };

            let message = if self.status.is_empty() {
                selected_hint.to_string()
            } else {
                format!("{} | {}", selected_hint, self.status)
            };

            Line::from(message)
        }

        fn scan_progress_label(&self) -> String {
            if !self.scan_progress.started {
                return if self.scan_progress.scanning {
                    "preparing...".to_string()
                } else {
                    "unavailable".to_string()
                };
            }

            format!(
                "{}% ({}/{})",
                self.scan_percent(),
                self.scan_progress.resolved_entries,
                self.scan_progress.total_entries
            )
        }

        fn scan_percent(&self) -> usize {
            if !self.scan_progress.started {
                return 0;
            }

            if self.scan_progress.total_entries == 0 {
                return 100;
            }

            self.scan_progress.resolved_entries.saturating_mul(100)
                / self.scan_progress.total_entries
        }

        fn refresh_scan_status(&mut self) {
            let Some(path) = self.current_path.as_ref() else {
                return;
            };

            if !self.scan_progress.scanning {
                return;
            }

            if !self.scan_progress.started {
                self.status = format!("Reading entries in {}...", path.display());
                return;
            }

            self.status = format!(
                "Scanning {}... {}% ({}/{}) resolved at this level",
                path.display(),
                self.scan_percent(),
                self.scan_progress.resolved_entries,
                self.scan_progress.total_entries
            );
        }

        fn handle_key(&mut self, code: KeyCode) -> AppResult<bool> {
            if self.is_deleting {
                return Ok(true);
            }

            if self.pending_delete.is_some() {
                match code {
                    KeyCode::Enter => self.confirm_delete(),
                    KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('q') => self.cancel_delete(),
                    _ => {}
                }
                return Ok(true);
            }

            match code {
                KeyCode::Char('q') => return Ok(false),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Home => self.move_selection_to_start(),
                KeyCode::End => self.move_selection_to_end(),
                KeyCode::PageUp => self.move_selection(-self.page_size()),
                KeyCode::PageDown => self.move_selection(self.page_size()),
                KeyCode::Enter => self.open_selected(),
                KeyCode::Backspace | KeyCode::Esc => self.go_up(),
                KeyCode::Delete => self.initiate_delete(),
                KeyCode::Char('d') => self.show_drives(),
                KeyCode::Char('r') => self.refresh_current()?,
                _ => {}
            }

            Ok(true)
        }

        fn move_selection(&mut self, delta: isize) {
            self.user_moved_selection = true;
            match self.view {
                View::Drives => {
                    let len = self.drives.len();
                    move_state_selection(&mut self.drives_state, len, delta);
                }
                View::Directory => {
                    let len = self.entries.len();
                    move_state_selection(&mut self.entries_state, len, delta);
                }
            }
        }

        fn move_selection_to_start(&mut self) {
            self.user_moved_selection = true;
            match self.view {
                View::Drives => {
                    if !self.drives.is_empty() {
                        self.drives_state.select(Some(0));
                    }
                }
                View::Directory => {
                    if !self.entries.is_empty() {
                        self.entries_state.select(Some(0));
                    }
                }
            }
        }

        fn move_selection_to_end(&mut self) {
            self.user_moved_selection = true;
            match self.view {
                View::Drives => {
                    if let Some(last) = self.drives.len().checked_sub(1) {
                        self.drives_state.select(Some(last));
                    }
                }
                View::Directory => {
                    if let Some(last) = self.entries.len().checked_sub(1) {
                        self.entries_state.select(Some(last));
                    }
                }
            }
        }

        fn page_size(&self) -> isize {
            self.last_content_height.saturating_sub(3).max(1) as isize
        }

        fn open_selected(&mut self) {
            match self.view {
                View::Drives => {
                    if let Some(index) = self.drives_state.selected() {
                        if let Some(drive) = self.drives.get(index) {
                            self.current_root = Some(drive.root.clone());
                            self.navigate_to(drive.root.clone());
                        }
                    }
                }
                View::Directory => {
                    if let Some(index) = self.entries_state.selected() {
                        if let Some(entry) = self.entries.get(index) {
                            if entry.kind.can_enter() {
                                self.navigate_to(entry.path.clone());
                            } else {
                                self.status = format!(
                                    "{} is a {} and cannot be entered.",
                                    entry.name,
                                    entry.kind.label()
                                );
                            }
                        }
                    }
                }
            }
        }

        fn go_up(&mut self) {
            if self.view == View::Drives {
                return;
            }

            let Some(current_path) = self.current_path.clone() else {
                self.show_drives();
                return;
            };

            if let Some(root) = &self.current_root {
                if &current_path == root {
                    self.show_drives();
                    return;
                }
            }

            if let Some(parent) = current_path.parent() {
                self.navigate_to(parent.to_path_buf());
            } else {
                self.show_drives();
            }
        }

        fn refresh_current(&mut self) -> AppResult<()> {
            match self.view {
                View::Drives => {
                    self.drives = discover_drives()?;
                    self.drives_state = select_first(TableState::default(), self.drives.len());
                    self.status = if self.drives.is_empty() {
                        "No accessible drives were found.".to_string()
                    } else {
                        "Drive list refreshed.".to_string()
                    };
                }
                View::Directory => {
                    if let Some(path) = self.current_path.clone() {
                        self.navigate_to(path);
                    }
                }
            }

            Ok(())
        }

        fn initiate_delete(&mut self) {
            if self.view != View::Directory {
                self.status = "Deletion is only available in directory view.".to_string();
                return;
            }

            if let Some(index) = self.entries_state.selected() {
                if let Some(entry) = self.entries.get(index) {
                    self.pending_delete = Some(entry.path.clone());
                }
            }
        }

        fn confirm_delete(&mut self) {
            let Some(path) = self.pending_delete.clone() else {
                return;
            };

            self.is_deleting = true;
            let tx = self.deletion_tx.clone();

            thread::spawn(move || {
                let result = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };

                let event = match result {
                    Ok(()) => DeletionResult::Success(path),
                    Err(error) => DeletionResult::Failure(path, error.to_string()),
                };

                let _ = tx.send(event);
            });
        }

        fn process_deletion_events(&mut self) {
            while let Ok(event) = self.deletion_rx.try_recv() {
                self.is_deleting = false;
                self.pending_delete = None;

                match event {
                    DeletionResult::Success(path) => {
                        self.entries.retain(|e| e.path != path);
                        self.sort_entries();
                        self.status = format!("Deleted {}.", path.display());
                    }
                    DeletionResult::Failure(path, error) => {
                        self.status = format!("Failed to delete {}: {}", path.display(), error);
                    }
                }
            }
        }

        fn cancel_delete(&mut self) {
            self.pending_delete = None;
            self.is_deleting = false;
            self.status = "Deletion cancelled.".to_string();
        }

        fn show_drives(&mut self) {
            self.view = View::Drives;
            self.entries.clear();
            self.entries_state = TableState::default();
            self.current_path = None;
            self.scan_progress = ScanProgress::default();
            self.scan_generation.fetch_add(1, Ordering::Relaxed);
            self.drives_state = select_first(TableState::default(), self.drives.len());
            self.user_moved_selection = false;
            self.pending_delete = None;
            self.is_deleting = false;
            self.status = if self.drives.is_empty() {
                "No accessible drives were found.".to_string()
            } else {
                "Select a drive and press Enter.".to_string()
            };
        }

        fn navigate_to(&mut self, path: PathBuf) {
            self.view = View::Directory;
            self.current_path = Some(path.clone());
            self.entries.clear();
            self.entries_state = TableState::default();
            self.scan_progress = ScanProgress::default();
            self.user_moved_selection = false;
            self.pending_delete = None;
            self.is_deleting = false;

            let request_id = self.scan_generation.fetch_add(1, Ordering::Relaxed) + 1;
            self.scan_progress.request_id = request_id;
            self.scan_progress.scanning = true;
            self.status = format!("Reading entries in {}...", path.display());

            spawn_scan(
                request_id,
                path,
                self.scanner_tx.clone(),
                Arc::clone(&self.scan_generation),
                self.cache_store.clone(),
            );
        }

        fn process_scan_events(&mut self) {
            while let Ok(event) = self.scanner_rx.try_recv() {
                match event {
                    ScanEvent::Started {
                        request_id,
                        path,
                        total_entries,
                    } => {
                        if !self.is_current_request(request_id, &path) {
                            continue;
                        }

                        self.scan_progress.request_id = request_id;
                        self.scan_progress.total_entries = total_entries;
                        self.scan_progress.resolved_entries = 0;
                        self.scan_progress.error_count = 0;
                        self.scan_progress.started = true;
                        self.scan_progress.scanning = true;
                        self.refresh_scan_status();
                    }
                    ScanEvent::Entry { request_id, entry } => {
                        if !self.is_current_request(request_id, &entry.path) {
                            continue;
                        }

                        if entry.size_state.is_complete() {
                            self.scan_progress.resolved_entries += 1;
                        }

                        self.entries.push(entry);
                        self.sort_entries();
                        if self.entries_state.selected().is_none() && !self.entries.is_empty() {
                            self.entries_state.select(Some(0));
                        }
                        self.refresh_scan_status();
                    }
                    ScanEvent::SizeUpdated {
                        request_id,
                        path,
                        size,
                        state,
                    } => {
                        if !self.is_current_request(request_id, &path) {
                            continue;
                        }

                        if let Some(entry) =
                            self.entries.iter_mut().find(|entry| entry.path == path)
                        {
                            if state.is_complete() && !entry.size_state.is_complete() {
                                self.scan_progress.resolved_entries += 1;
                            }
                            entry.size_bytes = size;
                            entry.size_state = state;
                        }

                        self.sort_entries();
                        self.refresh_scan_status();
                    }
                    ScanEvent::Finished {
                        request_id,
                        path,
                        error_count,
                    } => {
                        if !self.is_current_request(request_id, &path) {
                            continue;
                        }

                        self.scan_progress.scanning = false;
                        self.scan_progress.error_count = error_count;
                        if error_count == 0 {
                            self.status = format!(
                                "Finished scanning {} at 100% ({} item(s)).",
                                path.display(),
                                self.entries.len()
                            );
                        } else {
                            self.status = format!(
                                "Finished scanning {} with {} access error(s).",
                                path.display(),
                                error_count
                            );
                        }
                    }
                    ScanEvent::Failed {
                        request_id,
                        path,
                        error,
                    } => {
                        if !self.is_current_request(request_id, &path) {
                            continue;
                        }

                        self.scan_progress.started = false;
                        self.scan_progress.scanning = false;
                        self.status = format!("Could not scan {}: {}", path.display(), error);
                    }
                }
            }
        }

        fn is_current_request(&self, request_id: u64, path: &Path) -> bool {
            self.scan_progress.request_id == request_id
                && self.current_path.as_deref().is_some_and(|current| {
                    current == path || path.starts_with(current) || current.starts_with(path)
                })
        }

        fn sort_entries(&mut self) {
            let selected_path = if self.user_moved_selection {
                self.entries_state
                    .selected()
                    .and_then(|index| self.entries.get(index))
                    .map(|entry| entry.path.clone())
            } else {
                None
            };

            self.entries.sort_by(entry_sort_key);

            let next_selection = if self.user_moved_selection {
                selected_path
                    .as_ref()
                    .and_then(|path| self.entries.iter().position(|entry| &entry.path == path))
                    .or_else(|| (!self.entries.is_empty()).then_some(0))
            } else {
                (!self.entries.is_empty()).then_some(0)
            };

            self.entries_state.select(next_selection);
        }
    }

    fn move_state_selection(state: &mut TableState, len: usize, delta: isize) {
        if len == 0 {
            state.select(None);
            return;
        }

        let current = state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        state.select(Some(next));
    }

    fn select_first(mut state: TableState, len: usize) -> TableState {
        if len > 0 {
            state.select(Some(0));
        }
        state
    }

    fn centered_rect(percent_x: u16, percent_y: u16, r: ratatui::layout::Rect) -> ratatui::layout::Rect {
        let popup_layout = Layout::vertical([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

        Layout::horizontal([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
    }

    fn entry_sort_key(left: &EntryInfo, right: &EntryInfo) -> std::cmp::Ordering {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| {
                priority_for_state(right.size_state).cmp(&priority_for_state(left.size_state))
            })
            .then_with(|| right.kind.can_enter().cmp(&left.kind.can_enter()))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    }

    fn format_entry_size(entry: &EntryInfo) -> String {
        match entry.size_state {
            SizeState::Pending => "scanning...".to_string(),
            SizeState::Cached => format!("{} (cached)", human_bytes(entry.size_bytes)),
            SizeState::Partial => format!("{}...", human_bytes(entry.size_bytes)),
            SizeState::Complete => human_bytes(entry.size_bytes),
        }
    }

    fn format_entry_percentage(size_bytes: u64, total_bytes: u64) -> String {
        if total_bytes == 0 || size_bytes == 0 {
            return "--".to_string();
        }

        let pct = (size_bytes as f64 / total_bytes as f64) * 100.0;
        if pct >= 99.95 {
            "100%".to_string()
        } else if pct >= 10.0 {
            format!("{:.1}%", pct)
        } else {
            format!("{:.2}%", pct)
        }
    }

    fn priority_for_state(state: SizeState) -> u8 {
        match state {
            SizeState::Complete => 3,
            SizeState::Cached => 2,
            SizeState::Partial => 1,
            SizeState::Pending => 0,
        }
    }

    fn spawn_scan(
        request_id: u64,
        path: PathBuf,
        sender: Sender<ScanEvent>,
        generation: Arc<AtomicU64>,
        cache_store: CacheStore,
    ) {
        thread::spawn(move || {
            let mut error_count = 0usize;
            let mut entries = Vec::new();
            let preload_cache_conn = cache_store.open_connection().ok();

            let read_dir = match fs::read_dir(&path) {
                Ok(read_dir) => read_dir,
                Err(error) => {
                    let _ = sender.send(ScanEvent::Failed {
                        request_id,
                        path,
                        error: error.to_string(),
                    });
                    return;
                }
            };

            for result in read_dir {
                if generation.load(Ordering::Relaxed) != request_id {
                    return;
                }

                match result {
                    Ok(dir_entry) => match build_entry(&dir_entry, preload_cache_conn.as_ref()) {
                        Ok(entry) => entries.push(entry),
                        Err(_) => error_count += 1,
                    },
                    Err(_) => error_count += 1,
                }
            }

            if generation.load(Ordering::Relaxed) != request_id {
                return;
            }

            let total_entries = entries.len();
            let _ = sender.send(ScanEvent::Started {
                request_id,
                path: path.clone(),
                total_entries,
            });

            let mut directories = Vec::new();
            for entry in entries {
                if entry.kind == EntryKind::Directory {
                    directories.push(DirectoryTask {
                        path: entry.path.clone(),
                        cached_hint: entry.size_state == SizeState::Cached,
                    });
                }

                let _ = sender.send(ScanEvent::Entry { request_id, entry });
            }

            if !directories.is_empty() {
                let worker_count = directories.len().min(MAX_SCAN_WORKERS);
                let queue = Arc::new(Mutex::new(VecDeque::from(directories)));
                let mut handles = Vec::with_capacity(worker_count);

                for _ in 0..worker_count {
                    let queue = Arc::clone(&queue);
                    let sender = sender.clone();
                    let generation = Arc::clone(&generation);
                    let cache_store = cache_store.clone();
                    handles.push(thread::spawn(move || {
                        scan_directory_queue(request_id, queue, sender, generation, cache_store)
                    }));
                }

                for handle in handles {
                    if generation.load(Ordering::Relaxed) != request_id {
                        return;
                    }

                    match handle.join() {
                        Ok(worker_errors) => error_count += worker_errors,
                        Err(_) => error_count += 1,
                    }
                }
            }

            if generation.load(Ordering::Relaxed) != request_id {
                return;
            }

            let _ = sender.send(ScanEvent::Finished {
                request_id,
                path,
                error_count,
            });
        });
    }

    fn build_entry(
        dir_entry: &fs::DirEntry,
        cache_conn: Option<&Connection>,
    ) -> io::Result<EntryInfo> {
        let path = dir_entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let name = dir_entry.file_name().to_string_lossy().to_string();
        let is_reparse_point = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let file_type = metadata.file_type();

        let (kind, size_bytes, size_state) = if file_type.is_dir() && !is_reparse_point {
            if let Some(conn) = cache_conn {
                if let Ok(Some(cached)) = load_cached_directory(conn, &path) {
                    (
                        EntryKind::Directory,
                        cached.recursive_size,
                        SizeState::Cached,
                    )
                } else {
                    (EntryKind::Directory, 0, SizeState::Pending)
                }
            } else {
                (EntryKind::Directory, 0, SizeState::Pending)
            }
        } else if file_type.is_file() {
            (EntryKind::File, metadata.len(), SizeState::Complete)
        } else if file_type.is_symlink() || is_reparse_point {
            (EntryKind::Link, 0, SizeState::Complete)
        } else {
            (EntryKind::Other, metadata.len(), SizeState::Complete)
        };

        Ok(EntryInfo {
            path,
            name,
            kind,
            size_bytes,
            size_state,
        })
    }

    fn scan_directory_queue(
        request_id: u64,
        queue: Arc<Mutex<VecDeque<DirectoryTask>>>,
        sender: Sender<ScanEvent>,
        generation: Arc<AtomicU64>,
        cache_store: CacheStore,
    ) -> usize {
        let mut error_count = 0usize;
        let cache_conn = cache_store.open_connection().ok();

        loop {
            if generation.load(Ordering::Relaxed) != request_id {
                return error_count;
            }

            let next_directory = {
                let mut queue = queue.lock().expect("directory scan queue poisoned");
                queue.pop_front()
            };

            let Some(directory) = next_directory else {
                return error_count;
            };

            error_count += compute_directory_size(
                &directory.path,
                directory.cached_hint,
                request_id,
                &generation,
                &sender,
                cache_conn.as_ref(),
            );
        }
    }

    fn compute_directory_size(
        root_path: &Path,
        cached_hint: bool,
        request_id: u64,
        generation: &Arc<AtomicU64>,
        sender: &Sender<ScanEvent>,
        cache_conn: Option<&Connection>,
    ) -> usize {
        let mut progress = DirectoryProgress::new();
        let allow_partial_updates = !cached_hint;
        let result = scan_directory_recursive(
            root_path,
            root_path,
            allow_partial_updates,
            request_id,
            generation,
            sender,
            &mut progress,
            cache_conn,
        );

        if generation.load(Ordering::Relaxed) != request_id {
            return result.error_count;
        }

        if allow_partial_updates {
            progress.emit(sender, request_id, root_path, SizeState::Complete);
        } else {
            let _ = sender.send(ScanEvent::SizeUpdated {
                request_id,
                path: root_path.to_path_buf(),
                size: result.size_bytes,
                state: SizeState::Complete,
            });
        }

        result.error_count
    }

    fn scan_directory_recursive(
        current_path: &Path,
        report_path: &Path,
        allow_partial_updates: bool,
        request_id: u64,
        generation: &Arc<AtomicU64>,
        sender: &Sender<ScanEvent>,
        progress: &mut DirectoryProgress,
        cache_conn: Option<&Connection>,
    ) -> DirectoryScanResult {
        if generation.load(Ordering::Relaxed) != request_id {
            return DirectoryScanResult {
                size_bytes: 0,
                error_count: 0,
            };
        }

        let listing = match enumerate_directory_listing(current_path, request_id, generation) {
            Ok(Some(listing)) => listing,
            Ok(None) => {
                return DirectoryScanResult {
                    size_bytes: 0,
                    error_count: 0,
                };
            }
            Err(_) => {
                return DirectoryScanResult {
                    size_bytes: 0,
                    error_count: 1,
                };
            }
        };

        let mut error_count = listing.error_count;
        if listing.cacheable {
            if let Some(conn) = cache_conn {
                if let Ok(Some(cached)) = load_cached_directory(conn, current_path) {
                    if cached.listing_fingerprint == listing.fingerprint {
                        if allow_partial_updates {
                            progress.add_bytes(cached.recursive_size);
                            progress.emit(sender, request_id, report_path, SizeState::Partial);
                        }

                        return DirectoryScanResult {
                            size_bytes: cached.recursive_size,
                            error_count,
                        };
                    }
                }
            }
        }

        let mut total_bytes = listing.direct_non_dir_bytes;
        if allow_partial_updates && total_bytes > 0 {
            progress.add_bytes(total_bytes);
            progress.emit(sender, request_id, report_path, SizeState::Partial);
        }

        for child_dir in listing.child_dirs {
            if generation.load(Ordering::Relaxed) != request_id {
                return DirectoryScanResult {
                    size_bytes: total_bytes,
                    error_count,
                };
            }

            let child_result = scan_directory_recursive(
                &child_dir,
                report_path,
                allow_partial_updates,
                request_id,
                generation,
                sender,
                progress,
                cache_conn,
            );
            total_bytes = total_bytes.saturating_add(child_result.size_bytes);
            error_count += child_result.error_count;
        }

        if listing.cacheable && error_count == 0 {
            if let Some(conn) = cache_conn {
                let _ =
                    save_cached_directory(conn, current_path, &listing.fingerprint, total_bytes);
            }
        }

        DirectoryScanResult {
            size_bytes: total_bytes,
            error_count,
        }
    }

    fn enumerate_directory_listing(
        current_path: &Path,
        request_id: u64,
        generation: &Arc<AtomicU64>,
    ) -> io::Result<Option<DirectoryListing>> {
        let read_dir = fs::read_dir(current_path)?;
        let mut direct_non_dir_bytes = 0u64;
        let mut child_dirs = Vec::new();
        let mut error_count = 0usize;
        let mut cacheable = true;
        let mut listing_entries = Vec::new();

        for result in read_dir {
            if generation.load(Ordering::Relaxed) != request_id {
                return Ok(None);
            }

            let dir_entry = match result {
                Ok(dir_entry) => dir_entry,
                Err(_) => {
                    error_count += 1;
                    cacheable = false;
                    continue;
                }
            };

            let path = dir_entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    error_count += 1;
                    cacheable = false;
                    continue;
                }
            };
            let name = dir_entry.file_name().to_string_lossy().to_string();
            let listing_entry = build_listing_entry(name, &metadata);

            if listing_entry.kind == EntryKind::Directory {
                child_dirs.push(path);
            } else {
                direct_non_dir_bytes =
                    direct_non_dir_bytes.saturating_add(listing_entry.size_bytes);
            }

            listing_entries.push(listing_entry);
        }

        let fingerprint = compute_listing_fingerprint(&mut listing_entries);
        Ok(Some(DirectoryListing {
            fingerprint,
            direct_non_dir_bytes,
            child_dirs,
            error_count,
            cacheable,
        }))
    }

    fn build_listing_entry(name: String, metadata: &fs::Metadata) -> ListingEntry {
        let file_type = metadata.file_type();
        let is_reparse_point = metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let (kind, size_bytes) = if file_type.is_dir() && !is_reparse_point {
            (EntryKind::Directory, 0)
        } else if file_type.is_file() {
            (EntryKind::File, metadata.len())
        } else if file_type.is_symlink() || is_reparse_point {
            (EntryKind::Link, 0)
        } else {
            (EntryKind::Other, metadata.len())
        };

        ListingEntry {
            name,
            kind,
            size_bytes,
            last_write_time: metadata.last_write_time(),
            creation_time: metadata.creation_time(),
            file_attributes: metadata.file_attributes(),
        }
    }

    fn compute_listing_fingerprint(entries: &mut [ListingEntry]) -> String {
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.kind.label().cmp(right.kind.label()))
        });

        let mut hasher = blake3::Hasher::new();
        hasher.update(b"listing-fingerprint-v1");

        for entry in entries {
            hasher.update(entry.name.as_bytes());
            hasher.update(&[0]);
            hasher.update(entry.kind.label().as_bytes());
            hasher.update(&[0]);
            hasher.update(&entry.size_bytes.to_le_bytes());
            hasher.update(&entry.last_write_time.to_le_bytes());
            hasher.update(&entry.creation_time.to_le_bytes());
            hasher.update(&entry.file_attributes.to_le_bytes());
        }

        hasher.finalize().to_hex().to_string()
    }

    fn load_cached_directory(
        conn: &Connection,
        path: &Path,
    ) -> io::Result<Option<CachedDirectory>> {
        let path_key = normalize_path_key(path);
        conn.query_row(
            "
            SELECT listing_fingerprint, recursive_size
            FROM directory_cache
            WHERE path_key = ?1
            ",
            params![path_key],
            |row| {
                let listing_fingerprint: String = row.get(0)?;
                let recursive_size: i64 = row.get(1)?;
                Ok(CachedDirectory {
                    listing_fingerprint,
                    recursive_size: recursive_size as u64,
                })
            },
        )
        .optional()
        .map_err(sqlite_err_to_io)
    }

    fn save_cached_directory(
        conn: &Connection,
        path: &Path,
        fingerprint: &str,
        size_bytes: u64,
    ) -> io::Result<()> {
        let path_key = normalize_path_key(path);
        let display_path = path.to_string_lossy().to_string();
        let recursive_size = i64::try_from(size_bytes).map_err(|_| {
            io::Error::other(format!(
                "directory size for {} exceeds SQLite integer range",
                path.display()
            ))
        })?;

        conn.execute(
            "
            INSERT INTO directory_cache (
                path_key,
                display_path,
                listing_fingerprint,
                recursive_size,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(path_key) DO UPDATE SET
                display_path = excluded.display_path,
                listing_fingerprint = excluded.listing_fingerprint,
                recursive_size = excluded.recursive_size,
                updated_at = excluded.updated_at
            ",
            params![
                path_key,
                display_path,
                fingerprint,
                recursive_size,
                current_unix_timestamp()
            ],
        )
        .map_err(sqlite_err_to_io)?;

        Ok(())
    }

    fn normalize_path_key(path: &Path) -> String {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    }

    fn current_unix_timestamp() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default()
    }

    fn sqlite_err_to_io(error: rusqlite::Error) -> io::Error {
        io::Error::other(error)
    }

    fn discover_drives() -> io::Result<Vec<DriveInfo>> {
        let mask = unsafe { GetLogicalDrives() };
        if mask == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut drives = Vec::new();
        for index in 0..26 {
            if mask & (1 << index) == 0 {
                continue;
            }

            let letter = char::from(b'A' + index as u8);
            let root_str = format!("{letter}:\\");
            let root_wide = wide_null(&root_str);
            let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
            if matches!(drive_type, DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR) {
                continue;
            }

            let mut free_for_caller = 0u64;
            let mut total_bytes = 0u64;
            let mut total_free = 0u64;

            let has_space = unsafe {
                GetDiskFreeSpaceExW(
                    root_wide.as_ptr(),
                    &mut free_for_caller,
                    &mut total_bytes,
                    &mut total_free,
                )
            };
            if has_space == 0 {
                continue;
            }

            drives.push(DriveInfo {
                root: PathBuf::from(&root_str),
                name: format!("{letter}:"),
                label: volume_label(&root_wide),
                drive_type: drive_type_label(drive_type),
                total_bytes,
                free_bytes: total_free,
            });
        }

        drives.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(drives)
    }

    fn volume_label(root: &[u16]) -> Option<String> {
        let mut name_buffer = [0u16; 260];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                name_buffer.as_mut_ptr(),
                name_buffer.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };

        if ok == 0 {
            return None;
        }

        let length = name_buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(name_buffer.len());

        if length == 0 {
            None
        } else {
            Some(String::from_utf16_lossy(&name_buffer[..length]))
        }
    }

    fn drive_type_label(drive_type: u32) -> &'static str {
        match drive_type {
            DRIVE_FIXED => "fixed",
            DRIVE_REMOVABLE => "remov.",
            DRIVE_REMOTE => "network",
            DRIVE_CDROM => "optical",
            DRIVE_RAMDISK => "ramdisk",
            _ => "other",
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn human_bytes(bytes: u64) -> String {
        const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

        if bytes < 1024 {
            return format!("{bytes} B");
        }

        let mut value = bytes as f64;
        let mut index = 0usize;
        while value >= 1024.0 && index < UNITS.len() - 1 {
            value /= 1024.0;
            index += 1;
        }

        if value >= 100.0 {
            format!("{value:.0} {}", UNITS[index])
        } else if value >= 10.0 {
            format!("{value:.1} {}", UNITS[index])
        } else {
            format!("{value:.2} {}", UNITS[index])
        }
    }

    struct TerminalGuard;

    impl TerminalGuard {
        fn enter() -> AppResult<Self> {
            enable_raw_mode()?;
            execute!(io::stdout(), EnterAlternateScreen)?;
            Ok(Self)
        }
    }

    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }

    pub fn main() -> AppResult<()> {
        let _guard = TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        let mut app = App::new()?;
        let result = app.run(&mut terminal);
        terminal.show_cursor()?;
        result
    }

    #[cfg(test)]
    mod tests {
        use super::human_bytes;

        #[test]
        fn formats_small_bytes() {
            assert_eq!(human_bytes(999), "999 B");
            assert_eq!(human_bytes(1024), "1.00 KB");
        }

        #[test]
        fn formats_large_bytes() {
            assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MB");
            assert_eq!(human_bytes(250 * 1024 * 1024 * 1024), "250 GB");
        }
    }
}

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_app::main()
}
