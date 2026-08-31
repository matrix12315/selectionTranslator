//! Versioned SQLite schema creation and validation.

use rusqlite::{Connection, Error as SqliteError, ErrorCode, OptionalExtension};

pub const LATEST_SCHEMA_VERSION: u32 = 1;

const CREATE_HISTORY: &str = r#"
CREATE TABLE history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_utc TEXT NOT NULL,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    context TEXT,
    output TEXT NOT NULL,
    prompt_id TEXT NOT NULL,
    model TEXT NOT NULL,
    served_from_cache INTEGER NOT NULL CHECK (served_from_cache IN (0, 1))
);
CREATE INDEX history_created_at_idx ON history(created_at_utc DESC);
"#;

/// Create a new database or validate the supported schema.
///
/// A non-empty database with no version, a future version, or a version whose
/// required objects are missing is rejected.  We never drop or recreate user
/// data as an automatic recovery strategy.
pub fn ensure_schema(connection: &mut Connection) -> Result<(), SqliteError> {
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    match version {
        0 => {
            let has_user_table: bool = connection.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master
                    WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                )",
                [],
                |row| row.get(0),
            )?;
            if has_user_table {
                return Err(SqliteError::InvalidQuery);
            }

            let transaction = connection.transaction()?;
            transaction.execute_batch(CREATE_HISTORY)?;
            transaction.execute_batch("PRAGMA user_version = 1;")?;
            transaction.commit()?;
            Ok(())
        }
        LATEST_SCHEMA_VERSION => validate_schema(connection),
        _unsupported => Err(SqliteError::InvalidQuery),
    }
}

fn validate_schema(connection: &Connection) -> Result<(), SqliteError> {
    let table_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'history'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(table_sql) = table_sql else {
        return Err(SqliteError::InvalidQuery);
    };

    let columns = connection
        .prepare("PRAGMA table_info(history)")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let expected_columns = [
        ("id", "INTEGER", 0, 1),
        ("created_at_utc", "TEXT", 1, 0),
        ("source", "TEXT", 1, 0),
        ("target", "TEXT", 1, 0),
        ("context", "TEXT", 0, 0),
        ("output", "TEXT", 1, 0),
        ("prompt_id", "TEXT", 1, 0),
        ("model", "TEXT", 1, 0),
        ("served_from_cache", "INTEGER", 1, 0),
    ];
    let index_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'history_created_at_idx')",
        [],
        |row| row.get(0),
    )?;
    let column_shape_matches = columns.len() == expected_columns.len()
        && columns.iter().zip(expected_columns).all(
            |(
                (name, declared_type, not_null, primary_key),
                (expected_name, expected_type, expected_not_null, expected_primary_key),
            )| {
                name == expected_name
                    && declared_type.eq_ignore_ascii_case(expected_type)
                    && *not_null == expected_not_null
                    && *primary_key == expected_primary_key
            },
        );

    // These clauses distinguish the planned table from a same-name look-alike
    // that merely happens to expose the same column names.
    let normalized_sql: String = table_sql
        .split_whitespace()
        .collect::<String>()
        .to_ascii_lowercase();
    let table_constraints_match = normalized_sql.contains("idintegerprimarykeyautoincrement")
        && normalized_sql
            .contains("served_from_cacheintegernotnullcheck(served_from_cachein(0,1))");

    let index_columns = connection
        .prepare("PRAGMA index_xinfo(history_created_at_idx)")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(_, column_id, _, _)| *column_id >= 0)
        .collect::<Vec<_>>();
    let index_shape_matches = index_columns.len() == 1
        && index_columns[0].0 == 0
        && index_columns[0].2.as_deref() == Some("created_at_utc")
        && index_columns[0].3 == 1;

    if !column_shape_matches || !table_constraints_match || !index_exists || !index_shape_matches {
        return Err(SqliteError::InvalidQuery);
    }
    Ok(())
}

/// Returns true for errors that indicate the file is not a usable SQLite DB.
pub fn is_corrupt(error: &SqliteError) -> bool {
    matches!(
        error,
        SqliteError::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
            )
    )
}
