//! Completed-entry insertion and manager-facing history queries.

use rusqlite::{params, params_from_iter, types::Type, Connection, Error as SqliteError, ToSql};
use selection_core::{normalize::normalize_target, ExtractionSource};
use selection_platform_interface::CompletedEntry;

use crate::db::StorageError;

pub use crate::db::MAX_HISTORY_ENTRIES;

/// A stored completed result, including its database identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub created_at_utc: String,
    pub source: ExtractionSource,
    pub target: String,
    pub context: Option<String>,
    pub output: String,
    pub prompt_id: String,
    pub model: String,
    pub served_from_cache: bool,
}

/// Stable ordering options exposed to the manager.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HistoryOrder {
    #[default]
    NewestFirst,
    OldestFirst,
}

/// Parameterized search and filter options.  Search is over target and output
/// text; all user-provided values are bound parameters, never SQL fragments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryQuery {
    pub search: Option<String>,
    pub prompt_id: Option<String>,
    pub source: Option<ExtractionSource>,
    pub order: HistoryOrder,
    pub limit: usize,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            search: None,
            prompt_id: None,
            source: None,
            order: HistoryOrder::NewestFirst,
            limit: MAX_HISTORY_ENTRIES,
        }
    }
}

pub(crate) fn validate_entry(entry: &CompletedEntry) -> Result<(), StorageError> {
    if normalize_target(&entry.target).is_empty() {
        return Err(StorageError::InvalidInput(
            "target cannot be empty".to_owned(),
        ));
    }
    if normalize_target(&entry.output).is_empty() {
        return Err(StorageError::InvalidInput(
            "output cannot be empty".to_owned(),
        ));
    }
    if normalize_target(&entry.created_at_utc).is_empty() {
        return Err(StorageError::InvalidInput(
            "created_at_utc cannot be empty".to_owned(),
        ));
    }
    if !is_canonical_utc_timestamp(&entry.created_at_utc) {
        return Err(StorageError::InvalidInput(
            "created_at_utc must be a canonical UTC timestamp ending in Z".to_owned(),
        ));
    }
    if normalize_target(&entry.prompt_id).is_empty() {
        return Err(StorageError::InvalidInput(
            "prompt_id cannot be empty".to_owned(),
        ));
    }
    if normalize_target(&entry.model).is_empty() {
        return Err(StorageError::InvalidInput(
            "model cannot be empty".to_owned(),
        ));
    }
    Ok(())
}

/// Accept the UTC/RFC-3339 shape emitted by the application while preserving
/// lexicographic chronological ordering.  Fractional seconds are optional,
/// but if present must contain one or more ASCII digits.
fn is_canonical_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || *bytes.last().unwrap_or(&0) != b'Z' {
        return false;
    }
    for (index, expected) in [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')] {
        if bytes.get(index) != Some(&expected) {
            return false;
        }
    }
    for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18] {
        if !bytes[index].is_ascii_digit() {
            return false;
        }
    }

    let Some(year) = parse_digits(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = parse_digits(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = parse_digits(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = parse_digits(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = parse_digits(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = parse_digits(&bytes[17..19]) else {
        return false;
    };
    if year == 0
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return false;
    }

    // The first possible fractional character is immediately after seconds;
    // an absent fraction means the trailing Z is at byte 19.
    if bytes.len() == 20 {
        return true;
    }
    bytes[19] == b'.'
        && bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit)
        && bytes.len() > 21
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, digit| {
        digit
            .is_ascii_digit()
            .then_some(value * 10 + u32::from(digit - b'0'))
    })
}

pub(crate) fn insert_completed(
    connection: &mut Connection,
    entry: &CompletedEntry,
) -> Result<(), SqliteError> {
    insert_completed_batch(connection, std::slice::from_ref(entry))
}

pub(crate) fn insert_completed_batch(
    connection: &mut Connection,
    entries: &[CompletedEntry],
) -> Result<(), SqliteError> {
    let transaction = connection.transaction()?;
    for entry in entries {
        transaction.execute(
            "INSERT INTO history
             (created_at_utc, source, target, context, output, prompt_id, model, served_from_cache)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.created_at_utc,
                source_to_string(entry.source),
                entry.target,
                entry.context,
                entry.output,
                entry.prompt_id,
                entry.model,
                i64::from(entry.served_from_cache),
            ],
        )?;
    }
    // Keep pruning in the same transaction as insertion.  The id tie-breaker
    // makes equal timestamps deterministic while preserving the newest rows.
    transaction.execute(
        "DELETE FROM history
         WHERE id NOT IN (
             SELECT id FROM history
             ORDER BY created_at_utc DESC, id DESC
             LIMIT ?1
         )",
        params![MAX_HISTORY_ENTRIES as i64],
    )?;
    transaction.commit()
}

