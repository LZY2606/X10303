//! 事件溯源存储层（SQLite）。
//!
//! 事件是唯一事实来源；层级文件等投影通过重放重建。
//! 安装结果走"暂存 -> 原子提交"两段式，进程在两段之间崩溃时
//! `recover_pending` 把残留暂存判为未完成（半写恢复），绝不出现一半文件。

use crate::domain::*;
use crate::engine::{self, CompactionRequest, CompactionResult, EngineState};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const DEFAULT_PARAMS: &str = "params:v1";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EventRecord {
    pub seq: i64,
    pub event_id: String,
    pub branch: String,
    pub parent_event_id: Option<String>,
    pub kind: String,
    pub at_basis: String,
    pub payload: Value,
    /// 派生事件记录输入摘要、参数版本、父级依赖。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MaterialBundle {
    pub wal: Vec<WalEntry>,
    pub snapshots: Vec<Snapshot>,
    pub sst: Vec<SstFile>,
    pub params: Vec<ParamsV>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BranchInfo {
    pub name: String,
    pub created_from: Option<String>,
    pub head_event_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PendingInstall {
    pub token: String,
    pub branch: String,
    pub result_event_id: String,
    pub request: CompactionRequest,
    pub result: CompactionResult,
    pub installed_count: usize,
}

/// 重放某分支得到的完整状态（材料 + 当前文件集 + 决定）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReplayState {
    pub rule_version: String,
    pub params: ParamsV,
    pub params_history: Vec<ParamsV>,
    pub wal: Vec<WalEntry>,
    pub snapshots: Vec<Snapshot>,
    pub files: Vec<SstFile>,
    pub events: Vec<EventRecord>,
    /// 已安装的压实结果 id（重复安装据此幂等拒绝）。
    pub installed_results: Vec<String>,
    pub rejected_results: Vec<String>,
    pub interrupted: Vec<PendingInstall>,
    pub source_digests: BTreeMap<String, String>,
}

impl Default for ReplayState {
    fn default() -> Self {
        ReplayState {
            rule_version: RULE_VERSION.to_string(),
            params: default_params(),
            params_history: Vec::new(),
            wal: Vec::new(), snapshots: Vec::new(), files: Vec::new(),
            events: Vec::new(), installed_results: Vec::new(),
            rejected_results: Vec::new(), interrupted: Vec::new(),
            source_digests: BTreeMap::new(),
        }
    }
}

pub struct Store {
    pub db: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS branches (
  name TEXT PRIMARY KEY,
  created_from TEXT,
  head_event_id TEXT
);
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT UNIQUE NOT NULL DEFAULT (lower(hex(randomblob(16)))),
  branch TEXT NOT NULL,
  parent_event_id TEXT,
  kind TEXT NOT NULL,
  at_basis TEXT NOT NULL,
  payload TEXT NOT NULL,
  input_digest TEXT,
  params_version TEXT,
  depends_on TEXT
);
CREATE TABLE IF NOT EXISTS branch_events (branch TEXT NOT NULL, event_id TEXT NOT NULL, PRIMARY KEY(branch, event_id));
-- 半写暂存：结果事件已写入，但文件安装尚未原子提交。
CREATE TABLE IF NOT EXISTS install_staging (
  token TEXT PRIMARY KEY,
  branch TEXT NOT NULL,
  result_event_id TEXT NOT NULL,
  request TEXT NOT NULL,
  result TEXT NOT NULL,
  installed_count INTEGER NOT NULL
);
-- 原始材料暂存：导入两段式（先落 staging，原子提交为 events）。
CREATE TABLE IF NOT EXISTS import_staging (
  token TEXT PRIMARY KEY,
  bundle TEXT NOT NULL,
  branch TEXT NOT NULL
);
"#;

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(SCHEMA)?;
        let mut s = Store { db };
        s.recover_pending()?;
        Ok(s)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let db = Connection::open_in_memory()?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch(SCHEMA)?;
        Ok(Store { db })
    }

    /// 半写恢复：进程上次若在安装提交前退出，残留暂存一律回滚为"未安装"，
    /// 结果事件保留并标记 interrupted，审阅者可显式续装或放弃。
    pub fn recover_pending(&mut self) -> rusqlite::Result<Vec<PendingInstall>> {
        let mut stmt = self.db.prepare(
            "SELECT token, branch, result_event_id, request, result, installed_count FROM install_staging")?;
        let rows = stmt.query_map([], |r| Ok(PendingInstall {
            token: r.get(0)?, branch: r.get(1)?, result_event_id: r.get(2)?,
            request: serde_json::from_str(&r.get::<_, String>(3)?).unwrap(),
            result: serde_json::from_str(&r.get::<_, String>(4)?).unwrap(),
            installed_count: r.get::<_, i64>(5)? as usize,
        }))?;
        let mut pending = Vec::new();
        for row in rows { pending.push(row?); }
        drop(stmt);
        // 回滚所有半写暂存：删除 staging；中断状态由 replay 依据结果事件重建。
        self.db.execute("DELETE FROM install_staging", [])?;
        let abandoned: Vec<(String, String)> = pending.iter()
            .map(|p| (p.branch.clone(), p.result_event_id.clone())).collect();
        for (branch, eid) in abandoned {
            // 在该结果事件后追加一条恢复事件，记录"检测到半写并回滚"，历史不被覆盖。
            let note = serde_json::json!({
                "note": "检测到安装半写（进程在原子提交前退出），已回滚为未安装状态",
                "result_event_id": eid,
            });
            self.append_event(&branch, "install_recovered", note,
                None, Some(vec![eid]))?;
        }
        Ok(pending)
    }

    pub fn list_branches(&self) -> rusqlite::Result<Vec<BranchInfo>> {
        let mut stmt = self.db.prepare("SELECT name, created_from, head_event_id FROM branches ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok(BranchInfo {
            name: r.get(0)?, created_from: r.get(1)?, head_event_id: r.get(2)?,
        }))?;
        rows.collect()
    }

    pub fn ensure_branch(&mut self, name: &str, created_from: Option<String>) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO branches(name, created_from, head_event_id) VALUES(?1, ?2, NULL)
             ON CONFLICT(name) DO NOTHING",
            params![name, created_from],
        )?;
        Ok(())
    }

    pub fn head_event_id(&self, branch: &str) -> rusqlite::Result<Option<String>> {
        self.db.query_row("SELECT head_event_id FROM branches WHERE name=?1",
            params![branch], |r| r.get::<_, Option<String>>(0))
    }

    pub fn append_event(&mut self, branch: &str, kind: &str, payload: Value,
                        digest: Option<String>, depends_on: Option<Vec<String>>)
        -> rusqlite::Result<EventRecord>
    {
        self.append_event_full(branch, kind, payload, digest, None, depends_on)
    }

    pub fn append_event_full(&mut self, branch: &str, kind: &str, payload: Value,
                             digest: Option<String>, params_version: Option<String>,
                             depends_on: Option<Vec<String>>)
        -> rusqlite::Result<EventRecord>
    {
        self.ensure_branch(branch, None)?;
        let parent = self.head_event_id(branch)?;
        let dep_json = depends_on.as_ref().map(|d| serde_json::to_string(d).unwrap());
        let at = payload.get("at_basis").and_then(|v| v.as_str())
            .unwrap_or(TIME_BASIS).to_string();
        let tx = self.db.transaction()?;
        tx.execute("INSERT INTO events
             (branch, parent_event_id, kind, at_basis, payload, input_digest, params_version, depends_on)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![branch, parent, kind, at, payload.to_string(), digest, params_version, dep_json])?;
        let seq = tx.last_insert_rowid();
        let event_id = tx.query_row(
            "SELECT event_id FROM events WHERE seq=?1", params![seq], |r| r.get::<_, String>(0))?;
        tx.execute("INSERT INTO branch_events(branch, event_id) VALUES(?1, ?2)
                    ON CONFLICT DO NOTHING", params![branch, event_id])?;
        tx.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2",
            params![event_id, branch])?;
        tx.commit()?;
        Ok(EventRecord {
            seq, event_id, branch: branch.to_string(), parent_event_id: parent,
            kind: kind.to_string(), at_basis: at, payload,
            input_digest: digest, params_version, depends_on,
        })
    }

    /// 导出/再导入专用：保留稳定 event_id 与 seq，保证身份、顺序、来源关系不变。
    pub fn insert_event_preserving(&mut self, ev: &EventRecord) -> rusqlite::Result<()> {
        self.ensure_branch(&ev.branch, None)?;
        let dep_json = ev.depends_on.as_ref().map(|d| serde_json::to_string(d).unwrap());
        self.db.execute(
            "INSERT INTO events(seq, event_id, branch, parent_event_id, kind, at_basis,
               payload, input_digest, params_version, depends_on)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![ev.seq, ev.event_id, ev.branch, ev.parent_event_id, ev.kind, ev.at_basis,
                ev.payload.to_string(), ev.input_digest, ev.params_version, dep_json])?;
        self.db.execute("INSERT INTO branch_events(branch, event_id) VALUES(?1, ?2)
                         ON CONFLICT DO NOTHING", params![ev.branch, ev.event_id])?;
        self.db.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2",
            params![ev.event_id, ev.branch])?;
        Ok(())
    }

    pub fn events_on_branch(&self, branch: &str) -> rusqlite::Result<Vec<EventRecord>> {
        let mut stmt = self.db.prepare(
            "SELECT e.seq, e.event_id, e.branch, e.parent_event_id, e.kind, e.at_basis,
                    e.payload, e.input_digest, e.params_version, e.depends_on
             FROM events e JOIN branch_events be ON be.event_id = e.event_id
             WHERE be.branch=?1 ORDER BY e.seq")?;
        let rows = stmt.query_map(params![branch], row_to_event)?;
        rows.collect()
    }

    pub fn all_events(&self) -> rusqlite::Result<Vec<EventRecord>> {
        let mut stmt = self.db.prepare(
            "SELECT seq, event_id, branch, parent_event_id, kind, at_basis,
                    payload, input_digest, params_version, depends_on
             FROM events ORDER BY seq")?;
        let rows = stmt.query_map([], row_to_event)?;
        rows.collect()
    }
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRecord> {
    let dep: Option<String> = r.get(9)?;
    Ok(EventRecord {
        seq: r.get(0)?, event_id: r.get(1)?, branch: r.get(2)?,
        parent_event_id: r.get(3)?, kind: r.get(4)?, at_basis: r.get(5)?,
        payload: serde_json::from_str(&r.get::<_, String>(6)?).unwrap(),
        input_digest: r.get(7)?, params_version: r.get(8)?,
        depends_on: dep.map(|s| serde_json::from_str(&s).unwrap()),
    })
}

impl Store {
    /// 重放分支到给定事件（None=HEAD），重建完整状态与投影。
    pub fn replay(&self, branch: &str, until_event_id: Option<&str>) -> rusqlite::Result<ReplayState> {
        let all = self.events_on_branch(branch)?;
        let mut state = ReplayState::default();
        state.rule_version = RULE_VERSION.to_string();
        state.params = default_params();
        state.params_history.push(state.params.clone());

        for ev in &all {
            let stop = until_event_id.map_or(false, |u| u == ev.event_id);
            apply_event(&mut state, ev);
            if stop { state.events.push(ev.clone()); break; }
            state.events.push(ev.clone());
        }
        Ok(state)
    }

    pub fn engine_state(&self, branch: &str, at: Option<&str>) -> rusqlite::Result<EngineState> {
        let s = self.replay(branch, at)?;
        Ok(EngineState {
            rule_version: RULE_VERSION.to_string(),
            params_version: s.params.version.clone(),
            params: s.params.clone(),
            snapshots: s.snapshots.clone(),
            files: s.files.clone(),
        })
    }
}

pub fn default_params() -> ParamsV {
    ParamsV {
        version: DEFAULT_PARAMS.to_string(),
        tie_policy: TiePolicy::DeleteWins,
        max_output_bytes: qty_bytes("64", 64),
        max_total_input_bytes: qty_bytes("4096", 4096),
        key_basis: KEY_BASIS.to_string(),
        seq_basis: SEQ_BASIS.to_string(),
        note: "基线参数：删除在内部键序中优先；输出预算 64 byte；输入预算 4096 byte".to_string(),
    }
}

fn qty_bytes(raw: &str, n: u64) -> RawQty {
    RawQty { raw: raw.to_string(), unit: UNIT_BYTE.to_string(), normalized: Some(n), note: None }
}

fn apply_event(state: &mut ReplayState, ev: &EventRecord) {
    let p = &ev.payload;
    match ev.kind.as_str() {
        "materials_imported" => {
            if let Some(v) = p.get("wal").and_then(|v| serde_json::from_value::<Vec<WalEntry>>(v.clone()).ok()) {
                state.wal.extend(v);
            }
            if let Some(v) = p.get("snapshots").and_then(|v| serde_json::from_value::<Vec<Snapshot>>(v.clone()).ok()) {
                state.snapshots.extend(v);
            }
            if let Some(v) = p.get("sst").and_then(|v| serde_json::from_value::<Vec<SstFile>>(v.clone()).ok()) {
                state.files.extend(v);
            }
            if let Some(d) = p.get("source_digest").and_then(|v| v.as_str()) {
                state.source_digests.insert(ev.event_id.clone(), d.to_string());
            }
        }
        "raw_material_appended" => {
            if let Some(v) = serde_json::from_value::<WalEntry>(p.get("entry").cloned().unwrap_or(Value::Null)).ok() {
                state.wal.push(v);
            }
        }
        "snapshot_added" => {
            if let Ok(s) = serde_json::from_value::<Snapshot>(p.get("snapshot").cloned().unwrap_or(Value::Null)) {
                state.snapshots.push(s);
            }
        }
        "snapshot_released" => {
            if let Some(id) = p.get("snapshot_id").and_then(|v| v.as_str()) {
                if let Some(s) = state.snapshots.iter_mut().find(|s| s.id == id) { s.active = false; }
            }
        }
        "params_revised" => {
            if let Ok(params) = serde_json::from_value::<ParamsV>(p.get("params").cloned().unwrap_or(Value::Null)) {
                state.params = params.clone();
                state.params_history.push(params);
            }
        }
        "memtable_flushed" => {
            if let Ok(f) = serde_json::from_value::<SstFile>(p.get("sst").cloned().unwrap_or(Value::Null)) {
                state.files.push(f);
            }
        }
        "compaction_planned" | "compaction_recomputed" => {
            // 计划本身不改文件；中断/完成都在 install 事件投影。
        }
        "compaction_installed" => {
            install_result(state, p, ev);
        }
        "install_recovered" => { /* 半写已回滚：投影不动，事件本身即证据 */ }
        "compaction_rejected" => {
            if let Some(id) = p.get("result_event_id").and_then(|v| v.as_str()) {
                if !state.rejected_results.contains(&id.to_string()) {
                    state.rejected_results.push(id.to_string());
                }
            }
        }
        "compaction_abandoned" => {
            if let Some(id) = p.get("result_event_id").and_then(|v| v.as_str()) {
                state.interrupted.retain(|pi| pi.result_event_id != id);
            }
        }
        "branch_forked" | "note" => {}
        _ => {}
    }
}

fn install_result(state: &mut ReplayState, p: &Value, ev: &EventRecord) {
    let result: CompactionResult = match serde_json::from_value(p.get("result").cloned().unwrap_or(Value::Null)) {
        Ok(r) => r, Err(_) => return,
    };
    let result_event_id = p.get("result_event_id").and_then(|v| v.as_str())
        .unwrap_or(&ev.event_id).to_string();
    // 幂等：同一结果重复安装只记录一次，文件集不变。
    if state.installed_results.contains(&result_event_id) { return; }
    let installed_count = p.get("installed_count").and_then(|v| v.as_u64()).unwrap_or(result.outputs.len() as u64) as usize;
    let request: CompactionRequest = serde_json::from_value(p.get("request").cloned().unwrap_or(Value::Null))
        .unwrap_or(CompactionRequest { anchor_level: 0, anchor_file_id: None, include_levels: vec![], crash_before_install: None });

    let mut outputs_meta: Vec<SstFile> = match serde_json::from_value::<Vec<SstFile>>(p.get("output_files").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v, Err(_) => build_output_files(&result, &request, &state),
    };
    outputs_meta.truncate(installed_count);

    // 移除已安装的输入文件（未安装的输入在中断时保留）。
    if installed_count == result.outputs.len() {
        state.files.retain(|f| !result.selected_file_ids.contains(&f.id));
        state.files.extend(outputs_meta);
        state.installed_results.push(result_event_id.clone());
        state.interrupted.retain(|pi| pi.result_event_id != result_event_id);
    } else {
        // 部分安装（压实中断后续装）：已提交输出原子进入，输入文件仍保留；
        // 实际生产中输入在最后一笔事务里删除，这里同样不在半安装时删除。
        state.files.extend(outputs_meta);
        state.interrupted.push(PendingInstall {
            token: p.get("token").and_then(|v| v.as_str()).unwrap_or("staged").to_string(),
            branch: ev.branch.clone(),
            result_event_id: result_event_id.clone(),
            request, result, installed_count,
        });
    }
}

/// 根据压实结果摘要构造输出 SSTable 元数据（points/ranges 从存活条目重建）。
pub fn build_output_files(result: &CompactionResult, request: &CompactionRequest, state: &ReplayState) -> Vec<SstFile> {
    let target_level = if request.anchor_level == 0 { 1 } else { request.anchor_level + 1 };
    let mut all_points: BTreeMap<String, PointOp> = BTreeMap::new();
    let mut all_ranges: BTreeMap<String, RangeTombstone> = BTreeMap::new();
    for f in &state.files {
        for pt in &f.points { all_points.insert(pt.id.clone(), pt.clone()); }
        for r in &f.ranges { all_ranges.insert(r.id.clone(), r.clone()); }
    }
    result.outputs.iter().map(|o| {
        let points: Vec<PointOp> = o.point_ids.iter().filter_map(|id| all_points.get(id).cloned()).collect();
        let ranges: Vec<RangeTombstone> = o.range_ids.iter().filter_map(|id| all_ranges.get(id).cloned()).collect();
        SstFile {
            id: o.proposed_id.clone(),
            level: o.level.max(target_level),
            min_key: o.min_key.clone(), max_key: o.max_key.clone(),
            min_seq: o.min_seq, max_seq: o.max_seq,
            size: qty_bytes(&o.size_bytes.to_string(), o.size_bytes),
            points, ranges,
            origin: FileOrigin::Compaction { result_id: result.input_digest.clone(), index: 0 },
            key_basis: KEY_BASIS.to_string(),
        }
    }).collect()
}

/// 规范化导入材料：保留 raw，派生层补 normalized，并产出带位置诊断。
pub fn normalize_bundle(b: &mut MaterialBundle) {
    for f in b.sst.iter_mut() {
        if f.size.normalized.is_none() {
            if let Ok(n) = parse_bytes(&f.size.raw) { f.size.normalized = Some(n); }
        }
        for pt in f.points.iter_mut() {
            if let Some(v) = pt.value.as_mut() {
                if v.normalized.is_none() {
                    if let Ok(n) = parse_bytes(&v.raw) { v.normalized = Some(n); }
                }
            }
        }
    }
    let diags = engine::validate_files(&b.sst);
    b.diagnostics = diags;
}

pub fn parse_bytes(raw: &str) -> Result<u64, std::num::ParseIntError> {
    let trimmed = raw.trim().trim_end_matches(|c: char| c.is_alphabetic() || c == 'B' || c == 'b');
    trimmed.parse::<u64>()
}

// ---------------- 高层操作：导入、压实决定、分支、导出 ----------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExportFile {
    pub schema_version: String,
    pub rule_version: String,
    pub branches: Vec<BranchInfo>,
    pub events: Vec<EventRecord>,
}

impl Store {
    /// 导入第一段：材料落到 import_staging（崩溃后残留即半写，重启清理）。
    pub fn stage_import(&mut self, token: &str, branch: &str, bundle: &MaterialBundle) -> rusqlite::Result<()> {
        self.ensure_branch(branch, None)?;
        self.db.execute("INSERT INTO import_staging(token, bundle, branch) VALUES(?1, ?2, ?3)",
            params![token, serde_json::to_string(bundle).unwrap(), branch])?;
        Ok(())
    }

    /// 导入第二段：原子提交为一个 materials_imported 事件，并清理暂存。
    pub fn commit_import(&mut self, token: &str) -> rusqlite::Result<EventRecord> {
        let row = self.db.query_row("SELECT bundle, branch FROM import_staging WHERE token=?1",
            params![token], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
        let (bundle_json, branch) = row?;
        let bundle: MaterialBundle = serde_json::from_str(&bundle_json).unwrap();
        let digest = crate::util::digest_hex(&bundle);
        let payload = serde_json::json!({
            "at_basis": TIME_BASIS,
            "wal": bundle.wal, "snapshots": bundle.snapshots, "sst": bundle.sst,
            "source_digest": digest,
            "diagnostics": bundle.diagnostics,
        });
        let tx = self.db.transaction()?;
        let mut conn = tx;
        conn.execute("DELETE FROM import_staging WHERE token=?1", params![token])?;
        // 直接在事务里插入事件以保证两段原子性：
        let parent: Option<String> = conn.query_row("SELECT head_event_id FROM branches WHERE name=?1",
            params![branch], |r| r.get(0)).unwrap_or(None);
        conn.execute("INSERT INTO events(branch, parent_event_id, kind, at_basis, payload, input_digest, params_version, depends_on)
             VALUES(?1, ?2, 'materials_imported', ?3, ?4, ?5, NULL, NULL)",
            params![branch, parent, TIME_BASIS, payload.to_string(), digest])?;
        let seq = conn.last_insert_rowid();
        let event_id = conn.query_row("SELECT event_id FROM events WHERE seq=?1", params![seq], |r| r.get::<_, String>(0))?;
        conn.execute("INSERT INTO branch_events(branch,event_id) VALUES(?1,?2) ON CONFLICT DO NOTHING",
            params![branch, event_id])?;
        conn.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2", params![event_id, branch])?;
        conn.commit()?;
        Ok(EventRecord {
            seq, event_id, branch, parent_event_id: parent,
            kind: "materials_imported".to_string(), at_basis: TIME_BASIS.to_string(),
            payload, input_digest: Some(digest), params_version: None, depends_on: None,
        })
    }

    /// 崩溃后清理：删除所有未提交的导入暂存（半写恢复，材料不会半生效）。
    pub fn abort_stale_imports(&mut self) -> rusqlite::Result<usize> {
        self.db.execute("DELETE FROM import_staging", [])
    }

    /// 计算并记录一次压实计划（纯引擎结果落事件，文件不变）。重复等价输入得到同一 digest。
    pub fn plan_compaction(&mut self, branch: &str, request: &CompactionRequest)
        -> rusqlite::Result<(EventRecord, CompactionResult)>
    {
        let state = self.engine_state(branch, None)?;
        let input = serde_json::json!({"state": &state, "request": request});
        let digest = crate::util::digest_hex(&input);
        let result = engine::run_compaction(&state, request, digest.clone());
        let payload = serde_json::json!({
            "at_basis": TIME_BASIS, "request": request, "result": result,
        });
        let ev = self.append_event_full(branch, "compaction_planned", payload,
            Some(digest), Some(state.params.version.clone()),
            Some(result.selected_file_ids.iter().cloned().collect()))?;
        Ok((ev, result))
    }

    /// 安装：写入 install_staging（半段），随后原子提交为 compaction_installed 事件。
    /// `simulate_crash` 只写暂存不提交，用于半写恢复测试。
    pub fn install_compaction(&mut self, branch: &str, result_event_id: &str,
                              output_files: Vec<SstFile>, installed_count: usize,
                              simulate_crash: bool) -> rusqlite::Result<Option<EventRecord>>
    {
        let ev = self.event_by_id(result_event_id)?;
        let result: CompactionResult = serde_json::from_value(
            ev.payload.get("result").cloned().unwrap()).unwrap();
        let request: CompactionRequest = serde_json::from_value(
            ev.payload.get("request").cloned().unwrap()).unwrap();
        let token = format!("install:{}", result_event_id);
        let staged = serde_json::json!({
            "token": token, "branch": branch, "result_event_id": result_event_id,
            "request": request, "result": result, "installed_count": installed_count,
            "output_files": output_files,
        });
        self.db.execute(
            "INSERT OR REPLACE INTO install_staging(token, branch, result_event_id, request, result, installed_count)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![token, branch, result_event_id,
                serde_json::to_string(&staged["request"]).unwrap(),
                serde_json::to_string(&staged["result"]).unwrap(),
                installed_count as i64])?;
        if simulate_crash { return Ok(None); }
        self.commit_install(&token)
    }

    pub fn commit_install(&mut self, token: &str) -> rusqlite::Result<Option<EventRecord>> {
        let row = self.db.query_row(
            "SELECT branch, result_event_id, request, result, installed_count FROM install_staging WHERE token=?1",
            params![token], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?)));
        let Ok((branch, rid, reqj, resj, count)) = row else { return Ok(None); };
        let replay = self.replay(&branch, None)?;
        if replay.installed_results.contains(&rid) {
            // 重复安装：幂等，直接清理暂存并返回已有事件，不改文件。
            self.db.execute("DELETE FROM install_staging WHERE token=?1", params![token])?;
            return Ok(Some(self.event_by_id(&rid)?));
        }
        let result: CompactionResult = serde_json::from_str(&resj).unwrap();
        let request: CompactionRequest = serde_json::from_str(&reqj).unwrap();
        let output_files = build_output_files(&result, &request, &replay);
        let rid_for_json = rid.clone();
        let result_digest = result.input_digest.clone();
        let result_params_v = result.params_version.clone();
        let payload = serde_json::json!({
            "at_basis": TIME_BASIS, "token": token, "result_event_id": rid_for_json,
            "request": request, "result": result, "installed_count": count,
            "output_files": output_files,
        });
        let tx = self.db.transaction()?;
        let mut conn = tx;
        let parent: Option<String> = conn.query_row("SELECT head_event_id FROM branches WHERE name=?1",
            params![branch], |r| r.get(0)).unwrap_or(None);
        conn.execute("DELETE FROM install_staging WHERE token=?1", params![token])?;
        conn.execute("INSERT INTO events(branch, parent_event_id, kind, at_basis, payload, input_digest, params_version, depends_on)
             VALUES(?1, ?2, 'compaction_installed', ?3, ?4, ?5, ?6, ?7)",
            params![branch, parent, TIME_BASIS, payload.to_string(),
                result_digest, result_params_v,
                serde_json::to_string(&vec![rid.clone()]).unwrap()])?;
        let seq = conn.last_insert_rowid();
        let event_id = conn.query_row("SELECT event_id FROM events WHERE seq=?1", params![seq], |r| r.get::<_, String>(0))?;
        conn.execute("INSERT INTO branch_events(branch,event_id) VALUES(?1,?2) ON CONFLICT DO NOTHING",
            params![branch, event_id])?;
        conn.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2", params![event_id, branch])?;
        conn.commit()?;
        Ok(Some(EventRecord {
            seq, event_id, branch, parent_event_id: parent,
            kind: "compaction_installed".to_string(), at_basis: TIME_BASIS.to_string(),
            payload, input_digest: Some(result_digest),
            params_version: Some(result_params_v),
            depends_on: Some(vec![rid]),
        }))
    }

    pub fn decision_reject(&mut self, branch: &str, result_event_id: &str, reason: &str)
        -> rusqlite::Result<EventRecord>
    {
        let payload = serde_json::json!({
            "at_basis": TIME_BASIS, "result_event_id": result_event_id,
            "decision": "reject", "reason": reason,
        });
        self.append_event(branch, "compaction_rejected", payload,
            None, Some(vec![result_event_id.to_string()]))
    }

    pub fn revise_params(&mut self, branch: &str, params: ParamsV) -> rusqlite::Result<EventRecord> {
        let digest = crate::util::digest_hex(&params);
        let payload = serde_json::json!({"at_basis": TIME_BASIS, "params": params});
        self.append_event_full(branch, "params_revised", payload, Some(digest),
            Some(params.version.clone()), None)
    }

    pub fn fork(&mut self, new_branch: &str, from_branch: &str, at_event: Option<&str>, reason: &str)
        -> rusqlite::Result<()>
    {
        let evs = self.events_on_branch(from_branch)?;
        self.db.execute("INSERT INTO branches(name, created_from, head_event_id) VALUES(?1, ?2, NULL)
                         ON CONFLICT(name) DO NOTHING",
            params![new_branch, from_branch])?;
        for ev in &evs {
            self.db.execute("INSERT INTO branch_events(branch,event_id) VALUES(?1,?2)
                             ON CONFLICT DO NOTHING", params![new_branch, ev.event_id])?;
            if let Some(stop) = at_event {
                if ev.event_id == stop {
                    self.db.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2",
                        params![ev.event_id, new_branch])?;
                    break;
                }
            }
        }
        if at_event.is_none() {
            if let Some(last) = evs.last() {
                self.db.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2",
                    params![last.event_id, new_branch])?;
            }
        }
        let note = serde_json::json!({"at_basis": TIME_BASIS, "from_branch": from_branch,
            "at_event": at_event, "reason": reason});
        self.append_event(new_branch, "branch_forked", note, None,
            at_event.map(|s| vec![s.to_string()]))?;
        Ok(())
    }

    pub fn event_by_id(&self, id: &str) -> rusqlite::Result<EventRecord> {
        let mut stmt = self.db.prepare(
            "SELECT seq, event_id, branch, parent_event_id, kind, at_basis,
                    payload, input_digest, params_version, depends_on FROM events WHERE event_id=?1")?;
        stmt.query_row(params![id], row_to_event)
    }

    pub fn export(&self) -> rusqlite::Result<ExportFile> {
        Ok(ExportFile {
            schema_version: "export:lsm-review:v1".to_string(),
            rule_version: RULE_VERSION.to_string(),
            branches: self.list_branches()?,
            events: self.all_events()?,
        })
    }

    /// 导入导出包：保留全部 event_id/seq/父子关系。返回导入事件数。
    pub fn import_export(&mut self, f: ExportFile) -> rusqlite::Result<usize> {
        let mut n = 0;
        for b in &f.branches {
            self.ensure_branch(&b.name, b.created_from.clone())?;
        }
        for ev in &f.events {
            let exists: i64 = self.db.query_row(
                "SELECT COUNT(*) FROM events WHERE event_id=?1", params![ev.event_id], |r| r.get(0))?;
            if exists == 0 {
                self.insert_event_preserving(ev)?;
                n += 1;
            }
        }
        for b in &f.branches {
            if let Some(h) = &b.head_event_id {
                self.db.execute("UPDATE branches SET head_event_id=?1 WHERE name=?2",
                    params![h, b.name])?;
            }
        }
        Ok(n)
    }

    pub fn flush_memtable(&mut self, branch: &str, wal_ids: &[String], file: SstFile)
        -> rusqlite::Result<EventRecord>
    {
        let digest = crate::util::digest_hex(&(&wal_ids, &file));
        let payload = serde_json::json!({
            "at_basis": TIME_BASIS, "wal_ids": wal_ids, "sst": file,
        });
        self.append_event_full(branch, "memtable_flushed", payload, Some(digest),
            None, Some(wal_ids.iter().map(|s| s.to_string()).collect()))
    }
}
