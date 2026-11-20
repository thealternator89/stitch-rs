use std::ffi::{CStr, CString, c_char, c_double, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::types::CopilotUsage;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;

const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;

type SqliteDestructor = Option<unsafe extern "C" fn(*mut c_void)>;

fn sqlite_transient() -> SqliteDestructor {
    // SQLITE_TRANSIENT is defined as ((sqlite3_destructor_type)-1)
    unsafe { std::mem::transmute::<isize, SqliteDestructor>(-1) }
}

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        pp_db: *mut *mut sqlite3,
        flags: c_int,
        z_vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close_v2(db: *mut sqlite3) -> c_int;
    fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;
    fn sqlite3_free(ptr: *mut c_void);
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_changes(db: *mut sqlite3) -> c_int;
    fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        z_sql: *const c_char,
        n_byte: c_int,
        pp_stmt: *mut *mut sqlite3_stmt,
        pz_tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_finalize(p_stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_step(p_stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_bind_int64(p_stmt: *mut sqlite3_stmt, idx: c_int, val: i64) -> c_int;
    fn sqlite3_bind_double(p_stmt: *mut sqlite3_stmt, idx: c_int, val: c_double) -> c_int;
    fn sqlite3_bind_null(p_stmt: *mut sqlite3_stmt, idx: c_int) -> c_int;
    fn sqlite3_bind_text(
        p_stmt: *mut sqlite3_stmt,
        idx: c_int,
        val: *const c_char,
        n_bytes: c_int,
        destructor: SqliteDestructor,
    ) -> c_int;
    fn sqlite3_column_type(p_stmt: *mut sqlite3_stmt, i_col: c_int) -> c_int;
    fn sqlite3_column_int64(p_stmt: *mut sqlite3_stmt, i_col: c_int) -> i64;
    fn sqlite3_column_double(p_stmt: *mut sqlite3_stmt, i_col: c_int) -> c_double;
    fn sqlite3_column_text(p_stmt: *mut sqlite3_stmt, i_col: c_int) -> *const u8;
    fn sqlite3_column_bytes(p_stmt: *mut sqlite3_stmt, i_col: c_int) -> c_int;
}

struct SqliteConn {
    raw: *mut sqlite3,
}

// SQLITE_OPEN_FULLMUTEX serializes access inside SQLite, and we additionally guard with a Rust Mutex.
unsafe impl Send for SqliteConn {}

impl Drop for SqliteConn {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_close_v2(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

struct SqliteStmt<'a> {
    conn: &'a SqliteConn,
    raw: *mut sqlite3_stmt,
}

impl<'a> Drop for SqliteStmt<'a> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                sqlite3_finalize(self.raw);
            }
            self.raw = ptr::null_mut();
        }
    }
}

impl SqliteConn {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let path_str = path.to_string_lossy();
        let c_path = CString::new(path_str.as_bytes())
            .context("database path contains interior NUL byte")?;
        let mut raw: *mut sqlite3 = ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
        let rc = unsafe { sqlite3_open_v2(c_path.as_ptr(), &mut raw, flags, ptr::null()) };
        if rc != SQLITE_OK || raw.is_null() {
            let msg = if raw.is_null() {
                format!("sqlite3_open_v2 failed with code {rc}")
            } else {
                let err = unsafe { db_error_message(raw) };
                unsafe {
                    sqlite3_close_v2(raw);
                }
                err
            };
            bail!("failed to open SQLite database at {}: {msg}", path.display());
        }
        Ok(Self { raw })
    }

    fn execute_batch(&self, sql: &str) -> anyhow::Result<()> {
        let c_sql = CString::new(sql).context("SQL batch contains interior NUL byte")?;
        let mut errmsg: *mut c_char = ptr::null_mut();
        let rc = unsafe {
            sqlite3_exec(
                self.raw,
                c_sql.as_ptr(),
                None,
                ptr::null_mut(),
                &mut errmsg,
            )
        };
        if rc != SQLITE_OK {
            let msg = if !errmsg.is_null() {
                let s = unsafe { CStr::from_ptr(errmsg).to_string_lossy().into_owned() };
                unsafe {
                    sqlite3_free(errmsg as *mut c_void);
                }
                s
            } else {
                unsafe { db_error_message(self.raw) }
            };
            bail!("SQLite exec error ({rc}): {msg}");
        }
        Ok(())
    }

    fn prepare(&self, sql: &str) -> anyhow::Result<SqliteStmt<'_>> {
        let c_sql = CString::new(sql).context("SQL statement contains interior NUL byte")?;
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(
                self.raw,
                c_sql.as_ptr(),
                -1,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            let msg = unsafe { db_error_message(self.raw) };
            bail!("SQLite prepare error ({rc}) for `{sql}`: {msg}");
        }
        Ok(SqliteStmt {
            conn: self,
            raw: stmt,
        })
    }

    fn last_insert_rowid(&self) -> i64 {
        unsafe { sqlite3_last_insert_rowid(self.raw) }
    }

    fn changes(&self) -> usize {
        let count = unsafe { sqlite3_changes(self.raw) };
        count.max(0) as usize
    }
}

