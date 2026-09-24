//! 事件存储：不可变材料与可修改决定分层。
//! 所有事件携带稳定身份、来源关系、参数版本与父级依赖；
//! 回到任意旧事件可恢复当时的依赖版本和可见结论。

use crate::domain::*;
use crate::engine::{CompactionInput, CompactionOutput};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;

/// 事件类型。机器可读取，作为判断依据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    /// 导入不可变材料（WAL / memtable flush / SSTable 摘要）
    MaterialImported { files: Vec<SstFile>, snapshots: Vec<Snapshot>, source_label: String },
    /// 参数修订（生成新参数版本）
    ParamsRevised { params: CompactionParams, note: String },
    /// 源数据局部改动（只影响局部依赖）
    SourceAmended { file: SstFile, replaces: String, note: String },
    /// 压实计算：输入摘要 + 参数版本 + 父级依赖 + 输出摘要
    CompactionComputed { input: CompactionInput, output: Option<CompactionOutput>, error: Option<String>, parent_events: Vec<String>, reason: String },
    /// 人工决定（接受/否定/调整），可互相冲突，全部保留
    DecisionRecorded { action: String, target_event: String, rationale: String, adopted: bool },
    /// 压实中断（资源预算/人工中止），保留现场
    CompactionInterrupted { input_digest: String, processed: usize, reason: String },
    /// 安装结果（重复安装同一结果必须幂等）
    ResultInstalled { output_digest: String, files: Vec<SstFile>, install_seq: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// 稳定身份：内容哈希，导出/导入往返保持不变
    pub id: String,
    pub seq: u64,
    pub branch: String,
    pub payload: EventPayload,
    pub created_at: String,
}

pub struct Store {
    pub conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub position: String,
    pub message: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("数据库错误: {0}")]
    Db(String),
    #[error("导入校验失败: {0}")]
    Validation(String),
    #[error("带位置诊断: {}", .0.iter().map(|d| format!("[{}] {}", d.position, d.message)).collect::<Vec<_>>().join("; "))]
    Diagnostics(Vec<Diagnostic>),
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Db(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn open_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Db(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    fn init(conn: &Connection) -> Result<(), StoreError> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS events (
               seq INTEGER PRIMARY KEY AUTOINCREMENT,
               id TEXT NOT NULL,
               branch TEXT NOT NULL,
               payload TEXT NOT NULL,
               created_at TEXT NOT NULL,
               UNIQUE(branch, id)
             );
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS import_staging (chunk BLOB);",
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
        // 半写恢复：清理上次导入中断留下的暂存区
        conn.execute("DELETE FROM import_staging", [])
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(())
    }

    pub fn append(&self, branch: &str, payload: EventPayload) -> Result<Event, StoreError> {
        let payload_json = serde_json::to_string(&payload).map_err(|e| StoreError::Db(e.to_string()))?;
        let id = content_id(&payload_json);
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO events (id, branch, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, branch, payload_json, created_at],
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
        let seq: i64 = conn
            .query_row("SELECT seq FROM events WHERE branch=?1 AND id=?2", params![branch, id], |r| r.get(0))
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(Event { id, seq: seq as u64, branch: branch.into(), payload, created_at })
    }

    pub fn events(&self, branch: Option<&str>) -> Result<Vec<Event>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match branch {
            Some(_) => conn
                    .prepare("SELECT seq, id, branch, payload, created_at FROM events WHERE branch=?1 ORDER BY seq")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
            None => conn
                    .prepare("SELECT seq, id, branch, payload, created_at FROM events ORDER BY seq")
                    .map_err(|e| StoreError::Db(e.to_string()))?,
        };
        let rows = match branch {
            Some(b) => stmt.query_map(params![b], row_to_event),
            None => stmt.query_map([], row_to_event),
        }
        .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| StoreError::Db(e.to_string()))?);
        }
        Ok(out)
    }

    pub fn get_event(&self, id: &str) -> Result<Option<Event>, StoreError> {
        Ok(self.events(None)?.into_iter().find(|e| e.id == id))
    }

    /// 导出：完整事件流 + 校验和，身份、分支、决定历史全部保留。
    pub fn export(&self) -> Result<String, StoreError> {
        let events = self.events(None)?;
        let body = serde_json::to_string_pretty(&events).map_err(|e| StoreError::Db(e.to_string()))?;
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        let checksum = format!("{:x}", h.finalize());
        Ok(serde_json::json!({
            "format": "lsm-review-export",
            "version": 1,
            "checksum": checksum,
            "events": events,
        })
        .to_string())
    }

    /// 导入：校验和验证；失败返回带位置诊断，不得用空值伪装成功。
    /// 重复导入幂等（UNIQUE(branch,id) + INSERT OR IGNORE）。
    pub fn import(&self, data: &str) -> Result<usize, StoreError> {
        let doc: serde_json::Value =
            serde_json::from_str(data).map_err(|e| StoreError::Diagnostics(vec![Diagnostic {
                position: "$".into(),
                message: format!("导出文件格式损坏: {}", e),
            }]))?;
        if doc.get("format").and_then(|v| v.as_str()) != Some("lsm-review-export") {
            return Err(StoreError::Diagnostics(vec![Diagnostic {
                position: "$.format".into(),
                message: "缺少或未知的 format 标识".into(),
            }]));
        }
        let events_val = doc.get("events").cloned().ok_or_else(|| {
            StoreError::Diagnostics(vec![Diagnostic { position: "$.events".into(), message: "缺少 events 数组".into() }])
        })?;
        let events: Vec<Event> = serde_json::from_value(events_val).map_err(|e| {
            StoreError::Diagnostics(vec![Diagnostic { position: "$.events".into(), message: format!("事件反序列化失败: {}", e) }])
        })?;
        // 校验和基于导出时的事件序列化形式重新计算并比对
        let body = serde_json::to_string_pretty(&events).map_err(|e| StoreError::Db(e.to_string()))?;
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        let expected = doc.get("checksum").and_then(|v| v.as_str()).unwrap_or("");
        let actual = format!("{:x}", h.finalize());
        if expected != actual {
            return Err(StoreError::Diagnostics(vec![Diagnostic {
                position: "$.checksum".into(),
                message: format!("校验和不匹配: 期望 {} 实际 {}", expected, actual),
            }]));
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| StoreError::Db(e.to_string()))?;
        let mut count = 0usize;
        for (i, ev) in events.iter().enumerate() {
            if ev.id.is_empty() || ev.branch.is_empty() {
                return Err(StoreError::Diagnostics(vec![Diagnostic {
                    position: format!("$.events[{}]", i),
                    message: "事件缺少稳定身份或分支".into(),
                }]));
            }
            let payload_json = serde_json::to_string(&ev.payload).map_err(|e| StoreError::Db(e.to_string()))?;
            tx.execute(
                "INSERT OR IGNORE INTO events (id, branch, payload, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![ev.id, ev.branch, payload_json, ev.created_at],
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
            count += 1;
        }
        tx.commit().map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(count)
    }
}

fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<Event> {
    let payload_json: String = row.get(3)?;
    let payload: EventPayload = serde_json::from_str(&payload_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(Event { seq: row.get::<_, i64>(0)? as u64, id: row.get(1)?, branch: row.get(2)?, payload, created_at: row.get(4)? })
}

pub fn content_id(payload_json: &str) -> String {
    let mut h = Sha256::new();
    h.update(payload_json.as_bytes());
    format!("ev-{:x}", h.finalize())[..23].to_string()
}

fn now_iso() -> String {
    // 不引入额外依赖：用系统时间格式化
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:06}Z", d.as_secs(), d.subsec_micros())
}
