use crate::opencode::{
    ModelChoice, OpencodeClient, OpencodeServer, default_model_choice, pick_default_model,
};
use crate::platform::{
    self, DriveInfo, ScanPolicy, discover_drives, is_link, metadata_fingerprint, normalize_path_key,
};

use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    io::{self, BufRead},
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
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

const SPINNER: [&str; 8] = ["|", "/", "-", "\\", "|", "/", "-", "\\"];
const MAX_SCAN_WORKERS: usize = 4;
const UPDATE_BYTES_THRESHOLD: u64 = 32 * 1024 * 1024;
const UPDATE_INTERVAL: Duration = Duration::from_millis(200);
const CACHE_DB_FILE: &str = "storage_analytics_cache.sqlite3";
const CHAT_CONTEXT_MARKER: &str = "[storage-analytics-context]";
const CHAT_CONTEXT_ENTRY_LIMIT: usize = 80;
const CHAT_SCROLL_STEP: isize = 3;

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

#[derive(Clone, Debug, Eq, PartialEq)]
enum EntryKind {
    Directory,
    File,
    Link,
    Mount,
    Virtual,
    Other,
}

impl EntryKind {
    fn label(&self) -> &'static str {
        match self {
            EntryKind::Directory => "dir",
            EntryKind::File => "file",
            EntryKind::Link => "link",
            EntryKind::Mount => "mount",
            EntryKind::Virtual => "virtual",
            EntryKind::Other => "other",
        }
    }

    fn can_enter(&self) -> bool {
        matches!(self, EntryKind::Directory | EntryKind::Mount)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Explorer,
    Chat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatRole {
    User,
    Assistant,
    System,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatPartKind {
    Text,
    Reasoning,
    File,
    Tool,
}

#[derive(Clone, Debug)]
struct ChatTool {
    name: String,
    status: String,
    title: String,
    error: Option<String>,
}

#[derive(Clone, Debug)]
struct ChatPart {
    id: String,
    kind: ChatPartKind,
    text: String,
    tool: Option<ChatTool>,
}

#[derive(Clone, Debug)]
struct ChatMessage {
    id: String,
    role: ChatRole,
    parts: Vec<ChatPart>,
    error: Option<String>,
    completed: bool,
}

struct PermissionPrompt {
    id: String,
    permission: String,
    patterns: Vec<String>,
}

struct ModelPicker {
    filter: String,
    selected: usize,
}

enum ChatPhase {
    Starting,
    Ready,
    Failed,
}

struct ChatRuntime {
    _server: OpencodeServer,
    client: OpencodeClient,
    session_id: String,
}

struct RuntimeReadyData {
    server: OpencodeServer,
    client: OpencodeClient,
    session_id: String,
    version: String,
    models: Vec<ModelChoice>,
    selected_model: Option<ModelChoice>,
    warning: Option<String>,
}

struct ChatState {
    phase: ChatPhase,
    runtime: Option<ChatRuntime>,
    models: Vec<ModelChoice>,
    selected_model: Option<ModelChoice>,
    messages: Vec<ChatMessage>,
    input: String,
    input_cursor: usize,
    busy: bool,
    status: String,
    stick_to_bottom: bool,
    scroll: usize,
    max_scroll: usize,
    permission: Option<PermissionPrompt>,
    permission_queue: VecDeque<PermissionPrompt>,
    model_picker: Option<ModelPicker>,
    stream_connected: bool,
    session_cost: f64,
    session_tokens: u64,
    pending_echo_id: Option<String>,
    pending_prompt_text: String,
    show_reasoning: bool,
    next_local_id: u64,
}

impl ChatState {
    fn new() -> Self {
        Self {
            phase: ChatPhase::Starting,
            runtime: None,
            models: Vec::new(),
            selected_model: None,
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            busy: false,
            status: "Starting OpenCode...".to_string(),
            stick_to_bottom: true,
            scroll: 0,
            max_scroll: 0,
            permission: None,
            permission_queue: VecDeque::new(),
            model_picker: None,
            stream_connected: false,
            session_cost: 0.0,
            session_tokens: 0,
            pending_echo_id: None,
            pending_prompt_text: String::new(),
            show_reasoning: true,
            next_local_id: 0,
        }
    }

    fn push_notice(&mut self, text: impl Into<String>) {
        self.push_local_message(ChatRole::System, text.into());
    }

    fn push_error(&mut self, text: impl Into<String>) {
        self.push_local_message(ChatRole::Error, text.into());
    }

    fn push_local_message(&mut self, role: ChatRole, text: String) {
        self.next_local_id += 1;
        self.messages.push(ChatMessage {
            id: format!("local-{}", self.next_local_id),
            role,
            parts: vec![ChatPart {
                id: format!("local-part-{}", self.next_local_id),
                kind: ChatPartKind::Text,
                text,
                tool: None,
            }],
            error: None,
            completed: true,
        });
        self.stick_to_bottom = true;
    }

    fn push_user_echo(&mut self, text: String) -> String {
        self.next_local_id += 1;
        let id = format!("local-echo-{}", self.next_local_id);
        self.messages.push(ChatMessage {
            id: id.clone(),
            role: ChatRole::User,
            parts: vec![ChatPart {
                id: format!("local-echo-part-{}", self.next_local_id),
                kind: ChatPartKind::Text,
                text,
                tool: None,
            }],
            error: None,
            completed: true,
        });
        self.stick_to_bottom = true;
        id
    }

    fn message_mut(&mut self, id: &str, role: ChatRole) -> &mut ChatMessage {
        if let Some(index) = self.messages.iter().position(|message| message.id == id) {
            return &mut self.messages[index];
        }

        self.messages.push(ChatMessage {
            id: id.to_string(),
            role,
            parts: Vec::new(),
            error: None,
            completed: false,
        });
        self.messages.last_mut().expect("message was just pushed")
    }

    fn apply_message_updated(&mut self, info: &Value) {
        let Some(id) = info.get("id").and_then(Value::as_str) else {
            return;
        };

        let role = match info.get("role").and_then(Value::as_str) {
            Some("user") => ChatRole::User,
            _ => ChatRole::Assistant,
        };

        if role == ChatRole::User {
            if let Some(echo_id) = self.pending_echo_id.take() {
                self.messages.retain(|message| message.id != echo_id);
            }
            self.pending_prompt_text.clear();
        }

        let error = error_message(info.get("error"));
        let completed = info
            .get("time")
            .and_then(|time| time.get("completed"))
            .is_some_and(|value| !value.is_null());

        let message = self.message_mut(id, role);
        message.role = role;
        message.error = error;
        message.completed = completed;
    }

    fn apply_part_updated(&mut self, part: &Value) {
        let Some(id) = part.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(message_id) = part.get("messageID").and_then(Value::as_str) else {
            return;
        };

        let Some(part_kind) = part.get("type").and_then(Value::as_str) else {
            return;
        };

        let (kind, text, tool) = match part_kind {
            "text" => (
                ChatPartKind::Text,
                part.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                None,
            ),
            "reasoning" => (
                ChatPartKind::Reasoning,
                part.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                None,
            ),
            "file" => (
                ChatPartKind::File,
                part.get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("attachment")
                    .to_string(),
                None,
            ),
            "tool" => {
                let state = part.get("state").unwrap_or(&Value::Null);
                let name = part
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let status = state
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_string();
                let title = state
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| {
                        tool_input_summary(state.get("input").unwrap_or(&Value::Null))
                    });
                let error = state
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                (
                    ChatPartKind::Tool,
                    String::new(),
                    Some(ChatTool {
                        name,
                        status,
                        title,
                        error,
                    }),
                )
            }
            _ => return,
        };

        let message = self.message_mut(message_id, ChatRole::Assistant);
        if let Some(existing) = message.parts.iter_mut().find(|part| part.id == id) {
            existing.kind = kind;
            existing.text = text;
            existing.tool = tool;
        } else {
            message.parts.push(ChatPart {
                id: id.to_string(),
                kind,
                text,
                tool,
            });
        }
    }

    fn apply_part_delta(&mut self, properties: &Value) {
        let Some(message_id) = properties.get("messageID").and_then(Value::as_str) else {
            return;
        };
        let Some(part_id) = properties.get("partID").and_then(Value::as_str) else {
            return;
        };
        let Some(delta) = properties.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }

        let kind = match properties.get("field").and_then(Value::as_str) {
            Some("text") => ChatPartKind::Text,
            Some("reasoning") => ChatPartKind::Reasoning,
            _ => return,
        };

        let message = self.message_mut(message_id, ChatRole::Assistant);
        if let Some(existing) = message.parts.iter_mut().find(|part| part.id == part_id) {
            existing.text.push_str(delta);
        } else {
            message.parts.push(ChatPart {
                id: part_id.to_string(),
                kind,
                text: delta.to_string(),
                tool: None,
            });
        }
    }

    fn apply_part_removed(&mut self, properties: &Value) {
        let part_id = properties
            .get("partID")
            .and_then(Value::as_str)
            .or_else(|| properties.get("id").and_then(Value::as_str));
        let Some(part_id) = part_id else {
            return;
        };

        for message in &mut self.messages {
            message.parts.retain(|part| part.id != part_id);
        }
    }

    fn remove_message(&mut self, properties: &Value) {
        let Some(id) = properties
            .get("messageID")
            .and_then(Value::as_str)
            .or_else(|| properties.get("id").and_then(Value::as_str))
        else {
            return;
        };

        self.messages.retain(|message| message.id != id);
    }

    fn update_usage(&mut self, info: &Value) {
        if let Some(cost) = info.get("cost").and_then(Value::as_f64) {
            self.session_cost = cost;
        }
        if let Some(tokens) = info.get("tokens") {
            let total = tokens.get("total").and_then(Value::as_u64).or_else(|| {
                let input = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
                let output = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
                let reasoning = tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0);
                Some(input + output + reasoning)
            });
            if let Some(total) = total {
                self.session_tokens = total;
            }
        }
    }

    fn enqueue_permission(&mut self, properties: &Value) {
        let Some(id) = properties.get("id").and_then(Value::as_str) else {
            return;
        };

        let permission = properties
            .get("permission")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let patterns = properties
            .get("patterns")
            .and_then(Value::as_array)
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let prompt = PermissionPrompt {
            id: id.to_string(),
            permission,
            patterns,
        };

        self.model_picker = None;

        if self.permission.is_some() {
            self.permission_queue.push_back(prompt);
        } else {
            self.permission = Some(prompt);
        }
    }

    fn activate_next_permission(&mut self) {
        self.permission = self.permission_queue.pop_front();
    }

    fn reset_for_new_session(&mut self) {
        self.messages.clear();
        self.busy = false;
        self.permission = None;
        self.permission_queue.clear();
        self.model_picker = None;
        self.pending_echo_id = None;
        self.pending_prompt_text.clear();
        self.session_cost = 0.0;
        self.session_tokens = 0;
        self.scroll = 0;
        self.stick_to_bottom = true;
    }

    fn selected_model_label(&self) -> String {
        self.selected_model
            .as_ref()
            .map(ModelChoice::key)
            .unwrap_or_else(|| "default model".to_string())
    }

    fn phase_is_ready(&self) -> bool {
        matches!(self.phase, ChatPhase::Ready)
    }

    fn filtered_model_indices(&self, filter: &str) -> Vec<usize> {
        self.models
            .iter()
            .enumerate()
            .filter(|(_, model)| model.matches_filter(filter))
            .map(|(index, _)| index)
            .collect()
    }
}

