use ratatui::backend::TestBackend;
use serde_json::json;

use super::*;

#[cfg(target_os = "linux")]
mod linux {
    use crate::app::*;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};

    fn scan(path: &Path, conn: Option<&Connection>) -> DirectoryScanResult {
        let (sender, _) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(1));
        scan_directory_recursive(
            path,
            path,
            false,
            1,
            &generation,
            &sender,
            &mut DirectoryProgress::new(),
            conn,
            &ScanPolicy::new(path).unwrap(),
        )
    }

    #[test]
    fn totals_nested_files_without_following_directory_or_file_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("small"), [0; 7]).unwrap();
        fs::write(root.join("nested/large"), [0; 31]).unwrap();
        fs::write(fixture.path().join("outside"), [0; 100]).unwrap();
        symlink(&root, root.join("loop")).unwrap();
        symlink(fixture.path().join("outside"), root.join("external-link")).unwrap();
        let result = scan(&root, None);
        assert_eq!(result.size_bytes, 38);
        assert_eq!(result.error_count, 0);
        let link = fs::read_dir(&root)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() == "loop")
            .unwrap();
        assert_eq!(
            build_entry(&link, None, &ScanPolicy::new(&root).unwrap())
                .unwrap()
                .kind,
            EntryKind::Link
        );
    }

    #[test]
    fn cache_keeps_case_sensitive_directories_separate() {
        let fixture = tempfile::tempdir().unwrap();
        let cache = CacheStore::new(fixture.path()).unwrap();
        let conn = cache.open_connection().unwrap();
        let upper = fixture.path().join("Documents");
        let lower = fixture.path().join("documents");
        fs::create_dir(&upper).unwrap();
        fs::create_dir(&lower).unwrap();
        fs::write(upper.join("file"), [0; 9]).unwrap();
        fs::write(lower.join("file"), [0; 21]).unwrap();
        assert_eq!(scan(&upper, Some(&conn)).size_bytes, 9);
        assert_eq!(scan(&lower, Some(&conn)).size_bytes, 21);
        assert_eq!(
            load_cached_directory(&conn, &upper)
                .unwrap()
                .unwrap()
                .recursive_size,
            9
        );
        assert_eq!(
            load_cached_directory(&conn, &lower)
                .unwrap()
                .unwrap()
                .recursive_size,
            21
        );
    }

    #[test]
    fn cache_keys_preserve_non_utf8_paths() {
        let first = PathBuf::from(OsString::from_vec(b"/tmp/\xff".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/\xfe".to_vec()));
        assert_ne!(normalize_path_key(&first), normalize_path_key(&second));
    }

    #[test]
    fn changed_file_metadata_invalidates_the_directory_cache() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("file"), [0; 7]).unwrap();
        let cache = CacheStore::new(fixture.path()).unwrap();
        let conn = cache.open_connection().unwrap();
        assert_eq!(scan(&root, Some(&conn)).size_bytes, 7);
        fs::write(root.join("file"), [0; 42]).unwrap();
        assert_eq!(scan(&root, Some(&conn)).size_bytes, 42);
    }

    #[test]
    fn terminal_control_characters_are_escaped() {
        assert_eq!(
            platform::display_text("folder\x1b[31m\n"),
            "folder\\u{1b}[31m\\n"
        );
    }

    #[test]
    fn mount_boundaries_are_excluded_from_initial_tasks_and_recursive_totals() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let virtual_mount = root.join("proc");
        let bind_mount = root.join("nested/bind");
        fs::create_dir_all(&virtual_mount).unwrap();
        fs::create_dir_all(&bind_mount).unwrap();
        fs::write(virtual_mount.join("synthetic"), [0; 900]).unwrap();
        fs::write(bind_mount.join("duplicate"), [0; 700]).unwrap();
        fs::write(root.join("nested/local"), [0; 31]).unwrap();
        fs::write(root.join("direct"), [0; 7]).unwrap();
        let policy = ScanPolicy::from_mounts(std::collections::BTreeMap::from([
            (virtual_mount.clone(), true),
            (bind_mount.clone(), false),
        ]));
        let entry = fs::read_dir(&root)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() == "proc")
            .unwrap();
        let initial = build_entry(&entry, None, &policy).unwrap();
        assert_eq!(initial.kind, EntryKind::Virtual);
        assert_eq!(initial.size_state, SizeState::Complete);
        assert!(!initial.kind.can_enter());

        let cache = CacheStore::new(fixture.path()).unwrap();
        let conn = cache.open_connection().unwrap();
        assert_eq!(scan(&root, Some(&conn)).size_bytes, 1638);
        let (sender, _) = mpsc::channel();
        let result = scan_directory_recursive(
            &root,
            &root,
            false,
            1,
            &Arc::new(AtomicU64::new(1)),
            &sender,
            &mut DirectoryProgress::new(),
            Some(&conn),
            &policy,
        );
        assert_eq!(result.size_bytes, 38);
        assert_eq!(result.error_count, 0);
        assert!(policy.check_deletion(&virtual_mount).is_err());
        assert!(policy.check_deletion(&root).is_err());
        assert!(policy.check_deletion(&root.join("direct")).is_ok());
        assert!(ScanPolicy::new(Path::new("/proc")).is_err());
    }
}

