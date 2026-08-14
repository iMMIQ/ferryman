use super::jobs_api::JobPhase;
use super::{JobEntry, JobRecord, JobStatus, StorageKind};
use anyhow::{Context, Result};
use ferryman::format::OutputMode;
use ferryman::preset::Preset;
use ferryman::settings::TranslationSettings;
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::task;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub(crate) struct JobStore {
    connection: Arc<StdMutex<Connection>>,
}

#[derive(Clone, Debug)]
pub(crate) struct JobCursor {
    pub(crate) created_at: u64,
    pub(crate) id: Uuid,
}

pub(crate) struct JobPage {
    pub(crate) jobs: Vec<JobRecord>,
    pub(crate) next_cursor: Option<JobCursor>,
    pub(crate) total: usize,
}

pub(crate) enum RetryJobOutcome {
    Retried(Box<JobEntry>),
    NotFound,
    NotFailed,
    AtLimit,
}

impl JobStore {
    pub(crate) async fn open(path: PathBuf) -> Result<Self> {
        task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let connection = Connection::open(&path)
                .with_context(|| format!("open job database {}", path.display()))?;
            connection.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )?;
            let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version == 0 {
                create_schema(&connection)?;
            } else if version != SCHEMA_VERSION {
                anyhow::bail!(
                    "unsupported job database schema {version}; expected {SCHEMA_VERSION}"
                );
            }
            Ok(Self {
                connection: Arc::new(StdMutex::new(connection)),
            })
        })
        .await
        .context("join database initialization")?
    }

    async fn call<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| anyhow::anyhow!("job database lock poisoned"))?;
            operation(&mut connection)
        })
        .await
        .context("join database operation")?
    }

    pub(crate) async fn insert(&self, entry: JobEntry, active_limit: usize) -> Result<()> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let active = transaction.query_row(
                "SELECT COUNT(*) FROM jobs
                 WHERE owner=?1 AND status IN ('queued', 'starting_model', 'translating', 'writing')",
                [&entry.owner],
                |row| row.get::<_, i64>(0),
            )? as usize;
            if active >= active_limit {
                anyhow::bail!("too many queued or active jobs");
            }
            transaction.execute(
                "INSERT INTO jobs (
                    id, owner, filename, preset, target_language, output_mode, status,
                    total, completed, translated, failed_segments, error,
                    batch_size, context_segments, cache_enabled, result_available,
                    source_path, source_storage, save_path, save_storage,
                    job_dir, input_path, output_path, save_to, save_root, overwrite,
                    created_at, updated_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16,
                    ?17, ?18, ?19, ?20,
                    ?21, ?22, ?23, ?24, ?25, ?26,
                    ?27, ?28
                 )",
                rusqlite::params_from_iter(entry_params(&entry)?),
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn update(&self, entry: JobEntry) -> Result<()> {
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE jobs SET
                    owner=?2, filename=?3, preset=?4, target_language=?5,
                    output_mode=?6, status=?7, total=?8, completed=?9,
                    translated=?10, failed_segments=?11, error=?12,
                    batch_size=?13, context_segments=?14, cache_enabled=?15,
                    result_available=?16, source_path=?17, source_storage=?18,
                    save_path=?19, save_storage=?20, job_dir=?21, input_path=?22,
                    output_path=?23, save_to=?24, save_root=?25, overwrite=?26,
                    created_at=?27, updated_at=?28
                 WHERE id=?1",
                rusqlite::params_from_iter(entry_params(&entry)?),
            )?;
            if changed != 1 {
                // The row was deleted concurrently (user delete racing a queued
                // persistence snapshot) — nothing left to update, not an error.
                return Ok(());
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn recover_nonterminal(&self, updated_at: u64) -> Result<Vec<JobEntry>> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "UPDATE jobs
                 SET status='queued', updated_at=?1
                 WHERE status IN ('starting_model', 'translating', 'writing')",
                [to_i64(updated_at)?],
            )?;
            let jobs = {
                let mut statement = transaction.prepare(
                    "SELECT * FROM jobs WHERE status='queued'
                     ORDER BY created_at ASC, id ASC",
                )?;
                let jobs = statement
                    .query_map([], row_to_entry)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                jobs
            };
            transaction.commit()?;
            Ok(jobs)
        })
        .await
    }

    pub(crate) async fn claim(&self, id: Uuid, updated_at: u64) -> Result<Option<JobEntry>> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let changed = transaction.execute(
                "UPDATE jobs SET status='starting_model', error=NULL, updated_at=?2
                 WHERE id=?1 AND status='queued'",
                params![id.to_string(), to_i64(updated_at)?],
            )?;
            if changed == 0 {
                transaction.commit()?;
                return Ok(None);
            }
            let entry = transaction.query_row(
                "SELECT * FROM jobs WHERE id=?1",
                [id.to_string()],
                row_to_entry,
            )?;
            transaction.commit()?;
            Ok(Some(entry))
        })
        .await
    }

    pub(crate) async fn get(&self, owner: String, id: Uuid) -> Result<Option<JobEntry>> {
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT * FROM jobs WHERE id=?1 AND owner=?2",
                    params![id.to_string(), owner],
                    row_to_entry,
                )
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    pub(crate) async fn list_page(
        &self,
        owner: String,
        phase: Option<JobPhase>,
        cursor: Option<JobCursor>,
        limit: usize,
    ) -> Result<JobPage> {
        self.call(move |connection| {
            let status_filter = phase.map(JobPhase::sql_filter).unwrap_or_default();
            let count_sql = format!("SELECT COUNT(*) FROM jobs WHERE owner=?1{status_filter}");
            let total =
                connection.query_row(&count_sql, [&owner], |row| row.get::<_, i64>(0))? as usize;
            let mut jobs = if let Some(cursor) = cursor {
                let sql = format!(
                    "SELECT * FROM jobs
                     WHERE owner=?1{status_filter}
                       AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))
                     ORDER BY created_at DESC, id DESC LIMIT ?4"
                );
                let mut statement = connection.prepare(&sql)?;
                let jobs = statement
                    .query_map(
                        params![
                            owner,
                            to_i64(cursor.created_at)?,
                            cursor.id.to_string(),
                            to_i64(limit.saturating_add(1))?
                        ],
                        row_to_entry,
                    )?
                    .map(|entry| entry.map(|entry| entry.record))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                jobs
            } else {
                let sql = format!(
                    "SELECT * FROM jobs WHERE owner=?1{status_filter}
                     ORDER BY created_at DESC, id DESC LIMIT ?2"
                );
                let mut statement = connection.prepare(&sql)?;
                let jobs = statement
                    .query_map(
                        params![owner, to_i64(limit.saturating_add(1))?],
                        row_to_entry,
                    )?
                    .map(|entry| entry.map(|entry| entry.record))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                jobs
            };
            let has_more = jobs.len() > limit;
            jobs.truncate(limit);
            let next_cursor = has_more
                .then(|| jobs.last())
                .flatten()
                .map(|job| JobCursor {
                    created_at: job.created_at,
                    id: job.id,
                });
            Ok(JobPage {
                jobs,
                next_cursor,
                total,
            })
        })
        .await
    }

    pub(crate) async fn list_active(&self, owner: String) -> Result<Vec<JobRecord>> {
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM jobs
                 WHERE owner=?1 AND status IN ('queued', 'starting_model', 'translating', 'writing')
                 ORDER BY created_at DESC, id DESC",
            )?;
            let jobs = statement
                .query_map([owner], row_to_entry)?
                .map(|entry| entry.map(|entry| entry.record))
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into);
            jobs
        })
        .await
    }

    pub(crate) async fn count_active(&self, owner: String) -> Result<usize> {
        self.call(move |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM jobs
                 WHERE owner=?1 AND status IN ('queued', 'starting_model', 'translating', 'writing')",
                [owner],
                |row| row.get::<_, i64>(0),
            )? as usize)
        })
        .await
    }

    pub(crate) async fn retry_failed(
        &self,
        owner: String,
        id: Uuid,
        now: u64,
        active_limit: usize,
    ) -> Result<RetryJobOutcome> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let status = transaction
                .query_row(
                    "SELECT status FROM jobs WHERE id=?1 AND owner=?2",
                    params![id.to_string(), &owner],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            match status.as_deref() {
                None => return Ok(RetryJobOutcome::NotFound),
                Some("failed") => {}
                Some(_) => return Ok(RetryJobOutcome::NotFailed),
            }
            let active = transaction.query_row(
                "SELECT COUNT(*) FROM jobs
                 WHERE owner=?1 AND status IN ('queued', 'starting_model', 'translating', 'writing')",
                [&owner],
                |row| row.get::<_, i64>(0),
            )? as usize;
            if active >= active_limit {
                return Ok(RetryJobOutcome::AtLimit);
            }
            transaction.execute(
                "UPDATE jobs SET
                    status='queued', total=0, completed=0, translated=0,
                    failed_segments=0, error=NULL, result_available=0,
                    created_at=?2, updated_at=?2
                 WHERE id=?1 AND owner=?3 AND status='failed'",
                params![id.to_string(), to_i64(now)?, &owner],
            )?;
            let entry = transaction.query_row(
                "SELECT * FROM jobs WHERE id=?1 AND owner=?2",
                params![id.to_string(), owner],
                row_to_entry,
            )?;
            transaction.commit()?;
            Ok(RetryJobOutcome::Retried(Box::new(entry)))
        })
        .await
    }

    pub(crate) async fn delete_terminal(&self, owner: String, id: Uuid) -> Result<bool> {
        self.call(move |connection| {
            Ok(connection.execute(
                "DELETE FROM jobs
                 WHERE id=?1 AND owner=?2 AND status IN ('completed', 'failed', 'cancelled')",
                params![id.to_string(), owner],
            )? == 1)
        })
        .await
    }

    /// Terminal jobs whose `updated_at` fell below `cutoff` (epoch seconds),
    /// oldest first, at most `limit` — the retention sweep's work list. The
    /// rows are not deleted here: the caller removes each job directory first
    /// and only then deletes the rows that succeeded, so a dir that refuses to
    /// be removed keeps its row and is retried on the next sweep instead of
    /// leaking on disk forever.
    pub(crate) async fn expired_terminal(
        &self,
        cutoff: u64,
        limit: usize,
    ) -> Result<Vec<JobEntry>> {
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM jobs
                 WHERE status IN ('completed', 'failed', 'cancelled') AND updated_at < ?1
                 ORDER BY updated_at ASC, id ASC LIMIT ?2",
            )?;
            let jobs = statement
                .query_map(params![to_i64(cutoff)?, to_i64(limit)?], row_to_entry)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(Into::into);
            jobs
        })
        .await
    }

    /// Delete terminal rows by id (the second half of the retention sweep).
    pub(crate) async fn delete_terminal_ids(&self, ids: Vec<Uuid>) -> Result<usize> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut removed = 0;
            for id in &ids {
                removed += transaction.execute(
                    "DELETE FROM jobs
                     WHERE id=?1 AND status IN ('completed', 'failed', 'cancelled')",
                    [id.to_string()],
                )?;
            }
            transaction.commit()?;
            Ok(removed)
        })
        .await
    }
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE jobs (
            id TEXT PRIMARY KEY NOT NULL,
            owner TEXT NOT NULL,
            filename TEXT NOT NULL,
            preset TEXT NOT NULL,
            target_language TEXT NOT NULL,
            output_mode TEXT NOT NULL,
            status TEXT NOT NULL,
            total INTEGER NOT NULL DEFAULT 0,
            completed INTEGER NOT NULL DEFAULT 0,
            translated INTEGER NOT NULL DEFAULT 0,
            failed_segments INTEGER NOT NULL DEFAULT 0,
            error TEXT,
            batch_size INTEGER NOT NULL,
            context_segments INTEGER NOT NULL,
            cache_enabled INTEGER NOT NULL,
            result_available INTEGER NOT NULL DEFAULT 0,
            source_path TEXT,
            source_storage TEXT,
            save_path TEXT,
            save_storage TEXT,
            job_dir TEXT NOT NULL,
            input_path TEXT NOT NULL,
            output_path TEXT NOT NULL,
            save_to TEXT,
            save_root TEXT,
            overwrite INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            CHECK (preset IN ('7b-fp8', '30b-fp8')),
            CHECK (output_mode IN ('bilingual', 'replace')),
            CHECK (status IN ('queued', 'starting_model', 'translating', 'writing', 'completed', 'failed', 'cancelled')),
            CHECK (source_storage IS NULL OR source_storage IN ('documents', 'remote_fs')),
            CHECK (save_storage IS NULL OR save_storage IN ('documents', 'remote_fs'))
         );
         CREATE INDEX jobs_owner_created_idx ON jobs(owner, created_at DESC, id DESC);
         CREATE INDEX jobs_status_created_idx ON jobs(status, created_at, id);
         CREATE INDEX jobs_owner_status_created_idx
            ON jobs(owner, status, created_at DESC, id DESC);
         PRAGMA user_version=1;
         COMMIT;",
    )?;
    Ok(())
}

