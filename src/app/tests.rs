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