enum ChatEvent {
    RuntimeReady(Box<RuntimeReadyData>),
    RuntimeFailed(String),
    StreamConnected,
    StreamDisconnected(String),
    BusEvent(Value),
    PromptFailed(String),
    AbortFinished(Result<(), String>),
    PermissionReplyFinished { result: Result<(), String> },
    QuestionRejectFinished { result: Result<(), String> },
    SessionCreated(String),
    SessionCreateFailed(String),
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
    focus: Focus,
    chat: ChatState,
    chat_tx: Sender<ChatEvent>,
    chat_rx: Receiver<ChatEvent>,
    chat_init_in_flight: bool,
    explorer_area: Rect,
    chat_area: Rect,
    chat_transcript_area: Rect,
    chat_input_area: Rect,
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
    name: OsString,
    kind: EntryKind,
    size_bytes: u64,
    metadata_fingerprint: [u64; 8],
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

    fn emit(&mut self, sender: &Sender<ScanEvent>, request_id: u64, path: &Path, state: SizeState) {
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
    fn new(initial_path: Option<PathBuf>) -> AppResult<Self> {
        let base_dir = std::env::current_dir()?;
        let cache_store = CacheStore::new(&base_dir)?;
        let drives = discover_drives()?;
        let drives_state = select_first(TableState::default(), drives.len());
        let (scanner_tx, scanner_rx) = mpsc::channel();
        let (deletion_tx, deletion_rx) = mpsc::channel();
        let (chat_tx, chat_rx) = mpsc::channel();
        let status = if drives.is_empty() {
            "No accessible drives were found.".to_string()
        } else {
            "Select a drive and press Enter.".to_string()
        };

        let empty_area = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };

        let mut app = Self {
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
            focus: Focus::Explorer,
            chat: ChatState::new(),
            chat_tx,
            chat_rx,
            chat_init_in_flight: false,
            explorer_area: empty_area,
            chat_area: empty_area,
            chat_transcript_area: empty_area,
            chat_input_area: empty_area,
        };
        if let Some(path) = initial_path {
            app.current_root = Some(path.clone());
            app.navigate_to(path);
        }
        app.start_chat_runtime();
        Ok(app)
    }

    fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> AppResult<()> {
        loop {
            self.process_scan_events();
            self.process_deletion_events();
            self.process_chat_events();
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(150))? {
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Press && !self.handle_key(key)? {
                            return Ok(());
                        }
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            } else if self.scan_progress.scanning || self.is_deleting || self.chat.busy {
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
                format!("Storage Analytics - {}", platform::display_path(path))
            }
            (View::Directory, None) => "Storage Analytics - Directory".to_string(),
        };

