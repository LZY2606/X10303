//! SQLite 存储：事件追加、半写恢复、导出/导入往返。
//!
//! 半写模型：压实安装分两阶段（先写 pending_installs 行，再提交 install 事件）。
//! 任何在提交前的崩溃，重启恢复时都会回滚 pending——事件流从未出现半条提交。

use crate::domain::case::{case_pack, CasePack};
use crate::domain::engine::{Engine, Event};
use crate::domain::errors::{corrupt, LoadError, LoadResult};
use crate::domain::model::Params;
use crate::domain::units::Basis;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

pub const BUNDLE_FORMAT: &str = "lsm-audit/export-bundle";
pub const BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportBundle {
    pub format: String,
    pub version: u32,
    pub basis: Basis,
    pub initial_params: Params,
    pub head_ordinal: u64,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportReport {
    pub imported_events: usize,
    pub head_event_id: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingInstall {
    pub id: i64,
    pub compaction_id: String,
    pub phase: String,
    pub created_epoch_ms: u64,
    pub detail: String,
}

pub struct Store {
    conn: Mutex<Connection>,
    path: String,
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = if path == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").ok();
        conn.execute_batch(SCHEMA)?;
        let mut store = Store { conn: Mutex::new(conn), path: path.to_string() };
        store.recover_pending();
        Ok(store)
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn recover_pending(&self) {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("DELETE FROM pending_installs;").ok();
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get::<_, String>(0))
            .ok()
    }

    fn set_meta(&self, key: &str, value: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )
            .unwrap();
    }

    fn basis_and_params(&self) -> (Basis, Params) {
        let basis = self
            .meta("basis")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| Basis::case_default());
        let params = self
            .meta("initial_params")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| case_pack().initial_params);
        (basis, params)
    }

    pub fn load_events(&self) -> Vec<Event> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT payload FROM events ORDER BY ordinal ASC")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.iter().map(|j| serde_json::from_str(j).unwrap()).collect()
    }

    pub fn engine(&self) -> Engine {
        let (basis, params) = self.basis_and_params();
        Engine::replay(basis, params, &self.load_events())
    }

    pub fn append_event(&self, event: &Event) -> LoadResult<()> {
        let payload = serde_json::to_string(event).map_err(|e| corrupt("event", e.to_string()))?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events(ordinal, id, parent, branch, epoch_ms, payload) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.ordinal as i64,
                event.id,
                event.parent,
                event.branch,
                event.epoch_ms as i64,
                payload
            ],
        )
        .map_err(|e| corrupt("events.ordinal", format!("事件序号冲突或存储失败：{e}")))?;
        Ok(())
    }

    pub fn event_count(&self) -> usize {
        self.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.event_count() == 0
    }

    /// 重置并播种离线案例包（完整叙事一次跑通，包括预期内的失败尝试）。
    pub fn reseed_case(&self) -> Vec<crate::domain::case::StepReport> {
        let pack = case_pack();
        let (engine, reports) = crate::domain::case::run_case(&pack);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch("DELETE FROM events; DELETE FROM pending_installs;").unwrap();
        for event in &engine.log {
            let payload = serde_json::to_string(event).unwrap();
            tx.execute(
                "INSERT INTO events(ordinal, id, parent, branch, epoch_ms, payload) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    event.ordinal as i64,
                    event.id,
                    event.parent,
                    event.branch,
                    event.epoch_ms as i64,
                    payload
                ],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('basis', ?1),('initial_params', ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![
                serde_json::to_string(&pack.basis).unwrap(),
                serde_json::to_string(&pack.initial_params).unwrap()
            ],
        )
        .unwrap();
        tx.commit().unwrap();
        reports
    }

    /// 半写模拟：写入“安装进行中”标记（提交事件之前）。
    pub fn begin_install(&self, compaction_id: &str, phase: &str, created_epoch_ms: u64, detail: &str) {
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO pending_installs(compaction_id, phase, created_epoch_ms, detail) VALUES(?1, ?2, ?3, ?4)",
                params![compaction_id, phase, created_epoch_ms as i64, detail],
            )
            .unwrap();
    }

    pub fn pending_installs(&self) -> Vec<PendingInstall> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, compaction_id, phase, created_epoch_ms, detail FROM pending_installs ORDER BY id")
            .unwrap();
        stmt.query_map([], |row| {
            Ok(PendingInstall {
                id: row.get(0)?,
                compaction_id: row.get(1)?,
                phase: row.get(2)?,
                created_epoch_ms: row.get::<_, i64>(3)? as u64,
                detail: row.get(4)?,
            })
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
    }

    pub fn export_bundle(&self) -> ExportBundle {
        let (basis, params) = self.basis_and_params();
        let events = self.load_events();
        ExportBundle {
            format: BUNDLE_FORMAT.to_string(),
            version: BUNDLE_VERSION,
            basis,
            initial_params: params,
            head_ordinal: events.len() as u64,
            events,
        }
    }

    /// 导入：完整校验通过后整批写入。身份、顺序、父子关系逐一核对。
    pub fn import_bundle(&self, bundle: &ExportBundle) -> LoadResult<ImportReport> {
        if bundle.format != BUNDLE_FORMAT {
            return Err(corrupt("bundle.format", format!("格式标识不匹配：{}", bundle.format)));
        }
        if bundle.version != BUNDLE_VERSION {
            return Err(corrupt("bundle.version", format!("版本 {bundle_version} 不受支持", bundle_version = bundle.version)));
        }
        if bundle.events.is_empty() {
            return Err(corrupt("bundle.events", "事件流为空：输入不完整，禁止导入"));
        }

        // 顺序、父子链、身份重算校验。
        let mut parent: Option<String> = None;
        for (idx, event) in bundle.events.iter().enumerate() {
            let ordinal = idx as u64 + 1;
            if event.ordinal != ordinal {
                return Err(LoadError::Corrupt {
                    pos: crate::domain::errors::DiagnosticPos {
                        path: format!("bundle.events[{idx}].ordinal"),
                        detail: format!("ordinal 应为 {ordinal}，实际为 {}", event.ordinal),
                    },
                    diagnostics: Vec::new(),
                });
            }
            if event.parent != parent {
                return Err(corrupt(
                    format!("bundle.events[{idx}].parent"),
                    "父子链断裂：parent 与前一事件 id 不一致",
                ));
            }
            parent = Some(event.id.clone());
        }

        // 身份确定性重算（parent + branch + command）。
        for (idx, event) in bundle.events.iter().enumerate() {
            let recomputed = crate::domain::ids::stable_id(
                "evt",
                &serde_json::json!({"parent": event.parent, "branch": event.branch, "command": event.command}),
            );
            if recomputed != event.id {
                return Err(corrupt(
                    format!("bundle.events[{idx}].id"),
                    "事件身份与载荷不一致（材料可能被篡改）",
                ));
            }
        }

        let existing = self.load_events();
        if existing.is_empty() {
            for event in &bundle.events {
                self.append_event(event)?;
            }
        } else if existing == bundle.events {
            return Ok(ImportReport {
                imported_events: 0,
                head_event_id: bundle.events.last().map(|e| e.id.clone()),
                note: "identical_replay：目标库与导入包完全一致，幂等返回".to_string(),
            });
        } else {
            return Err(corrupt(
                "store.events",
                "目标数据库非空且与导入包不一致；请先 reseed 或使用空库",
            ));
        }

        self.set_meta("basis", &serde_json::to_string(&bundle.basis).unwrap());
        self.set_meta("initial_params", &serde_json::to_string(&bundle.initial_params).unwrap());

        // 导入后立即重放校验：所有事件必须可应用，状态头一致。
        let mut engine = Engine::replay(bundle.basis.clone(), bundle.initial_params.clone(), &bundle.events);
        let _ = &mut engine;
        Ok(ImportReport {
            imported_events: bundle.events.len(),
            head_event_id: bundle.events.last().map(|e| e.id.clone()),
            note: "imported".to_string(),
        })
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events(
  ordinal INTEGER PRIMARY KEY,
  id TEXT NOT NULL UNIQUE,
  parent TEXT,
  branch TEXT NOT NULL,
  epoch_ms INTEGER NOT NULL,
  payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pending_installs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  compaction_id TEXT NOT NULL,
  phase TEXT NOT NULL,
  created_epoch_ms INTEGER NOT NULL,
  detail TEXT NOT NULL
);
"#;

pub fn parse_bundle(json_text: &str) -> LoadResult<ExportBundle> {
    serde_json::from_str(json_text).map_err(|e| {
        LoadError::Corrupt {
            pos: crate::domain::errors::DiagnosticPos {
                path: "bundle".to_string(),
                detail: format!("导出包 JSON 解析失败：{e}"),
            },
            diagnostics: vec![e.to_string()],
        }
    })
}
