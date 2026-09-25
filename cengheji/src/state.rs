//! 事件溯源的 LSM 状态：所有变动只追加事件，状态由重放得到；回到任意事件即恢复历史结论。

use crate::domain::*;
use crate::engine::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateEvent {
    pub seq_no: i64,
    pub event_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: Value,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_event_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LsmState {
    pub files: BTreeMap<String, SSTable>,
    pub snapshots: BTreeMap<String, Snapshot>,
    pub materials: BTreeMap<String, RawMaterial>,
    pub params: BTreeMap<String, EngineParams>,
    pub events: Vec<StateEvent>,
    pub runs: BTreeMap<String, RunRecord>,
    /// 已安装 run 集合（重复安装幂等的依据）。
    pub installed_runs: std::collections::BTreeSet<String>,
    /// 已见事件 id（重复安装结果检测）。
    pub seen_event_ids: std::collections::BTreeSet<String>,
    pub decisions: Vec<DecisionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    pub run_id: String,
    pub params_version: String,
    pub status: RunStatus,
    #[serde(default)]
    pub result: Option<CompactionResult>,
    pub input_digest: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecisionRecord {
    pub decision_id: String,
    pub run_id: Option<String>,
    pub action: String,
    pub rationale: String,
    pub params_version: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub conflicts_with: Vec<String>,
}

impl LsmState {
    pub fn live_files(&self) -> Vec<SSTable> {
        self.files.values().filter(|f| f.state == FileState::Live).cloned().collect()
    }
    pub fn live_sorted_by_key(&self) -> Vec<SSTable> {
        let mut v = self.live_files();
        v.sort_by(|a, b| a.level.cmp(&b.level).then_with(|| {
            let ar = a.key_range.start.raw.cmp(&b.key_range.start.raw);
            ar.then_with(|| a.id.cmp(&b.id))
        }));
        v
    }
    pub fn current_params(&self) -> EngineParams {
        self.params.values().last().cloned().unwrap_or_else(EngineParams::v1)
    }

    /// 重放到第 n 个事件（含）。n = events.len()-1 即当前。
    pub fn replay_to(&self, n: Option<usize>) -> Result<LsmState, EngineError> {
        let mut fresh = LsmState::default();
        let limit = n.map(|x| x + 1).unwrap_or(self.events.len());
        for ev in self.events.iter().take(limit) {
            fresh.apply_event(ev.clone())?;
        }
        Ok(fresh)
    }

    pub fn apply_event(&mut self, ev: StateEvent) -> Result<(), EngineError> {
        if self.seen_event_ids.contains(&ev.event_id) {
            // 重复安装结果：记录但不二次生效。
            return Ok(());
        }
        self.seen_event_ids.insert(ev.event_id.clone());
        match ev.kind.as_str() {
            "material_imported" | "material_amended" => {
                let mat: RawMaterial = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("材料载荷损坏: {e}")))?;
                if let Some(old) = &mat.supersedes {
                    if let Some(prev) = self.materials.get_mut(old) {
                        // 旧材料保留但标记逻辑失效（通过 superseded-by 链，不删除原件）。
                        let _ = prev;
                    }
                }
                self.materials.insert(mat.id.clone(), mat);
            }
            "params_revised" => {
                let p: EngineParams = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("参数版本损坏: {e}")))?;
                self.params.insert(p.version.clone(), p);
            }
            "snapshot_registered" | "snapshot_released" => {
                let mut snap: Snapshot = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("snapshot 损坏: {e}")))?;
                if ev.kind == "snapshot_released" { snap.active = false; }
                self.snapshots.insert(snap.id.clone(), snap);
            }
            "file_flushed" | "file_ingested" | "file_staged" => {
                let f: SSTable = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("SSTable 载荷损坏: {e}")))?;
                if self.files.contains_key(&f.id) {
                    return Err(corrupt("file", &f.id, "SSTable 身份冲突：重复安装同 id 文件"));
                }
                self.files.insert(f.id.clone(), f);
            }
            "file_obsoleted" => {
                let id = ev.payload.get("file_id").and_then(|v| v.as_str()).unwrap_or_default();
                if let Some(f) = self.files.get_mut(id) { f.state = FileState::Obsolete; }
            }
            "run_staged" | "run_interrupted" | "run_failed" => {
                let rec: RunRecord = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("run 载荷损坏: {e}")))?;
                self.runs.insert(rec.run_id.clone(), rec);
            }
            "run_installed" => {
                let rec: RunRecord = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("run 载荷损坏: {e}")))?;
                let run_id = rec.run_id.clone();
                if self.installed_runs.contains(&run_id) {
                    self.runs.insert(run_id, rec);
                    return Ok(());
                }
                if let Some(result) = &rec.result {
                    install_result(self, result, &ev.created_at)?;
                }
                self.installed_runs.insert(run_id);
                self.runs.insert(rec.run_id.clone(), rec);
            }
            "decision_recorded" => {
                let d: DecisionRecord = serde_json::from_value(ev.payload.clone())
                    .map_err(|e| corrupt("event", &ev.event_id, &format!("决定载荷损坏: {e}")))?;
                self.decisions.push(d);
            }
            "visibility_checked" | "diagnostic_attached" => {
                // 证据型事件只留存于事件流，不改状态。
            }
            other => {
                return Err(corrupt("event", &ev.event_id, &format!("未知事件类型: {other}")));
            }
        }
        self.events.push(ev);
        Ok(())
    }
}

