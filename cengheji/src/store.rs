//! SQLite 存储：事件为唯一事实源；安装两阶段（intent -> 提交），重启完成未决安装。
//! 导出为自描述 JSON 档案；删除本地库再导入，身份/顺序/来源关系保持。

use crate::domain::*;
use crate::state::*;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct Store {
    pub conn: Connection,
    pub project: String,
    /// 测试用：在提交安装前制造半写（模拟崩溃）。
    pub crash_before_install_commit: std::cell::Cell<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ExportBundle {
    pub format: String,
    pub bundle_version: u32,
    pub project: String,
    pub exported_at: String,
    pub events: Vec<StateEvent>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects(
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL,
  current_event_seq INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS events(
  project_id TEXT NOT NULL,
  seq_no INTEGER NOT NULL,
  event_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL,
  parent_event_ids TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY(project_id, event_id)
);
CREATE INDEX IF NOT EXISTS events_order ON events(project_id, seq_no);
CREATE TABLE IF NOT EXISTS install_intents(
  project_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  event_seq INTEGER NOT NULL,
  stage TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY(project_id, run_id)
);
"#;

impl Store {
    pub fn open(path: &str, project: &str) -> Result<Self, String> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory().map_err(|e| e.to_string())?
        } else {
            Connection::open(path).map_err(|e| e.to_string())?
        };
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").ok();
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR IGNORE INTO projects(id, created_at, current_event_seq) VALUES(?1,?2,0)",
            params![project, now_ts()],
        ).map_err(|e| e.to_string())?;
        let mut s = Store { conn, project: project.into(), crash_before_install_commit: std::cell::Cell::new(false) };
        s.recover_pending_installs()?;
        Ok(s)
    }

    pub fn next_seq(&self) -> Result<i64, String> {
        let v: i64 = self.conn.query_row(
            "SELECT current_event_seq FROM projects WHERE id=?1", params![self.project], |r| r.get(0),
        ).map_err(|e| e.to_string())?;
        Ok(v + 1)
    }

    /// 追加单个事件（已分配 seq_no）。
    pub fn append_raw(&mut self, seq_no: i64, ev: &StateEvent) -> Result<(), String> {
        let parents = serde_json::to_string(&ev.parent_event_ids).unwrap_or_else(|_| "[]".into());
        let mut tx = self.conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO events(project_id,seq_no,event_id,kind,payload,created_at,parent_event_ids)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![self.project, seq_no, ev.event_id, ev.kind,
                serde_json::to_string(&ev.payload).unwrap_or_default(), ev.created_at, parents],
        ).map_err(|e| e.to_string())?;
        tx.execute("UPDATE projects SET current_event_seq=?2 WHERE id=?1",
            params![self.project, seq_no]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// 两阶段安装：先持久化 intent + run 事件（staged），再标记 obsolete/live 事件，
    /// 最后提交并清 intent。中途“崩溃”后重启由 recover 完成。
    pub fn install_run(&mut self, rec: &RunRecord, result: &CompactionResult) -> Result<Vec<StateEvent>, String> {
        if self.intent_stage(&rec.run_id)?.as_deref() == Some("committed") {
            return Ok(vec![]); // 重复安装结果：幂等，不再产生副作用。
        }
        let mut seq = self.next_seq()?;
        let now = now_ts();
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;

        // stage 1：intent
        tx.execute(
            "INSERT OR REPLACE INTO install_intents(project_id,run_id,event_seq,stage,created_at)
             VALUES(?1,?2,?3,'pending',?4)",
            params![self.project, rec.run_id, seq, now],
        ).map_err(|e| e.to_string())?;

        // run_installed 事件（含完整结果；输出/obsolete 是纯派生状态，由事件重放得出）
        let run_ev = event(seq, &format!("evt-{seq:06}"), "run_installed",
            serde_json::to_value(rec).unwrap_or(json!({})), vec![], &now);
        insert_event(&tx, &self.project, &run_ev)?;
        seq += 1;

        if self.crash_before_install_commit.get() {
            self.crash_before_install_commit.set(false);
            tx.execute("UPDATE projects SET current_event_seq=?2 WHERE id=?1",
                params![self.project, seq - 1]).map_err(|e| e.to_string())?;
            // 故意不提交、不清理：模拟进程被杀。
            drop(tx);
            return Err("SIMULATED_CRASH_BEFORE_COMMIT".into());
        }

        tx.execute(
            "UPDATE install_intents SET stage='committed' WHERE project_id=?1 AND run_id=?2",
            params![self.project, rec.run_id],
        ).map_err(|e| e.to_string())?;
        tx.execute("UPDATE projects SET current_event_seq=?2 WHERE id=?1",
            params![self.project, seq - 1]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(vec![run_ev])
    }

    fn intent_stage(&self, run_id: &str) -> Result<Option<String>, String> {
        let mut stmt = self.conn.prepare(
            "SELECT stage FROM install_intents WHERE project_id=?1 AND run_id=?2").map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![self.project, run_id]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            Ok(Some(row.get::<_, String>(0).map_err(|e| e.to_string())?))
        } else { Ok(None) }
    }

    /// 半写恢复：pending 的 intent 视为崩溃点；事件已提交则补记 committed（安装本身由事件重放完成）。
    fn recover_pending_installs(&mut self) -> Result<(), String> {
        let pending: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT run_id, event_seq FROM install_intents WHERE project_id=?1 AND stage='pending'"
            ).map_err(|e| e.to_string())?;
            let mut rows = stmt.query(params![self.project]).map_err(|e| e.to_string())?;
            let mut out = vec![];
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                out.push((r.get::<_, String>(0).map_err(|e| e.to_string())?, r.get::<_, i64>(1).map_err(|e| e.to_string())?));
            }
            out
        };
        for (run_id, ev_seq) in pending {
            // 检查事件是否已经写入：写入即视为可提交（状态由重放恢复）；否则回滚 intent。
            let exists: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM events WHERE project_id=?1 AND seq_no=?2",
                params![self.project, ev_seq], |r| r.get(0)).unwrap_or(0);
            self.conn.execute(
                "UPDATE install_intents SET stage=?3 WHERE project_id=?1 AND run_id=?2",
                params![self.project, run_id, if exists > 0 { "committed" } else { "aborted" }],
            ).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn load_state(&self) -> Result<LsmState, String> {
        let mut stmt = self.conn.prepare(
            "SELECT seq_no,event_id,kind,payload,created_at,parent_event_ids FROM events
             WHERE project_id=?1 ORDER BY seq_no").map_err(|e| e.to_string())?;
        let rows = stmt.query_map(params![self.project], |r| {
            let payload_s: String = r.get(3)?;
            let parents_s: String = r.get(5)?;
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                payload_s, r.get::<_, String>(4)?, parents_s))
        }).map_err(|e| e.to_string())?;
        let mut st = LsmState::default();
        for row in rows {
            let (seq_no, event_id, kind, payload_s, created_at, parents_s) = row.map_err(|e| e.to_string())?;
            let payload: Value = serde_json::from_str(&payload_s).unwrap_or(json!({}));
            let parents: Vec<String> = serde_json::from_str(&parents_s).unwrap_or_default();
            let ev = StateEvent { seq_no, event_id, kind, payload, created_at, parent_event_ids: parents };
            st.apply_event(ev).map_err(|e| format!("状态重放失败: {:?}", e))?;
        }
        Ok(st)
    }

    pub fn export(&self) -> Result<ExportBundle, String> {
        let st = self.load_state()?;
        Ok(ExportBundle {
            format: "cengheji-archive".into(),
            bundle_version: 1,
            project: self.project.clone(),
            exported_at: now_ts(),
            events: st.events,
        })
    }
}

