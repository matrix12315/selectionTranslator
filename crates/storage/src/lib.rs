//! Short-lived SQLite persistence for completed translation history.
//!
//! The resident owns a [`HistoryDatabase`] value containing only a path.  Each
//! operation opens, validates, uses, and closes SQLite, so the resident does
//! not keep a database connection or SQLite page cache alive while idle.

mod db;
mod history;
mod migrations;

pub use db::{default_history_path, HistoryDatabase, StorageError};
pub use history::{HistoryEntry, HistoryOrder, HistoryQuery, MAX_HISTORY_ENTRIES};

/// Marker retained for compatibility with the bootstrap crate.
pub const CRATE_NAME: &str = "selection-storage";

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;

    use rusqlite::Connection;
    use selection_core::ExtractionSource;
    use selection_platform_interface::CompletedEntry;

    use super::{
        default_history_path, HistoryDatabase, HistoryOrder, HistoryQuery, StorageError,
        MAX_HISTORY_ENTRIES,
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    struct TestDatabase {
        database: HistoryDatabase,
        directory: PathBuf,
    }

    impl TestDatabase {
        fn new() -> Self {
            let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
            let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("windows")
                .join("tmp")
                .join("storage-tests")
                .join(format!("{}-{id}", std::process::id()));
            let path = directory.join("history.sqlite3");
            let database = HistoryDatabase::open(&path).expect("open test database");
            Self {
                database,
                directory,
            }
        }
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    fn entry(created_at_utc: &str, target: &str, output: &str) -> CompletedEntry {
        CompletedEntry {
            created_at_utc: created_at_utc.to_owned(),
            source: ExtractionSource::UiaSelection,
            target: target.to_owned(),
            context: Some("完整 sentence context".to_owned()),
            output: output.to_owned(),
            prompt_id: "translate".to_owned(),
            model: "test-model".to_owned(),
            served_from_cache: false,
        }
    }

    #[test]
    fn first_creation_has_schema_version_and_index() {
        let test = TestDatabase::new();
        let connection = Connection::open(test.database.path()).unwrap();
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let table: String = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'history'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table, "history");
        let index: String = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'history_created_at_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, "history_created_at_idx");
    }

    #[test]
    fn open_creates_missing_parent_without_replacing_existing_database() {
        let test = TestDatabase::new();
        assert!(test.database.path().parent().unwrap().is_dir());

        test.database
            .insert_completed(entry("2026-08-19T10:00:00Z", "keep", "keep"))
            .unwrap();
        let reopened = HistoryDatabase::open(test.database.path()).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
    }

    #[test]
    fn default_history_path_uses_the_canonical_profile_layout() {
        let local_app_data = PathBuf::from(r"D:\profile\local-app-data");
        let expected = local_app_data
            .join("SelectionTranslate")
            .join("history.sqlite3");
        // Exercise the path-shaping contract without changing the process-wide
        // environment, which could race with parallel tests.
        assert_eq!(
            super::db::history_path_from_local_app_data(local_app_data),
            expected
        );
        // The public helper is intentionally conditional when Windows has no
        // LOCALAPPDATA (for example, a service or a restricted test runner).
        if std::env::var_os("LOCALAPPDATA").is_some() {
            assert!(default_history_path().is_some());
        }
    }

    #[test]
    fn insertion_maps_all_fields_and_round_trips_unicode() {
        let test = TestDatabase::new();
        let mut expected = entry("2026-08-19T10:00:00Z", "你好 世界 🌏", "你好，世界");
        expected.source = ExtractionSource::Ocr;
        expected.context = Some("文脈と контекст".to_owned());
        expected.served_from_cache = true;
        test.database.insert_completed(expected.clone()).unwrap();

        let rows = test.database.search(&HistoryQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        let actual = &rows[0];
        assert_eq!(actual.id, 1);
        assert_eq!(actual.created_at_utc, expected.created_at_utc);
        assert_eq!(actual.source, expected.source);
        assert_eq!(actual.target, expected.target);
        assert_eq!(actual.context, expected.context);
        assert_eq!(actual.output, expected.output);
        assert_eq!(actual.prompt_id, expected.prompt_id);
        assert_eq!(actual.model, expected.model);
        assert_eq!(actual.served_from_cache, expected.served_from_cache);
    }

    #[test]
    fn insertion_and_pruning_are_transactional_and_keep_newest_1000() {
        let test = TestDatabase::new();
        for number in 0..=MAX_HISTORY_ENTRIES {
            let timestamp = format!("2026-08-19T00:{:02}:{:02}Z", number / 60, number % 60);
            test.database
                .insert_completed(entry(&timestamp, &format!("target-{number}"), "output"))
                .unwrap();
        }

        assert_eq!(test.database.count().unwrap(), MAX_HISTORY_ENTRIES);
        let rows = test
            .database
            .search(&HistoryQuery {
                limit: MAX_HISTORY_ENTRIES,
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(rows.first().unwrap().target, "target-1000");
        assert!(!rows.iter().any(|row| row.target == "target-0"));
        assert!(rows.iter().any(|row| row.target == "target-1"));
    }

    #[test]
    fn search_filters_target_output_prompt_source_and_orders_dates() {
        let test = TestDatabase::new();
        let mut first = entry("2026-08-18T00:00:00Z", "alpha", "unrelated");
        first.prompt_id = "explain".to_owned();
        first.source = ExtractionSource::Clipboard;
        test.database.insert_completed(first).unwrap();
        let mut second = entry("2026-08-19T00:00:00Z", "beta", "needle output");
        second.prompt_id = "translate".to_owned();
        second.source = ExtractionSource::Ocr;
        test.database.insert_completed(second).unwrap();

        let output_match = test
            .database
            .search(&HistoryQuery {
                search: Some("needle".to_owned()),
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(output_match.len(), 1);
        assert_eq!(output_match[0].target, "beta");

        let prompt_match = test
            .database
            .search(&HistoryQuery {
                prompt_id: Some("explain".to_owned()),
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(prompt_match[0].target, "alpha");

        let source_match = test
            .database
            .search(&HistoryQuery {
                source: Some(ExtractionSource::Ocr),
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(source_match[0].target, "beta");

        let oldest = test
            .database
            .search(&HistoryQuery {
                order: HistoryOrder::OldestFirst,
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(oldest[0].target, "alpha");
    }

    #[test]
    fn empty_target_or_output_cannot_be_inserted() {
        let test = TestDatabase::new();
        let mut no_target = entry("2026-08-19T00:00:00Z", "\u{200b}\u{3000}", "output");
        assert!(matches!(
            test.database.insert_completed(no_target.clone()),
            Err(StorageError::InvalidInput(_))
        ));
        no_target.target = "target".to_owned();
        no_target.output = "\u{200b}\u{3000}".to_owned();
        assert!(matches!(
            test.database.insert_completed(no_target),
            Err(StorageError::InvalidInput(_))
        ));
        assert_eq!(test.database.count().unwrap(), 0);
    }

    #[test]
    fn noncanonical_timestamps_are_rejected() {
        let test = TestDatabase::new();
        for timestamp in [
            "2026-08-19 10:00:00Z",
            "2026-08-19T10:00:00+00:00",
            "2026-08-19T10:00:00",
            "2026-08-19T10:00:00.Z",
            "2026-13-19T10:00:00Z",
            "not-a-timestamp",
        ] {
            assert!(matches!(
                test.database
                    .insert_completed(entry(timestamp, "target", "output")),
                Err(StorageError::InvalidInput(_))
            ));
        }
        test.database
            .insert_completed(entry("2026-08-19T10:00:00.123Z", "target", "output"))
            .unwrap();
    }

    #[test]
    fn delete_one_is_parameterized_and_reports_presence() {
        let test = TestDatabase::new();
        test.database
            .insert_completed(entry("2026-08-19T00:00:00Z", "target", "output"))
            .unwrap();
        assert!(test.database.delete_one(1).unwrap());
        assert!(!test.database.delete_one(1).unwrap());
        assert_eq!(test.database.count().unwrap(), 0);
    }

    #[test]
    fn concurrent_reader_and_writer_complete_without_lost_rows() {
        let test = TestDatabase::new();
        let database = Arc::new(test.database.clone());
        let writer_database = Arc::clone(&database);
        let writer = thread::spawn(move || {
            for number in 0..20 {
                writer_database
                    .insert_completed(entry(
                        &format!("2026-08-19T00:00:{number:02}Z"),
                        &format!("target-{number}"),
                        "output",
                    ))
                    .unwrap();
            }
        });
        for _ in 0..20 {
            database.search(&HistoryQuery::default()).unwrap();
        }
        writer.join().unwrap();
        assert_eq!(database.count().unwrap(), 20);
    }

    #[test]
    fn corrupt_database_returns_error_without_recovery() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("windows")
            .join("tmp")
            .join("storage-tests")
            .join(format!("corrupt-{}-{id}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("history.sqlite3");
        fs::write(&path, b"this is not sqlite").unwrap();
        let error = HistoryDatabase::open(&path).unwrap_err();
        assert!(matches!(error, StorageError::CorruptDatabase(_)));
        assert_eq!(fs::read(&path).unwrap(), b"this is not sqlite");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn malformed_version_one_schema_is_rejected_without_recovery() {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("windows")
            .join("tmp")
            .join("storage-tests")
            .join(format!("malformed-{}-{id}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("history.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE history (
                    id TEXT,
                    created_at_utc TEXT,
                    source TEXT,
                    target TEXT,
                    context TEXT,
                    output TEXT,
                    prompt_id TEXT,
                    model TEXT,
                    served_from_cache INTEGER
                );
                CREATE INDEX history_created_at_idx ON history(created_at_utc ASC);
                PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let error = HistoryDatabase::open(&path).unwrap_err();
        assert!(matches!(error, StorageError::UnsupportedSchema(_)));
        let _ = fs::remove_dir_all(directory);
    }
}