pub(crate) fn query(
    connection: &Connection,
    query: &HistoryQuery,
) -> Result<Vec<HistoryEntry>, SqliteError> {
    let mut sql = String::from(
        "SELECT id, created_at_utc, source, target, context, output, prompt_id, model,
                served_from_cache
         FROM history WHERE 1 = 1",
    );
    let mut values: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(search) = query.search.as_deref() {
        sql.push_str(" AND (instr(target, ?1) > 0 OR instr(output, ?2) > 0)");
        values.push(Box::new(search.to_owned()));
        values.push(Box::new(search.to_owned()));
    }
    if let Some(prompt_id) = query.prompt_id.as_deref() {
        let index = values.len() + 1;
        sql.push_str(&format!(" AND prompt_id = ?{index}"));
        values.push(Box::new(prompt_id.to_owned()));
    }
    if let Some(source) = query.source {
        let index = values.len() + 1;
        sql.push_str(&format!(" AND source = ?{index}"));
        values.push(Box::new(source_to_string(source)));
    }

    match query.order {
        HistoryOrder::NewestFirst => sql.push_str(" ORDER BY created_at_utc DESC, id DESC"),
        HistoryOrder::OldestFirst => sql.push_str(" ORDER BY created_at_utc ASC, id ASC"),
    }
    let index = values.len() + 1;
    sql.push_str(&format!(" LIMIT ?{index}"));
    values.push(Box::new(query.limit.min(MAX_HISTORY_ENTRIES) as i64));

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| {
            let source_name: String = row.get(2)?;
            let source = source_from_string(&source_name).ok_or_else(|| {
                SqliteError::FromSqlConversionFailure(
                    2,
                    Type::Text,
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown extraction source {source_name:?}"),
                    )
                    .into(),
                )
            })?;
            let served_from_cache: i64 = row.get(8)?;
            let served_from_cache = match served_from_cache {
                0 => false,
                1 => true,
                value => {
                    return Err(SqliteError::FromSqlConversionFailure(
                        8,
                        Type::Integer,
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("invalid served_from_cache value {value}"),
                        )
                        .into(),
                    ))
                }
            };
            Ok(HistoryEntry {
                id: row.get(0)?,
                created_at_utc: row.get(1)?,
                source,
                target: row.get(3)?,
                context: row.get(4)?,
                output: row.get(5)?,
                prompt_id: row.get(6)?,
                model: row.get(7)?,
                served_from_cache,
            })
        },
    )?;
    rows.collect()
}

pub(crate) fn delete_one(connection: &Connection, id: i64) -> Result<bool, SqliteError> {
    let changed = connection.execute("DELETE FROM history WHERE id = ?1", params![id])?;
    Ok(changed == 1)
}

fn source_to_string(source: ExtractionSource) -> &'static str {
    match source {
        ExtractionSource::UiaSelection => "uia_selection",
        ExtractionSource::UiaPoint => "uia_point",
        ExtractionSource::Clipboard => "clipboard",
        ExtractionSource::Ocr => "ocr",
    }
}

fn source_from_string(value: &str) -> Option<ExtractionSource> {
    match value {
        "uia_selection" => Some(ExtractionSource::UiaSelection),
        "uia_point" => Some(ExtractionSource::UiaPoint),
        "clipboard" => Some(ExtractionSource::Clipboard),
        "ocr" => Some(ExtractionSource::Ocr),
        _ => None,
    }
}
