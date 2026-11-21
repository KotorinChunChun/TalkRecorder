//! 保存された音声と独立した、集計用の録音履歴。
use anyhow::{Context, Result};
use chrono::{Days, Local, NaiveDate};
use rusqlite::{Connection, OpenFlags, Row, TransactionBehavior, params_from_iter, types::Value};
use serde::Serialize;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const FILENAME: &str = "talkrecorder-audit.sqlite3";
const QUEUE_CAPACITY: usize = 64;

macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Default, Serialize)]
        pub(crate) struct Counters {
            $(pub $field: u64,)+
        }
        const COLUMNS: &[&str] = &[$(stringify!($field)),+];
        impl Counters {
            fn bind(self) -> Result<impl Iterator<Item = Value>> {
                Ok([$(Value::Integer(i64::try_from(self.$field)?)),+].into_iter())
            }
            fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
                Ok(Self { $($field: read_counter(row, stringify!($field))?,)+ })
            }
        }
    };
}

counters!(
    recorded_frames,
    saved_frames,
    silence_cut_frames,
    excluded_frames,
    unknown_frames,
    missing_track_frames,
    unsynchronized_track_frames,
    reclaimed_bytes,
    saved_segments,
    excluded_segments,
    save_failures,
    recording_failures,
);

fn read_counter(row: &Row<'_>, column: &str) -> rusqlite::Result<u64> {
    let index = row.as_ref().column_index(column)?;
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

#[derive(Clone, Copy)]
pub(crate) enum EventKind {
    Segment,
    Capture,
    RecordingFailure,
}

impl EventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Segment => "segment",
            Self::Capture => "capture",
            Self::RecordingFailure => "recording_failure",
        }
    }
}

struct Event {
    day: NaiveDate,
    utc_ms: i64,
    kind: EventKind,
    counters: Counters,
}

pub(crate) type AuditWarning = Arc<Mutex<Option<&'static str>>>;

#[derive(Clone)]
pub(crate) struct AuditClient {
    edits: SyncSender<Option<Event>>,
    warning: AuditWarning,
}

impl AuditClient {
    pub(crate) fn record(&self, kind: EventKind, counters: Counters) {
        let now = Local::now();
        if self
            .edits
            .try_send(Some(Event {
                day: now.date_naive(),
                utc_ms: now.timestamp_millis(),
                kind,
                counters,
            }))
            .is_err()
        {
            warn(&self.warning, "録音統計の待ち行列に記録できませんでした");
        }
    }
}

pub(crate) struct AuditWorker {
    client: AuditClient,
    thread: Option<JoinHandle<()>>,
}