impl<'a> SqliteStmt<'a> {
    fn bind_i64(&self, idx: c_int, val: i64) -> anyhow::Result<()> {
        let rc = unsafe { sqlite3_bind_int64(self.raw, idx, val) };
        if rc != SQLITE_OK {
            let msg = unsafe { db_error_message(self.conn.raw) };
            bail!("sqlite3_bind_int64 failed at index {idx}: {msg}");
        }
        Ok(())
    }

    fn bind_f64(&self, idx: c_int, val: f64) -> anyhow::Result<()> {
        let rc = unsafe { sqlite3_bind_double(self.raw, idx, val) };
        if rc != SQLITE_OK {
            let msg = unsafe { db_error_message(self.conn.raw) };
            bail!("sqlite3_bind_double failed at index {idx}: {msg}");
        }
        Ok(())
    }

    fn bind_opt_f64(&self, idx: c_int, val: Option<f64>) -> anyhow::Result<()> {
        match val {
            Some(v) => self.bind_f64(idx, v),
            None => self.bind_null(idx),
        }
    }

    fn bind_null(&self, idx: c_int) -> anyhow::Result<()> {
        let rc = unsafe { sqlite3_bind_null(self.raw, idx) };
        if rc != SQLITE_OK {
            let msg = unsafe { db_error_message(self.conn.raw) };
            bail!("sqlite3_bind_null failed at index {idx}: {msg}");
        }
        Ok(())
    }

    fn bind_text(&self, idx: c_int, val: &str) -> anyhow::Result<()> {
        let bytes = val.as_bytes();
        let len = c_int::try_from(bytes.len()).context("bound text too large for SQLite")?;
        let ptr = if bytes.is_empty() {
            c"".as_ptr()
        } else {
            bytes.as_ptr() as *const c_char
        };
        let rc = unsafe { sqlite3_bind_text(self.raw, idx, ptr, len, sqlite_transient()) };
        if rc != SQLITE_OK {
            let msg = unsafe { db_error_message(self.conn.raw) };
            bail!("sqlite3_bind_text failed at index {idx}: {msg}");
        }
        Ok(())
    }

    fn bind_opt_text(&self, idx: c_int, val: Option<&str>) -> anyhow::Result<()> {
        match val {
            Some(text) => self.bind_text(idx, text),
            None => self.bind_null(idx),
        }
    }

    fn step(&self) -> anyhow::Result<bool> {
        let rc = unsafe { sqlite3_step(self.raw) };
        match rc {
            SQLITE_ROW => Ok(true),
            SQLITE_DONE => Ok(false),
            _ => {
                let msg = unsafe { db_error_message(self.conn.raw) };
                bail!("sqlite3_step failed ({rc}): {msg}");
            }
        }
    }

    fn col_is_null(&self, col: c_int) -> bool {
        unsafe { sqlite3_column_type(self.raw, col) == SQLITE_NULL }
    }

    fn col_i64(&self, col: c_int) -> i64 {
        unsafe { sqlite3_column_int64(self.raw, col) }
    }

    fn col_f64(&self, col: c_int) -> f64 {
        unsafe { sqlite3_column_double(self.raw, col) }
    }

    fn col_opt_f64(&self, col: c_int) -> Option<f64> {
        if self.col_is_null(col) {
            None
        } else {
            Some(self.col_f64(col))
        }
    }

