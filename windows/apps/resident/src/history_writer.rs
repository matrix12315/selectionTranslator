//! Non-blocking resident-side history recording.
//!
//! The resident message loop owns the admission decision, but never opens
//! SQLite or waits for a database lock. A single writer drains entries
//! and delegates each write to `HistoryDatabase`, whose connection is opened
//! and closed for that operation.

use selection_platform_interface::{CompletedEntry, HistoryError, HistoryStore};
use selection_storage::HistoryDatabase;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_millis(500);
const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(5);
const WRITE_BATCH_CAPACITY: usize = 256;

enum Message {
    Entry(CompletedEntry),
    Flush(SyncSender<bool>),
}

pub struct HistoryWriter {
    sender: Mutex<Option<Sender<Message>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl HistoryWriter {
    pub fn start(database: HistoryDatabase) -> Self {
        // Provider completions and cache hits are already rate-limited by the
        // resident coordinator. An unbounded channel is the only std channel
        // that is both non-blocking for the UI thread and does not discard an
        // accepted completed result when a temporary database lock slows the
        // consumer. The worker remains the sole owner of queued entries.
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("selection-translate-history".to_owned())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    let mut entries = Vec::with_capacity(WRITE_BATCH_CAPACITY);
                    let mut flushes = Vec::new();
                    match message {
                        Message::Entry(entry) => entries.push(entry),
                        Message::Flush(acknowledgement) => flushes.push(acknowledgement),
                    }
                    while entries.len() < WRITE_BATCH_CAPACITY {
                        match receiver.try_recv() {
                            Ok(Message::Entry(entry)) => entries.push(entry),
                            Ok(Message::Flush(acknowledgement)) => flushes.push(acknowledgement),
                            Err(_) => break,
                        }
                    }
                    let persisted =
                        entries.is_empty() || database.insert_completed_batch(&entries).is_ok();
                    if !persisted {
                        // Persistence failures must not affect translation or
                        // expose target/output/provider data in logs.
                        eprintln!("Selection Translate history write failed");
                    }
                    for acknowledgement in flushes {
                        let _ = acknowledgement.send(persisted);
                    }
                }
            })
            .ok();
        Self {
            sender: Mutex::new(Some(sender)),
            join: Mutex::new(join),
        }
    }

    pub fn enqueue(&self, entry: CompletedEntry) -> Result<(), HistoryError> {
        let sender = self
            .sender
            .lock()
            .map_err(|_| HistoryError("history writer unavailable".to_owned()))?
            .as_ref()
            .cloned()
            .ok_or_else(|| HistoryError("history writer unavailable".to_owned()))?;
        sender
            .send(Message::Entry(entry))
            .map_err(|_| HistoryError("history writer unavailable".to_owned()))
    }

    /// Wait until every message admitted before this call has been attempted.
    /// This is used only for orderly shutdown and deterministic verification;
    /// normal result admission remains non-blocking on the resident UI thread.
    fn flush(&self, timeout: Duration) -> bool {
        let (acknowledgement, response) = mpsc::sync_channel(1);
        let sender = match self.sender.lock() {
            Ok(sender) => sender.as_ref().cloned(),
            Err(_) => None,
        };
        sender.is_some_and(|sender| sender.send(Message::Flush(acknowledgement)).is_ok())
            && response.recv_timeout(timeout).unwrap_or(false)
    }
}

impl HistoryStore for HistoryWriter {
    fn insert_completed(&self, entry: CompletedEntry) -> Result<(), HistoryError> {
        self.enqueue(entry)
    }
}

impl Drop for HistoryWriter {
    fn drop(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_FLUSH_BUDGET;
        let _ = self.flush(SHUTDOWN_FLUSH_BUDGET);
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        if let Ok(mut join) = self.join.lock() {
            if let Some(handle) = join.take() {
                // Closing the sender asks the worker to drain accepted rows.
                // Give normal shutdown enough time for the short SQLite
                // writes, but never hang process exit behind a database lock.
                while !handle.is_finished() && Instant::now() < deadline {
                    thread::sleep(JOIN_POLL_INTERVAL);
                }
                if handle.is_finished() {
                    let _ = handle.join();
                }
                // Otherwise dropping JoinHandle detaches the worker. Windows
                // will terminate it with the process; shutdown stays bounded.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HistoryWriter;
    use selection_platform_interface::{CompletedEntry, ExtractionSource};
    use selection_storage::HistoryDatabase;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn entry(number: u64) -> CompletedEntry {
        let minute = (number / 60) % 60;
        let second = number % 60;
        CompletedEntry {
            created_at_utc: format!("2026-08-19T00:{minute:02}:{second:02}Z"),
            source: ExtractionSource::UiaSelection,
            target: format!("target-{number}"),
            context: Some("sentence context".to_owned()),
            output: format!("output-{number}"),
            prompt_id: "translate".to_owned(),
            model: "test-model".to_owned(),
            served_from_cache: false,
        }
    }

    #[test]
    fn burst_admission_is_lossless_and_nonblocking() {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tmp")
            .join(format!("history-writer-{id}"));
        let database = HistoryDatabase::open(directory.join("history.sqlite3")).unwrap();
        let writer = HistoryWriter::start(database);
        for number in 0..1_088 {
            writer.enqueue(entry(number)).unwrap();
        }
        assert!(writer.flush(std::time::Duration::from_secs(5)));
        let database = HistoryDatabase::open(directory.join("history.sqlite3")).unwrap();
        assert_eq!(database.count().unwrap(), 1_000);
        drop(writer);
        let _ = std::fs::remove_dir_all(directory);
    }
}
