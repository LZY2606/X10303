//! SQLite 存储层：不可变事件链 + 分支游标 + 投影表 + 半写暂存区。
//!
//! 不变量：
//! * `event.id = stable_id("ev", {seq, branch_id, parent_id, kind, payload_digest})`，
//!   因此等价的重放历史必然得到等价身份；`occurred_at_ms` 不参与身份计算。
//! * 所有领域表（`_cur` 投影）都由事件链重放得到；半写时暂存行留在
//!   `scratch_*` 中并可由 [`Db::recover_staging`] 判别（继续安装或丢弃）。
//! * 导出包逐事件写临时文件，最后原子 rename；崩溃留下 `.partial`，导入拒绝。

pub mod schema;
pub mod types;
pub mod project;
pub mod bundle;

use rusqlite::Connection;
use std::sync::Mutex;

pub use types::*;

#[derive(Debug, Clone)]
pub struct Db {
    pub path: String,
}

pub struct Handle {
    pub db: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &str) -> rusqlite::Result<(Db, Handle)> {
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 10_000)?;
        conn.execute_batch(schema::DDL)?;
        schema::ensure_meta(&conn)?;
        Ok((Db { path: path.to_string() }, Handle { db: Mutex::new(conn) }))
    }

    pub fn open_memory() -> rusqlite::Result<(Db, Handle)> {
        let mut conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(schema::DDL)?;
        schema::ensure_meta(&conn)?;
        Ok((Db { path: ":memory:".to_string() }, Handle { db: Mutex::new(conn) }))
    }
}