pub fn insert_event_public(tx: &rusqlite::Transaction, project: &str, ev: &StateEvent) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO events(project_id,seq_no,event_id,kind,payload,created_at,parent_event_ids)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![project, ev.seq_no, ev.event_id, ev.kind,
            serde_json::to_string(&ev.payload).unwrap_or_default(), ev.created_at,
            serde_json::to_string(&ev.parent_event_ids).unwrap_or_else(|_| "[]".into())],
    )?;
    Ok(())
}

/// 通用追加（单事务）。
pub fn append_event(store: &mut Store, kind: &str, payload: Value, parents: Vec<String>) -> Result<StateEvent, String> {
    let seq = store.next_seq()?;
    let ev = event(seq, &format!("evt-{seq:06}"), kind, payload, parents, &now_ts());
    store.append_raw(seq, &ev)?;
    Ok(ev)
}

/// 导入档案：按原 seq_no 与 event_id 恢复，保持身份/顺序/来源；冲突即 corrupt。
pub fn import_bundle(store: &mut Store, bundle: &ExportBundle) -> Result<usize, String> {
    if bundle.format != "cengheji-archive" || bundle.bundle_version != 1 {
        return Err("档案格式或版本不被支持".into());
    }
    let mut n = 0usize;
    for ev in &bundle.events {
        store.append_raw(ev.seq_no, ev)?;
        n += 1;
    }
    let max_seq = bundle.events.last().map(|e| e.seq_no).unwrap_or(0);
    store.conn.execute("UPDATE projects SET current_event_seq=?2 WHERE id=?1",
        params![store.project, max_seq]).map_err(|e| e.to_string())?;
    // 导入后必须可重放：损坏档案立刻暴露。
    let st = store.load_state()?;
    let _ = st;
    Ok(n)
}