    fn col_text(&self, col: c_int) -> String {
        self.col_opt_text(col).unwrap_or_default()
    }

    fn col_opt_text(&self, col: c_int) -> Option<String> {
        if self.col_is_null(col) {
            return None;
        }
        unsafe {
            let ptr = sqlite3_column_text(self.raw, col);
            if ptr.is_null() {
                return Some(String::new());
            }
            let len = sqlite3_column_bytes(self.raw, col).max(0) as usize;
            let slice = std::slice::from_raw_parts(ptr, len);
            Some(String::from_utf8_lossy(slice).into_owned())
        }
    }
}

unsafe fn db_error_message(db: *mut sqlite3) -> String {
    if db.is_null() {
        return "null sqlite3 handle".to_string();
    }
    let ptr = unsafe { sqlite3_errmsg(db) };
    if ptr.is_null() {
        "unknown SQLite error".to_string()
    } else {
        unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
    }
}

/// A single LLM usage record belonging to a [`DbSession`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbLlmUsage {
    pub id: i64,
    pub session_id: i64,
    pub label: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost: f64,
    pub multiplier: Option<f64>,
}

/// A persisted tool execution session in `history.db`, along with its per-phase/turn [`DbLlmUsage`] rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DbSession {
    pub id: i64,
    pub tool_name: String,
    pub context_reference: Option<String>,
    pub timestamp: i64,
    pub ai_output: Option<String>,
    pub pushed: Option<String>,
    pub llm_usages: Vec<DbLlmUsage>,
}

impl DbSession {
    pub fn total_input_tokens(&self) -> u64 {
        self.llm_usages.iter().map(|u| u.input_tokens).sum()
    }

    pub fn total_output_tokens(&self) -> u64 {
        self.llm_usages.iter().map(|u| u.output_tokens).sum()
    }

    pub fn total_cache_read_tokens(&self) -> u64 {
        self.llm_usages.iter().map(|u| u.cache_read_tokens).sum()
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens() + self.total_output_tokens()
    }

    #[allow(dead_code)]
    pub fn total_cost(&self) -> f64 {
        self.llm_usages.iter().map(|u| u.cost).sum()
    }
}

/// Local SQLite usage telemetry database (`<app_data_dir>/history.db`).
pub struct UsageDatabase {
    db_path: PathBuf,
    conn: Mutex<SqliteConn>,
}

const RETENTION_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1000;