fn test_app() -> App {
    let (scanner_tx, scanner_rx) = mpsc::channel();
    let (deletion_tx, deletion_rx) = mpsc::channel();
    let (chat_tx, chat_rx) = mpsc::channel();
    let empty = Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    let mut chat = ChatState::new();
    chat.phase = ChatPhase::Ready;
    chat.stream_connected = true;
    chat.selected_model = Some(default_model_choice());
    chat.push_notice("Connected to OpenCode test.");
    chat.push_user_echo("What is using space?".to_string());
    chat.push_notice("The largest entry is big-folder.");

    App {
        view: View::Directory,
        drives: Vec::new(),
        drives_state: TableState::default(),
        entries: vec![
            EntryInfo {
                path: PathBuf::from("D:\\test\\big-folder"),
                name: "big-folder".to_string(),
                kind: EntryKind::Directory,
                size_bytes: 123_456_789,
                size_state: SizeState::Complete,
            },
            EntryInfo {
                path: PathBuf::from("D:\\test\\file.bin"),
                name: "file.bin".to_string(),
                kind: EntryKind::File,
                size_bytes: 42_000,
                size_state: SizeState::Complete,
            },
        ],
        entries_state: select_first(TableState::default(), 2),
        current_root: Some(PathBuf::from("D:\\")),
        current_path: Some(PathBuf::from("D:\\test")),
        scan_progress: ScanProgress {
            request_id: 1,
            total_entries: 2,
            resolved_entries: 2,
            error_count: 0,
            started: true,
            scanning: false,
        },
        scanner_tx,
        scanner_rx,
        scan_generation: Arc::new(AtomicU64::new(1)),
        cache_store: CacheStore {
            db_path: Arc::new(PathBuf::from("test-cache.sqlite3")),
        },
        status: "Finished scanning.".to_string(),
        spinner_index: 0,
        last_content_height: 10,
        user_moved_selection: false,
        pending_delete: None,
        is_deleting: false,
        deletion_tx,
        deletion_rx,
        focus: Focus::Chat,
        chat,
        chat_tx,
        chat_rx,
        chat_init_in_flight: false,
        explorer_area: empty,
        chat_area: empty,
        chat_transcript_area: empty,
        chat_input_area: empty,
    }
}

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
fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn draws_chat_layout_in_all_states() {
    let mut app = test_app();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("AI Chat"));
    assert!(text.contains("What is using space?"));
    assert!(text.contains("Message"));
    assert!(text.contains("opencode-go/deepseek-v4.1-flash"));

    app.focus = Focus::Explorer;
    terminal.draw(|frame| app.draw(frame)).unwrap();

    app.chat.models = vec![default_model_choice()];
    app.chat.model_picker = Some(ModelPicker {
        filter: "deep".to_string(),
        selected: 0,
    });
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("Select Model"));
    assert!(text.contains("Filter: deep"));

    app.chat.model_picker = None;
    app.chat.permission = Some(PermissionPrompt {
        id: "per_1".to_string(),
        permission: "bash".to_string(),
        patterns: vec!["rm -rf".to_string()],
    });
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let text = buffer_text(&terminal);
    assert!(text.contains("Permission Required"));
    assert!(text.contains("bash"));
}

