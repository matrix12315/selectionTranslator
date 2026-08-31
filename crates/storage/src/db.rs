//! Connection lifecycle and public database handle.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Error as SqliteError};
use selection_platform_interface::{CompletedEntry, HistoryStore};

use crate::history::{
    delete_one, insert_completed, insert_completed_batch, query, validate_entry, HistoryEntry,
    HistoryQuery,
};
use crate::migrations;

/// The maximum number of completed entries retained by the application.
pub const MAX_HISTORY_ENTRIES: usize = 1_000;

/// Return the one conventional on-disk history location used by resident and
/// manager.  The caller still decides when to open it; this helper does not
/// touch the filesystem or environment beyond reading `LOCALAPPDATA`.
pub fn default_history_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(history_path_from_local_app_data)
}

pub(crate) fn history_path_from_local_app_data(local_app_data: impl AsRef<Path>) -> PathBuf {
    local_app_data
        .as_ref()
        .join("SelectionTranslate")
        .join("history.sqlite3")
}

/// Errors returned by history storage.  Corrupt and unsupported files are
/// reported without deleting, truncating, or replacing the database.
#[derive(Debug)]
pub enum StorageError {
    InvalidInput(String),
    Io(String),
    Sqlite(String),
    CorruptDatabase(String),
    UnsupportedSchema(String),
    InvalidRow(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(formatter, "invalid history input: {message}"),
            Self::Io(message) => write!(formatter, "history I/O error: {message}"),
            Self::Sqlite(message) => write!(formatter, "history SQLite error: {message}"),
            Self::CorruptDatabase(message) => {
                write!(formatter, "history database is corrupt: {message}")
            }
            Self::UnsupportedSchema(message) => {
                write!(formatter, "unsupported history schema: {message}")
            }
            Self::InvalidRow(message) => write!(formatter, "invalid history row: {message}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl From<SqliteError> for StorageError {
    fn from(error: SqliteError) -> Self {
        if migrations::is_corrupt(&error) {
            return Self::CorruptDatabase(error.to_string());
        }
        Self::Sqlite(error.to_string())
    }
}

/// A lightweight, cloneable path handle.  No SQLite connection is retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryDatabase {
    path: PathBuf,
}

impl HistoryDatabase {
    /// Opens or creates the database and validates schema version 1.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() {
            return Err(StorageError::InvalidInput(
                "database path is empty".to_owned(),
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                StorageError::Io(format!("could not create history directory: {error}"))
            })?;
        }
        let database = Self { path };
        database.with_connection(|_| Ok(()))?;
        Ok(database)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Inserts one completed result and prunes older rows in the same
    /// transaction.  The type accepted here cannot represent a failed or
    /// cancelled job.
    pub fn insert_completed(&self, entry: CompletedEntry) -> Result<(), StorageError> {
        validate_entry(&entry)?;
        self.with_connection(|connection| insert_completed(connection, &entry))
    }

    /// Insert several completed results using one short-lived connection and
    /// one transaction, pruning once after the batch. This lets the resident
    /// drain a burst without retaining an idle SQLite connection.
    pub fn insert_completed_batch(&self, entries: &[CompletedEntry]) -> Result<(), StorageError> {
        for entry in entries {
            validate_entry(entry)?;
        }
        if entries.is_empty() {
            return Ok(());
        }
        self.with_connection(|connection| insert_completed_batch(connection, entries))
    }

    pub fn search(&self, query_options: &HistoryQuery) -> Result<Vec<HistoryEntry>, StorageError> {
        self.with_connection(|connection| query(connection, query_options))
    }

    pub fn delete_one(&self, id: i64) -> Result<bool, StorageError> {
        self.with_connection(|connection| delete_one(connection, id))
    }

    pub fn count(&self) -> Result<usize, StorageError> {
        self.with_connection(|connection| {
            let count: i64 =
                connection.query_row("SELECT COUNT(*) FROM history", [], |row| row.get(0))?;
            usize::try_from(count).map_err(|_| rusqlite::Error::InvalidQuery)
        })
    }

    fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, SqliteError>,
    ) -> Result<T, StorageError> {
        let mut connection = Connection::open(&self.path).map_err(StorageError::from)?;
        connection.busy_timeout(std::time::Duration::from_millis(1_000))?;
        // WAL lets the on-demand manager read while the resident performs its
        // short write.  The connection is dropped at the end of this method.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        migrations::ensure_schema(&mut connection).map_err(|error| {
            if migrations::is_corrupt(&error) {
                StorageError::CorruptDatabase(error.to_string())
            } else if matches!(error, SqliteError::InvalidQuery) {
                StorageError::UnsupportedSchema(error.to_string())
            } else {
                StorageError::from(error)
            }
        })?;
        operation(&mut connection).map_err(StorageError::from)
    }
}

impl HistoryStore for HistoryDatabase {
    fn insert_completed(
        &self,
        entry: CompletedEntry,
    ) -> Result<(), selection_platform_interface::HistoryError> {
        HistoryDatabase::insert_completed(self, entry)
            .map_err(|error| selection_platform_interface::HistoryError(error.to_string()))
    }
}