fn entry_params(entry: &JobEntry) -> Result<Vec<rusqlite::types::Value>> {
    use rusqlite::types::Value as SqlValue;
    let path = |value: &Path| -> Result<SqlValue> {
        Ok(SqlValue::Text(
            value
                .to_str()
                .context("job path is not valid UTF-8")?
                .to_string(),
        ))
    };
    let optional_path = |value: Option<&PathBuf>| -> Result<SqlValue> {
        value
            .map(|path| path.as_path())
            .map(path)
            .transpose()
            .map(|value| value.unwrap_or(SqlValue::Null))
    };
    let optional_text =
        |value: Option<&String>| value.cloned().map(SqlValue::Text).unwrap_or(SqlValue::Null);
    Ok(vec![
        SqlValue::Text(entry.record.id.to_string()),
        SqlValue::Text(entry.owner.clone()),
        SqlValue::Text(entry.record.filename.clone()),
        SqlValue::Text(entry.record.preset.as_str().to_string()),
        SqlValue::Text(entry.record.target.clone()),
        SqlValue::Text(entry.record.mode.as_str().to_string()),
        SqlValue::Text(entry.record.status.as_str().to_string()),
        SqlValue::Integer(to_i64(entry.record.total)?),
        SqlValue::Integer(to_i64(entry.record.completed)?),
        SqlValue::Integer(to_i64(entry.record.translated)?),
        SqlValue::Integer(to_i64(entry.record.failed_segments)?),
        optional_text(entry.record.error.as_ref()),
        SqlValue::Integer(to_i64(entry.record.settings.batch_size)?),
        SqlValue::Integer(to_i64(entry.record.settings.context_segments)?),
        SqlValue::Integer(i64::from(entry.record.settings.cache_enabled)),
        SqlValue::Integer(i64::from(entry.record.result_available)),
        optional_text(entry.record.source_path.as_ref()),
        entry
            .record
            .source_storage
            .map(|value| SqlValue::Text(value.as_str().to_string()))
            .unwrap_or(SqlValue::Null),
        optional_text(entry.record.save_path.as_ref()),
        entry
            .record
            .save_storage
            .map(|value| SqlValue::Text(value.as_str().to_string()))
            .unwrap_or(SqlValue::Null),
        path(&entry.dir)?,
        path(&entry.input)?,
        path(&entry.output)?,
        optional_path(entry.save_to.as_ref())?,
        optional_path(entry.save_root.as_ref())?,
        SqlValue::Integer(i64::from(entry.overwrite)),
        SqlValue::Integer(to_i64(entry.record.created_at)?),
        SqlValue::Integer(to_i64(entry.record.updated_at)?),
    ])
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<JobEntry> {
    let parse_error = |index, error: String| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        )
    };
    let preset_value: String = row.get("preset")?;
    let preset = match preset_value.as_str() {
        "7b-fp8" => Preset::SevenBFp8,
        "30b-fp8" => Preset::ThirtyBFp8,
        _ => return Err(parse_error(3, format!("invalid preset {preset_value}"))),
    };
    let mode_value: String = row.get("output_mode")?;
    let mode = OutputMode::from_str(&mode_value).map_err(|error| parse_error(5, error))?;
    let status_value: String = row.get("status")?;
    let status = JobStatus::from_str(&status_value).map_err(|error| parse_error(6, error))?;
    let source_storage = row
        .get::<_, Option<String>>("source_storage")?
        .map(|value| StorageKind::from_str(&value).map_err(|error| parse_error(17, error)))
        .transpose()?;
    let save_storage = row
        .get::<_, Option<String>>("save_storage")?
        .map(|value| StorageKind::from_str(&value).map_err(|error| parse_error(19, error)))
        .transpose()?;
    let id_value: String = row.get("id")?;
    let id = Uuid::parse_str(&id_value).map_err(|error| parse_error(0, error.to_string()))?;
    Ok(JobEntry {
        owner: row.get("owner")?,
        dir: PathBuf::from(row.get::<_, String>("job_dir")?),
        input: PathBuf::from(row.get::<_, String>("input_path")?),
        output: PathBuf::from(row.get::<_, String>("output_path")?),
        save_to: row.get::<_, Option<String>>("save_to")?.map(PathBuf::from),
        save_root: row
            .get::<_, Option<String>>("save_root")?
            .map(PathBuf::from),
        overwrite: row.get::<_, i64>("overwrite")? != 0,
        record: JobRecord {
            id,
            filename: row.get("filename")?,
            preset,
            target: row.get("target_language")?,
            mode,
            status,
            total: from_i64(row.get("total")?, "total")?,
            completed: from_i64(row.get("completed")?, "completed")?,
            translated: from_i64(row.get("translated")?, "translated")?,
            failed_segments: from_i64(row.get("failed_segments")?, "failed_segments")?,
            error: row.get("error")?,
            settings: TranslationSettings {
                batch_size: from_i64(row.get("batch_size")?, "batch_size")?,
                context_segments: from_i64(row.get("context_segments")?, "context_segments")?,
                cache_enabled: row.get::<_, i64>("cache_enabled")? != 0,
            },
            result_available: row.get::<_, i64>("result_available")? != 0,
            source_path: row.get("source_path")?,
            source_storage,
            save_path: row.get("save_path")?,
            save_storage,
            created_at: from_i64_u64(row.get("created_at")?, "created_at")?,
            updated_at: from_i64_u64(row.get("updated_at")?, "updated_at")?,
        },
    })
}

