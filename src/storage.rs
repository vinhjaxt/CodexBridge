use std::{
    path::Path,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

pub const MEMORY_KEY_MAX_BYTES: usize = 256;
pub const MEMORY_VALUE_MAX_BYTES: usize = 64 * 1024;
pub const MEMORY_MAX_ENTRIES: usize = 64;
pub const MEMORY_MAX_TOTAL_BYTES: usize = 64 * 1024;
pub const MEMORY_ARCHIVE_VALUE_MAX_BYTES: usize = 256 * 1024;
pub const MEMORY_ARCHIVE_MAX_ENTRIES: usize = 4096;
pub const MEMORY_ARCHIVE_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub const MEMORY_RECALL_MAX_ENTRIES: usize = 128;
pub const MEMORY_RECALL_MAX_BYTES: usize = 1024 * 1024;

pub const PLAN_MAX_ITEMS: usize = 100;
pub const PLAN_ITEM_MAX_BYTES: usize = 4096;
pub const PLAN_EXPLANATION_MAX_BYTES: usize = 16 * 1024;
pub const PLAN_MAX_TOTAL_BYTES: usize = 256 * 1024;
const PLAN_STORAGE_MAX_BYTES: usize = 512 * 1024;
const STORAGE_SCHEMA_VERSION: i64 = 5;
const STORAGE_READ_CONNECTIONS: usize = 4;
const STORAGE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub const TASK_MAX_ENTRIES: usize = 500;
pub const TASK_ID_MAX_BYTES: usize = 256;
pub const TASK_TITLE_MAX_BYTES: usize = 512;
pub const TASK_DETAILS_MAX_BYTES: usize = 16 * 1024;
pub const TASK_PARENT_MAX_BYTES: usize = 256;
pub const TASK_LIST_OUTPUT_MAX: usize = 100;
pub const TASK_AUDIT_OUTPUT_MAX: usize = 20;

#[derive(Clone)]
pub struct Storage {
    inner: Arc<StorageInner>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnRefCommit<'a> {
    pub turn_ref: &'a str,
    pub parent_turn_ref: Option<&'a str>,
    pub force_full_brief: bool,
    pub instruction_hash: &'a str,
    pub state_hash: &'a str,
    pub subject_key: &'a str,
    pub brief_snapshot: &'a str,
    pub state_snapshot: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnRefCommitOutcome {
    pub turn_ref: String,
    pub parent_turn_ref: Option<String>,
    pub parent_native_key: Option<String>,
    pub effective_key: String,
    pub project_alias: Option<String>,
    pub parent_instruction_hash: Option<String>,
    pub parent_state_hash: Option<String>,
    pub instruction_hash: String,
    pub state_hash: String,
    pub brief_snapshot: Option<String>,
    pub state_snapshot: Option<String>,
    pub reused_existing_turn: bool,
}

struct StorageInner {
    writer: StorageWriter,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

#[derive(Clone)]
struct StorageWriter {
    sender: mpsc::Sender<WriteJob>,
}

impl StorageWriter {
    fn call<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.sender
            .send(Box::new(move |connection| {
                let _ = result_tx.send(operation(connection));
            }))
            .map_err(|_| AppError::new("STORAGE_ERROR", "storage writer is unavailable"))?;
        result_rx
            .recv()
            .map_err(|_| AppError::new("STORAGE_ERROR", "storage writer stopped unexpectedly"))?
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub title: String,
    pub status: String,
    pub details: Option<String>,
    pub parent_task: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItemRecord {
    pub step: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRecord {
    pub explanation: Option<String>,
    pub items: Vec<PlanItemRecord>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryRecord {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryPage {
    pub notes: Vec<MemoryRecord>,
    pub total: usize,
    pub offset: usize,
    pub truncated: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TaskSummary {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub total: usize,
}

fn input_too_large(message: impl Into<String>) -> AppError {
    AppError::new("INPUT_TOO_LARGE", message)
}

fn resource_limit(message: impl Into<String>) -> AppError {
    AppError::new("RESOURCE_LIMIT_EXCEEDED", message)
}

fn validate_memory_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "memory key must not be empty",
        ));
    }
    if key.len() > MEMORY_KEY_MAX_BYTES {
        return Err(input_too_large(format!(
            "memory key exceeds {MEMORY_KEY_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_archive_value(value: &str) -> Result<()> {
    if value.len() > MEMORY_ARCHIVE_VALUE_MAX_BYTES {
        return Err(input_too_large(format!(
            "archive memory value exceeds {MEMORY_ARCHIVE_VALUE_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn migrate_v4_to_v5(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE memory_archive(project_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(project_key,key));",
    )?;

    let mut projects_statement =
        transaction.prepare("SELECT DISTINCT project_key FROM memories ORDER BY project_key")?;
    let projects = projects_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(projects_statement);

    for project in projects {
        let mut statement = transaction.prepare(
            "SELECT key,value,updated_at FROM memories WHERE project_key=?1 ORDER BY updated_at DESC,key ASC",
        )?;
        let rows = statement
            .query_map([&project], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let mut kept_entries = 0usize;
        let mut kept_bytes = 0usize;
        for (key, value, updated_at) in rows {
            let entry_bytes = key.len().saturating_add(value.len());
            let keep_active = value.len() <= MEMORY_VALUE_MAX_BYTES
                && kept_entries < MEMORY_MAX_ENTRIES
                && kept_bytes.saturating_add(entry_bytes) <= MEMORY_MAX_TOTAL_BYTES;
            if keep_active {
                kept_entries += 1;
                kept_bytes = kept_bytes.saturating_add(entry_bytes);
                continue;
            }

            validate_archive_value(&value)?;
            transaction.execute(
                "INSERT INTO memory_archive(project_key,key,value,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(project_key,key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                params![&project, &key, &value, &updated_at],
            )?;
            transaction.execute(
                "DELETE FROM memories WHERE project_key=?1 AND key=?2",
                params![&project, &key],
            )?;
        }
    }

    transaction.pragma_update(None, "user_version", STORAGE_SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn validate_task_text(task: &TaskRecord) -> Result<()> {
    if !matches!(
        task.status.as_str(),
        "pending" | "in_progress" | "completed" | "failed" | "cancelled"
    ) {
        return Err(AppError::new("INVALID_INPUT", "invalid task status"));
    }
    validate_task_id(&task.id)?;
    if task.title.trim().is_empty() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "task title must not be empty",
        ));
    }
    if task.title.len() > TASK_TITLE_MAX_BYTES {
        return Err(input_too_large(format!(
            "task title exceeds {TASK_TITLE_MAX_BYTES} bytes"
        )));
    }
    if task
        .details
        .as_ref()
        .is_some_and(|value| value.len() > TASK_DETAILS_MAX_BYTES)
    {
        return Err(input_too_large(format!(
            "task details exceed {TASK_DETAILS_MAX_BYTES} bytes"
        )));
    }
    if task
        .parent_task
        .as_ref()
        .is_some_and(|value| value.len() > TASK_PARENT_MAX_BYTES)
    {
        return Err(input_too_large(format!(
            "parent task id exceeds {TASK_PARENT_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_task_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(AppError::new("INVALID_INPUT", "task id must not be empty"));
    }
    if id.len() > TASK_ID_MAX_BYTES {
        return Err(input_too_large(format!(
            "task id exceeds {TASK_ID_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_plan(explanation: Option<&str>, items: &[PlanItemRecord]) -> Result<()> {
    if items.len() > PLAN_MAX_ITEMS {
        return Err(resource_limit(format!(
            "plan exceeds {PLAN_MAX_ITEMS} items"
        )));
    }
    if explanation.is_some_and(|value| value.len() > PLAN_EXPLANATION_MAX_BYTES) {
        return Err(input_too_large(format!(
            "plan explanation exceeds {PLAN_EXPLANATION_MAX_BYTES} bytes"
        )));
    }
    let mut total_bytes = explanation.map_or(0, str::len);
    let mut in_progress = 0usize;
    for item in items {
        if item.step.trim().is_empty() {
            return Err(AppError::new(
                "INVALID_INPUT",
                "plan steps must not be empty",
            ));
        }
        if item.step.len() > PLAN_ITEM_MAX_BYTES {
            return Err(input_too_large(format!(
                "plan step exceeds {PLAN_ITEM_MAX_BYTES} bytes"
            )));
        }
        if !matches!(
            item.status.as_str(),
            "pending" | "in_progress" | "completed"
        ) {
            return Err(AppError::new("INVALID_INPUT", "invalid plan item status"));
        }
        in_progress += usize::from(item.status == "in_progress");
        total_bytes = total_bytes
            .saturating_add(item.step.len())
            .saturating_add(item.status.len());
    }
    if in_progress > 1 {
        return Err(AppError::new(
            "INVALID_INPUT",
            "plan may contain at most one in_progress item",
        ));
    }
    if total_bytes > PLAN_MAX_TOTAL_BYTES {
        return Err(input_too_large(format!(
            "plan exceeds {PLAN_MAX_TOTAL_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut writer_connection = Connection::open(path)?;
        #[cfg(unix)]
        {
            let mut permissions = std::fs::metadata(path)?.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(path, permissions)?;
        }
        writer_connection.busy_timeout(STORAGE_BUSY_TIMEOUT)?;
        writer_connection.pragma_update(None, "journal_mode", "WAL")?;
        writer_connection.pragma_update(None, "synchronous", "FULL")?;
        let schema_version: i64 =
            writer_connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match schema_version {
            0 => writer_connection.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE aliases(alias TEXT PRIMARY KEY COLLATE NOCASE, effective_key TEXT NOT NULL);
                 CREATE TABLE bindings(native_key TEXT PRIMARY KEY, effective_key TEXT NOT NULL);
                 CREATE TABLE memories(project_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(project_key,key));
                 CREATE TABLE memory_archive(project_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(project_key,key));
                 CREATE TABLE plans(project_key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
                 CREATE TABLE tasks(project_key TEXT NOT NULL, id TEXT NOT NULL, title TEXT NOT NULL, status TEXT NOT NULL, details TEXT, parent_task TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, started_at TEXT, completed_at TEXT, PRIMARY KEY(project_key,id));
                 CREATE TABLE turn_refs(id INTEGER PRIMARY KEY AUTOINCREMENT, turn_ref TEXT NOT NULL UNIQUE, native_key TEXT NOT NULL, effective_key TEXT NOT NULL, subject_key TEXT NOT NULL, parent_turn_ref TEXT, instruction_hash TEXT NOT NULL, state_hash TEXT NOT NULL, brief_snapshot TEXT, state_snapshot TEXT, created_at TEXT NOT NULL);
                 CREATE INDEX turn_refs_native_id ON turn_refs(native_key,id DESC);
                 CREATE UNIQUE INDEX turn_refs_native_parent_unique ON turn_refs(native_key,parent_turn_ref) WHERE parent_turn_ref IS NOT NULL;
                 PRAGMA user_version=5;
                 COMMIT;",
            )?,
            4 => migrate_v4_to_v5(&mut writer_connection)?,
            STORAGE_SCHEMA_VERSION => {}
            version => {
                return Err(AppError::new(
                    "STORAGE_SCHEMA_UNSUPPORTED",
                    format!(
                        "database schema version {version} is unsupported; CodexBridge currently requires a fresh schema version {STORAGE_SCHEMA_VERSION} database"
                    ),
                ));
            }
        }
        validate_schema_v5(&writer_connection)?;

        let mut readers = Vec::with_capacity(STORAGE_READ_CONNECTIONS);
        for _ in 0..STORAGE_READ_CONNECTIONS {
            let reader = Connection::open(path)?;
            reader.busy_timeout(STORAGE_BUSY_TIMEOUT)?;
            reader.pragma_update(None, "query_only", "ON")?;
            readers.push(Mutex::new(reader));
        }

        let (writer_tx, writer_rx) = mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("codexbridge-sqlite-writer".to_owned())
            .spawn(move || {
                for job in writer_rx {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&mut writer_connection);
                    }))
                    .is_err()
                    {
                        tracing::error!("storage writer job panicked; keeping writer thread alive");
                    }
                }
            })
            .map_err(|error| {
                AppError::new(
                    "STORAGE_ERROR",
                    format!("failed to start storage writer: {error}"),
                )
            })?;

        Ok(Self {
            inner: Arc::new(StorageInner {
                writer: StorageWriter { sender: writer_tx },
                readers,
                next_reader: AtomicUsize::new(0),
            }),
        })
    }

    fn with_read<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let len = self.inner.readers.len();
        let start = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % len;
        for offset in 0..len {
            let index = (start + offset) % len;
            match self.inner.readers[index].try_lock() {
                Ok(connection) => return operation(&connection),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Poisoned(_)) => {
                    return Err(AppError::new("STORAGE_ERROR", "storage read lock poisoned"));
                }
            }
        }
        let connection = self.inner.readers[start]
            .lock()
            .map_err(|_| AppError::new("STORAGE_ERROR", "storage read lock poisoned"))?;
        operation(&connection)
    }

    fn with_write<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        self.inner.writer.call(operation)
    }

    pub fn effective_binding(&self, native_key: &str) -> Result<Option<String>> {
        self.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT effective_key FROM bindings WHERE native_key=?1",
                    [native_key],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn alias_for_effective(&self, effective_key: &str) -> Result<Option<String>> {
        self.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT alias FROM aliases WHERE effective_key=?1 ORDER BY alias LIMIT 1",
                    [effective_key],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn effective_for_alias(&self, alias: &str) -> Result<Option<String>> {
        self.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT effective_key FROM aliases WHERE alias=?1",
                    [alias],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn commit_initialization(
        &self,
        native_key: &str,
        effective_key: &str,
        alias: Option<&str>,
        expected_binding: Option<&str>,
        expected_alias_binding: Option<&str>,
    ) -> Result<()> {
        self.commit_initialization_inner(
            native_key,
            effective_key,
            alias,
            expected_binding,
            expected_alias_binding,
            None,
        )
        .map(|_| ())
    }

    pub(crate) fn commit_initialization_with_turn_ref(
        &self,
        native_key: &str,
        effective_key: &str,
        alias: Option<&str>,
        expected_binding: Option<&str>,
        expected_alias_binding: Option<&str>,
        turn: TurnRefCommit<'_>,
    ) -> Result<TurnRefCommitOutcome> {
        self.commit_initialization_inner(
            native_key,
            effective_key,
            alias,
            expected_binding,
            expected_alias_binding,
            Some((
                turn.turn_ref,
                turn.parent_turn_ref,
                turn.force_full_brief,
                turn.instruction_hash,
                turn.state_hash,
                turn.subject_key,
                turn.brief_snapshot,
                turn.state_snapshot,
            )),
        )
        .and_then(|outcome| {
            outcome.ok_or_else(|| {
                AppError::new(
                    "STORAGE_ERROR",
                    "turn initialization committed without a turn reference",
                )
            })
        })
    }

    #[allow(clippy::type_complexity)]
    fn commit_initialization_inner(
        &self,
        native_key: &str,
        effective_key: &str,
        alias: Option<&str>,
        expected_binding: Option<&str>,
        expected_alias_binding: Option<&str>,
        turn_ref: Option<(
            &str,
            Option<&str>,
            bool,
            &str,
            &str,
            &str,
            &str,
            Option<&str>,
        )>,
    ) -> Result<Option<TurnRefCommitOutcome>> {
        let native_key = native_key.to_owned();
        let effective_key = effective_key.to_owned();
        let alias = alias.map(str::to_owned);
        let expected_binding = expected_binding.map(str::to_owned);
        let expected_alias_binding = expected_alias_binding.map(str::to_owned);
        let turn_ref = turn_ref.map(
            |(
                turn_ref,
                parent_turn_ref,
                force_full_brief,
                instruction_hash,
                state_hash,
                subject_key,
                brief_snapshot,
                state_snapshot,
            )| {
                (
                    turn_ref.to_owned(),
                    parent_turn_ref.map(str::to_owned),
                    force_full_brief,
                    instruction_hash.to_owned(),
                    state_hash.to_owned(),
                    subject_key.to_owned(),
                    brief_snapshot.to_owned(),
                    state_snapshot.map(str::to_owned),
                )
            },
        );
        self.with_write(move |connection| {
            let transaction = connection.transaction()?;
            if let Some((_, Some(parent_turn_ref), _, _, _, subject_key, _, _)) = turn_ref.as_ref()
                && let Some(existing) = transaction
                    .query_row(
                        "SELECT turn_ref,effective_key,instruction_hash,state_hash,brief_snapshot,state_snapshot FROM turn_refs WHERE native_key=?1 AND parent_turn_ref=?2",
                        params![&native_key, parent_turn_ref],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, Option<String>>(5)?,
                            ))
                        },
                    )
                    .optional()?
            {
                if existing.1 != effective_key {
                    return Err(AppError::new(
                        "TURN_PROJECT_MISMATCH",
                        "previous_turn_ref is already continued by this conversation in another project",
                    ));
                }
                let parent = transaction
                    .query_row(
                        "SELECT native_key,effective_key,subject_key,instruction_hash,state_hash FROM turn_refs WHERE turn_ref=?1",
                        [parent_turn_ref],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or_else(|| {
                        AppError::new("TURN_REF_NOT_FOUND", "previous_turn_ref does not exist")
                    })?;
                if parent.1 != effective_key {
                    return Err(AppError::new(
                        "TURN_PROJECT_MISMATCH",
                        "previous_turn_ref belongs to another effective project",
                    ));
                }
                if parent.2.as_str() != subject_key.as_str() {
                    return Err(AppError::new(
                        "TURN_REF_NOT_FOUND",
                        "previous_turn_ref is not available to this ChatGPT subject",
                    ));
                }
                  let project_alias = transaction
                      .query_row(
                          "SELECT alias FROM aliases WHERE effective_key=?1 ORDER BY alias LIMIT 1",
                          [&existing.1],
                          |row| row.get::<_, String>(0),
                      )
                      .optional()?;
                transaction.commit()?;
                return Ok(Some(TurnRefCommitOutcome {
                    turn_ref: existing.0,
                    parent_turn_ref: Some(parent_turn_ref.clone()),
                    parent_native_key: Some(parent.0),
                      effective_key: existing.1,
                      project_alias,
                    parent_instruction_hash: Some(parent.3),
                    parent_state_hash: Some(parent.4),
                    instruction_hash: existing.2,
                    state_hash: existing.3,
                    brief_snapshot: existing.4,
                    state_snapshot: existing.5,
                    reused_existing_turn: true,
                }));
            }

            let current_binding: Option<String> = transaction
                .query_row(
                    "SELECT effective_key FROM bindings WHERE native_key=?1",
                    [&native_key],
                    |row| row.get(0),
                )
                .optional()?;
            if current_binding.as_deref() != expected_binding.as_deref() {
                return Err(AppError::new(
                    "SERVER_BUSY",
                    "project initialization changed concurrently; retry initialization",
                ));
            }

            if turn_ref
                .as_ref()
                .is_some_and(|(_, parent_turn_ref, _, _, _, _, _, _)| parent_turn_ref.is_none())
                && current_binding.is_some()
            {
                let has_turn: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM turn_refs WHERE native_key=?1)",
                    [&native_key],
                    |row| row.get(0),
                )?;
                if has_turn {
                    return Err(AppError::new(
                        "PREVIOUS_TURN_REF_REQUIRED",
                        "this conversation already has turn history but no continuity parent was resolved",
                    ));
                }
            }

            let mut alias_needs_insert = false;
            let joining_existing_alias = if let Some(alias) = alias.as_deref() {
                let current_alias: Option<String> = transaction
                    .query_row(
                        "SELECT effective_key FROM aliases WHERE alias=?1",
                        [alias],
                        |row| row.get(0),
                    )
                    .optional()?;
                if current_alias.as_deref() != expected_alias_binding.as_deref() {
                    return Err(AppError::new(
                        "SERVER_BUSY",
                        "project alias changed concurrently; retry initialization",
                    ));
                }
                alias_needs_insert = current_alias.is_none();
                current_alias.is_some()
            } else {
                false
            };

            let inherited_existing_project = turn_ref
                .as_ref()
                .is_some_and(|(_, parent_turn_ref, _, _, _, _, _, _)| parent_turn_ref.is_some());
            if current_binding.is_none()
                && !joining_existing_alias
                && !inherited_existing_project
            {
                let colliding_effective: Option<String> = transaction
                    .query_row(
                        "SELECT effective_key FROM (SELECT effective_key FROM bindings UNION ALL SELECT effective_key FROM aliases) WHERE effective_key=?1 COLLATE NOCASE LIMIT 1",
                        [&effective_key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing) = colliding_effective {
                    return Err(AppError::new(
                        "PROJECT_PATH_COLLISION",
                        format!(
                            "new project key `{effective_key}` collides with existing project key `{existing}` on case-insensitive filesystems"
                        ),
                    ));
                }
            }

            if alias_needs_insert
                && let Some(alias) = alias.as_deref()
            {
                transaction.execute(
                    "INSERT INTO aliases(alias,effective_key) VALUES(?1,?2)",
                    params![alias, &effective_key],
                )?;
            }

            transaction.execute(
                "INSERT INTO bindings(native_key,effective_key) VALUES(?1,?2) ON CONFLICT(native_key) DO UPDATE SET effective_key=excluded.effective_key",
                params![&native_key, &effective_key],
            )?;
              let project_alias = transaction
                  .query_row(
                      "SELECT alias FROM aliases WHERE effective_key=?1 ORDER BY alias LIMIT 1",
                      [&effective_key],
                      |row| row.get::<_, String>(0),
                  )
                  .optional()?;
            let turn_outcome = if let Some((turn_ref, parent_turn_ref, force_full_brief, instruction_hash, state_hash, subject_key, brief_snapshot, state_snapshot)) = turn_ref.as_ref() {
                let (parent_native_key, parent_instruction_hash, parent_state_hash) = if let Some(parent_turn_ref) = parent_turn_ref.as_ref() {
                    let parent = transaction
                        .query_row(
                            "SELECT native_key,effective_key,subject_key,instruction_hash,state_hash FROM turn_refs WHERE turn_ref=?1",
                            [parent_turn_ref],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, String>(3)?,
                                    row.get::<_, String>(4)?,
                                ))
                            },
                        )
                        .optional()?
                        .ok_or_else(|| {
                            AppError::new("TURN_REF_NOT_FOUND", "previous_turn_ref does not exist")
                        })?;
                    if parent.1 != effective_key {
                        return Err(AppError::new(
                            "TURN_PROJECT_MISMATCH",
                            "previous_turn_ref belongs to another effective project",
                        ));
                    }
                    if parent.2.as_str() != subject_key.as_str() {
                        return Err(AppError::new(
                            "TURN_REF_NOT_FOUND",
                            "previous_turn_ref is not available to this ChatGPT subject",
                        ));
                    }
                    if current_binding.is_some() {
                        let latest: Option<String> = transaction
                            .query_row(
                                "SELECT turn_ref FROM turn_refs WHERE native_key=?1 ORDER BY id DESC LIMIT 1",
                                [&native_key],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if latest.as_deref() != Some(parent_turn_ref.as_str()) {
                            return Err(AppError::new(
                                "STALE_TURN_REF",
                                "previous_turn_ref is not the latest turn for this conversation",
                            ));
                        }
                    }
                    (Some(parent.0), Some(parent.3), Some(parent.4))
                } else {
                    (None, None, None)
                };
                let branched = parent_native_key
                    .as_deref()
                    .is_some_and(|parent_native| parent_native != native_key.as_str());
                let instructions_changed = *force_full_brief
                    || parent_turn_ref.is_none()
                    || branched
                    || parent_instruction_hash.as_deref() != Some(instruction_hash.as_str());
                let state_changed = *force_full_brief
                    || parent_turn_ref.is_none()
                    || branched
                    || parent_state_hash.as_deref() != Some(state_hash.as_str());
                let stored_brief = instructions_changed.then_some(brief_snapshot.as_str());
                let stored_state = (!instructions_changed && state_changed)
                    .then_some(state_snapshot.as_deref())
                    .flatten();
                // TODO(rewind): attach immutable workspace checkpoint metadata to
                // this turn reference. The reference chain is persisted now so a
                // future rewind implementation can add snapshots without changing
                // the model-visible token format.
                transaction.execute(
                    "INSERT INTO turn_refs(turn_ref,native_key,effective_key,subject_key,parent_turn_ref,instruction_hash,state_hash,brief_snapshot,state_snapshot,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![
                        turn_ref,
                        &native_key,
                        &effective_key,
                        subject_key,
                        parent_turn_ref.as_deref(),
                        instruction_hash,
                        state_hash,
                        stored_brief,
                        stored_state,
                        Utc::now().to_rfc3339()
                    ],
                )?;
                Some(TurnRefCommitOutcome {
                    turn_ref: turn_ref.clone(),
                    parent_turn_ref: parent_turn_ref.clone(),
                    parent_native_key,
                      effective_key: effective_key.clone(),
                      project_alias: project_alias.clone(),
                    parent_instruction_hash,
                    parent_state_hash,
                    instruction_hash: instruction_hash.clone(),
                    state_hash: state_hash.clone(),
                    brief_snapshot: stored_brief.map(str::to_owned),
                    state_snapshot: stored_state.map(str::to_owned),
                    reused_existing_turn: false,
                })
            } else {
                None
            };
            transaction.commit()?;
            Ok(turn_outcome)
        })
    }

    pub(crate) fn turn_ref_effective_for_subject(
        &self,
        turn_ref: &str,
        subject_key: &str,
    ) -> Result<Option<String>> {
        let turn_ref = turn_ref.to_owned();
        let subject_key = subject_key.to_owned();
        self.with_read(move |connection| {
            let row = connection
                .query_row(
                    "SELECT effective_key,subject_key FROM turn_refs WHERE turn_ref=?1",
                    [&turn_ref],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            Ok(row.and_then(|(effective, parent_subject)| {
                if parent_subject == subject_key {
                    Some(effective)
                } else {
                    None
                }
            }))
        })
    }

    pub(crate) fn continuity_parent_for_bound_native(
        &self,
        native_key: &str,
        requested_turn_ref: Option<&str>,
    ) -> Result<Option<String>> {
        let native_key = native_key.to_owned();
        let requested_turn_ref = requested_turn_ref.map(str::to_owned);
        self.with_read(move |connection| {
            if let Some(requested_turn_ref) = requested_turn_ref.as_deref() {
                let already_continued: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM turn_refs WHERE native_key=?1 AND parent_turn_ref=?2)",
                    params![&native_key, requested_turn_ref],
                    |row| row.get(0),
                )?;
                if already_continued {
                    return Ok(Some(requested_turn_ref.to_owned()));
                }
            }

            let latest = connection
                .query_row(
                    "SELECT turn_ref FROM turn_refs WHERE native_key=?1 ORDER BY id DESC LIMIT 1",
                    [&native_key],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if requested_turn_ref.as_deref() == latest.as_deref() {
                return Ok(requested_turn_ref);
            }
            Ok(latest)
        })
    }

    pub fn ensure_binding(&self, native_key: &str) -> Result<String> {
        let native_key = native_key.to_owned();
        self.with_write(move |connection| {
            if let Some(existing) = connection
                .query_row(
                    "SELECT effective_key FROM bindings WHERE native_key=?1",
                    [&native_key],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Ok(existing);
            }
            connection.execute(
                "INSERT INTO bindings(native_key,effective_key) VALUES(?1,?1)",
                [&native_key],
            )?;
            Ok(native_key)
        })
    }

    pub fn bind_alias(&self, native_key: &str, alias: &str) -> Result<(String, bool)> {
        let native_key = native_key.to_owned();
        let alias = alias.to_owned();
        self.with_write(move |connection| {
            let transaction = connection.transaction()?;
            let existing: Option<String> = transaction.query_row("SELECT effective_key FROM aliases WHERE alias=?1", [&alias], |row| row.get(0)).optional()?;
            let (effective, joined) = if let Some(existing) = existing {
                (existing, true)
            } else {
                let current: Option<String> = transaction.query_row("SELECT effective_key FROM bindings WHERE native_key=?1", [&native_key], |row| row.get(0)).optional()?;
                let effective = current.unwrap_or_else(|| native_key.clone());
                transaction.execute("INSERT INTO aliases(alias,effective_key) VALUES(?1,?2)", params![&alias, &effective])?;
                (effective, false)
            };
            transaction.execute("INSERT INTO bindings(native_key,effective_key) VALUES(?1,?2) ON CONFLICT(native_key) DO UPDATE SET effective_key=excluded.effective_key", params![&native_key, &effective])?;
            transaction.commit()?;
            Ok((effective, joined))
        })
    }

    pub fn memory_list(&self, project: &str) -> Result<Vec<String>> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT substr(key,1,?2) FROM memories WHERE project_key=?1 ORDER BY key LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![
                    project,
                    (MEMORY_KEY_MAX_BYTES + 1) as i64,
                    (MEMORY_MAX_ENTRIES + 1) as i64
                ],
                |row| row.get(0),
            )?;
            let mut keys = rows.collect::<rusqlite::Result<Vec<String>>>()?;
            if keys.len() > MEMORY_MAX_ENTRIES {
                keys.truncate(MEMORY_MAX_ENTRIES);
            }
            Ok(keys)
        })
    }

    pub fn memory_count(&self, project: &str) -> Result<usize> {
        self.with_read(|connection| {
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM memories WHERE project_key=?1",
                [project],
                |row| row.get(0),
            )?;
            Ok(count.max(0) as usize)
        })
    }

    pub fn memory_get(&self, project: &str, key: &str) -> Result<Option<String>> {
        validate_memory_key(key)?;
        self.with_read(|connection| {
            let length: Option<i64> = connection
                .query_row(
                    "SELECT length(CAST(value AS BLOB)) FROM memories WHERE project_key=?1 AND key=?2",
                    params![project, key],
                    |row| row.get(0),
                )
                .optional()?;
            if length.is_some_and(|length| length > MEMORY_VALUE_MAX_BYTES as i64) {
                return Err(resource_limit(
                    "stored memory value exceeds the current safe retrieval limit",
                ));
            }
            if length.is_none() {
                return Ok(None);
            }
            Ok(Some(connection.query_row(
                "SELECT value FROM memories WHERE project_key=?1 AND key=?2",
                params![project, key],
                |row| row.get(0),
            )?))
        })
    }

    pub fn memory_set(&self, project: &str, key: &str, value: &str) -> Result<()> {
        validate_memory_key(key)?;
        if value.len() > MEMORY_VALUE_MAX_BYTES {
            return Err(input_too_large(format!(
                "memory value exceeds {MEMORY_VALUE_MAX_BYTES} bytes"
            )));
        }
        let project = project.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_write(move |connection| {
            let transaction = connection.transaction()?;
            let (entry_count, total_bytes): (i64, i64) = transaction.query_row(
                "SELECT count(*),coalesce(sum(length(CAST(key AS BLOB))+length(CAST(value AS BLOB))),0) FROM memories WHERE project_key=?1",
                [&project],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let old_bytes: Option<i64> = transaction
                .query_row(
                    "SELECT length(CAST(key AS BLOB))+length(CAST(value AS BLOB)) FROM memories WHERE project_key=?1 AND key=?2",
                    params![&project, &key],
                    |row| row.get(0),
                )
                .optional()?;
            if old_bytes.is_none() && entry_count >= MEMORY_MAX_ENTRIES as i64 {
                return Err(resource_limit(format!(
                    "active project memory is limited to {MEMORY_MAX_ENTRIES} entries; archive durable history with remember scope=archive"
                )));
            }
            let next_total = total_bytes
                .saturating_sub(old_bytes.unwrap_or_default())
                .saturating_add((key.len() + value.len()) as i64);
            if next_total > MEMORY_MAX_TOTAL_BYTES as i64 {
                return Err(resource_limit(format!(
                    "active project memory exceeds the {MEMORY_MAX_TOTAL_BYTES}-byte aggregate limit; archive durable history with remember scope=archive"
                )));
            }
            transaction.execute("INSERT INTO memories(project_key,key,value,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(project_key,key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at", params![&project,&key,&value,Utc::now().to_rfc3339()])?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn memory_delete(&self, project: &str, key: &str) -> Result<bool> {
        validate_memory_key(key)?;
        let project = project.to_owned();
        let key = key.to_owned();
        self.with_write(move |connection| {
            Ok(connection.execute(
                "DELETE FROM memories WHERE project_key=?1 AND key=?2",
                params![&project, &key],
            )? > 0)
        })
    }

    pub fn memory_archive_get(&self, project: &str, key: &str) -> Result<Option<String>> {
        validate_memory_key(key)?;
        self.with_read(|connection| {
            let length: Option<i64> = connection
                .query_row(
                    "SELECT length(CAST(value AS BLOB)) FROM memory_archive WHERE project_key=?1 AND key=?2",
                    params![project, key],
                    |row| row.get(0),
                )
                .optional()?;
            if length.is_some_and(|length| length > MEMORY_ARCHIVE_VALUE_MAX_BYTES as i64) {
                return Err(resource_limit(
                    "stored archive memory value exceeds the current safe retrieval limit",
                ));
            }
            if length.is_none() {
                return Ok(None);
            }
            Ok(Some(connection.query_row(
                "SELECT value FROM memory_archive WHERE project_key=?1 AND key=?2",
                params![project, key],
                |row| row.get(0),
            )?))
        })
    }

    pub fn memory_archive_set(&self, project: &str, key: &str, value: &str) -> Result<()> {
        validate_memory_key(key)?;
        validate_archive_value(value)?;
        let project = project.to_owned();
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_write(move |connection| {
            let transaction = connection.transaction()?;
            let (entry_count, total_bytes): (i64, i64) = transaction.query_row(
                "SELECT count(*),coalesce(sum(length(CAST(key AS BLOB))+length(CAST(value AS BLOB))),0) FROM memory_archive WHERE project_key=?1",
                [&project],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let old_bytes: Option<i64> = transaction
                .query_row(
                    "SELECT length(CAST(key AS BLOB))+length(CAST(value AS BLOB)) FROM memory_archive WHERE project_key=?1 AND key=?2",
                    params![&project, &key],
                    |row| row.get(0),
                )
                .optional()?;
            if old_bytes.is_none() && entry_count >= MEMORY_ARCHIVE_MAX_ENTRIES as i64 {
                return Err(resource_limit(format!(
                    "project archive is limited to {MEMORY_ARCHIVE_MAX_ENTRIES} entries"
                )));
            }
            let next_total = total_bytes
                .saturating_sub(old_bytes.unwrap_or_default())
                .saturating_add((key.len() + value.len()) as i64);
            if next_total > MEMORY_ARCHIVE_MAX_TOTAL_BYTES as i64 {
                return Err(resource_limit(format!(
                    "project archive exceeds the {MEMORY_ARCHIVE_MAX_TOTAL_BYTES}-byte aggregate limit"
                )));
            }
            transaction.execute(
                "INSERT INTO memory_archive(project_key,key,value,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(project_key,key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                params![&project,&key,&value,Utc::now().to_rfc3339()],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn memory_archive_delete(&self, project: &str, key: &str) -> Result<bool> {
        validate_memory_key(key)?;
        let project = project.to_owned();
        let key = key.to_owned();
        self.with_write(move |connection| {
            Ok(connection.execute(
                "DELETE FROM memory_archive WHERE project_key=?1 AND key=?2",
                params![&project, &key],
            )? > 0)
        })
    }

    pub fn memory_archive_recall_page_from_snapshot(
        &self,
        project: &str,
        offset: usize,
        requested: usize,
        expected_snapshot_hash: Option<&str>,
    ) -> Result<(MemoryPage, String)> {
        let requested = requested.min(MEMORY_RECALL_MAX_ENTRIES);
        if requested == 0 {
            return Err(AppError::new(
                "INVALID_INPUT",
                "memory page size must be positive",
            ));
        }
        self.with_read(|connection| {
            connection.execute_batch("BEGIN DEFERRED")?;
            let result = (|| {
                let mut hash_statement = connection.prepare(
                    "SELECT key,value FROM memory_archive WHERE project_key=?1 ORDER BY key",
                )?;
                let mut hash_rows = hash_statement.query([project])?;
                let mut hasher = Sha256::new();
                hasher.update(b"codexbridge-memory-archive-v1\0");
                while let Some(row) = hash_rows.next()? {
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    validate_memory_key(&key)?;
                    validate_archive_value(&value)?;
                    hasher.update((key.len() as u64).to_be_bytes());
                    hasher.update(key.as_bytes());
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value.as_bytes());
                }
                drop(hash_rows);
                drop(hash_statement);
                let snapshot_hash = format!("{:x}", hasher.finalize());
                if expected_snapshot_hash.is_some_and(|expected| expected != snapshot_hash) {
                    return Err(AppError::new(
                        "PAGINATION_STALE",
                        "project archive changed during pagination; restart recall from offset=0",
                    ));
                }

                let total: i64 = connection.query_row(
                    "SELECT count(*) FROM memory_archive WHERE project_key=?1",
                    [project],
                    |row| row.get(0),
                )?;
                let total = total.max(0) as usize;
                if offset > total {
                    return Err(AppError::new(
                        "INVALID_INPUT",
                        "memory offset is outside the available archive notes",
                    ));
                }
                let mut statement = connection.prepare(
                    "SELECT key,value FROM memory_archive WHERE project_key=?1 ORDER BY key LIMIT ?2 OFFSET ?3",
                )?;
                let mut rows = statement.query(params![project, (requested + 1) as i64, offset as i64])?;
                let mut notes = Vec::new();
                let mut retained_bytes = 0usize;
                let mut more = false;
                while let Some(row) = rows.next()? {
                    if notes.len() >= requested {
                        more = true;
                        break;
                    }
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    let next_bytes = retained_bytes
                        .saturating_add(key.len())
                        .saturating_add(value.len());
                    if next_bytes > MEMORY_RECALL_MAX_BYTES {
                        more = true;
                        break;
                    }
                    retained_bytes = next_bytes;
                    notes.push(MemoryRecord { key, value });
                }
                more |= offset.saturating_add(notes.len()) < total;
                let next_offset = more.then_some(offset.saturating_add(notes.len()));
                Ok((
                    MemoryPage {
                        notes,
                        total,
                        offset,
                        truncated: more,
                        next_offset,
                    },
                    snapshot_hash,
                ))
            })();
            match result {
                Ok(value) => {
                    connection.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(error) => {
                    let _ = connection.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })
    }

    pub fn memory_recall_page(&self, project: &str) -> Result<MemoryPage> {
        self.memory_recall_page_from(project, 0, MEMORY_RECALL_MAX_ENTRIES)
    }

    pub fn memory_recall_page_from(
        &self,
        project: &str,
        offset: usize,
        requested: usize,
    ) -> Result<MemoryPage> {
        self.memory_recall_page_from_snapshot(project, offset, requested, None)
            .map(|(page, _)| page)
    }

    /// Return one lexicographically sorted positional page together with a
    /// semantic snapshot hash. Supplying the hash from the first page makes a
    /// continuation fail explicitly if memory changed, rather than silently
    /// repeating/skipping rows after OFFSET positions shifted.
    pub fn memory_recall_page_from_snapshot(
        &self,
        project: &str,
        offset: usize,
        requested: usize,
        expected_snapshot_hash: Option<&str>,
    ) -> Result<(MemoryPage, String)> {
        let requested = requested.min(MEMORY_RECALL_MAX_ENTRIES);
        if requested == 0 {
            return Err(AppError::new(
                "INVALID_INPUT",
                "memory page size must be positive",
            ));
        }
        self.with_read(|connection| {
            connection.execute_batch("BEGIN DEFERRED")?;
            let result = (|| {
                let mut hash_statement = connection
                    .prepare("SELECT key,value FROM memories WHERE project_key=?1 ORDER BY key")?;
                let mut hash_rows = hash_statement.query([project])?;
                let mut hasher = Sha256::new();
                hasher.update(b"codexbridge-memory-v1\0");
                while let Some(row) = hash_rows.next()? {
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    validate_memory_key(&key)?;
                    if value.len() > MEMORY_VALUE_MAX_BYTES {
                        return Err(resource_limit(
                            "stored memory value exceeds the current safe hashing limit",
                        ));
                    }
                    hasher.update((key.len() as u64).to_be_bytes());
                    hasher.update(key.as_bytes());
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value.as_bytes());
                }
                drop(hash_rows);
                drop(hash_statement);
                let snapshot_hash = format!("{:x}", hasher.finalize());
                if expected_snapshot_hash.is_some_and(|expected| expected != snapshot_hash) {
                    return Err(AppError::new(
                        "PAGINATION_STALE",
                        "project memory changed during pagination; restart recall from offset=0",
                    ));
                }

                let total: i64 = connection.query_row(
                    "SELECT count(*) FROM memories WHERE project_key=?1",
                    [project],
                    |row| row.get(0),
                )?;
                let total = total.max(0) as usize;
                if offset > total {
                    return Err(AppError::new(
                        "INVALID_INPUT",
                        "memory offset is outside the available notes",
                    ));
                }
                let mut statement = connection.prepare(
                    "SELECT substr(key,1,?2),substr(value,1,?3),length(CAST(value AS BLOB)) FROM memories WHERE project_key=?1 ORDER BY key LIMIT ?4 OFFSET ?5",
                )?;
                let mut rows = statement.query(params![
                    project,
                    (MEMORY_KEY_MAX_BYTES + 1) as i64,
                    (MEMORY_VALUE_MAX_BYTES + 1) as i64,
                    (requested + 1) as i64,
                    offset as i64,
                ])?;
                let mut notes = Vec::new();
                let mut retained_bytes = 0usize;
                let mut more = false;
                while let Some(row) = rows.next()? {
                    if notes.len() >= requested {
                        more = true;
                        break;
                    }
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    let original_value_bytes: i64 = row.get(2)?;
                    if key.len() > MEMORY_KEY_MAX_BYTES
                        || original_value_bytes > MEMORY_VALUE_MAX_BYTES as i64
                        || retained_bytes.saturating_add(key.len()).saturating_add(value.len())
                            > MEMORY_RECALL_MAX_BYTES
                    {
                        more = true;
                        break;
                    }
                    retained_bytes = retained_bytes
                        .saturating_add(key.len())
                        .saturating_add(value.len());
                    notes.push(MemoryRecord { key, value });
                }
                more |= offset.saturating_add(notes.len()) < total;
                let next_offset = more.then_some(offset.saturating_add(notes.len()));
                Ok((
                    MemoryPage {
                        notes,
                        total,
                        offset,
                        truncated: more,
                        next_offset,
                    },
                    snapshot_hash,
                ))
            })();
            match result {
                Ok(value) => {
                    connection.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(error) => {
                    let _ = connection.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })
    }

    /// Hash every semantic memory key/value pair for a project in stable key
    /// order. Presentation paging must not affect turn-state change detection.
    pub fn memory_semantic_hash(&self, project: &str) -> Result<String> {
        self.with_read(|connection| {
            let mut statement = connection
                .prepare("SELECT key,value FROM memories WHERE project_key=?1 ORDER BY key")?;
            let mut rows = statement.query([project])?;
            let mut hasher = Sha256::new();
            hasher.update(b"codexbridge-memory-v1\0");
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                validate_memory_key(&key)?;
                if value.len() > MEMORY_VALUE_MAX_BYTES {
                    return Err(resource_limit(
                        "stored memory value exceeds the current safe hashing limit",
                    ));
                }
                hasher.update((key.len() as u64).to_be_bytes());
                hasher.update(key.as_bytes());
                hasher.update((value.len() as u64).to_be_bytes());
                hasher.update(value.as_bytes());
            }
            Ok(format!("{:x}", hasher.finalize()))
        })
    }

    /// Read the complete hard-bounded active memory, its semantic hash, and the
    /// current plan from one SQLite read transaction. Archive/history is not
    /// part of turn state. WAL permits concurrent writers, while the explicit
    /// transaction keeps every component pinned to the same database snapshot.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn project_state_read_with_hook<F>(
        &self,
        project: &str,
        after_memory_page: F,
    ) -> Result<(MemoryPage, String, Option<PlanRecord>)>
    where
        F: FnOnce(),
    {
        self.with_read(|connection| {
            connection.execute_batch("BEGIN DEFERRED")?;
            let result = (|| {
                let total: i64 = connection.query_row(
                    "SELECT count(*) FROM memories WHERE project_key=?1",
                    [project],
                    |row| row.get(0),
                )?;
                let total = total.max(0) as usize;
                if total > MEMORY_MAX_ENTRIES {
                    return Err(resource_limit(
                        "active project memory exceeds its entry quota; archive excess notes before turn synchronization",
                    ));
                }

                let mut statement = connection
                    .prepare("SELECT key,value FROM memories WHERE project_key=?1 ORDER BY key")?;
                let mut rows = statement.query([project])?;
                let mut notes = Vec::new();
                let mut retained_bytes = 0usize;
                let mut hasher = Sha256::new();
                hasher.update(b"codexbridge-memory-v1\0");
                while let Some(row) = rows.next()? {
                    let key: String = row.get(0)?;
                    let value: String = row.get(1)?;
                    validate_memory_key(&key)?;
                    if value.len() > MEMORY_VALUE_MAX_BYTES {
                        return Err(resource_limit(
                            "stored memory value exceeds the current safe hashing limit",
                        ));
                    }
                    hasher.update((key.len() as u64).to_be_bytes());
                    hasher.update(key.as_bytes());
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value.as_bytes());

                    retained_bytes = retained_bytes
                        .saturating_add(key.len())
                        .saturating_add(value.len());
                    if retained_bytes > MEMORY_MAX_TOTAL_BYTES {
                        return Err(resource_limit(
                            "active project memory exceeds its aggregate quota; archive excess notes before turn synchronization",
                        ));
                    }
                    notes.push(MemoryRecord { key, value });
                }
                drop(rows);
                drop(statement);

                let memory = MemoryPage {
                    truncated: false,
                    next_offset: None,
                    notes,
                    total,
                    offset: 0,
                };
                after_memory_page();

                let memory_hash = format!("{:x}", hasher.finalize());
                let metadata: Option<(i64, String)> = connection
                    .query_row(
                        "SELECT length(CAST(value AS BLOB)),updated_at FROM plans WHERE project_key=?1",
                        [project],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let plan = if let Some((length, updated_at)) = metadata {
                    if length > PLAN_STORAGE_MAX_BYTES as i64 {
                        return Err(resource_limit(
                            "stored plan exceeds the current safe retrieval limit",
                        ));
                    }
                    let value: String = connection.query_row(
                        "SELECT value FROM plans WHERE project_key=?1",
                        [project],
                        |row| row.get(0),
                    )?;
                    Some(decode_plan(&value, updated_at)?)
                } else {
                    None
                };
                Ok((memory, memory_hash, plan))
            })();

            match result {
                Ok(value) => {
                    connection.execute_batch("COMMIT")?;
                    Ok(value)
                }
                Err(error) => {
                    let _ = connection.execute_batch("ROLLBACK");
                    Err(error)
                }
            }
        })
    }

    pub fn plan_get(&self, project: &str) -> Result<Option<PlanRecord>> {
        self.with_read(|connection| {
            let metadata: Option<(i64, String)> = connection
                .query_row(
                    "SELECT length(CAST(value AS BLOB)),updated_at FROM plans WHERE project_key=?1",
                    [project],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((length, updated_at)) = metadata else {
                return Ok(None);
            };
            if length > PLAN_STORAGE_MAX_BYTES as i64 {
                return Err(resource_limit(
                    "stored plan exceeds the current safe retrieval limit",
                ));
            }
            let value: String = connection.query_row(
                "SELECT value FROM plans WHERE project_key=?1",
                [project],
                |row| row.get(0),
            )?;
            Ok(Some(decode_plan(&value, updated_at)?))
        })
    }

    pub fn plan_set(
        &self,
        project: &str,
        explanation: Option<String>,
        items: Vec<PlanItemRecord>,
    ) -> Result<PlanRecord> {
        validate_plan(explanation.as_deref(), &items)?;
        let updated_at = Utc::now().to_rfc3339();
        let plan = PlanRecord {
            explanation,
            items,
            updated_at: updated_at.clone(),
        };
        let encoded = serde_json::to_string(&plan)?;
        if encoded.len() > PLAN_STORAGE_MAX_BYTES {
            return Err(input_too_large(format!(
                "serialized plan exceeds {PLAN_STORAGE_MAX_BYTES} bytes"
            )));
        }
        let project = project.to_owned();
        self.with_write(move |connection| {
            connection.execute("INSERT INTO plans(project_key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(project_key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at", params![&project,&encoded,&updated_at])?;
            Ok(())
        })?;
        Ok(plan)
    }

    pub fn plan_clear(&self, project: &str) -> Result<bool> {
        let project = project.to_owned();
        self.with_write(move |connection| {
            Ok(connection.execute("DELETE FROM plans WHERE project_key=?1", [&project])? > 0)
        })
    }

    pub fn task_list(&self, project: &str) -> Result<Vec<TaskRecord>> {
        self.task_list_limited(project, TASK_MAX_ENTRIES)
    }

    pub fn task_list_limited(&self, project: &str, limit: usize) -> Result<Vec<TaskRecord>> {
        let limit = limit.min(TASK_MAX_ENTRIES);
        self.with_read(|connection| {
            let mut statement = connection.prepare("SELECT id,CASE WHEN length(CAST(title AS BLOB))<=?2 THEN title ELSE '[oversized legacy title]' END,status,CASE WHEN details IS NULL THEN NULL WHEN length(CAST(details AS BLOB))<=?3 THEN details ELSE '[oversized legacy details omitted]' END,CASE WHEN parent_task IS NULL THEN NULL WHEN length(CAST(parent_task AS BLOB))<=?4 THEN parent_task ELSE NULL END,created_at,updated_at,started_at,completed_at FROM tasks WHERE project_key=?1 ORDER BY created_at,id LIMIT ?5")?;
            let rows = statement.query_map(params![project,TASK_TITLE_MAX_BYTES as i64,TASK_DETAILS_MAX_BYTES as i64,TASK_PARENT_MAX_BYTES as i64,limit as i64], |row| Ok(TaskRecord { id:row.get(0)?,title:row.get(1)?,status:row.get(2)?,details:row.get(3)?,parent_task:row.get(4)?,created_at:row.get(5)?,updated_at:row.get(6)?,started_at:row.get(7)?,completed_at:row.get(8)? }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn task_summary(&self, project: &str) -> Result<TaskSummary> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT status,count(*) FROM tasks WHERE project_key=?1 GROUP BY status",
            )?;
            let rows = statement.query_map([project], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut summary = TaskSummary::default();
            for row in rows {
                let (status, count) = row?;
                let count = count.max(0) as usize;
                summary.total = summary.total.saturating_add(count);
                match status.as_str() {
                    "pending" => summary.pending = count,
                    "in_progress" => summary.in_progress = count,
                    "completed" => summary.completed = count,
                    "failed" => summary.failed = count,
                    "cancelled" => summary.cancelled = count,
                    _ => {}
                }
            }
            Ok(summary)
        })
    }

    pub fn task_add(&self, project: &str, task: &TaskRecord) -> Result<()> {
        validate_task_text(task)?;
        let project = project.to_owned();
        let task = task.clone();
        self.with_write(move |connection| {
            let transaction = connection.transaction()?;
            let count: i64 = transaction.query_row(
                "SELECT count(*) FROM tasks WHERE project_key=?1",
                [&project],
                |row| row.get(0),
            )?;
            if count >= TASK_MAX_ENTRIES as i64 {
                return Err(resource_limit(format!(
                    "project task list is limited to {TASK_MAX_ENTRIES} entries"
                )));
            }
            transaction.execute("INSERT INTO tasks(project_key,id,title,status,details,parent_task,created_at,updated_at,started_at,completed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![&project,&task.id,&task.title,&task.status,&task.details,&task.parent_task,&task.created_at,&task.updated_at,&task.started_at,&task.completed_at])?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn task_update(
        &self,
        project: &str,
        id: &str,
        status: &str,
        title: Option<&str>,
        details: Option<&str>,
    ) -> Result<bool> {
        validate_task_id(id)?;
        if !matches!(
            status,
            "pending" | "in_progress" | "completed" | "failed" | "cancelled"
        ) {
            return Err(AppError::new("INVALID_INPUT", "invalid task status"));
        }
        if title.is_some_and(|value| value.trim().is_empty()) {
            return Err(AppError::new(
                "INVALID_INPUT",
                "task title must not be empty",
            ));
        }
        if title.is_some_and(|value| value.len() > TASK_TITLE_MAX_BYTES) {
            return Err(input_too_large(format!(
                "task title exceeds {TASK_TITLE_MAX_BYTES} bytes"
            )));
        }
        if details.is_some_and(|value| value.len() > TASK_DETAILS_MAX_BYTES) {
            return Err(input_too_large(format!(
                "task details exceed {TASK_DETAILS_MAX_BYTES} bytes"
            )));
        }
        let now = Utc::now().to_rfc3339();
        let started = (status == "in_progress").then_some(now.as_str());
        let completed =
            matches!(status, "completed" | "failed" | "cancelled").then_some(now.as_str());
        let project = project.to_owned();
        let id = id.to_owned();
        let status = status.to_owned();
        let title = title.map(str::to_owned);
        let details = details.map(str::to_owned);
        let started = started.map(str::to_owned);
        let completed = completed.map(str::to_owned);
        self.with_write(move |connection| Ok(connection.execute("UPDATE tasks SET status=?3,title=COALESCE(?4,title),details=COALESCE(?5,details),updated_at=?6,started_at=COALESCE(started_at,?7),completed_at=?8 WHERE project_key=?1 AND id=?2", params![&project,&id,&status,&title,&details,&now,&started,&completed])? > 0))
    }

    pub fn task_remove(&self, project: &str, id: &str) -> Result<bool> {
        validate_task_id(id)?;
        let project = project.to_owned();
        let id = id.to_owned();
        self.with_write(move |connection| {
            Ok(connection.execute(
                "DELETE FROM tasks WHERE project_key=?1 AND id=?2",
                params![&project, &id],
            )? > 0)
        })
    }
}

fn validate_schema_v5(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(turn_refs)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<std::collections::HashSet<_>, _>>()?;
    for required in [
        "turn_ref",
        "native_key",
        "effective_key",
        "subject_key",
        "parent_turn_ref",
        "instruction_hash",
        "state_hash",
        "brief_snapshot",
        "state_snapshot",
        "created_at",
    ] {
        if !columns.contains(required) {
            return Err(AppError::new(
                "STORAGE_SCHEMA_UNSUPPORTED",
                format!(
                    "database reports schema version 5 but is missing required column `{required}`; recreate the development database"
                ),
            ));
        }
    }
    let aliases_schema: String = connection.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='aliases'",
        [],
        |row| row.get(0),
    )?;
    if !aliases_schema
        .to_ascii_uppercase()
        .contains("COLLATE NOCASE")
    {
        return Err(AppError::new(
            "STORAGE_SCHEMA_UNSUPPORTED",
            "database reports schema version 5 but aliases are not case-insensitive",
        ));
    }
    let archive_exists: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memory_archive'",
        [],
        |row| row.get(0),
    )?;
    if archive_exists != 1 {
        return Err(AppError::new(
            "STORAGE_SCHEMA_UNSUPPORTED",
            "database reports schema version 5 but memory_archive is missing",
        ));
    }
    Ok(())
}

fn decode_plan(value: &str, updated_at: String) -> Result<PlanRecord> {
    let value: serde_json::Value = serde_json::from_str(value)?;
    let explanation = value
        .as_object()
        .and_then(|object| object.get("explanation"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let raw_items = if let Some(items) = value
        .as_object()
        .and_then(|object| object.get("items"))
        .and_then(serde_json::Value::as_array)
    {
        items
    } else if let Some(items) = value.as_array() {
        items
    } else {
        return Err(AppError::new(
            "INVALID_INPUT",
            "stored plan has an unsupported representation",
        ));
    };
    let items = raw_items
        .iter()
        .map(|item| {
            if let Some(step) = item.as_str() {
                return Ok(PlanItemRecord {
                    step: step.to_owned(),
                    status: "pending".to_owned(),
                });
            }
            serde_json::from_value::<PlanItemRecord>(item.clone()).map_err(AppError::from)
        })
        .collect::<Result<Vec<_>>>()?;
    validate_plan(explanation.as_deref(), &items)?;
    Ok(PlanRecord {
        explanation,
        items,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> (tempfile::TempDir, Storage) {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&directory.path().join("state.sqlite3")).expect("storage");
        (directory, storage)
    }

    #[test]
    fn storage_initializes_and_persists_schema_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let storage = Storage::open(&path).unwrap();
        let version = storage
            .with_read(|connection| {
                Ok(connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?)
            })
            .unwrap();
        assert_eq!(version, STORAGE_SCHEMA_VERSION);
        let turn_ref_schema = storage
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='turn_refs'",
                    [],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert!(turn_ref_schema.contains("subject_key TEXT NOT NULL"));
        assert!(turn_ref_schema.contains("instruction_hash TEXT NOT NULL"));
        assert!(turn_ref_schema.contains("state_hash TEXT NOT NULL"));
        assert!(turn_ref_schema.contains("brief_snapshot TEXT"));
        assert!(turn_ref_schema.contains("state_snapshot TEXT"));
        let aliases_schema = storage
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name='aliases'",
                    [],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert!(
            aliases_schema
                .to_ascii_uppercase()
                .contains("COLLATE NOCASE")
        );
        let journal_mode = storage
            .with_read(|connection| {
                Ok(connection
                    .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?)
            })
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        drop(storage);
        Storage::open(&path).unwrap();
    }

    #[test]
    fn old_schema_versions_are_rejected_instead_of_migrated() {
        for version in [1_i64, 2_i64, 3_i64] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.sqlite3");
            let connection = Connection::open(&path).unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            drop(connection);

            let error = Storage::open(&path).err().unwrap();
            assert_eq!(error.code(), "STORAGE_SCHEMA_UNSUPPORTED");
            assert!(error.message().contains("fresh schema version 5"));
        }
    }

    #[test]
    fn schema_v4_memory_overflow_is_migrated_to_archive_without_loss() {
        use std::collections::BTreeMap;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE aliases(alias TEXT PRIMARY KEY COLLATE NOCASE, effective_key TEXT NOT NULL);
                 CREATE TABLE bindings(native_key TEXT PRIMARY KEY, effective_key TEXT NOT NULL);
                 CREATE TABLE memories(project_key TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY(project_key,key));
                 CREATE TABLE plans(project_key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
                 CREATE TABLE tasks(project_key TEXT NOT NULL, id TEXT NOT NULL, title TEXT NOT NULL, status TEXT NOT NULL, details TEXT, parent_task TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, started_at TEXT, completed_at TEXT, PRIMARY KEY(project_key,id));
                 CREATE TABLE turn_refs(id INTEGER PRIMARY KEY AUTOINCREMENT, turn_ref TEXT NOT NULL UNIQUE, native_key TEXT NOT NULL, effective_key TEXT NOT NULL, subject_key TEXT NOT NULL, parent_turn_ref TEXT, instruction_hash TEXT NOT NULL, state_hash TEXT NOT NULL, brief_snapshot TEXT, state_snapshot TEXT, created_at TEXT NOT NULL);
                 CREATE INDEX turn_refs_native_id ON turn_refs(native_key,id DESC);
                 CREATE UNIQUE INDEX turn_refs_native_parent_unique ON turn_refs(native_key,parent_turn_ref) WHERE parent_turn_ref IS NOT NULL;
                 PRAGMA user_version=4;",
            )
            .unwrap();
        let mut seeded = Vec::new();
        for index in 0..MEMORY_MAX_ENTRIES + 2 {
            let key = format!("key-{index:04}");
            let value = match index {
                1 => "value-0001-first-overflow-boundary".to_owned(),
                2 => "value-0002-last-active-boundary".to_owned(),
                _ => format!("value-{index:04}-payload"),
            };
            let updated_at = format!("2026-{:02}-{:02}T00:00:00Z", 1 + index / 28, 1 + index % 28);
            connection
                .execute(
                    "INSERT INTO memories(project_key,key,value,updated_at) VALUES('p',?1,?2,?3)",
                    params![&key, &value, &updated_at],
                )
                .unwrap();
            seeded.push((key, value));
        }
        drop(connection);

        let storage = Storage::open(&path).unwrap();
        let (active, _) = storage
            .memory_recall_page_from_snapshot("p", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .unwrap();
        let (archive, _) = storage
            .memory_archive_recall_page_from_snapshot("p", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .unwrap();

        assert_eq!(active.total, MEMORY_MAX_ENTRIES);
        assert_eq!(archive.total, 2);
        assert!(!active.truncated);
        assert!(!archive.truncated);

        let active_by_key = active
            .notes
            .iter()
            .map(|note| (note.key.clone(), note.value.clone()))
            .collect::<BTreeMap<_, _>>();
        let archive_by_key = archive
            .notes
            .iter()
            .map(|note| (note.key.clone(), note.value.clone()))
            .collect::<BTreeMap<_, _>>();
        let expected_active = seeded.iter().skip(2).cloned().collect::<BTreeMap<_, _>>();
        let expected_archive = seeded.iter().take(2).cloned().collect::<BTreeMap<_, _>>();

        assert_eq!(active_by_key, expected_active);
        assert_eq!(archive_by_key, expected_archive);
        assert_eq!(
            active_by_key.get("key-0002").map(String::as_str),
            Some("value-0002-last-active-boundary")
        );
        assert_eq!(
            archive_by_key.get("key-0001").map(String::as_str),
            Some("value-0001-first-overflow-boundary")
        );

        let mut combined = active_by_key.clone();
        for (key, value) in &archive_by_key {
            assert!(
                combined.insert(key.clone(), value.clone()).is_none(),
                "memory key {key} exists in both active and archive after migration"
            );
        }
        assert_eq!(combined, seeded.into_iter().collect::<BTreeMap<_, _>>());
    }

    #[test]
    fn new_project_cannot_claim_an_existing_effective_key_as_an_alias() {
        let (_directory, storage) = storage();
        storage
            .commit_initialization("native-private", "OpaquePrivateKey", None, None, None)
            .unwrap();

        for alias in ["OpaquePrivateKey", "opaqueprivatekey"] {
            let error = storage
                .commit_initialization("native-named", alias, Some(alias), None, None)
                .unwrap_err();
            assert_eq!(error.code(), "PROJECT_PATH_COLLISION");
            assert_eq!(storage.effective_binding("native-named").unwrap(), None);
            assert_eq!(storage.effective_for_alias(alias).unwrap(), None);
        }
    }

    #[test]
    fn new_private_effective_keys_are_case_insensitively_unique() {
        let (_directory, storage) = storage();
        storage
            .commit_initialization("native-one", "OpaquePrivateKey", None, None, None)
            .unwrap();
        let error = storage
            .commit_initialization("native-two", "opaqueprivatekey", None, None, None)
            .unwrap_err();
        assert_eq!(error.code(), "PROJECT_PATH_COLLISION");
        assert_eq!(storage.effective_binding("native-two").unwrap(), None);
    }

    #[test]
    fn turn_refs_form_a_native_conversation_chain() {
        let (_directory, storage) = storage();
        let first = storage
            .commit_initialization_with_turn_ref(
                "native",
                "effective",
                None,
                None,
                None,
                TurnRefCommit {
                    turn_ref: "r_first",
                    parent_turn_ref: None,
                    force_full_brief: false,
                    instruction_hash: "instructions-one",
                    state_hash: "state-one",
                    subject_key: "subject",
                    brief_snapshot: "brief-one",
                    state_snapshot: Some("snapshot-one"),
                },
            )
            .unwrap();
        assert_eq!(first.parent_turn_ref, None);
        assert_eq!(first.parent_instruction_hash, None);
        assert_eq!(first.parent_state_hash, None);
        assert_eq!(first.instruction_hash, "instructions-one");
        assert_eq!(first.state_hash, "state-one");

        let second = storage
            .commit_initialization_with_turn_ref(
                "native",
                "effective",
                None,
                Some("effective"),
                None,
                TurnRefCommit {
                    turn_ref: "r_second",
                    parent_turn_ref: Some("r_first"),
                    force_full_brief: false,
                    instruction_hash: "instructions-two",
                    state_hash: "state-two",
                    subject_key: "subject",
                    brief_snapshot: "brief-two",
                    state_snapshot: Some("snapshot-two"),
                },
            )
            .unwrap();
        assert_eq!(second.parent_turn_ref.as_deref(), Some("r_first"));
        assert_eq!(
            second.parent_instruction_hash.as_deref(),
            Some("instructions-one")
        );
        assert_eq!(second.parent_state_hash.as_deref(), Some("state-one"));

        let rows = storage
            .with_read(|connection| {
                let mut statement = connection.prepare(
                    "SELECT turn_ref,parent_turn_ref,effective_key,instruction_hash,state_hash FROM turn_refs WHERE native_key='native' ORDER BY id",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            (
                "r_first".to_owned(),
                None,
                "effective".to_owned(),
                "instructions-one".to_owned(),
                "state-one".to_owned()
            )
        );
        assert_eq!(
            rows[1],
            (
                "r_second".to_owned(),
                Some("r_first".to_owned()),
                "effective".to_owned(),
                "instructions-two".to_owned(),
                "state-two".to_owned()
            )
        );
    }

    #[test]
    fn duplicate_parent_is_idempotent_for_one_native_conversation() {
        let (_directory, storage) = storage();
        storage
            .commit_initialization_with_turn_ref(
                "native",
                "effective",
                None,
                None,
                None,
                TurnRefCommit {
                    turn_ref: "r_parent",
                    parent_turn_ref: None,
                    force_full_brief: false,
                    instruction_hash: "instructions-parent",
                    state_hash: "state-parent",
                    subject_key: "subject",
                    brief_snapshot: "brief-parent",
                    state_snapshot: Some("snapshot-parent"),
                },
            )
            .unwrap();
        let first = storage
            .commit_initialization_with_turn_ref(
                "native",
                "effective",
                None,
                Some("effective"),
                None,
                TurnRefCommit {
                    turn_ref: "r_child",
                    parent_turn_ref: Some("r_parent"),
                    force_full_brief: false,
                    instruction_hash: "instructions-child",
                    state_hash: "state-child",
                    subject_key: "subject",
                    brief_snapshot: "brief-child",
                    state_snapshot: Some("snapshot-child"),
                },
            )
            .unwrap();
        let duplicate = storage
            .commit_initialization_with_turn_ref(
                "native",
                "effective",
                None,
                Some("effective"),
                None,
                TurnRefCommit {
                    turn_ref: "r_other_candidate",
                    parent_turn_ref: Some("r_parent"),
                    force_full_brief: false,
                    instruction_hash: "instructions-other",
                    state_hash: "state-other",
                    subject_key: "subject",
                    brief_snapshot: "brief-other",
                    state_snapshot: Some("snapshot-other"),
                },
            )
            .unwrap();
        assert_eq!(first.turn_ref, "r_child");
        assert_eq!(duplicate.turn_ref, "r_child");
        assert!(duplicate.reused_existing_turn);
        assert_eq!(duplicate.instruction_hash, "instructions-child");
        assert_eq!(duplicate.state_hash, "state-child");
        assert_eq!(duplicate.brief_snapshot.as_deref(), Some("brief-child"));
        assert_eq!(duplicate.state_snapshot, None);
    }

    #[test]
    fn turn_ref_insert_failure_rolls_back_initialization_binding() {
        let (_directory, storage) = storage();
        storage
            .commit_initialization_with_turn_ref(
                "native-one",
                "effective-one",
                None,
                None,
                None,
                TurnRefCommit {
                    turn_ref: "r_collision",
                    parent_turn_ref: None,
                    force_full_brief: false,
                    instruction_hash: "instructions-one",
                    state_hash: "state-one",
                    subject_key: "subject-one",
                    brief_snapshot: "brief-one",
                    state_snapshot: None,
                },
            )
            .unwrap();

        let error = storage
            .commit_initialization_with_turn_ref(
                "native-two",
                "effective-two",
                None,
                None,
                None,
                TurnRefCommit {
                    turn_ref: "r_collision",
                    parent_turn_ref: None,
                    force_full_brief: false,
                    instruction_hash: "instructions-two",
                    state_hash: "state-two",
                    subject_key: "subject-two",
                    brief_snapshot: "brief-two",
                    state_snapshot: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code(), "STORAGE_ERROR");
        assert_eq!(storage.effective_binding("native-two").unwrap(), None);
    }

    #[test]
    fn wal_read_pool_allows_parallel_reader_and_writer_progress() {
        use std::{
            sync::{Barrier, mpsc::RecvTimeoutError},
            time::Instant,
        };

        enum Progress {
            Reader(Result<usize>),
            Writer(Result<()>),
        }

        let (_directory, storage) = storage();
        storage.memory_set("p", "first", "value").unwrap();

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let held_storage = storage.clone();
        let holder = std::thread::spawn(move || {
            held_storage
                .with_read(|connection| {
                    connection.execute_batch("BEGIN")?;
                    let _: i64 = connection.query_row(
                        "SELECT count(*) FROM memories WHERE project_key='p'",
                        [],
                        |row| row.get(0),
                    )?;
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    connection.execute_batch("COMMIT")?;
                    Ok(())
                })
                .unwrap();
        });
        entered_rx.recv().unwrap();

        let start = Arc::new(Barrier::new(3));
        let (progress_tx, progress_rx) = mpsc::channel();
        let read_storage = storage.clone();
        let read_start = start.clone();
        let read_progress = progress_tx.clone();
        let reader = std::thread::spawn(move || {
            read_start.wait();
            let _ = read_progress.send(Progress::Reader(read_storage.memory_count("p")));
        });

        let write_storage = storage.clone();
        let write_start = start.clone();
        let write_progress = progress_tx.clone();
        let writer = std::thread::spawn(move || {
            write_start.wait();
            let _ = write_progress.send(Progress::Writer(write_storage.memory_set(
                "p", "second", "value",
            )));
        });

        start.wait();
        drop(progress_tx);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut read_result = None;
        let mut write_result = None;
        while read_result.is_none() || write_result.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match progress_rx.recv_timeout(remaining) {
                Ok(Progress::Reader(result)) => read_result = Some(result),
                Ok(Progress::Writer(result)) => write_result = Some(result),
                Err(RecvTimeoutError::Timeout) => {
                    let reader_pending = read_result.is_none();
                    let writer_pending = write_result.is_none();
                    release_tx.send(()).unwrap();
                    holder.join().unwrap();
                    reader.join().unwrap();
                    writer.join().unwrap();
                    panic!(
                        "read/write progress deadlocked while first read transaction was active (reader_pending={reader_pending}, writer_pending={writer_pending})"
                    );
                }
                Err(error) => panic!("progress channel failed: {error}"),
            }
        }
        assert_eq!(read_result.unwrap().unwrap(), 1);
        write_result.unwrap().unwrap();

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        reader.join().unwrap();
        writer.join().unwrap();
        assert_eq!(storage.memory_count("p").unwrap(), 2);
        assert_eq!(
            storage.memory_get("p", "first").unwrap().as_deref(),
            Some("value")
        );
        assert_eq!(
            storage.memory_get("p", "second").unwrap().as_deref(),
            Some("value")
        );
    }

    #[test]
    fn pooled_read_connections_are_query_only() {
        let (_directory, storage) = storage();
        let error = storage
            .with_read(|connection| {
                connection.execute(
                    "INSERT INTO memories(project_key,key,value,updated_at) VALUES('p','x','y','now')",
                    [],
                )?;
                Ok(())
            })
            .expect_err("read connection must reject writes");
        assert_eq!(error.code(), "STORAGE_ERROR");
    }

    #[test]
    fn storage_rejects_newer_schema_versions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "user_version", STORAGE_SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);
        let error = match Storage::open(&path) {
            Ok(_) => panic!("newer schema must fail closed"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "STORAGE_SCHEMA_UNSUPPORTED");
    }

    fn task(id: usize) -> TaskRecord {
        let now = Utc::now().to_rfc3339();
        TaskRecord {
            id: format!("task_{id}"),
            title: format!("Task {id}"),
            status: "pending".to_owned(),
            details: None,
            parent_task: None,
            created_at: now.clone(),
            updated_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn memory_rejects_large_keys_and_values() {
        let (_directory, storage) = storage();
        let key_error = storage
            .memory_set("p", &"k".repeat(MEMORY_KEY_MAX_BYTES + 1), "v")
            .expect_err("oversized key");
        assert_eq!(key_error.code(), "INPUT_TOO_LARGE");
        let value_error = storage
            .memory_set("p", "key", &"v".repeat(MEMORY_VALUE_MAX_BYTES + 1))
            .expect_err("oversized value");
        assert_eq!(value_error.code(), "INPUT_TOO_LARGE");
    }

    #[test]
    fn memory_entry_quota_is_atomic_and_updates_still_work() {
        let (_directory, storage) = storage();
        storage
            .with_write(|connection| {
                let transaction = connection.transaction()?;
                {
                    let mut insert = transaction.prepare("INSERT INTO memories(project_key,key,value,updated_at) VALUES('p',?1,'v','now')")?;
                    for index in 0..MEMORY_MAX_ENTRIES {
                        insert.execute([format!("key-{index:04}")])?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .expect("seed memory quota");
        let error = storage
            .memory_set("p", "one-too-many", "v")
            .expect_err("entry quota");
        assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
        storage
            .memory_set("p", "key-0000", "updated")
            .expect("update existing entry at quota");
        assert_eq!(
            storage.memory_get("p", "key-0000").expect("get").as_deref(),
            Some("updated")
        );
    }

    #[test]
    fn archive_recall_page_has_entry_and_byte_bounds() {
        let (_directory, entry_storage) = storage();
        for index in 0..MEMORY_RECALL_MAX_ENTRIES + 4 {
            entry_storage
                .memory_archive_set("p", &format!("key-{index:04}"), "value")
                .unwrap();
        }
        let (page, hash) = entry_storage
            .memory_archive_recall_page_from_snapshot("p", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .expect("recall");
        assert_eq!(page.notes.len(), MEMORY_RECALL_MAX_ENTRIES);
        assert_eq!(page.total, MEMORY_RECALL_MAX_ENTRIES + 4);
        assert!(page.truncated);
        assert_eq!(page.next_offset, Some(MEMORY_RECALL_MAX_ENTRIES));
        let (tail, _) = entry_storage
            .memory_archive_recall_page_from_snapshot(
                "p",
                page.next_offset.unwrap(),
                16,
                Some(&hash),
            )
            .expect("tail page");
        assert_eq!(tail.notes.len(), 4);
        assert!(!tail.truncated);
        assert_eq!(tail.next_offset, None);

        let page_bytes = |page: &MemoryPage| {
            page.notes
                .iter()
                .map(|note| note.key.len() + note.value.len())
                .sum::<usize>()
        };
        let seed_to_bytes = |storage: &Storage, project: &str, target_bytes: usize| {
            let max_value = "v".repeat(MEMORY_ARCHIVE_VALUE_MAX_BYTES);
            for key in ["a", "b", "c"] {
                storage
                    .memory_archive_set(project, key, &max_value)
                    .unwrap();
            }
            let final_value_bytes = target_bytes
                .checked_sub(4 + 3 * MEMORY_ARCHIVE_VALUE_MAX_BYTES)
                .expect("target accommodates three full values and four keys");
            assert!(final_value_bytes <= MEMORY_ARCHIVE_VALUE_MAX_BYTES);
            storage
                .memory_archive_set(project, "d", &"v".repeat(final_value_bytes))
                .unwrap();
        };

        let (_under_directory, under_storage) = storage();
        seed_to_bytes(&under_storage, "under", MEMORY_RECALL_MAX_BYTES - 1);
        let (under, _) = under_storage
            .memory_archive_recall_page_from_snapshot("under", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .expect("recall below byte budget");
        assert_eq!(under.notes.len(), 4);
        assert_eq!(page_bytes(&under), MEMORY_RECALL_MAX_BYTES - 1);
        assert!(!under.truncated);
        assert_eq!(under.next_offset, None);

        let (_exact_directory, exact_storage) = storage();
        seed_to_bytes(&exact_storage, "exact", MEMORY_RECALL_MAX_BYTES);
        let (exact, _) = exact_storage
            .memory_archive_recall_page_from_snapshot("exact", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .expect("recall at exact byte budget");
        assert_eq!(exact.notes.len(), 4);
        assert_eq!(page_bytes(&exact), MEMORY_RECALL_MAX_BYTES);
        assert!(!exact.truncated);
        assert_eq!(exact.next_offset, None);

        let (_over_directory, over_storage) = storage();
        seed_to_bytes(&over_storage, "over", MEMORY_RECALL_MAX_BYTES);
        over_storage.memory_archive_set("over", "e", "x").unwrap();
        let (over, over_hash) = over_storage
            .memory_archive_recall_page_from_snapshot("over", 0, MEMORY_RECALL_MAX_ENTRIES, None)
            .expect("recall over byte budget");
        assert_eq!(over.notes.len(), 4);
        assert_eq!(over.total, 5);
        assert_eq!(page_bytes(&over), MEMORY_RECALL_MAX_BYTES);
        assert!(over.truncated);
        assert_eq!(over.next_offset, Some(4));

        let (over_tail, _) = over_storage
            .memory_archive_recall_page_from_snapshot(
                "over",
                over.next_offset.unwrap(),
                MEMORY_RECALL_MAX_ENTRIES,
                Some(&over_hash),
            )
            .expect("recall over-budget continuation");
        assert_eq!(over_tail.notes.len(), 1);
        assert_eq!(page_bytes(&over_tail), 2);
        assert!(!over_tail.truncated);
        assert_eq!(over_tail.next_offset, None);
    }

    #[test]
    fn regression_memory_pagination_must_not_repeat_rows_after_insert_before_offset() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "b", "B").unwrap();
        storage.memory_set("p", "d", "D").unwrap();

        let (first, snapshot_hash) = storage
            .memory_recall_page_from_snapshot("p", 0, 1, None)
            .unwrap();
        assert_eq!(first.notes[0].key, "b");
        let next = first.next_offset.expect("second page");

        // A concurrent/new note sorting before the first page shifts OFFSET 1.
        // A continuation pinned to the first page's snapshot must fail rather
        // than return a duplicate/skip from a shifted ordered set.
        storage.memory_set("p", "a", "A").unwrap();
        let error = storage
            .memory_recall_page_from_snapshot("p", next, 1, Some(&snapshot_hash))
            .unwrap_err();
        assert_eq!(error.code(), "PAGINATION_STALE");
    }

    #[test]
    fn active_memory_semantic_hash_excludes_archive_history() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "active", "before").unwrap();
        let before = storage.memory_semantic_hash("p").unwrap();
        storage
            .memory_archive_set("p", "historical", "after")
            .unwrap();
        let after = storage.memory_semantic_hash("p").unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn memory_aggregate_quota_is_enforced_atomically() {
        let (_directory, storage) = storage();
        let existing_key = "legacy";
        let existing_value = "v".repeat(MEMORY_MAX_TOTAL_BYTES - existing_key.len());
        storage
            .memory_set("p", existing_key, &existing_value)
            .expect("seed exact aggregate limit");
        let error = storage
            .memory_set("p", "new", "value")
            .expect_err("aggregate limit");
        assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
        assert!(error.message().contains("aggregate limit"));
        assert_eq!(
            storage
                .memory_get("p", existing_key)
                .expect("existing memory"),
            Some(existing_value)
        );
        assert_eq!(storage.memory_get("p", "new").expect("new memory"), None);
    }

    #[test]
    fn plan_round_trip_is_canonical() {
        let (_directory, storage) = storage();
        let saved = storage
            .plan_set(
                "p",
                Some("why".to_owned()),
                vec![PlanItemRecord {
                    step: "build".to_owned(),
                    status: "in_progress".to_owned(),
                }],
            )
            .expect("save plan");
        let loaded = storage.plan_get("p").expect("load plan").expect("present");
        assert_eq!(loaded, saved);
        assert_eq!(loaded.explanation.as_deref(), Some("why"));
        assert_eq!(loaded.items[0].status, "in_progress");
    }

    #[test]
    fn legacy_string_array_plan_is_migrated_on_read() {
        let (_directory, storage) = storage();
        storage
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO plans(project_key,value,updated_at) VALUES(?1,?2,?3)",
                    params!["p", r#"["inspect","implement"]"#, "2026-01-01T00:00:00Z"],
                )?;
                Ok(())
            })
            .expect("seed legacy plan");
        let plan = storage.plan_get("p").expect("read").expect("present");
        assert_eq!(plan.items.len(), 2);
        assert!(plan.items.iter().all(|item| item.status == "pending"));
    }

    #[test]
    fn plan_rejects_multiple_in_progress_steps() {
        let (_directory, storage) = storage();
        let items = vec![
            PlanItemRecord {
                step: "one".to_owned(),
                status: "in_progress".to_owned(),
            },
            PlanItemRecord {
                step: "two".to_owned(),
                status: "in_progress".to_owned(),
            },
        ];
        let error = storage
            .plan_set("p", None, items)
            .expect_err("invalid plan");
        assert_eq!(error.code(), "INVALID_INPUT");
    }

    #[test]
    fn plan_rejects_oversized_steps() {
        let (_directory, storage) = storage();
        let error = storage
            .plan_set(
                "p",
                None,
                vec![PlanItemRecord {
                    step: "x".repeat(PLAN_ITEM_MAX_BYTES + 1),
                    status: "pending".to_owned(),
                }],
            )
            .expect_err("step limit");
        assert_eq!(error.code(), "INPUT_TOO_LARGE");
    }

    #[test]
    fn task_quota_and_text_limits_are_enforced() {
        let (_directory, storage) = storage();
        let mut oversized = task(0);
        oversized.title = "x".repeat(TASK_TITLE_MAX_BYTES + 1);
        assert_eq!(
            storage
                .task_add("oversized", &oversized)
                .expect_err("title")
                .code(),
            "INPUT_TOO_LARGE"
        );
        storage
            .with_write(|connection| {
                let transaction = connection.transaction()?;
                {
                    let mut insert = transaction.prepare("INSERT INTO tasks(project_key,id,title,status,created_at,updated_at) VALUES('p',?1,?2,'pending','now','now')")?;
                    for index in 0..TASK_MAX_ENTRIES {
                        insert.execute(params![format!("task_{index}"), format!("Task {index}")])?;
                    }
                }
                transaction.commit()?;
                Ok(())
            })
            .expect("seed task quota");
        let error = storage
            .task_add("p", &task(TASK_MAX_ENTRIES))
            .expect_err("task quota");
        assert_eq!(error.code(), "RESOURCE_LIMIT_EXCEEDED");
        assert_eq!(
            storage.task_summary("p").expect("summary").total,
            TASK_MAX_ENTRIES
        );
        assert_eq!(
            storage
                .task_list_limited("p", TASK_LIST_OUTPUT_MAX)
                .expect("bounded list")
                .len(),
            TASK_LIST_OUTPUT_MAX
        );
    }

    #[test]
    fn reopening_storage_preserves_canonical_plan_and_state() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("state.sqlite3");
        let first = Storage::open(&path).expect("first open");
        first.memory_set("p", "key", "value").expect("memory");
        first
            .plan_set(
                "p",
                None,
                vec![PlanItemRecord {
                    step: "persist".to_owned(),
                    status: "completed".to_owned(),
                }],
            )
            .expect("plan");
        drop(first);
        let reopened = Storage::open(&path).expect("reopen");
        assert_eq!(
            reopened.memory_get("p", "key").expect("memory").as_deref(),
            Some("value")
        );
        assert_eq!(
            reopened
                .plan_get("p")
                .expect("plan")
                .expect("present")
                .items[0]
                .status,
            "completed"
        );
    }

    #[test]
    fn memory_update_replaces_value_without_creating_duplicate_entry() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "key", "first").unwrap();
        storage.memory_set("p", "key", "second").unwrap();
        assert_eq!(storage.memory_count("p").unwrap(), 1);
        assert_eq!(
            storage.memory_get("p", "key").unwrap().as_deref(),
            Some("second")
        );
    }

    #[test]
    fn deleting_missing_memory_is_false_and_existing_delete_persists() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.sqlite3");
        let storage = Storage::open(&path).unwrap();
        assert!(!storage.memory_delete("p", "ghost").unwrap());
        storage.memory_set("p", "key", "value").unwrap();
        assert!(storage.memory_delete("p", "key").unwrap());
        drop(storage);
        let reopened = Storage::open(&path).unwrap();
        assert_eq!(reopened.memory_get("p", "key").unwrap(), None);
    }

    #[test]
    fn memory_recall_is_sorted_by_key() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "zeta", "last").unwrap();
        storage.memory_set("p", "alpha", "first").unwrap();
        storage.memory_set("p", "middle", "middle").unwrap();
        let page = storage
            .memory_recall_page_from("p", 0, MEMORY_RECALL_MAX_ENTRIES)
            .unwrap();
        let keys = page
            .notes
            .into_iter()
            .map(|note| note.key)
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["alpha", "middle", "zeta"]);
    }

    #[test]
    fn plan_updates_do_not_disturb_memory_notes() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "decision", "keep me").unwrap();
        storage
            .plan_set(
                "p",
                Some("why".to_owned()),
                vec![PlanItemRecord {
                    step: "one".to_owned(),
                    status: "pending".to_owned(),
                }],
            )
            .unwrap();
        assert_eq!(
            storage.memory_get("p", "decision").unwrap().as_deref(),
            Some("keep me")
        );
    }

    #[test]
    fn rejected_plan_does_not_overwrite_last_valid_plan() {
        let (_directory, storage) = storage();
        let valid = storage
            .plan_set(
                "p",
                Some("valid".to_owned()),
                vec![PlanItemRecord {
                    step: "keep".to_owned(),
                    status: "in_progress".to_owned(),
                }],
            )
            .unwrap();
        let error = storage
            .plan_set(
                "p",
                Some("invalid".to_owned()),
                vec![
                    PlanItemRecord {
                        step: "a".to_owned(),
                        status: "in_progress".to_owned(),
                    },
                    PlanItemRecord {
                        step: "b".to_owned(),
                        status: "in_progress".to_owned(),
                    },
                ],
            )
            .unwrap_err();
        assert_eq!(error.code(), "INVALID_INPUT");
        assert_eq!(storage.plan_get("p").unwrap().unwrap(), valid);
    }

    #[test]
    fn oversized_memory_write_preserves_existing_notes() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "keep", "small").unwrap();
        let error = storage
            .memory_set("p", "too-big", &"x".repeat(MEMORY_VALUE_MAX_BYTES + 1))
            .unwrap_err();
        assert_eq!(error.code(), "INPUT_TOO_LARGE");
        assert_eq!(
            storage.memory_get("p", "keep").unwrap().as_deref(),
            Some("small")
        );
        assert_eq!(storage.memory_get("p", "too-big").unwrap(), None);
    }

    #[test]
    fn plan_clear_reports_presence_and_is_idempotent() {
        let (_directory, storage) = storage();
        assert!(!storage.plan_clear("p").unwrap());
        storage
            .plan_set(
                "p",
                None,
                vec![PlanItemRecord {
                    step: "one".to_owned(),
                    status: "completed".to_owned(),
                }],
            )
            .unwrap();
        assert!(storage.plan_clear("p").unwrap());
        assert!(!storage.plan_clear("p").unwrap());
        assert!(storage.plan_get("p").unwrap().is_none());
    }

    #[test]
    fn regression_storage_writer_recovers_after_one_job_panics() {
        let (_directory, storage) = storage();
        storage.memory_set("p", "before", "ok").unwrap();

        let panic_result = storage.with_write::<(), _>(|_| {
            panic!("intentional writer-job panic for recovery regression")
        });
        assert_eq!(panic_result.unwrap_err().code(), "STORAGE_ERROR");

        assert!(
            storage.memory_set("p", "after", "still-works").is_ok(),
            "one panicking write job permanently killed the dedicated SQLite writer"
        );
        assert_eq!(
            storage.memory_get("p", "after").unwrap().as_deref(),
            Some("still-works")
        );
    }
}