impl AuditWorker {
    pub(crate) fn start(dir: &Path, warning: AuditWarning) -> Result<Self> {
        let path = dir.join(FILENAME);
        let (edits, messages) = mpsc::sync_channel(QUEUE_CAPACITY);
        let worker_warning = Arc::clone(&warning);
        let thread = thread::Builder::new()
            .name("talkrecorder-audit".into())
            .spawn(move || {
                let mut database = None;
                let mut last_archive = None;
                let mut existing_database = path.is_file();
                loop {
                    let first = if std::mem::take(&mut existing_database) {
                        Err(RecvTimeoutError::Timeout)
                    } else {
                        messages.recv_timeout(Duration::from_secs(60))
                    };
                    let mut batch = Vec::new();
                    let mut stopping = false;
                    match first {
                        Ok(Some(event)) => batch.push(event),
                        Ok(None) | Err(RecvTimeoutError::Disconnected) => stopping = true,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    if !stopping {
                        for entry in messages.try_iter().take(QUEUE_CAPACITY) {
                            match entry {
                                Some(event) => batch.push(event),
                                None => {
                                    stopping = true;
                                    break;
                                }
                            }
                        }
                    }
                    let today = Local::now().date_naive();
                    let archive = last_archive != Some(today);
                    if archive || !batch.is_empty() {
                        let result = (|| -> Result<()> {
                            if database.is_none() {
                                database = Some(open(&path)?);
                            }
                            let db = database.as_mut().expect("録音統計DBの初期化済み");
                            if !batch.is_empty() {
                                store(db, &batch, None)?;
                            }
                            if archive {
                                store(db, &[], Some(today))?;
                                last_archive = Some(today);
                                // 回収済みのページだけを少量ずつ開放する。
                                db.execute_batch("PRAGMA incremental_vacuum(64)")?;
                            }
                            Ok(())
                        })();
                        if result.is_err() {
                            warn(&worker_warning, "録音統計の保存・集計に失敗しました");
                        }
                    }
                    if stopping {
                        break;
                    }
                }
            })?;
        Ok(Self {
            client: AuditClient { edits, warning },
            thread: Some(thread),
        })
    }

    pub(crate) fn client(&self) -> AuditClient {
        self.client.clone()
    }
}

impl Drop for AuditWorker {
    fn drop(&mut self) {
        let _ = self.client.edits.send(None);
        if self
            .thread
            .take()
            .is_some_and(|thread| thread.join().is_err())
        {
            warn(&self.client.warning, "録音統計の処理が途中で終了しました");
        }
    }
}

fn warn(warning: &AuditWarning, message: &'static str) {
    *warning.lock().unwrap_or_else(|error| error.into_inner()) = Some(message);
}

fn open(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_millis(250))?;
    connection.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
    let _: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    let columns = COLUMNS
        .iter()
        .map(|column| format!("{column} INTEGER NOT NULL"))
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS detail (
            id INTEGER PRIMARY KEY, day TEXT NOT NULL, utc_ms INTEGER NOT NULL,
            kind TEXT NOT NULL, {columns}
         );
         CREATE INDEX IF NOT EXISTS detail_day ON detail(day);
         CREATE TABLE IF NOT EXISTS daily (day TEXT PRIMARY KEY, {columns});"
    ))?;
    Ok(connection)
}