impl UsageDatabase {
    /// Opens `<app_data_dir>/history.db` in WAL mode and ensures schema tables exist.
    pub fn new() -> anyhow::Result<Self> {
        let dir = directories::ProjectDirs::from("com", "stitch", "Stitch")
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".stitch"))
                    .unwrap_or_else(|| PathBuf::from(".stitch"))
            });
        Self::open(dir.join("history.db"))
    }

    /// Opens a SQLite database at the specified `path` (or `":memory:"` for tests).
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db_path = path.as_ref().to_path_buf();
        if db_path != Path::new(":memory:") {
            if let Some(parent) = db_path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create database directory {}", parent.display())
                    })?;
                }
            }
        }

        let conn = SqliteConn::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA busy_timeout = 5000;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS usage_sessions (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               tool_name TEXT NOT NULL,
               context_reference TEXT,
               timestamp INTEGER NOT NULL,
               ai_output TEXT,
               pushed TEXT
             );

             CREATE TABLE IF NOT EXISTS llm_usages (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               session_id INTEGER NOT NULL,
               label TEXT NOT NULL,
               model TEXT NOT NULL,
               input_tokens INTEGER NOT NULL,
               output_tokens INTEGER NOT NULL,
               cache_read_tokens INTEGER NOT NULL,
               cost REAL NOT NULL,
               multiplier REAL,
               FOREIGN KEY(session_id) REFERENCES usage_sessions(id) ON DELETE CASCADE
             );

             CREATE INDEX IF NOT EXISTS idx_usage_sessions_timestamp ON usage_sessions(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_llm_usages_session_id ON llm_usages(session_id);",
        )?;

        Ok(Self {
            db_path,
            conn: Mutex::new(conn),
        })
    }

    #[allow(dead_code)]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Inserts a new row in `usage_sessions` with the current timestamp and returns its `id`.
    pub fn create_session(
        &self,
        tool_name: &str,
        context_reference: Option<&str>,
        ai_output: Option<&str>,
    ) -> anyhow::Result<i64> {
        self.create_session_at(tool_name, context_reference, Self::now_ms(), ai_output)
    }

    /// Inserts a new row in `usage_sessions` with an explicit millisecond timestamp.
    pub fn create_session_at(
        &self,
        tool_name: &str,
        context_reference: Option<&str>,
        timestamp_ms: i64,
        ai_output: Option<&str>,
    ) -> anyhow::Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let stmt = conn.prepare(
            "INSERT INTO usage_sessions (tool_name, context_reference, timestamp, ai_output, pushed)
             VALUES (?1, ?2, ?3, ?4, NULL)",
        )?;
        stmt.bind_text(1, tool_name)?;
        stmt.bind_opt_text(2, context_reference)?;
        stmt.bind_i64(3, timestamp_ms)?;
        stmt.bind_opt_text(4, ai_output)?;
        stmt.step()?;
        Ok(conn.last_insert_rowid())
    }

    /// Updates `ai_output` on an existing `usage_sessions` record.
    pub fn update_session_output(&self, session_id: i64, ai_output: &str) -> anyhow::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let stmt = conn.prepare("UPDATE usage_sessions SET ai_output = ?1 WHERE id = ?2")?;
        stmt.bind_text(1, ai_output)?;
        stmt.bind_i64(2, session_id)?;
        stmt.step()?;
        Ok(())
    }

    /// Inserts a single row into `llm_usages`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_llm_usage(
        &self,
        session_id: i64,
        label: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cost: f64,
        multiplier: Option<f64>,
    ) -> anyhow::Result<i64> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let stmt = conn.prepare(
            "INSERT INTO llm_usages (session_id, label, model, input_tokens, output_tokens, cache_read_tokens, cost, multiplier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        stmt.bind_i64(1, session_id)?;
        stmt.bind_text(2, label)?;
        stmt.bind_text(3, model)?;
        stmt.bind_i64(4, input_tokens as i64)?;
        stmt.bind_i64(5, output_tokens as i64)?;
        stmt.bind_i64(6, cache_read_tokens as i64)?;
        stmt.bind_f64(7, cost)?;
        stmt.bind_opt_f64(8, multiplier)?;
        stmt.step()?;
        Ok(conn.last_insert_rowid())
    }

    /// Records a [`CopilotUsage`] into `llm_usages` for `session_id`.
    /// If `usage.phases` has entries, inserts one row per phase; otherwise inserts a single row using `default_label`.
    pub fn record_copilot_usage(
        &self,
        session_id: i64,
        default_label: &str,
        default_model: &str,
        usage: &CopilotUsage,
        default_multiplier: Option<f64>,
    ) -> anyhow::Result<()> {
        if let Some(phases) = usage.phases.as_ref().filter(|p| !p.is_empty()) {
            for phase in phases {
                let model = if phase.model.trim().is_empty() {
                    default_model
                } else {
                    phase.model.trim()
                };
                self.record_llm_usage(
                    session_id,
                    &phase.phase_title,
                    model,
                    phase.input_tokens,
                    phase.output_tokens,
                    phase.cache_read_tokens,
                    phase.cost,
                    Some(phase.multiplier),
                )?;
            }
        } else {
            let model = usage
                .model
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(default_model);
            let model = if model.trim().is_empty() {
                "auto"
            } else {
                model.trim()
            };
            self.record_llm_usage(
                session_id,
                default_label,
                model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cost,
                default_multiplier,
            )?;
        }
        Ok(())
    }

    /// Replaces `llm_usages` for `session_id` with the latest cumulative [`CopilotUsage`].
    /// Used by multi-turn sessions (like Story Elaborator) that update cumulative token counts across refinements.
    pub fn replace_session_copilot_usage(
        &self,
        session_id: i64,
        default_label: &str,
        default_model: &str,
        usage: &CopilotUsage,
        default_multiplier: Option<f64>,
    ) -> anyhow::Result<()> {
        {
            let conn = self
                .conn
                .lock()
                .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
            let stmt = conn.prepare("DELETE FROM llm_usages WHERE session_id = ?1")?;
            stmt.bind_i64(1, session_id)?;
            stmt.step()?;
        }
        self.record_copilot_usage(
            session_id,
            default_label,
            default_model,
            usage,
            default_multiplier,
        )
    }

    /// Convenience helper that creates a `usage_sessions` record and logs its [`CopilotUsage`] in one call.
    #[allow(clippy::too_many_arguments)]
    pub fn log_completed_session(
        &self,
        tool_name: &str,
        context_reference: Option<&str>,
        ai_output: Option<&str>,
        default_label: &str,
        default_model: &str,
        usage: &CopilotUsage,
        default_multiplier: Option<f64>,
    ) -> anyhow::Result<i64> {
        let session_id = self.create_session(tool_name, context_reference, ai_output)?;
        self.record_copilot_usage(
            session_id,
            default_label,
            default_model,
            usage,
            default_multiplier,
        )?;
        Ok(session_id)
    }

    /// Sets `pushed = text` (e.g. `"1 comment"`, `"1 task"`) on `session_id`.
    pub fn record_push(&self, session_id: i64, text: &str) -> anyhow::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let stmt = conn.prepare("UPDATE usage_sessions SET pushed = ?1 WHERE id = ?2")?;
        stmt.bind_text(1, text)?;
        stmt.bind_i64(2, session_id)?;
        stmt.step()?;
        Ok(())
    }

    /// Reads the existing `pushed` column for `session_id`, parses `^(\d+)\s+PR\s+comment`,
    /// increments the counter, writes `"{count} PR comment(s)"`, and returns the updated string.
    pub fn increment_pushed_comment_count(&self, session_id: i64) -> anyhow::Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let existing = Self::read_pushed_locked(&conn, session_id)?;
        let current = existing
            .as_deref()
            .and_then(parse_pushed_pr_comment_count)
            .unwrap_or(0);
        let next = current + 1;
        let updated = format!("{next} PR comment(s)");
        let stmt = conn.prepare("UPDATE usage_sessions SET pushed = ?1 WHERE id = ?2")?;
        stmt.bind_text(1, &updated)?;
        stmt.bind_i64(2, session_id)?;
        stmt.step()?;
        Ok(updated)
    }

    /// Reads the existing `pushed` column for `session_id`, parses `^(\d+)\s+stor`,
    /// increments the counter, writes `"{count} story"` or `"{count} stories"`, and returns the updated string.
    pub fn increment_pushed_story_count(&self, session_id: i64) -> anyhow::Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let existing = Self::read_pushed_locked(&conn, session_id)?;
        let current = existing
            .as_deref()
            .and_then(parse_pushed_story_count)
            .unwrap_or(0);
        let next = current + 1;
        let updated = if next == 1 {
            "1 story".to_string()
        } else {
            format!("{next} stories")
        };
        let stmt = conn.prepare("UPDATE usage_sessions SET pushed = ?1 WHERE id = ?2")?;
        stmt.bind_text(1, &updated)?;
        stmt.bind_i64(2, session_id)?;
        stmt.step()?;
        Ok(updated)
    }

    fn read_pushed_locked(conn: &SqliteConn, session_id: i64) -> anyhow::Result<Option<String>> {
        let stmt = conn.prepare("SELECT pushed FROM usage_sessions WHERE id = ?1")?;
        stmt.bind_i64(1, session_id)?;
        if stmt.step()? {
            Ok(stmt.col_opt_text(0))
        } else {
            Ok(None)
        }
    }

    /// Fetches all sessions ordered by `timestamp DESC, id DESC` along with their `llm_usages`.
    pub fn get_history(&self) -> anyhow::Result<Vec<DbSession>> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;

        let mut sessions = Vec::new();
        {
            let stmt = conn.prepare(
                "SELECT id, tool_name, context_reference, timestamp, ai_output, pushed
                 FROM usage_sessions
                 ORDER BY timestamp DESC, id DESC",
            )?;
            while stmt.step()? {
                sessions.push(DbSession {
                    id: stmt.col_i64(0),
                    tool_name: stmt.col_text(1),
                    context_reference: stmt.col_opt_text(2),
                    timestamp: stmt.col_i64(3),
                    ai_output: stmt.col_opt_text(4),
                    pushed: stmt.col_opt_text(5),
                    llm_usages: Vec::new(),
                });
            }
        }

        if sessions.is_empty() {
            return Ok(sessions);
        }

        let usage_stmt = conn.prepare(
            "SELECT id, session_id, label, model, input_tokens, output_tokens, cache_read_tokens, cost, multiplier
             FROM llm_usages
             WHERE session_id = ?1
             ORDER BY id ASC",
        )?;

        // Prepare per-session query for llm_usages
        drop(usage_stmt);
        for session in &mut sessions {
            let stmt = conn.prepare(
                "SELECT id, session_id, label, model, input_tokens, output_tokens, cache_read_tokens, cost, multiplier
                 FROM llm_usages
                 WHERE session_id = ?1
                 ORDER BY id ASC",
            )?;
            stmt.bind_i64(1, session.id)?;
            while stmt.step()? {
                session.llm_usages.push(DbLlmUsage {
                    id: stmt.col_i64(0),
                    session_id: stmt.col_i64(1),
                    label: stmt.col_text(2),
                    model: stmt.col_text(3),
                    input_tokens: stmt.col_i64(4).max(0) as u64,
                    output_tokens: stmt.col_i64(5).max(0) as u64,
                    cache_read_tokens: stmt.col_i64(6).max(0) as u64,
                    cost: stmt.col_f64(7),
                    multiplier: stmt.col_opt_f64(8),
                });
            }
        }

        Ok(sessions)
    }

    /// Deletes all rows from `llm_usages` and `usage_sessions`.
    pub fn clear_history(&self) -> anyhow::Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        conn.execute_batch("DELETE FROM llm_usages; DELETE FROM usage_sessions;")?;
        Ok(())
    }

    /// Prunes sessions older than 30 days (`now_ms - 30 * 24 * 60 * 60 * 1000`).
    pub fn prune_expired_sessions(&self) -> anyhow::Result<usize> {
        let cutoff = Self::now_ms() - RETENTION_DAYS_MS;
        self.prune_older_than(cutoff)
    }

    /// Deletes sessions with `timestamp < cutoff_timestamp_ms` (cascading to `llm_usages`).
    pub fn prune_older_than(&self, cutoff_timestamp_ms: i64) -> anyhow::Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| anyhow!("UsageDatabase mutex poisoned"))?;
        let stmt = conn.prepare("DELETE FROM usage_sessions WHERE timestamp < ?1")?;
        stmt.bind_i64(1, cutoff_timestamp_ms)?;
        stmt.step()?;
        Ok(conn.changes())
    }
}