        let title_block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title));
        let summary = Paragraph::new(self.summary_line()).block(title_block);
        frame.render_widget(summary, areas[0]);

        let panes = Layout::horizontal([
            Constraint::Percentage(60),
            Constraint::Length(1),
            Constraint::Percentage(40),
        ])
        .split(areas[1]);

        self.explorer_area = panes[0];
        self.chat_area = panes[2];

        let explorer_focused = self.focus == Focus::Explorer;
        match self.view {
            View::Drives => self.draw_drives(frame, panes[0], explorer_focused),
            View::Directory => self.draw_entries(frame, panes[0], explorer_focused),
        }

        self.draw_chat(frame, panes[2]);

        let footer = Paragraph::new(self.footer_line())
            .block(Block::default().borders(Borders::ALL).title("Help"));
        frame.render_widget(footer, areas[2]);

        if self.pending_delete.is_some() {
            self.draw_delete_modal(frame);
        }
        self.draw_model_picker(frame);
        self.draw_permission_modal(frame);
    }

    fn draw_drives(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, focused: bool) {
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
                Constraint::Length(6),
                Constraint::Percentage(35),
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
        )
        .header(
            Row::new(["Drive", "Label", "Type", "Used", "Free", "Total"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Connected Drives")
                .border_style(focus_border_style(focused)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        let mut state = self.drives_state.clone();
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn draw_entries(&self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, focused: bool) {
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
                Constraint::Length(6),
                Constraint::Percentage(57),
                Constraint::Length(16),
                Constraint::Length(8),
            ],
        )
        .header(
            Row::new(["Type", "Name", "Size", "%"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Folder Contents")
                .border_style(focus_border_style(focused)),
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        let mut state = self.entries_state.clone();
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn draw_chat(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let focused = self.focus == Focus::Chat;
        let spinner = if self.chat.busy {
            format!(" {}", SPINNER[self.spinner_index % SPINNER.len()])
        } else {
            String::new()
        };
        let status = if self.chat.status.is_empty() {
            String::new()
        } else {
            format!(" - {}", truncate_text(&self.chat.status, 28))
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(format!(" AI Chat{spinner}{status} ")))
            .border_style(focus_border_style(focused));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let sections = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(inner);

        self.chat_transcript_area = sections[0];
        self.chat_input_area = sections[1];

        self.draw_chat_transcript(frame, sections[0]);
        self.draw_chat_input(frame, sections[1], focused);
        self.draw_chat_status(frame, sections[2]);
    }

    fn draw_chat_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let width = area.width.max(1) as usize;
        let lines = self.chat_lines(width);
        let height = area.height as usize;
        let max_scroll = lines.len().saturating_sub(height);
        self.chat.max_scroll = max_scroll;

        if self.chat.stick_to_bottom {
            self.chat.scroll = max_scroll;
        } else {
            self.chat.scroll = self.chat.scroll.min(max_scroll);
        }

        let scroll = self.chat.scroll.min(u16::MAX as usize) as u16;
        let paragraph = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }

    fn chat_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut lines = Vec::new();

        if self.chat.messages.is_empty() {
            lines.push(Line::from(Span::styled(
                "Ask the assistant about the folder you are viewing.",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "The current view, selected item, and visible sizes are attached to every prompt.",
                Style::default().fg(Color::DarkGray),
            )));
            return lines;
        }

        for (index, message) in self.chat.messages.iter().enumerate() {
            if index > 0 {
                lines.push(Line::from(""));
            }

            let (header, header_style) = match message.role {
                ChatRole::User => (
                    "You",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                ChatRole::Assistant => (
                    "Assistant",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                ChatRole::System => ("Notice", Style::default().fg(Color::Yellow)),
                ChatRole::Error => (
                    "Error",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
            };
            lines.push(Line::from(Span::styled(header, header_style)));

            let mut had_visible_part = false;
            for part in &message.parts {
                match part.kind {
                    ChatPartKind::Text => {
                        if message.role == ChatRole::User
                            && part.text.starts_with(CHAT_CONTEXT_MARKER)
                        {
                            continue;
                        }
                        if part.text.is_empty() {
                            continue;
                        }

                        for wrapped in wrap_text(&part.text, width) {
                            lines.push(Line::from(wrapped));
                        }
                        had_visible_part = true;
                    }
                    ChatPartKind::Reasoning => {
                        if !self.chat.show_reasoning || part.text.is_empty() {
                            continue;
                        }

                        let text = format!("[reasoning] {}", part.text);
                        let style = Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC);
                        for wrapped in wrap_text(&text, width) {
                            lines.push(Line::from(Span::styled(wrapped, style)));
                        }
                        had_visible_part = true;
                    }
                    ChatPartKind::Tool => {
                        let Some(tool) = &part.tool else {
                            continue;
                        };

                        let text = format_tool_line(tool);
                        for wrapped in wrap_text(&text, width) {
                            lines.push(Line::from(Span::styled(wrapped, tool_style(&tool.status))));
                        }
                        had_visible_part = true;
                    }
                    ChatPartKind::File => {
                        let text = format!("[file] {}", part.text);
                        for wrapped in wrap_text(&text, width) {
                            lines.push(Line::from(Span::styled(
                                wrapped,
                                Style::default().fg(Color::Cyan),
                            )));
                        }
                        had_visible_part = true;
                    }
                }
            }

            if let Some(error) = &message.error {
                for wrapped in wrap_text(&format!("error: {error}"), width) {
                    lines.push(Line::from(Span::styled(
                        wrapped,
                        Style::default().fg(Color::Red),
                    )));
                }
                had_visible_part = true;
            }

            let is_last_message = index + 1 == self.chat.messages.len();
            if !had_visible_part
                && message.role == ChatRole::Assistant
                && self.chat.busy
                && is_last_message
            {
                let spinner = SPINNER[self.spinner_index % SPINNER.len()];
                lines.push(Line::from(Span::styled(
                    format!("working... {spinner}"),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        lines
    }

    fn draw_chat_input(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Message")
            .border_style(focus_border_style(focused));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let width = inner.width.max(1) as usize;
        let (display, empty) = if self.chat.input.is_empty() {
            if focused {
                ("▌ Ask about the selected folder...".to_string(), true)
            } else {
                ("Ask about the selected folder...".to_string(), true)
            }
        } else {
            let (before, after) = split_at_char(&self.chat.input, self.chat.input_cursor);
            let cursor = if focused { "▌" } else { "" };
            (format!("{before}{cursor}{after}"), false)
        };

        let style = if empty {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        let lines = wrap_text(&display, width);
        let visible = inner.height.max(1) as usize;
        let scroll = lines.len().saturating_sub(visible).min(u16::MAX as usize) as u16;
        let lines = lines.into_iter().map(Line::from).collect::<Vec<_>>();
        let paragraph = Paragraph::new(lines).style(style).scroll((scroll, 0));
        frame.render_widget(paragraph, inner);
    }

    fn draw_chat_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let phase = match &self.chat.phase {
            ChatPhase::Starting => "starting",
            ChatPhase::Ready => "ready",
            ChatPhase::Failed => "offline",
        };

        let busy = if self.chat.busy {
            format!(" | {} working", SPINNER[self.spinner_index % SPINNER.len()])
        } else {
            String::new()
        };
        let stream = if self.chat.phase_is_ready() && !self.chat.stream_connected {
            " | reconnecting"
        } else {
            ""
        };
        let permission = if self.chat.permission.is_some() {
            " | permission needed"
        } else {
            ""
        };
        let usage = if self.chat.session_tokens > 0 {
            format!(
                " | {} tokens | ${:.4}",
                compact_tokens(self.chat.session_tokens),
                self.chat.session_cost
            )
        } else {
            String::new()
        };

        let text = format!(
            " {phase}{busy}{stream}{permission} | {}{usage}",
            self.chat.selected_model_label()
        );
        let style = if permission.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Yellow)
        };

        frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
    }

    fn draw_permission_modal(&self, frame: &mut Frame<'_>) {
        let Some(permission) = &self.chat.permission else {
            return;
        };

        let area = centered_rect(70, 40, frame.area());
        frame.render_widget(Clear, area);

        let mut lines = vec![
            Line::from(Span::styled(
                "The assistant requested permission to perform an action.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("Action: "),
                Span::styled(&permission.permission, Style::default().fg(Color::Yellow)),
            ]),
            Line::from(""),
            Line::from("Scope:"),
        ];

        if permission.patterns.is_empty() {
            lines.push(Line::from("  (not specified)"));
        } else {
            for pattern in &permission.patterns {
                lines.push(Line::from(format!("  {pattern}")));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "y allow once | a always allow | n or Esc reject",
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .title(" Permission Required ")
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn draw_model_picker(&self, frame: &mut Frame<'_>) {
        let Some(picker) = &self.chat.model_picker else {
            return;
        };

        let area = centered_rect(60, 70, frame.area());
        frame.render_widget(Clear, area);

        let block = Block::default()
            .title(" Select Model ")
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let sections = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

        let filter_display = if picker.filter.is_empty() {
            "type to filter models".to_string()
        } else {
            picker.filter.clone()
        };
        let filter_style = if picker.filter.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::raw("Filter: "),
                    Span::styled(filter_display, filter_style),
                ]),
                Line::from(""),
            ]),
            sections[0],
        );

        let filtered = self.chat.filtered_model_indices(&picker.filter);
        let visible = sections[1].height as usize;
        let selected = picker.selected.min(filtered.len().saturating_sub(1));
        let start = if visible == 0 {
            0
        } else {
            selected.saturating_sub(visible.saturating_sub(1))
        };

        let mut lines = Vec::new();
        if filtered.is_empty() {
            lines.push(Line::from(Span::styled(
                "No models match the filter.",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (position, index) in filtered.iter().enumerate().skip(start).take(visible) {
                let model = &self.chat.models[*index];
                let marker = if position == selected { ">" } else { " " };
                let current = if self.chat.selected_model.as_ref() == Some(model) {
                    " *"
                } else {
                    ""
                };
                let text = format!("{marker} {}{}", model.display(), current);
                let line = if position == selected {
                    Line::from(text).style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    Line::from(text)
                };
                lines.push(line);
            }
        }
        frame.render_widget(Paragraph::new(lines), sections[1]);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter select | Esc cancel | type to filter",
                Style::default().fg(Color::DarkGray),
            ))),
            sections[2],
        );
    }

    fn draw_delete_modal(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let popup_area = centered_rect(60, 25, area);

        frame.render_widget(Clear, popup_area);

        let name = self
            .pending_delete
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| platform::display_text(&n.to_string_lossy()))
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
                format!(
                    "Are you sure you want to delete this {}?\n\n{}",
                    item_type, name
                ),
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
            let hint = Paragraph::new(hint_text).alignment(Alignment::Center);
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
        let selected_hint = match self.focus {
            Focus::Chat => {
                "Enter send | Shift+Enter newline | Tab back | Ctrl+M model | Ctrl+N new | Ctrl+C abort | Ctrl+Q quit"
            }
            Focus::Explorer => match self.view {
                View::Drives => "Arrows/jk move | Enter open | r refresh | Tab chat | q quit",
                View::Directory => {
                    "Arrows/jk move | Enter open | Del delete | Bksp/Esc up | r rescan | d drives | Tab chat | q quit"
                }
            },
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

        self.scan_progress.resolved_entries.saturating_mul(100) / self.scan_progress.total_entries
    }

    fn refresh_scan_status(&mut self) {
        let Some(path) = self.current_path.as_ref() else {
            return;
        };

        if !self.scan_progress.scanning {
            return;
        }

        if !self.scan_progress.started {
            self.status = format!("Reading entries in {}...", platform::display_path(path));
            return;
        }

        self.status = format!(
            "Scanning {}... {}% ({}/{}) resolved at this level",
            platform::display_path(path),
            self.scan_percent(),
            self.scan_progress.resolved_entries,
            self.scan_progress.total_entries
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> AppResult<bool> {
        if self.is_deleting {
            return Ok(true);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return Ok(false);
        }

        if self.chat.permission.is_some() {
            self.handle_permission_key(key);
            return Ok(true);
        }

        if self.pending_delete.is_some() {
            match key.code {
                KeyCode::Enter => self.confirm_delete(),
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('q') => self.cancel_delete(),
                _ => {}
            }
            return Ok(true);
        }

        if self.chat.model_picker.is_some() {
            self.handle_model_picker_key(key);
            return Ok(true);
        }

        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.toggle_focus();
            return Ok(true);
        }

        if self.focus == Focus::Chat {
            self.handle_chat_key(key);
            return Ok(true);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return Ok(true);
        }

        match key.code {
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

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Explorer => Focus::Chat,
            Focus::Chat => Focus::Explorer,
        };
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.pending_delete.is_some()
            || self.chat.permission.is_some()
            || self.chat.model_picker.is_some()
        {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if rect_contains(self.chat_area, mouse.column, mouse.row) {
                    self.focus = Focus::Chat;
                } else if rect_contains(self.explorer_area, mouse.column, mouse.row) {
                    self.focus = Focus::Explorer;
                }
            }
            MouseEventKind::ScrollUp => {
                if rect_contains(self.chat_area, mouse.column, mouse.row) {
                    self.scroll_chat(-CHAT_SCROLL_STEP);
                } else if rect_contains(self.explorer_area, mouse.column, mouse.row) {
                    self.move_selection(-CHAT_SCROLL_STEP);
                }
            }
            MouseEventKind::ScrollDown => {
                if rect_contains(self.chat_area, mouse.column, mouse.row) {
                    self.scroll_chat(CHAT_SCROLL_STEP);
                } else if rect_contains(self.explorer_area, mouse.column, mouse.row) {
                    self.move_selection(CHAT_SCROLL_STEP);
                }
            }
            _ => {}
        }
    }

    fn handle_chat_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => self.abort_chat(),
                KeyCode::Char('m') | KeyCode::Enter => self.open_model_picker(),
                KeyCode::Char('n') => self.start_new_chat_session(),
                KeyCode::Char('r') => self.retry_chat_runtime(),
                KeyCode::Char('t') => {
                    self.chat.show_reasoning = !self.chat.show_reasoning;
                    self.chat.status = if self.chat.show_reasoning {
                        "Reasoning is shown.".to_string()
                    } else {
                        "Reasoning is hidden.".to_string()
                    };
                }
                KeyCode::Home => self.scroll_chat_to_start(),
                KeyCode::End => self.scroll_chat_to_end(),
                _ => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.chat_insert_char('\n');
                } else {
                    self.send_chat_prompt();
                }
            }
            KeyCode::Esc => self.focus = Focus::Explorer,
            KeyCode::Up => self.scroll_chat(-1),
            KeyCode::Down => self.scroll_chat(1),
            KeyCode::PageUp => {
                let page = self.chat_transcript_area.height.max(1) as isize;
                self.scroll_chat(-page);
            }
            KeyCode::PageDown => {
                let page = self.chat_transcript_area.height.max(1) as isize;
                self.scroll_chat(page);
            }
            KeyCode::Left => self.chat_move_cursor(-1),
            KeyCode::Right => self.chat_move_cursor(1),
            KeyCode::Home => self.chat_move_cursor_to(0),
            KeyCode::End => {
                let end = self.chat.input.chars().count();
                self.chat_move_cursor_to(end);
            }
            KeyCode::Backspace => self.chat_backspace(),
            KeyCode::Delete => self.chat_delete(),
            KeyCode::Char(c) => self.chat_insert_char(c),
            _ => {}
        }
    }

    fn scroll_chat(&mut self, delta: isize) {
        if self.chat.max_scroll == 0 {
            self.chat.scroll = 0;
            self.chat.stick_to_bottom = true;
            return;
        }

        let max = self.chat.max_scroll as isize;
        let next = (self.chat.scroll as isize + delta).clamp(0, max);
        self.chat.scroll = next as usize;
        self.chat.stick_to_bottom = next as usize >= self.chat.max_scroll;
    }

    fn scroll_chat_to_start(&mut self) {
        self.chat.scroll = 0;
        self.chat.stick_to_bottom = false;
    }

    fn scroll_chat_to_end(&mut self) {
        self.chat.scroll = self.chat.max_scroll;
        self.chat.stick_to_bottom = true;
    }

    fn chat_move_cursor(&mut self, delta: isize) {
        let length = self.chat.input.chars().count();
        let next = (self.chat.input_cursor as isize + delta).clamp(0, length as isize);
        self.chat.input_cursor = next as usize;
    }

    fn chat_move_cursor_to(&mut self, index: usize) {
        let length = self.chat.input.chars().count();
        self.chat.input_cursor = index.min(length);
    }

    fn chat_insert_char(&mut self, character: char) {
        let byte_index = char_to_byte_index(&self.chat.input, self.chat.input_cursor);
        self.chat.input.insert(byte_index, character);
        self.chat.input_cursor += 1;
    }

    fn chat_backspace(&mut self) {
        if self.chat.input_cursor == 0 {
            return;
        }

        let byte_index = char_to_byte_index(&self.chat.input, self.chat.input_cursor);
        if let Some(previous) = self.chat.input[..byte_index].chars().next_back() {
            let start = byte_index - previous.len_utf8();
            self.chat.input.replace_range(start..byte_index, "");
            self.chat.input_cursor -= 1;
        }
    }

    fn chat_delete(&mut self) {
        let length = self.chat.input.chars().count();
        if self.chat.input_cursor >= length {
            return;
        }

        let start = char_to_byte_index(&self.chat.input, self.chat.input_cursor);
        if let Some(next) = self.chat.input[start..].chars().next() {
            let end = start + next.len_utf8();
            self.chat.input.replace_range(start..end, "");
        }
    }

    fn open_model_picker(&mut self) {
        if self.chat.permission.is_some() {
            self.chat.status = "Resolve the permission request first.".to_string();
            return;
        }
        if self.chat.models.is_empty() {
            self.chat.status = "The model list is not loaded yet.".to_string();
            return;
        }

        let selected = self
            .chat
            .selected_model
            .as_ref()
            .and_then(|selected| self.chat.models.iter().position(|model| model == selected))
            .unwrap_or(0);

        self.chat.model_picker = Some(ModelPicker {
            filter: String::new(),
            selected,
        });
        self.focus = Focus::Chat;
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) {
        let Some(mut picker) = self.chat.model_picker.take() else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                self.chat.status = "Model selection cancelled.".to_string();
                return;
            }
            KeyCode::Enter => {
                let filtered = self.chat.filtered_model_indices(&picker.filter);
                if let Some(index) = filtered.get(picker.selected).copied() {
                    let model = self.chat.models[index].clone();
                    self.chat.selected_model = Some(model.clone());
                    self.chat.status = format!("Model set to {}.", model.key());
                }
                return;
            }
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => picker.selected = picker.selected.saturating_add(1),
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
            KeyCode::PageDown => picker.selected = picker.selected.saturating_add(10),
            KeyCode::Backspace => {
                picker.filter.pop();
                picker.selected = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                picker.filter.push(character);
                picker.selected = 0;
            }
            _ => {}
        }

        let filtered = self.chat.filtered_model_indices(&picker.filter);
        if filtered.is_empty() {
            picker.selected = 0;
        } else {
            picker.selected = picker.selected.min(filtered.len() - 1);
        }

        self.chat.model_picker = Some(picker);
    }

    fn handle_permission_key(&mut self, key: KeyEvent) {
        let Some(permission) = self.chat.permission.as_ref() else {
            return;
        };

        let reply = match key.code {
            KeyCode::Char('y') | KeyCode::Enter => "once",
            KeyCode::Char('a') => "always",
            KeyCode::Char('n') | KeyCode::Esc => "reject",
            _ => return,
        };

        let id = permission.id.clone();
        let Some(runtime) = self.chat.runtime.as_ref() else {
            self.chat.permission = None;
            self.chat.activate_next_permission();
            return;
        };
        let client = runtime.client.clone();
        let tx = self.chat_tx.clone();

        self.chat.permission = None;
        self.chat.activate_next_permission();
        self.chat.status = match reply {
            "reject" => "Permission rejected.".to_string(),
            _ => "Permission granted.".to_string(),
        };

        thread::spawn(move || {
            let result = client.reply_permission(&id, reply);
            let _ = tx.send(ChatEvent::PermissionReplyFinished { result });
        });
    }

    fn process_chat_events(&mut self) {
        while let Ok(event) = self.chat_rx.try_recv() {
            self.handle_chat_event(event);
        }
    }

    fn handle_chat_event(&mut self, event: ChatEvent) {
        match event {
            ChatEvent::RuntimeReady(data) => {
                let RuntimeReadyData {
                    server,
                    client,
                    session_id,
                    version,
                    models,
                    selected_model,
                    warning,
                } = *data;

                self.chat_init_in_flight = false;
                self.chat.phase = ChatPhase::Ready;
                self.chat.models = models;
                self.chat.selected_model = selected_model;
                self.chat.status = "Connected.".to_string();
                self.chat.permission = None;
                self.chat.permission_queue.clear();
                if let Some(echo_id) = self.chat.pending_echo_id.take() {
                    self.chat.messages.retain(|message| message.id != echo_id);
                }
                self.chat.pending_prompt_text.clear();

                let stream_client = client.clone();
                let stream_tx = self.chat_tx.clone();
                self.chat.runtime = Some(ChatRuntime {
                    _server: server,
                    client,
                    session_id,
                });
                spawn_event_stream(stream_client, stream_tx);

                let model_label = self.chat.selected_model_label();
                self.chat.push_notice(format!(
                    "Connected to OpenCode {version}. Model: {model_label}. Ask about the folder you are viewing."
                ));
                if let Some(warning) = warning {
                    self.chat.push_notice(format!("Warning: {warning}"));
                }
            }
            ChatEvent::RuntimeFailed(error) => {
                self.chat_init_in_flight = false;
                self.chat.phase = ChatPhase::Failed;
                self.chat.status = "OpenCode is unavailable.".to_string();
                self.chat
                    .push_error(format!("Could not start the AI chat: {error}"));
            }
            ChatEvent::StreamConnected => {
                self.chat.stream_connected = true;
                if self.chat.phase_is_ready() && !self.chat.busy {
                    self.chat.status = "Ready.".to_string();
                }
            }
            ChatEvent::StreamDisconnected(error) => {
                let was_connected = self.chat.stream_connected;
                self.chat.stream_connected = false;
                self.chat.status = "Event stream disconnected. Reconnecting...".to_string();
                if was_connected {
                    self.chat.push_notice(format!(
                        "Event stream disconnected: {error}. Reconnecting..."
                    ));
                }
            }
            ChatEvent::BusEvent(value) => self.handle_bus_event(value),
            ChatEvent::PromptFailed(error) => {
                self.chat.busy = false;
                if let Some(echo_id) = self.chat.pending_echo_id.take() {
                    self.chat.messages.retain(|message| message.id != echo_id);
                }
                if self.chat.input.is_empty() && !self.chat.pending_prompt_text.is_empty() {
                    self.chat.input = self.chat.pending_prompt_text.clone();
                    self.chat.input_cursor = self.chat.input.chars().count();
                }
                self.chat.pending_prompt_text.clear();
                self.chat.status = "Prompt failed.".to_string();
                self.chat.push_error(format!("Prompt failed: {error}"));
            }
            ChatEvent::AbortFinished(result) => match result {
                Ok(()) => self.chat.status = "Abort requested.".to_string(),
                Err(error) => {
                    self.chat.status = "Could not abort the response.".to_string();
                    self.chat.push_error(format!("Abort failed: {error}"));
                }
            },
            ChatEvent::PermissionReplyFinished { result } => {
                if let Err(error) = result {
                    self.chat
                        .push_error(format!("Could not send permission reply: {error}"));
                }
            }
            ChatEvent::QuestionRejectFinished { result } => {
                if let Err(error) = result {
                    self.chat
                        .push_error(format!("Could not decline the assistant question: {error}"));
                }
            }
            ChatEvent::SessionCreated(id) => {
                if let Some(runtime) = self.chat.runtime.as_mut() {
                    runtime.session_id = id;
                }
                self.chat.busy = false;
                self.chat.status = "New session ready.".to_string();
                self.chat.push_notice("New chat session ready.");
            }
            ChatEvent::SessionCreateFailed(error) => {
                self.chat.busy = false;
                self.chat.status = "Could not create a new session.".to_string();
                self.chat
                    .push_error(format!("Could not create a new session: {error}"));
            }
        }
    }

    fn handle_bus_event(&mut self, value: Value) {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let properties = value.get("properties").cloned().unwrap_or(Value::Null);

        let is_our_session = match properties.get("sessionID").and_then(Value::as_str) {
            Some(event_session) => self
                .chat
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.session_id == event_session),
            None => true,
        };
        if !is_our_session {
            return;
        }

        match event_type.as_str() {
            "message.updated" => {
                if let Some(info) = properties.get("info") {
                    self.chat.apply_message_updated(info);
                }
            }
            "message.part.updated" => {
                if let Some(part) = properties.get("part") {
                    self.chat.apply_part_updated(part);
                }
            }
            "message.part.delta" => self.chat.apply_part_delta(&properties),
            "message.part.removed" => self.chat.apply_part_removed(&properties),
            "message.removed" => self.chat.remove_message(&properties),
            "session.updated" => {
                if let Some(info) = properties.get("info") {
                    self.chat.update_usage(info);
                }
            }
            "session.status" => {
                let status = properties
                    .get("status")
                    .and_then(|status| status.get("type"))
                    .and_then(Value::as_str);
                match status {
                    Some("busy") => self.chat.busy = true,
                    Some("idle") => {
                        self.chat.busy = false;
                        if self.chat.permission.is_none() {
                            self.chat.status = "Ready.".to_string();
                        }
                    }
                    _ => {}
                }
            }
            "session.idle" => {
                self.chat.busy = false;
                if self.chat.permission.is_none() {
                    self.chat.status = "Ready.".to_string();
                }
            }
            "session.error" => {
                let message = error_message(properties.get("error"))
                    .unwrap_or_else(|| "unknown session error".to_string());
                self.chat.busy = false;
                self.chat.status = "The assistant reported an error.".to_string();
                self.chat.push_error(message);
            }
            "permission.asked" => self.chat.enqueue_permission(&properties),
            "question.asked" => self.handle_question_asked(&properties),
            _ => {}
        }
    }

    fn handle_question_asked(&mut self, properties: &Value) {
        let id = properties
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            return;
        }

        let question = properties
            .get("questions")
            .and_then(Value::as_array)
            .and_then(|questions| questions.first())
            .and_then(|question| question.get("question"))
            .and_then(Value::as_str)
            .unwrap_or("The assistant asked a question.");

        self.chat.push_notice(format!(
            "Assistant question: {question}\nInteractive question prompts are not supported yet, so the question was declined."
        ));

        let Some(runtime) = self.chat.runtime.as_ref() else {
            return;
        };
        let client = runtime.client.clone();
        let tx = self.chat_tx.clone();
        thread::spawn(move || {
            let result = client.reject_question(&id);
            let _ = tx.send(ChatEvent::QuestionRejectFinished { result });
        });
    }

    fn send_chat_prompt(&mut self) {
        if self.chat.input.trim().is_empty() {
            return;
        }

        if !self.chat.phase_is_ready() {
            self.chat.status = match &self.chat.phase {
                ChatPhase::Starting => "The chat is still starting.".to_string(),
                ChatPhase::Failed => "The chat is unavailable. Press Ctrl+R to retry.".to_string(),
                ChatPhase::Ready => String::new(),
            };
            return;
        }

        if self.chat.busy {
            self.chat.status =
                "Wait for the current response or press Ctrl+C to abort.".to_string();
            return;
        }

        let Some(runtime) = self.chat.runtime.as_ref() else {
            self.chat.status = "The chat is not ready.".to_string();
            return;
        };

        let text = self.chat.input.trim_end().to_string();
        let context = self.chat_context();
        let model = self.chat.selected_model.clone();
        let client = runtime.client.clone();
        let session_id = runtime.session_id.clone();
        let tx = self.chat_tx.clone();

        self.chat.input.clear();
        self.chat.input_cursor = 0;
        self.chat.busy = true;
        self.chat.stick_to_bottom = true;
        self.chat.status = "Waiting for the assistant...".to_string();

        let echo_id = self.chat.push_user_echo(text.clone());
        self.chat.pending_echo_id = Some(echo_id);
        self.chat.pending_prompt_text = text.clone();

        thread::spawn(move || {
            let texts = vec![context, text];
            if let Err(error) = client.prompt_async(&session_id, model.as_ref(), &texts) {
                let _ = tx.send(ChatEvent::PromptFailed(error));
            }
        });
    }

    fn abort_chat(&mut self) {
        let Some(runtime) = self.chat.runtime.as_ref() else {
            self.chat.status = "The chat is not ready.".to_string();
            return;
        };

        if !self.chat.busy {
            self.chat.status = "Nothing is running.".to_string();
            return;
        }

        let client = runtime.client.clone();
        let session_id = runtime.session_id.clone();
        let tx = self.chat_tx.clone();
        self.chat.status = "Aborting...".to_string();

        thread::spawn(move || {
            let result = client.abort(&session_id);
            let _ = tx.send(ChatEvent::AbortFinished(result));
        });
    }

    fn start_new_chat_session(&mut self) {
        if self.chat.busy {
            self.chat.status =
                "Wait for the current response or press Ctrl+C to abort.".to_string();
            return;
        }

        let Some(runtime) = self.chat.runtime.as_ref() else {
            self.chat.status = "The chat is not ready.".to_string();
            return;
        };

        let client = runtime.client.clone();
        let model = self.chat.selected_model.clone();
        let tx = self.chat_tx.clone();

        self.chat.reset_for_new_session();
        self.chat.status = "Creating a new session...".to_string();
        self.chat.push_notice("Creating a new chat session...");

        thread::spawn(move || {
            match client.create_session("Storage Analytics chat", model.as_ref()) {
                Ok(id) => {
                    let _ = tx.send(ChatEvent::SessionCreated(id));
                }
                Err(error) => {
                    let _ = tx.send(ChatEvent::SessionCreateFailed(error));
                }
            }
        });
    }

    fn retry_chat_runtime(&mut self) {
        if self.chat_init_in_flight {
            self.chat.status = "The chat is already starting.".to_string();
            return;
        }
        if self.chat.phase_is_ready() {
            self.chat.status = "The chat is already connected.".to_string();
            return;
        }

        self.start_chat_runtime();
    }

    fn start_chat_runtime(&mut self) {
        if self.chat_init_in_flight {
            return;
        }

        self.chat_init_in_flight = true;
        self.chat.phase = ChatPhase::Starting;
        self.chat.models.clear();
        self.chat.selected_model = None;
        self.chat.busy = false;
        self.chat.stream_connected = false;
        self.chat.permission = None;
        self.chat.permission_queue.clear();
        self.chat.status = "Starting OpenCode...".to_string();
        self.chat.push_notice("Starting the OpenCode server...");

        let tx = self.chat_tx.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        thread::spawn(move || {
            let result = (|| -> Result<RuntimeReadyData, String> {
                let server = OpencodeServer::connect_or_spawn(&cwd)?;
                let client = server.client();
                let version = server.version.clone();
                let mut warnings = Vec::new();

                let models = match client.fetch_models() {
                    Ok(models) => models,
                    Err(error) => {
                        warnings.push(format!("Could not load the model list: {error}"));
                        Vec::new()
                    }
                };

                let default_index = pick_default_model(&models);
                let mut selected_model = if let Some(index) = default_index {
                    Some(models[index].clone())
                } else if models.is_empty() {
                    Some(default_model_choice())
                } else {
                    Some(models[0].clone())
                };

                if !models.is_empty() && default_index.is_none() {
                    warnings.push(format!(
                        "{} is not available; using {}.",
                        default_model_choice().key(),
                        selected_model
                            .as_ref()
                            .map(ModelChoice::key)
                            .unwrap_or_else(|| "the server default".to_string())
                    ));
                }

                let session_id = match client
                    .create_session("Storage Analytics chat", selected_model.as_ref())
                {
                    Ok(id) => id,
                    Err(error) => {
                        if let Some(model) = selected_model.as_ref() {
                            warnings.push(format!(
                                "Could not create a session with {}: {error}. Falling back to the server default model.",
                                model.key()
                            ));
                            selected_model = None;
                            client.create_session("Storage Analytics chat", None)?
                        } else {
                            return Err(error);
                        }
                    }
                };

                let warning = if warnings.is_empty() {
                    None
                } else {
                    Some(warnings.join("\n"))
                };

                Ok(RuntimeReadyData {
                    server,
                    client,
                    session_id,
                    version,
                    models,
                    selected_model,
                    warning,
                })
            })();

            match result {
                Ok(data) => {
                    let _ = tx.send(ChatEvent::RuntimeReady(Box::new(data)));
                }
                Err(error) => {
                    let _ = tx.send(ChatEvent::RuntimeFailed(error));
                }
            }
        });
    }

    fn chat_context(&self) -> String {
        let mut lines = Vec::new();
        lines.push(CHAT_CONTEXT_MARKER.to_string());
        lines.push(
            "Storage Analytics context. This snapshot is generated from the disk-usage TUI the user is currently looking at.".to_string(),
        );
        if let Ok(cwd) = std::env::current_dir() {
            lines.push(format!(
                "OpenCode tool working directory: {}",
                cwd.display()
            ));
        }

        match self.view {
            View::Drives => {
                lines.push("View: connected drive list.".to_string());
                if let Some(index) = self.drives_state.selected()
                    && let Some(drive) = self.drives.get(index)
                {
                    lines.push(format!(
                        "Selected drive: {} label={} type={} used={} free={} total={}",
                        drive.name,
                        drive.label.as_deref().unwrap_or("-"),
                        drive.drive_type,
                        human_bytes(drive.used_bytes()),
                        human_bytes(drive.free_bytes),
                        human_bytes(drive.total_bytes)
                    ));
                }
                lines.push("Drives:".to_string());
                for drive in &self.drives {
                    lines.push(format!(
                        "- {} | label {} | {} | used {} | free {} | total {}",
                        drive.name,
                        drive.label.as_deref().unwrap_or("-"),
                        drive.drive_type,
                        human_bytes(drive.used_bytes()),
                        human_bytes(drive.free_bytes),
                        human_bytes(drive.total_bytes)
                    ));
                }
            }
            View::Directory => {
                lines.push("View: folder contents.".to_string());
                if let Some(root) = &self.current_root {
                    lines.push(format!("Drive root: {}", root.display()));
                    if let Some(drive) = self.drives.iter().find(|drive| &drive.root == root) {
                        lines.push(format!(
                            "Drive space: used {} | free {} | total {}",
                            human_bytes(drive.used_bytes()),
                            human_bytes(drive.free_bytes),
                            human_bytes(drive.total_bytes)
                        ));
                    }
                }
                if let Some(path) = &self.current_path {
                    lines.push(format!("Current folder: {}", path.display()));
                }

                let total_bytes: u64 = self.entries.iter().map(|entry| entry.size_bytes).sum();
                if let Some(index) = self.entries_state.selected()
                    && let Some(entry) = self.entries.get(index)
                {
                    lines.push(format!(
                        "Selected item: [{}] {} | full path: {} | size: {} | share of shown: {}",
                        entry.kind.label(),
                        entry.name,
                        entry.path.display(),
                        format_entry_size(entry),
                        format_entry_percentage(entry.size_bytes, total_bytes)
                    ));
                }

                lines.push(format!(
                    "Visible entries: {} | total shown size: {} | scan status: {}",
                    self.entries.len(),
                    human_bytes(total_bytes),
                    self.scan_progress_label()
                ));
                lines.push(
                    "Entries, largest first (full path | type | size | share of shown size):"
                        .to_string(),
                );

                let mut entries = self.entries.iter().collect::<Vec<_>>();
                entries.sort_by_key(|entry| std::cmp::Reverse(entry.size_bytes));
                for entry in entries.iter().take(CHAT_CONTEXT_ENTRY_LIMIT) {
                    lines.push(format!(
                        "- {} | {} | {} | {}",
                        entry.path.display(),
                        entry.kind.label(),
                        format_entry_size(entry),
                        format_entry_percentage(entry.size_bytes, total_bytes)
                    ));
                }
                if entries.len() > CHAT_CONTEXT_ENTRY_LIMIT {
                    lines.push(format!(
                        "... {} more entries are not shown.",
                        entries.len() - CHAT_CONTEXT_ENTRY_LIMIT
                    ));
                }
                lines.push("Sizes marked cached or partial may still be updating.".to_string());
            }
        }

        lines.push(
            "Use this snapshot first when the user asks about disk usage. You may use tools to inspect absolute paths; prefer absolute paths because the OpenCode project directory may differ from the folder being browsed.".to_string(),
        );

        lines.join("\n")
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
            let result = (|| {
                ScanPolicy::new(&path)?.check_deletion(&path)?;
                if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                }
            })();

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
                    self.status = format!("Deleted {}.", platform::display_path(&path));
                }
                DeletionResult::Failure(path, error) => {
                    self.status = format!(
                        "Failed to delete {}: {}",
                        platform::display_path(&path),
                        error
                    );
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
        self.status = format!("Reading entries in {}...", platform::display_path(&path));

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

                    if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
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
                            platform::display_path(&path),
                            self.entries.len()
                        );
                    } else {
                        self.status = format!(
                            "Finished scanning {} with {} access error(s).",
                            platform::display_path(&path),
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
                    self.status = format!(
                        "Could not scan {}: {}",
                        platform::display_path(&path),
                        error
                    );
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

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
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

fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

fn spawn_event_stream(client: OpencodeClient, sender: Sender<ChatEvent>) {
    thread::spawn(move || {
        loop {
            match client.open_event_stream() {
                Ok(mut reader) => {
                    if sender.send(ChatEvent::StreamConnected).is_err() {
                        return;
                    }

                    let mut data = String::new();
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) => {
                                let _ = sender.send(ChatEvent::StreamDisconnected(
                                    "event stream closed".to_string(),
                                ));
                                break;
                            }
                            Ok(_) => {
                                let trimmed = line.trim_end_matches(['\r', '\n']);
                                if trimmed.is_empty() {
                                    if !data.is_empty() {
                                        if let Ok(value) = serde_json::from_str::<Value>(&data) {
                                            if sender.send(ChatEvent::BusEvent(value)).is_err() {
                                                return;
                                            }
                                        }
                                        data.clear();
                                    }
                                } else if let Some(rest) = trimmed.strip_prefix("data:") {
                                    data.push_str(rest.trim_start());
                                    data.push('\n');
                                }
                            }
                            Err(error) => {
                                let _ =
                                    sender.send(ChatEvent::StreamDisconnected(error.to_string()));
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    if sender.send(ChatEvent::StreamDisconnected(error)).is_err() {
                        return;
                    }
                }
            }

            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn error_message(error: Option<&Value>) -> Option<String> {
    let error = error?;
    if error.is_null() {
        return None;
    }

    let name = error.get("name").and_then(Value::as_str).unwrap_or("error");
    let message = error
        .pointer("/data/message")
        .and_then(Value::as_str)
        .or_else(|| error.get("message").and_then(Value::as_str));

    Some(match message {
        Some(message) if !message.is_empty() => format!("{name}: {message}"),
        _ => name.to_string(),
    })
}

fn tool_input_summary(input: &Value) -> String {
    for key in [
        "filePath",
        "path",
        "pattern",
        "command",
        "description",
        "url",
        "query",
        "name",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return truncate_text(value, 120);
            }
        }
    }

    String::new()
}

fn tool_style(status: &str) -> Style {
    match status {
        "error" => Style::default().fg(Color::Red),
        "running" | "pending" => Style::default().fg(Color::Yellow),
        "completed" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn format_tool_line(tool: &ChatTool) -> String {
    let status = match tool.status.as_str() {
        "completed" => "done",
        "running" => "running",
        "pending" => "pending",
        "error" => "error",
        other => other,
    };

    let mut text = format!("[tool] {} ({status})", tool.name);
    if !tool.title.is_empty() {
        text.push_str(": ");
        text.push_str(&tool.title);
    }
    if let Some(error) = &tool.error {
        text.push_str(": ");
        text.push_str(error);
    }

    text
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn split_at_char(text: &str, index: usize) -> (String, String) {
    let byte_index = char_to_byte_index(text, index);
    (
        text[..byte_index].to_string(),
        text[byte_index..].to_string(),
    )
}

fn char_to_byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(byte_index, _)| byte_index)
        .unwrap_or(text.len())
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut output = Vec::new();

    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            output.push(String::new());
            continue;
        }

        let characters = raw_line.chars().collect::<Vec<_>>();
        let mut start = 0usize;
        while start < characters.len() {
            if start > 0 {
                while start < characters.len() && characters[start] == ' ' {
                    start += 1;
                }
            }
            if start >= characters.len() {
                break;
            }

            let hard_end = (start + width).min(characters.len());
            if hard_end == characters.len() {
                output.push(characters[start..].iter().collect());
                break;
            }

            let break_at = (start..hard_end)
                .rev()
                .find(|index| characters[*index] == ' ');

            match break_at {
                Some(space) if space > start => {
                    output.push(characters[start..space].iter().collect());
                    start = space + 1;
                }
                _ => {
                    output.push(characters[start..hard_end].iter().collect());
                    start = hard_end;
                }
            }
        }
    }

    output
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
        let policy = match ScanPolicy::new(&path) {
            Ok(policy) => Arc::new(policy),
            Err(error) => {
                let _ = sender.send(ScanEvent::Failed {
                    request_id,
                    path,
                    error: error.to_string(),
                });
                return;
            }
        };
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
                Ok(dir_entry) => {
                    match build_entry(&dir_entry, preload_cache_conn.as_ref(), &policy) {
                        Ok(entry) => entries.push(entry),
                        Err(_) => error_count += 1,
                    }
                }
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
                let policy = Arc::clone(&policy);
                handles.push(thread::spawn(move || {
                    scan_directory_queue(request_id, queue, sender, generation, cache_store, policy)
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
    policy: &ScanPolicy,
) -> io::Result<EntryInfo> {
    let path = dir_entry.path();
    let metadata = fs::symlink_metadata(&path)?;
    let name = platform::display_text(&dir_entry.file_name().to_string_lossy());
    let is_reparse_point = is_link(&metadata);
    let file_type = metadata.file_type();

    let (kind, size_bytes, size_state) =
        if file_type.is_dir() && !is_reparse_point && policy.mount_kind(&path).is_some() {
            let kind = if policy.mount_kind(&path) == Some(true) {
                EntryKind::Virtual
            } else {
                EntryKind::Mount
            };
            (kind, 0, SizeState::Complete)
        } else if file_type.is_dir() && !is_reparse_point {
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
        } else if file_type.is_file() && !is_reparse_point {
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
    policy: Arc<ScanPolicy>,
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
            &policy,
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
    policy: &ScanPolicy,
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
        policy,
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

// Keep the existing scanner's explicit progress and cancellation parameters.
#[allow(clippy::too_many_arguments)]
fn scan_directory_recursive(
    current_path: &Path,
    report_path: &Path,
    allow_partial_updates: bool,
    request_id: u64,
    generation: &Arc<AtomicU64>,
    sender: &Sender<ScanEvent>,
    progress: &mut DirectoryProgress,
    cache_conn: Option<&Connection>,
    policy: &ScanPolicy,
) -> DirectoryScanResult {
    if generation.load(Ordering::Relaxed) != request_id {
        return DirectoryScanResult {
            size_bytes: 0,
            error_count: 0,
        };
    }

    let listing = match enumerate_directory_listing(current_path, request_id, generation, policy) {
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
            policy,
        );
        total_bytes = total_bytes.saturating_add(child_result.size_bytes);
        error_count += child_result.error_count;
    }

    if listing.cacheable && error_count == 0 {
        if let Some(conn) = cache_conn {
            let _ = save_cached_directory(conn, current_path, &listing.fingerprint, total_bytes);
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
    policy: &ScanPolicy,
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
        let name = dir_entry.file_name();
        let listing_entry = build_listing_entry(name, &metadata, &path, policy);

        if listing_entry.kind == EntryKind::Directory {
            child_dirs.push(path);
        } else {
            direct_non_dir_bytes = direct_non_dir_bytes.saturating_add(listing_entry.size_bytes);
        }

        listing_entries.push(listing_entry);
    }

    let fingerprint = compute_listing_fingerprint(&mut listing_entries, policy);
    Ok(Some(DirectoryListing {
        fingerprint,
        direct_non_dir_bytes,
        child_dirs,
        error_count,
        cacheable,
    }))
}

fn build_listing_entry(
    name: OsString,
    metadata: &fs::Metadata,
    path: &Path,
    policy: &ScanPolicy,
) -> ListingEntry {
    let file_type = metadata.file_type();
    let is_reparse_point = is_link(metadata);
    let (kind, size_bytes) =
        if file_type.is_dir() && !is_reparse_point && policy.mount_kind(path).is_some() {
            (
                if policy.mount_kind(path) == Some(true) {
                    EntryKind::Virtual
                } else {
                    EntryKind::Mount
                },
                0,
            )
        } else if file_type.is_dir() && !is_reparse_point {
            (EntryKind::Directory, 0)
        } else if file_type.is_file() && !is_reparse_point {
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
        metadata_fingerprint: metadata_fingerprint(metadata),
    }
}

fn compute_listing_fingerprint(entries: &mut [ListingEntry], policy: &ScanPolicy) -> String {
    entries.sort_by(|left, right| {
        left.name
            .to_string_lossy()
            .to_lowercase()
            .cmp(&right.name.to_string_lossy().to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.kind.label().cmp(right.kind.label()))
    });

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"listing-fingerprint-v3");
    hasher.update(&policy.fingerprint);

    for entry in entries {
        hasher.update(entry.name.as_encoded_bytes());
        hasher.update(&[0]);
        hasher.update(entry.kind.label().as_bytes());
        hasher.update(&[0]);
        hasher.update(&entry.size_bytes.to_le_bytes());
        for value in entry.metadata_fingerprint {
            hasher.update(&value.to_le_bytes());
        }
    }

    hasher.finalize().to_hex().to_string()
}

fn load_cached_directory(conn: &Connection, path: &Path) -> io::Result<Option<CachedDirectory>> {
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
            platform::display_path(path)
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

fn current_unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn sqlite_err_to_io(error: rusqlite::Error) -> io::Error {
    io::Error::other(error)
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
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

pub fn run(initial_path: Option<PathBuf>) -> AppResult<()> {
    let mut app = App::new(initial_path)?;
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let result = app.run(&mut terminal);
    terminal.show_cursor()?;
    result
}

#[cfg(test)]
mod tests;