fn sums() -> String {
    COLUMNS
        .iter()
        .map(|column| format!("SUM({column}) AS {column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn store(db: &mut Connection, batch: &[Event], archive_on: Option<NaiveDate>) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let columns = COLUMNS.join(", ");
    if !batch.is_empty() {
        let placeholders = vec!["?"; COLUMNS.len() + 3].join(", ");
        let mut insert = tx.prepare(&format!(
            "INSERT INTO detail (day, utc_ms, kind, {columns}) VALUES ({placeholders})"
        ))?;
        for event in batch {
            let key = [
                Value::Text(event.day.to_string()),
                Value::Integer(event.utc_ms),
                Value::Text(event.kind.as_str().into()),
            ];
            insert.execute(params_from_iter(
                key.into_iter().chain(event.counters.bind()?),
            ))?;
        }
    }
    if let Some(today) = archive_on {
        // 各日を丸ごと保持するため、境界はローカル日付で固定する。
        let cutoff = today
            .checked_sub_days(Days::new(7))
            .context("集計開始日が範囲外です")?
            .to_string();
        let merge = COLUMNS
            .iter()
            .map(|column| format!("{column}=daily.{column}+excluded.{column}"))
            .collect::<Vec<_>>()
            .join(", ");
        tx.execute(
            &format!(
                "INSERT INTO daily (day, {columns})
                SELECT day, {} FROM detail WHERE day < ? GROUP BY day
                ON CONFLICT(day) DO UPDATE SET {merge}",
                sums()
            ),
            [&cutoff],
        )?;
        tx.execute("DELETE FROM detail WHERE day < ?", [&cutoff])?;
    }
    tx.commit()?;
    Ok(())
}

#[derive(Serialize)]
pub(crate) struct DaySummary {
    pub(crate) day: String,
    #[serde(flatten)]
    pub(crate) counters: Counters,
}

#[derive(Serialize)]
pub(crate) struct Detail {
    pub(crate) day: String,
    pub(crate) utc_ms: i64,
    pub(crate) kind: String,
    #[serde(flatten)]
    pub(crate) counters: Counters,
}

#[derive(Serialize)]
pub struct Report {
    pub(crate) sample_rate: u32,
    pub(crate) archived: Vec<DaySummary>,
    pub(crate) recent: Vec<DaySummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) details: Option<Vec<Detail>>,
}

pub fn read_report(dir: &Path, details: bool) -> Result<Report> {
    let path: PathBuf = dir.join(FILENAME);
    let mut report = Report {
        sample_rate: 48_000,
        archived: Vec::new(),
        recent: Vec::new(),
        details: details.then(Vec::new),
    };
    match path.metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(report),
        other => {
            other?;
        }
    }
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_millis(250))?;
    let snapshot = db.transaction()?;
    for (query, result) in [
        (
            "SELECT * FROM daily ORDER BY day".into(),
            &mut report.archived,
        ),
        (
            format!(
                "SELECT day, {} FROM detail GROUP BY day ORDER BY day",
                sums()
            ),
            &mut report.recent,
        ),
    ] {
        let mut statement = snapshot.prepare(&query)?;
        *result = statement
            .query_map([], |row| {
                Ok(DaySummary {
                    day: row.get("day")?,
                    counters: Counters::read(row)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    if let Some(result) = &mut report.details {
        let mut statement = snapshot.prepare("SELECT * FROM detail ORDER BY id")?;
        *result = statement
            .query_map([], |row| {
                Ok(Detail {
                    day: row.get("day")?,
                    utc_ms: row.get("utc_ms")?,
                    kind: row.get("kind")?,
                    counters: Counters::read(row)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(today: NaiveDate, age: u64, bytes: u64) -> Event {
        Event {
            day: today.checked_sub_days(Days::new(age)).unwrap(),
            utc_ms: 0,
            kind: EventKind::Segment,
            counters: Counters {
                saved_segments: 1,
                reclaimed_bytes: bytes,
                ..Counters::default()
            },
        }
    }

    #[test]
    fn older_days_are_compacted_and_recent_days_are_kept_without_double_counting() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut db = open(&temp.path().join(FILENAME))?;
        let day = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        store(
            &mut db,
            &[
                detail(day, 8, 1024),
                detail(day, 8, 2048),
                detail(day, 7, 32),
            ],
            Some(day),
        )?;
        store(&mut db, &[], Some(day))?;
        let first = read_report(temp.path(), true)?;
        assert_eq!(first.archived.len(), 1);
        assert_eq!(first.archived[0].counters.saved_segments, 2);
        assert_eq!(first.archived[0].counters.reclaimed_bytes, 3072);
        assert_eq!(first.details.unwrap().len(), 1);
        assert_eq!(first.recent[0].counters.reclaimed_bytes, 32);
        store(&mut db, &[detail(day, 8, 128)], Some(day))?;
        let next = read_report(temp.path(), true)?;
        assert_eq!(next.archived.len(), 1);
        assert_eq!(next.archived[0].counters.reclaimed_bytes, 3200);
        Ok(())
    }

    #[test]
    fn archive_and_detail_deletion_rollback_together() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut db = open(&temp.path().join(FILENAME))?;
        let day = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        store(&mut db, &[detail(day, 8, 1024)], None)?;
        db.execute_batch(
            "CREATE TRIGGER fail_delete BEFORE DELETE ON detail
          BEGIN SELECT RAISE(ABORT, 'test'); END;",
        )?;
        assert!(store(&mut db, &[], Some(day)).is_err());
        let report = read_report(temp.path(), true)?;
        assert!(report.archived.is_empty());
        assert_eq!(report.details.unwrap().len(), 1);
        db.execute_batch("DROP TRIGGER fail_delete")?;
        store(&mut db, &[], Some(day))?;
        assert_eq!(
            read_report(temp.path(), true)?.archived[0]
                .counters
                .reclaimed_bytes,
            1024
        );
        Ok(())
    }

    #[test]
    fn worker_flushes_before_exit_and_report_without_a_database_is_empty() -> Result<()> {
        let temp = tempfile::tempdir()?;
        assert!(read_report(temp.path(), true)?.recent.is_empty());
        let warning = AuditWarning::default();
        let worker = AuditWorker::start(temp.path(), warning.clone())?;
        worker.client().record(
            EventKind::Capture,
            Counters {
                missing_track_frames: 48,
                unknown_frames: 12_000,
                ..Counters::default()
            },
        );
        drop(worker);
        assert_eq!(*warning.lock().unwrap(), None);
        let report = read_report(temp.path(), true)?;
        assert_eq!(report.recent[0].counters.missing_track_frames, 48);
        assert_eq!(report.details.unwrap()[0].kind, "capture");
        Ok(())
    }
}