fn parse_pushed_pr_comment_count(pushed: &str) -> Option<u64> {
    let trimmed = pushed.trim();
    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digit_end == 0 {
        return None;
    }
    let count = trimmed[..digit_end].parse::<u64>().ok()?;
    let rest = trimmed[digit_end..].trim_start().to_ascii_lowercase();
    if rest.starts_with("pr comment") {
        let after_pr = rest["pr".len()..].trim_start();
        if after_pr.starts_with("comment") {
            return Some(count);
        }
    }
    // Also handle multiple spaces between PR and comment
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    if tokens.len() >= 2 && tokens[0] == "pr" && tokens[1].starts_with("comment") {
        Some(count)
    } else {
        None
    }
}

fn parse_pushed_story_count(pushed: &str) -> Option<u64> {
    let trimmed = pushed.trim();
    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digit_end == 0 {
        return None;
    }
    let count = trimmed[..digit_end].parse::<u64>().ok()?;
    let rest = trimmed[digit_end..].trim_start().to_ascii_lowercase();
    if rest.starts_with("stor") {
        Some(count)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PhaseUsage;

    #[test]
    fn sqlite_usage_database_creates_sessions_usages_and_push_counters() {
        let db = UsageDatabase::open(":memory:").expect("open in-memory SQLite DB");

        let s1 = db
            .create_session_at(
                "Story Writer",
                Some("Confluence - 9912"),
                1_700_000_000_000,
                Some("3 stories"),
            )
            .expect("create session 1");
        db.record_llm_usage(
            s1,
            "Story Generation",
            "claude-sonnet-4.5",
            12_000,
            3_500,
            4_000,
            0.12,
            Some(1.0),
        )
        .expect("record llm usage");

        assert_eq!(
            db.increment_pushed_story_count(s1).expect("story push 1"),
            "1 story"
        );
        assert_eq!(
            db.increment_pushed_story_count(s1).expect("story push 2"),
            "2 stories"
        );

        let pr_usage = CopilotUsage {
            input_tokens: 25_000,
            output_tokens: 4_000,
            cache_read_tokens: 10_000,
            cost: 0.25,
            model: Some("gpt-5".to_string()),
            phases: Some(vec![
                PhaseUsage {
                    phase_title: "Phase: Security".to_string(),
                    model: "gpt-5".to_string(),
                    input_tokens: 15_000,
                    output_tokens: 2_500,
                    cache_read_tokens: 6_000,
                    cost: 0.15,
                    multiplier: 1.0,
                },
                PhaseUsage {
                    phase_title: "Critic Phase".to_string(),
                    model: "claude-sonnet-4.5".to_string(),
                    input_tokens: 10_000,
                    output_tokens: 1_500,
                    cache_read_tokens: 4_000,
                    cost: 0.10,
                    multiplier: 1.5,
                },
            ]),
        };

        let s2 = db
            .create_session_at(
                "PR Reviewer",
                Some("PR - 42"),
                1_700_000_500_000,
                Some("4 comments"),
            )
            .expect("create session 2");
        db.record_copilot_usage(s2, "PR Review", "gpt-5", &pr_usage, Some(1.0))
            .expect("record multi-phase usage");

        assert_eq!(
            db.increment_pushed_comment_count(s2)
                .expect("pr comment push 1"),
            "1 PR comment(s)"
        );
        assert_eq!(
            db.increment_pushed_comment_count(s2)
                .expect("pr comment push 2"),
            "2 PR comment(s)"
        );

        let history = db.get_history().expect("get_history");
        assert_eq!(history.len(), 2);

        // Most recent first (s2 then s1)
        assert_eq!(history[0].id, s2);
        assert_eq!(history[0].tool_name, "PR Reviewer");
        assert_eq!(history[0].context_reference.as_deref(), Some("PR - 42"));
        assert_eq!(history[0].ai_output.as_deref(), Some("4 comments"));
        assert_eq!(history[0].pushed.as_deref(), Some("2 PR comment(s)"));
        assert_eq!(history[0].llm_usages.len(), 2);
        assert_eq!(history[0].total_input_tokens(), 25_000);
        assert_eq!(history[0].total_output_tokens(), 4_000);
        assert_eq!(history[0].total_cache_read_tokens(), 10_000);
        assert_eq!(history[0].llm_usages[1].label, "Critic Phase");
        assert_eq!(history[0].llm_usages[1].multiplier, Some(1.5));

        assert_eq!(history[1].id, s1);
        assert_eq!(history[1].pushed.as_deref(), Some("2 stories"));
        assert_eq!(history[1].llm_usages.len(), 1);
    }

    #[test]
    fn sqlite_usage_database_prunes_old_records_with_cascade_and_clears() {
        let db = UsageDatabase::open(":memory:").expect("open in-memory SQLite DB");

        let old_id = db
            .create_session_at("Test Case Writer", Some("Ticket - 10"), 1_000, Some("2 test cases"))
            .expect("old session");
        db.record_llm_usage(
            old_id,
            "Test Case Generation",
            "gpt-4.1",
            1_000,
            500,
            200,
            0.01,
            Some(1.0),
        )
        .expect("old usage");

        let new_id = db
            .create_session_at("Test Case Writer", Some("Ticket - 11"), 5_000, Some("5 test cases"))
            .expect("new session");
        db.record_llm_usage(
            new_id,
            "Test Case Generation",
            "gpt-4.1",
            2_000,
            800,
            400,
            0.02,
            Some(1.0),
        )
        .expect("new usage");
        db.record_push(new_id, "1 task").expect("record push");

        let pruned = db.prune_older_than(3_000).expect("prune older than 3000");
        assert_eq!(pruned, 1);

        let remaining = db.get_history().expect("history after prune");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, new_id);
        assert_eq!(remaining[0].pushed.as_deref(), Some("1 task"));
        assert_eq!(remaining[0].llm_usages.len(), 1);

        db.clear_history().expect("clear history");
        assert!(db.get_history().expect("history after clear").is_empty());
    }
}