fn corrupt(where_: &str, id: &str, msg: &str) -> EngineError {
    EngineError::Corrupt(Diagnostic {
        code: "corrupt_state_event".into(),
        severity: DiagSeverity::Error,
        location: format!("{}[{}]", where_, id),
        message: msg.into(),
        candidates: vec![],
    })
}

fn install_result(st: &mut LsmState, result: &CompactionResult, created_at: &str) -> Result<(), EngineError> {
    // 输出文件落盘（Staging 事件在 run_staged 时已创建；此处转 Live）。
    for b in &result.boundaries {
        if let Some(f) = st.files.get_mut(&b.file_id) {
            f.state = FileState::Live;
        }
    }
    // 输入文件转 obsolete。
    for id in &result.plan.input_file_ids {
        if let Some(f) = st.files.get_mut(id) {
            f.state = FileState::Obsolete;
        }
    }
    // 安装校验事件（身份/不重叠层约束）。
    let _ = created_at;
    let live = st.live_files();
    let decoded = decode_live_ranges(&live)?;
    let target = result.plan.selection.target_level;
    if target >= 1 {
        let tl: Vec<&(String, i64, DecodedRange)> = decoded.iter().filter(|(_, l, _)| *l == target).collect();
        for i in 0..tl.len() {
            for j in (i + 1)..tl.len() {
                if tl[i].2.intersects(&tl[j].2) {
                    return Err(EngineError::Unresolvable(Diagnostic {
                        code: "install_non_overlapping_violation".into(),
                        severity: DiagSeverity::Error,
                        location: format!("run[{}].install", result.plan.run_id),
                        message: format!("安装后 L{} 文件 {} 与 {} 区间相交", target, tl[i].0, tl[j].0),
                        candidates: vec![],
                    }));
                }
            }
        }
    }
    Ok(())
}

/// 由压实结果构造输出 SSTable（供 staged 事件）。
pub fn build_output_sstables(result: &CompactionResult, files_by_id: &BTreeMap<String, SSTable>) -> Vec<SSTable> {
    let mut out = Vec::new();
    for b in &result.boundaries {
        let mut points = Vec::new();
        for pid in &b.point_ids {
            for f in files_by_id.values() {
                if let Some(p) = f.points.iter().find(|p| p.id == pid) { points.push(p.clone()); }
            }
        }
        let mut ranges = Vec::new();
        for (rid, disp) in result.range_dispositions.iter().map(|r| (r.range_id.clone(), r)).collect::<BTreeMap<_, _>>() {
            for frag in &disp.fragments {
                if b.range_fragment_ids.contains(&frag.fragment_id) {
                    ranges.push(RangeTomb { id: frag.fragment_id.clone(), seq: disp.seq, range: frag.range.clone(), fragments: vec![rid.clone()] });
                }
            }
        }
        out.push(SSTable {
            id: b.file_id.clone(),
            level: result.plan.selection.target_level,
            state: FileState::Staging,
            origin: FileOrigin::CompactionOutput,
            key_range: b.range.clone(),
            size: b.size.clone(),
            points: points,
            ranges,
            provenance: Provenance {
                material_ids: vec![],
                run_id: Some(result.plan.run_id.clone()),
                params_version: Some(result.plan.params_version.clone()),
                parent_event_ids: vec![],
            },
        });
    }
    out
}

pub fn event(seq_no: i64, id: &str, kind: &str, payload: Value, parents: Vec<String>, now: &str) -> StateEvent {
    StateEvent {
        seq_no, event_id: id.into(), kind: kind.into(), payload, created_at: now.into(),
        parent_event_ids: parents,
    }
}

pub fn now_ts() -> String {
    // 确定性优先：测试注入；这里提供 UTC 基础时间（仅显示坐标）。
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("unix-ms:{}", n)
}

#[allow(dead_code)]
pub fn json_event(type_: &str, body: Value) -> Value {
    let mut m = body;
    if let Some(obj) = m.as_object_mut() { obj.insert("type".into(), json!(type_)); }
    m
}