fn to_i64<T>(value: T) -> Result<i64>
where
    T: TryInto<i64>,
    T::Error: std::fmt::Display,
{
    value
        .try_into()
        .map_err(|error| anyhow::anyhow!("integer is too large for SQLite: {error}"))
}

fn from_i64(value: i64, field: &'static str) -> rusqlite::Result<usize> {
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid {field}: {value}"),
            )),
        )
    })
}

fn from_i64_u64(value: i64, field: &'static str) -> rusqlite::Result<u64> {
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid {field}: {value}"),
            )),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(owner: &str, id: Uuid, created_at: u64, status: JobStatus) -> JobEntry {
        let dir = PathBuf::from(format!("/tmp/ferryman-job-{id}"));
        JobEntry {
            owner: owner.to_string(),
            input: dir.join("input.txt"),
            output: dir.join("result.txt"),
            save_to: None,
            save_root: None,
            overwrite: false,
            dir,
            record: JobRecord {
                id,
                filename: "notes.txt".to_string(),
                preset: Preset::SevenBFp8,
                target: "中文".to_string(),
                mode: OutputMode::Bilingual,
                status,
                total: 12,
                completed: 3,
                translated: 3,
                failed_segments: 0,
                error: None,
                settings: TranslationSettings::default(),
                result_available: false,
                source_path: None,
                source_storage: None,
                save_path: None,
                save_storage: None,
                created_at,
                updated_at: created_at,
            },
        }
    }

    async fn test_store() -> (PathBuf, JobStore) {
        let root = std::env::temp_dir().join(format!("ferryman-store-test-{}", Uuid::new_v4()));
        let store = JobStore::open(root.join("jobs.sqlite3")).await.unwrap();
        (root, store)
    }

    #[tokio::test]
    async fn initializes_wal_and_round_trips_owner_scoped_jobs() {
        let (root, store) = test_store().await;
        let id = Uuid::new_v4();
        let entry = test_entry("alice", id, 10, JobStatus::Queued);
        store.insert(entry.clone(), 10).await.unwrap();

        let loaded = store.get("alice".to_string(), id).await.unwrap().unwrap();
        assert_eq!(loaded.record.id, id);
        assert_eq!(loaded.record.completed, 3);
        assert!(store.get("bob".to_string(), id).await.unwrap().is_none());
        let journal_mode = store
            .call(|connection| {
                connection
                    .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        assert_eq!(journal_mode, "wal");

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn ignores_legacy_job_json_files() {
        let root = std::env::temp_dir().join(format!("ferryman-legacy-test-{}", Uuid::new_v4()));
        let legacy_dir = root.join("users/alice/jobs/legacy");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(legacy_dir.join("job.json"), br#"{"status":"queued"}"#).unwrap();

        let store = JobStore::open(root.join("jobs.sqlite3")).await.unwrap();
        let page = store
            .list_page("alice".to_string(), None, None, 10)
            .await
            .unwrap();
        assert!(page.jobs.is_empty());
        assert_eq!(page.total, 0);

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reopens_database_and_recovers_every_interrupted_state() {
        let (root, store) = test_store().await;
        let queued_id = Uuid::new_v4();
        store
            .insert(test_entry("alice", queued_id, 1, JobStatus::Queued), 10)
            .await
            .unwrap();
        let claimed = store.claim(queued_id, 2).await.unwrap().unwrap();
        assert_eq!(claimed.record.status, JobStatus::StartingModel);
        assert!(store.claim(queued_id, 3).await.unwrap().is_none());

        let waiting_id = Uuid::new_v4();
        let translating_id = Uuid::new_v4();
        let writing_id = Uuid::new_v4();
        let completed_id = Uuid::new_v4();
        store
            .insert(test_entry("alice", waiting_id, 2, JobStatus::Queued), 10)
            .await
            .unwrap();
        store
            .insert(
                test_entry("alice", translating_id, 2, JobStatus::Translating),
                10,
            )
            .await
            .unwrap();
        store
            .insert(test_entry("alice", writing_id, 3, JobStatus::Writing), 10)
            .await
            .unwrap();
        store
            .insert(
                test_entry("alice", completed_id, 4, JobStatus::Completed),
                10,
            )
            .await
            .unwrap();

        drop(store);
        let store = JobStore::open(root.join("jobs.sqlite3")).await.unwrap();

        let recovered = store.recover_nonterminal(5).await.unwrap();
        assert_eq!(recovered.len(), 4);
        assert!(recovered
            .iter()
            .all(|entry| entry.record.status == JobStatus::Queued));
        assert!(recovered
            .iter()
            .any(|entry| entry.record.id == waiting_id && entry.record.updated_at == 2));
        assert!(recovered
            .iter()
            .filter(|entry| entry.record.id != waiting_id)
            .all(|entry| entry.record.updated_at == 5));
        assert!(store
            .get("alice".to_string(), completed_id)
            .await
            .unwrap()
            .is_some_and(|entry| entry.record.status == JobStatus::Completed));

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cursor_pagination_is_stable_and_only_terminal_jobs_can_be_deleted() {
        let (root, store) = test_store().await;
        let ids = [
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        ];
        for id in ids {
            store
                .insert(test_entry("alice", id, 10, JobStatus::Queued), 10)
                .await
                .unwrap();
        }
        let first = store
            .list_page("alice".to_string(), None, None, 2)
            .await
            .unwrap();
        assert_eq!(first.total, 3);
        assert_eq!(
            first.jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );
        let second = store
            .list_page("alice".to_string(), None, first.next_cursor, 2)
            .await
            .unwrap();
        assert_eq!(
            second.jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![ids[0]]
        );
        assert!(!store
            .delete_terminal("alice".to_string(), ids[0])
            .await
            .unwrap());

        let mut completed = store
            .get("alice".to_string(), ids[0])
            .await
            .unwrap()
            .unwrap();
        completed.record.status = JobStatus::Completed;
        completed.record.result_available = true;
        store.update(completed).await.unwrap();
        let mut translating = store
            .get("alice".to_string(), ids[1])
            .await
            .unwrap()
            .unwrap();
        translating.record.status = JobStatus::Translating;
        store.update(translating).await.unwrap();

        let failed_id = Uuid::new_v4();
        store
            .insert(test_entry("alice", failed_id, 11, JobStatus::Failed), 10)
            .await
            .unwrap();

        for (phase, expected) in [
            (JobPhase::Queued, ids[2]),
            (JobPhase::InProgress, ids[1]),
            (JobPhase::Completed, ids[0]),
            (JobPhase::Failed, failed_id),
        ] {
            let page = store
                .list_page("alice".to_string(), Some(phase), None, 10)
                .await
                .unwrap();
            assert_eq!(page.total, 1);
            assert_eq!(page.jobs[0].id, expected);
        }

        assert!(store
            .delete_terminal("alice".to_string(), ids[0])
            .await
            .unwrap());
        assert!(store
            .get("alice".to_string(), ids[0])
            .await
            .unwrap()
            .is_none());

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retry_requeues_only_owned_failed_jobs_and_resets_progress() {
        let (root, store) = test_store().await;
        let failed_id = Uuid::new_v4();
        let mut failed = test_entry("alice", failed_id, 10, JobStatus::Failed);
        failed.record.error = Some("temporary inference failure".to_string());
        failed.record.failed_segments = 9;
        failed.record.result_available = true;
        store.insert(failed, 10).await.unwrap();

        assert!(matches!(
            store
                .retry_failed("bob".to_string(), failed_id, 20, 10)
                .await
                .unwrap(),
            RetryJobOutcome::NotFound
        ));

        let retried = match store
            .retry_failed("alice".to_string(), failed_id, 20, 10)
            .await
            .unwrap()
        {
            RetryJobOutcome::Retried(entry) => entry,
            _ => panic!("failed job should be requeued"),
        };
        assert_eq!(retried.record.status, JobStatus::Queued);
        assert_eq!(retried.record.total, 0);
        assert_eq!(retried.record.completed, 0);
        assert_eq!(retried.record.translated, 0);
        assert_eq!(retried.record.failed_segments, 0);
        assert!(retried.record.error.is_none());
        assert!(!retried.record.result_available);
        assert_eq!(retried.record.created_at, 20);
        assert_eq!(retried.record.updated_at, 20);

        assert!(matches!(
            store
                .retry_failed("alice".to_string(), failed_id, 30, 10)
                .await
                .unwrap(),
            RetryJobOutcome::NotFailed
        ));

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn expired_terminal_lists_and_deletes_only_old_terminal_rows() {
        let (root, store) = test_store().await;
        // Terminal + old (updated_at = created_at = 1) → swept.
        let old_done = Uuid::new_v4();
        store
            .insert(test_entry("alice", old_done, 1, JobStatus::Completed), 10)
            .await
            .unwrap();
        // Terminal + fresh → kept.
        let fresh_done = Uuid::new_v4();
        store
            .insert(
                test_entry("alice", fresh_done, 100, JobStatus::Completed),
                10,
            )
            .await
            .unwrap();
        // Nonterminal + old → kept.
        let old_live = Uuid::new_v4();
        store
            .insert(test_entry("alice", old_live, 1, JobStatus::Translating), 10)
            .await
            .unwrap();

        let expired = store.expired_terminal(50, 10).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].record.id, old_done);
        assert_eq!(store.delete_terminal_ids(vec![old_done]).await.unwrap(), 1);
        // Deleting again (already gone, or nonterminal) removes nothing.
        assert_eq!(
            store
                .delete_terminal_ids(vec![old_done, old_live])
                .await
                .unwrap(),
            0
        );
        assert!(store
            .get("alice".to_string(), fresh_done)
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get("alice".to_string(), old_live)
            .await
            .unwrap()
            .is_some());

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retry_respects_the_nonterminal_job_limit() {
        let (root, store) = test_store().await;
        let queued_id = Uuid::new_v4();
        let failed_id = Uuid::new_v4();
        store
            .insert(test_entry("alice", queued_id, 1, JobStatus::Queued), 10)
            .await
            .unwrap();
        store
            .insert(test_entry("alice", failed_id, 2, JobStatus::Failed), 10)
            .await
            .unwrap();

        assert!(matches!(
            store
                .retry_failed("alice".to_string(), failed_id, 3, 1)
                .await
                .unwrap(),
            RetryJobOutcome::AtLimit
        ));
        assert!(store
            .get("alice".to_string(), failed_id)
            .await
            .unwrap()
            .is_some_and(|entry| entry.record.status == JobStatus::Failed));

        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