#[test]
fn wraps_text_at_word_boundaries() {
    assert_eq!(wrap_text("hello world", 5), vec!["hello", "world"]);
    assert_eq!(wrap_text("abcdefgh", 4), vec!["abcd", "efgh"]);
    assert_eq!(wrap_text("", 4), vec![""]);
}

#[test]
fn splits_text_at_character_index() {
    assert_eq!(split_at_char("abc", 1), ("a".to_string(), "bc".to_string()));
    assert_eq!(
        split_at_char("ação", 2),
        ("aç".to_string(), "ão".to_string())
    );
}

#[test]
fn applies_streamed_text_deltas() {
    let mut chat = ChatState::new();
    chat.apply_message_updated(&json!({
        "id": "msg_1",
        "role": "assistant",
        "time": {}
    }));
    chat.apply_part_updated(&json!({
        "id": "prt_1",
        "messageID": "msg_1",
        "type": "text",
        "text": ""
    }));
    chat.apply_part_delta(&json!({
        "messageID": "msg_1",
        "partID": "prt_1",
        "field": "text",
        "delta": "Hel"
    }));
    chat.apply_part_delta(&json!({
        "messageID": "msg_1",
        "partID": "prt_1",
        "field": "text",
        "delta": "lo"
    }));

    assert_eq!(chat.messages[0].parts[0].text, "Hello");
}

#[test]
fn chat_context_includes_current_folder_and_selection() {
    let app = test_app();
    let context = app.chat_context();

    assert!(context.starts_with(CHAT_CONTEXT_MARKER));
    assert!(context.contains("Current folder: D:\\test"));
    assert!(context.contains("Selected item: [dir] big-folder"));
    assert!(context.contains("D:\\test\\big-folder"));
    assert!(context.contains("file.bin"));
}

#[test]
fn queues_permission_requests() {
    let mut chat = ChatState::new();
    chat.enqueue_permission(&json!({
        "id": "per_1",
        "sessionID": "ses_1",
        "permission": "bash",
        "patterns": ["rm -rf"]
    }));
    chat.enqueue_permission(&json!({
        "id": "per_2",
        "sessionID": "ses_1",
        "permission": "edit",
        "patterns": ["src/main.rs"]
    }));

    assert_eq!(chat.permission.as_ref().unwrap().id, "per_1");
    chat.activate_next_permission();
    assert_eq!(chat.permission.as_ref().unwrap().id, "per_2");
}

#[test]
fn filters_models_by_provider_and_name() {
    let models = vec![
        ModelChoice {
            provider_id: "opencode-go".to_string(),
            provider_name: "OpenCode Go".to_string(),
            model_id: "deepseek-v4.1-flash".to_string(),
            model_name: "DeepSeek V4.1 Flash".to_string(),
        },
        ModelChoice {
            provider_id: "openai".to_string(),
            provider_name: "OpenAI".to_string(),
            model_id: "gpt-5.6".to_string(),
            model_name: "GPT 5.6".to_string(),
        },
    ];

    let chat = ChatState {
        models: models.clone(),
        ..ChatState::new()
    };

    assert_eq!(chat.filtered_model_indices("deepseek"), vec![0]);
    assert_eq!(chat.filtered_model_indices("openai"), vec![1]);
    assert_eq!(chat.filtered_model_indices("open"), vec![0, 1]);
}

#[test]
fn formats_token_counts() {
    assert_eq!(compact_tokens(999), "999");
    assert_eq!(compact_tokens(1_500), "1.5k");
    assert_eq!(compact_tokens(2_500_000), "2.5M");
}
