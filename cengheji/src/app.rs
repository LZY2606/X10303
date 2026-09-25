//! 应用服务层：把导入、压实选择/执行/安装、参数修订、决定、读取查询串成完整叙事。
//! HTTP 与测试调用同一批函数，业务规则不在浏览器里。

use crate::domain::*;
use crate::engine::*;
use crate::import::*;
use crate::state::*;
use crate::store::{self, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppError {
    pub outcome: String,
    pub code: String,
    pub message: String,
    pub location: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<CompactionResult>,
}

impl AppError {
    fn from_engine(run_id: &str, e: EngineError) -> Self {
        match e {
            EngineError::BudgetExhausted { cursor_key, processed, message } => {
                let partial = parse_partial(&message);
                AppError { outcome: "budget_exhausted".into(), code: "budget_exhausted".into(),
                    message: message.split("|PARTIAL=").next().unwrap_or(&message).to_string(),
                    location: format!("run[{run_id}]"), candidates: vec![],
                    partial: partial.or_else(|| Some(partial_shell(run_id, cursor_key, processed))) }
            }
            EngineError::Corrupt(d) => AppError { outcome: "corrupt".into(), code: d.code,
                message: d.message, location: d.location, candidates: d.candidates, partial: None },
            EngineError::Unresolvable(d) => AppError { outcome: "unresolvable".into(), code: d.code,
                message: d.message, location: d.location, candidates: d.candidates, partial: None },
        }
    }
}

fn parse_partial(msg: &str) -> Option<CompactionResult> {
    let (_, js) = msg.split_once("|PARTIAL=")?;
    serde_json::from_str(js).ok()
}
fn partial_shell(run_id: &str, cursor: Option<String>, processed: usize) -> CompactionResult {
    CompactionResult {
        plan: CompactionPlan {
            run_id: run_id.into(), params_version: "unknown".into(),
            selection: SelectionReason { policy: "unknown".into(), level: -1, target_level: -1,
                trigger: String::new(), seed_file_id: String::new(), selected_file_ids: vec![],
                skipped_file_ids: vec![], tie_break: String::new() },
            input_file_ids: vec![], forced_cuts: vec![], input_digest: String::new(),
        },
        status: RunStatus::Interrupted, boundaries: vec![], point_dispositions: vec![],
        range_dispositions: vec![], events: vec![], oldest_active_snapshot_seq: None,
        cursor_key: cursor, processed_key_groups: processed, diagnostics: vec![],
    }
}

pub fn files_map(st: &LsmState) -> BTreeMap<String, SSTable> {
    st.files.clone()
}

/// 初始导入：材料事件 -> flush -> SSTable/snapshot 落库。
pub fn import_initial(store: &mut Store, materials: Vec<RawMaterial>) -> Result<Value, Vec<Diagnostic>> {
    let batch = decode_materials(&materials)?;
    let mut st = store.load_state().map_err(|e| vec![fatal("store", &e)])?;
    let wal_by_id: BTreeMap<String, WalRecord> = batch.wal.iter().map(|w| (w.id.clone(), w.clone())).collect();

    let mut produced: Vec<SSTable> = Vec::new();
    let mut material_ids_used = Vec::new();
    for fo in &batch.flushes {
        let ids: Vec<String> = std::iter::once(fo.id.clone()).chain(fo.wal_ids.iter().cloned()).collect();
        match materialize_flush(fo, &wal_by_id, &ids) {
            Ok(t) => produced.push(t),
            Err(d) => return Err(vec![d]),
        }
        material_ids_used.extend(ids);
    }

    // 写事件。
    let mut seq = store.next_seq().map_err(|e| vec![fatal("store", &e)])?;
    let now = now_ts();
    let mut parents: Vec<String> = vec![];
    for mat in &materials {
        let ev = event(seq, &format!("evt-{seq:06}"), "material_imported",
            serde_json::to_value(mat).unwrap_or(json!({})), parents.clone(), &now);
        store.append_raw(seq, &ev).map_err(|e| vec![fatal("store", &e)])?;
        parents = vec![ev.event_id.clone()];
        seq += 1;
    }
    for t in &produced {
        let ev = event(seq, &format!("evt-{seq:06}"), "file_flushed",
            serde_json::to_value(t).unwrap(), parents.clone(), &now);
        store.append_raw(seq, &ev).map_err(|e| vec![fatal("store", &e)])?;
        parents = vec![ev.event_id.clone()];
        seq += 1;
        st.files.insert(t.id.clone(), t.clone());
    }
    for t in &batch.tables {
        let ev = event(seq, &format!("evt-{seq:06}"), "file_ingested",
            serde_json::to_value(t).unwrap(), parents.clone(), &now);
        store.append_raw(seq, &ev).map_err(|e| vec![fatal("store", &e)])?;
        parents = vec![ev.event_id.clone()];
        seq += 1;
        st.files.insert(t.id.clone(), t.clone());
    }
    for s in &batch.snapshots {
        let ev = event(seq, &format!("evt-{seq:06}"), "snapshot_registered",
            serde_json::to_value(s).unwrap(), parents.clone(), &now);
        store.append_raw(seq, &ev).map_err(|e| vec![fatal("store", &e)])?;
        parents = vec![ev.event_id.clone()];
        seq += 1;
    }
    Ok(json!({"imported_materials":materials.len(),"flushed":produced.len(),
        "ingested":batch.tables.len(),"snapshots":batch.snapshots.len()}))
}

fn fatal(loc: &str, e: &str) -> Diagnostic {
    Diagnostic { code: "store_error".into(), severity: DiagSeverity::Error,
        location: loc.into(), message: e.into(), candidates: vec![] }
}

/// 只改动一个直接来源（材料修订）。supersedes 指向旧材料；局部依赖被标记，无关层不动。
pub fn amend_material(store: &mut Store, mat: RawMaterial) -> Result<Value, AppError> {
    if mat.supersedes.is_none() {
        return Err(AppError { outcome: "unresolvable".into(), code: "amend_requires_supersedes".into(),
            message: "材料修订必须通过 supersedes 指向一个直接来源".into(),
            location: format!("material[{}]", mat.id), candidates: vec![], partial: None });
    }
    let st = store.load_state().map_err(|e| AppError { outcome: "corrupt".into(), code: "store".into(),
        message: e, location: "store".into(), candidates: vec![], partial: None })?;
    let old = mat.supersedes.clone().unwrap();
    if !st.materials.contains_key(&old) {
        return Err(AppError { outcome: "corrupt".into(), code: "unknown_source".into(),
            message: format!("被修订来源 {old} 不存在"), location: format!("material[{}].supersedes", mat.id),
            candidates: vec![], partial: None });
    }
    // 局部影响：找到把该材料列入 provenance 的文件。
    let affected_files: Vec<String> = st.files.values()
        .filter(|f| f.provenance.material_ids.contains(&old))
        .map(|f| f.id.clone()).collect();
    let ev = store::append_event(store, "material_amended", serde_json::to_value(&mat).unwrap(),
        vec![format!("material:{old}")]).map_err(|e| AppError { outcome: "corrupt".into(),
        code: "store".into(), message: e, location: "store".into(), candidates: vec![], partial: None })?;
    Ok(json!({"event_id":ev.event_id,"supersedes":old,"affected_files":affected_files}))
}

pub fn revise_params(store: &mut Store, params: EngineParams) -> Result<Value, AppError> {
    let st = store.load_state().map_err(store_err)?;
    if st.params.contains_key(&params.version) {
        return Err(AppError { outcome: "unresolvable".into(), code: "duplicate_params_version".into(),
            message: format!("参数版本 {} 已存在", params.version), location: "params.version".into(),
            candidates: vec![], partial: None });
    }
    let parents: Vec<String> = st.events.last().map(|e| e.event_id.clone()).into_iter().collect();
    let ev = store::append_event(store, "params_revised", serde_json::to_value(&params).unwrap(), parents)
        .map_err(store_err)?;
    Ok(json!({"event_id":ev.event_id,"params_version":params.version}))
}

fn store_err(e: String) -> AppError {
    AppError { outcome: "corrupt".into(), code: "store".into(), message: e,
        location: "store".into(), candidates: vec![], partial: None }
}

/// 选择并生成压实计划（不落库执行）。
pub fn plan_compaction(st: &LsmState, params: &EngineParams, run_id: &str, forced_cuts: Vec<Endpoint>)
    -> Result<CompactionPlan, AppError> {
    let selection = select_compaction(run_id, &st.live_files(), params).map_err(|e| AppError::from_engine(run_id, e))?;
    if selection.policy == "none" {
        return Err(AppError { outcome: "unresolvable".into(), code: "no_compaction_needed".into(),
            message: "当前状态没有满足条件的压实".into(), location: "selector".into(),
            candidates: vec![], partial: None });
    }
    Ok(CompactionPlan {
        run_id: run_id.into(), params_version: params.version.clone(), selection: selection.clone(),
        input_file_ids: selection.selected_file_ids, forced_cuts,
        input_digest: input_digest(&selection.selected_file_ids, &st.files),
    })
}

fn input_digest(ids: &[String], files: &BTreeMap<String, SSTable>) -> String {
    let mut v = json!({"files": ids.iter().filter_map(|id| files.get(id).map(|f| json!({
        "id":f.id,"level":f.level,"range":f.key_range,
        "points":f.points.iter().map(|p| json!({"id":p.id,"seq":p.seq,"kind":p.kind,"key":p.key.raw})).collect::<Vec<_>>(),
        "ranges":f.ranges.iter().map(|r| json!({"id":r.id,"seq":r.seq,"range":r.range})).collect::<Vec<_>>(),
    }))).collect::<Vec<_>>()});
    digest_of(&v)
}

/// 启动压实：返回 staged result 或三类失败之一。budget_override=Some(k) 用于制造中断。
pub fn start_compaction(store: &mut Store, run_id: &str, forced_cuts: Vec<Endpoint>, budget_override: Option<usize>)
    -> Result<CompactionResult, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let params = st.current_params();
    let selection = select_compaction(run_id, &st.live_files(), &params).map_err(|e| AppError::from_engine(run_id, e))?;
    if selection.policy == "none" {
        return Err(AppError { outcome: "unresolvable".into(), code: "no_compaction_needed".into(),
            message: "当前状态没有满足条件的压实".into(), location: "selector".into(),
            candidates: vec![], partial: None });
    }
    let plan = CompactionPlan {
        run_id: run_id.into(), params_version: params.version.clone(),
        input_digest: input_digest(&selection.selected_file_ids, &st.files),
        input_file_ids: selection.selected_file_ids.clone(), forced_cuts, selection,
    };
    execute_and_stage(store, &st, &plan, &params, budget_override)
}

#[allow(clippy::too_many_arguments)]
fn execute_and_stage(store: &mut Store, st0: &LsmState, plan: &CompactionPlan, params: &EngineParams, budget_override: Option<usize>)
    -> Result<CompactionResult, AppError> {
    let input = LiveStateInput { files: &st0.live_files(), snapshots: &st0.snapshots.values().cloned().collect::<Vec<_>>() };
    match run_compaction(plan, input, &st0.files, params, &plan.forced_cuts, budget_override) {
        Ok(mut result) => {
            // 输出文件 staging 事件 + run_staged 事件（一个事务）。
            let outs = build_output_sstables(&result, &st0.files);
            let now = now_ts();
            let mut seq = store.next_seq().map_err(store_err)?;
            let mut tx = store.conn.transaction().map_err(store_err)?;
            let mut parents: Vec<String> = vec![];
            for f in &outs {
                let ev = event(seq, &format!("evt-{seq:06}"), "file_staged", serde_json::to_value(f).unwrap(), parents.clone(), &now);
                crate::store::insert_event_public(&tx, &store.project, &ev).map_err(store_err)?;
                parents = vec![ev.event_id.clone()];
                seq += 1;
            }
            for e in &result.events {
                let ev = event(seq, &format!("evt-{seq:06}"),
                    e.get("type").and_then(|v| v.as_str()).unwrap_or("compaction_evidence"),
                    e.clone(), parents.clone(), &now);
                crate::store::insert_event_public(&tx, &store.project, &ev).map_err(store_err)?;
                parents = vec![ev.event_id.clone()];
                seq += 1;
            }
            result.status = RunStatus::Staged;
            let rec = RunRecord { run_id: plan.run_id.clone(), params_version: plan.params_version.clone(),
                status: RunStatus::Staged, result: Some(result.clone()),
                input_digest: plan.input_digest.clone(), created_at: now.clone() };
            let run_ev = event(seq, &format!("evt-{seq:06}"), "run_staged", serde_json::to_value(&rec).unwrap(), parents.clone(), &now);
            crate::store::insert_event_public(&tx, &store.project, &run_ev).map_err(store_err)?;
            tx.execute("UPDATE projects SET current_event_seq=?2 WHERE id=?1",
                rusqlite::params![store.project, seq]).map_err(store_err)?;
            tx.commit().map_err(store_err)?;
            Ok(result)
        }
        Err(EngineError::BudgetExhausted { cursor_key, processed, message }) => {
            let partial = parse_partial(&message).unwrap_or_else(|| partial_shell(&plan.run_id, cursor_key, processed));
            persist_interrupted(store, plan, partial.clone())?;
            Err(AppError::from_engine(&plan.run_id, EngineError::BudgetExhausted { cursor_key: None, processed: 0, message }))
        }
        Err(other) => {
            // Corrupt / Unresolvable：写 run_failed 事件留证。
            let ae = AppError::from_engine(&plan.run_id, other);
            let now = now_ts();
            let rec = RunRecord { run_id: plan.run_id.clone(), params_version: plan.params_version.clone(),
                status: if ae.outcome == "corrupt" { RunStatus::FailedCorrupt } else { RunStatus::FailedUnresolvable },
                result: None, input_digest: plan.input_digest.clone(), created_at: now };
            let _ = store::append_event(store, "run_failed", serde_json::json!({
                "record":rec,"outcome":ae.outcome,"code":ae.code,"message":ae.message,
                "location":ae.location,"candidates":ae.candidates}), vec![]);
            Err(ae)
        }
    }
}

fn persist_interrupted(store: &mut Store, plan: &CompactionPlan, mut partial: CompactionResult) -> Result<(), AppError> {
    partial.status = RunStatus::Interrupted;
    let now = now_ts();
    let rec = RunRecord { run_id: plan.run_id.clone(), params_version: plan.params_version.clone(),
        status: RunStatus::Interrupted, result: Some(partial),
        input_digest: plan.input_digest.clone(), created_at: now };
    store::append_event(store, "run_interrupted", serde_json::to_value(&rec).unwrap(), vec![]).map_err(store_err)?;
    Ok(())
}

/// 安装（两阶段、幂等）。
pub fn install_compaction(store: &mut Store, run_id: &str) -> Result<Value, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let rec = st.runs.get(run_id).cloned().ok_or_else(|| AppError {
        outcome: "unresolvable".into(), code: "unknown_run".into(),
        message: format!("压实 run {run_id} 不存在"), location: format!("run[{run_id}]"),
        candidates: vec![], partial: None })?;
    if st.installed_runs.contains(run_id) {
        return Ok(json!({"run_id":run_id,"installed":false,"reason":"already_installed_idempotent"}));
    }
    let result = rec.result.clone().ok_or_else(|| AppError {
        outcome: "corrupt".into(), code: "run_without_result".into(),
        message: "该 run 没有可安装的输出（失败的压实）".into(), location: format!("run[{run_id}]"),
        candidates: vec![], partial: None })?;
    match store.install_run(&rec, &result) {
        Ok(evs) => Ok(json!({"run_id":run_id,"installed":true,"events":evs.iter().map(|e|e.event_id.clone()).collect::<Vec<_>>(),
            "outputs":result.boundaries.iter().map(|b|&b.file_id).collect::<Vec<_>>(),
            "obsoleted":result.plan.input_file_ids})),
        Err("SIMULATED_CRASH_BEFORE_COMMIT") =>
            Err(AppError { outcome: "budget_exhausted".into(), code: "simulated_half_write".into(),
                message: "安装提交前模拟崩溃；重启后打开库将完成恢复".into(),
                location: format!("run[{run_id}].install"), candidates: vec![], partial: Some(result) }),
        Err(e) => Err(store_err(e)),
    }
}

/// 从中断处续跑：用更大预算重算同一 run（输入摘要必须一致，保证可重放）。
pub fn resume_compaction(store: &mut Store, run_id: &str, budget: usize) -> Result<CompactionResult, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let rec = st.runs.get(run_id).cloned().ok_or_else(|| AppError {
        outcome: "unresolvable".into(), code: "unknown_run".into(),
        message: format!("run {run_id} 不存在"), location: format!("run[{run_id}]"),
        candidates: vec![], partial: None })?;
    let partial = rec.result.clone().ok_or_else(|| AppError {
        outcome: "corrupt".into(), code: "run_without_result".into(),
        message: "中断 run 缺少 partial".into(), location: format!("run[{run_id}]"),
        candidates: vec![], partial: None })?;
    if rec.status != RunStatus::Interrupted {
        return Err(AppError { outcome: "unresolvable".into(), code: "not_interrupted".into(),
            message: "只有 interrupted 的 run 可以续跑".into(), location: format!("run[{run_id}]"),
            candidates: vec![], partial: None });
    }
    let params = st.params.get(&rec.params_version).cloned().unwrap_or_else(|| st.current_params());
    let plan = partial.plan.clone();
    let now_digest = input_digest(&plan.input_file_ids, &st.files);
    if now_digest != plan.input_digest {
        return Err(AppError { outcome: "corrupt".into(), code: "input_changed_after_interrupt".into(),
            message: "中断后压实输入已变化，输入摘要不一致，拒绝覆写历史".into(),
            location: format!("run[{run_id}].input_digest"),
            candidates: vec![json!({"recorded":plan.input_digest,"current":now_digest})], partial: None });
    }
    execute_and_stage(store, &st, &plan, &params, Some(budget))
}

/// 人工决定：accept / reject / param-adjust / forced-cut。冲突决定全部留档。
pub fn record_decision(store: &mut Store, run_id: Option<String>, action: &str, rationale: &str,
    conflicts_with: Vec<String>) -> Result<Value, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let n = st.decisions.len() + 1;
    let params_version = st.current_params().version.clone();
    let d = DecisionRecord {
        decision_id: format!("dec-{n:03}"), run_id: run_id.clone(), action: action.into(),
        rationale: rationale.into(), params_version: Some(params_version),
        created_at: now_ts(), conflicts_with: conflicts_with.clone(),
    };
    let parents: Vec<String> = run_id.map(|r| format!("run:{r}")).into_iter().collect();
    let ev = store::append_event(store, "decision_recorded", serde_json::to_value(&d).unwrap(), parents).map_err(store_err)?;
    Ok(json!({"event_id":ev.event_id,"decision_id":d.decision_id,"conflicts":conflicts_with}))
}

pub fn release_snapshot(store: &mut Store, snapshot_id: &str) -> Result<Value, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let snap = st.snapshots.get(snapshot_id).cloned().ok_or_else(|| AppError {
        outcome: "unresolvable".into(), code: "unknown_snapshot".into(),
        message: format!("snapshot {snapshot_id} 不存在"), location: format!("snapshot[{snapshot_id}]"),
        candidates: vec![], partial: None })?;
    if !snap.active {
        return Err(AppError { outcome: "unresolvable".into(), code: "snapshot_inactive".into(),
            message: "snapshot 已释放".into(), location: format!("snapshot[{snapshot_id}]"),
            candidates: vec![], partial: None });
    }
    let mut released = snap.clone();
    released.active = false;
    let ev = store::append_event(store, "snapshot_released", serde_json::to_value(&released).unwrap(),
        vec![format!("snapshot:{}", snap.id)]).map_err(store_err)?;
    Ok(json!({"event_id":ev.event_id,"snapshot_id":snapshot_id,"active":false}))
}

/// point 读取（返回版本链 + tombstone 覆盖 + 判定），并把可见性判断固化为事件证据。
pub fn read_point(store: &mut Store, key: Endpoint, snapshot_seq: u64, persist: bool) -> Result<ReadOutcome, AppError> {
    let st = store.load_state().map_err(store_err)?;
    let outcome = point_read(&st.live_files(), &key, snapshot_seq).map_err(|e| AppError::from_engine("read", e))?;
    if persist {
        let _ = store::append_event(store, "visibility_checked", serde_json::to_value(&outcome).unwrap(),
            outcome.chain.iter().map(|l| format!("point:{}", l.point_id)).collect()).map_err(store_err)?;
    }
    Ok(outcome)
}

pub fn read_range(store: &mut Store, range: KeyRange, snapshot_seq: u64) -> Result<Vec<RangeCoverage>, AppError> {
    let st = store.load_state().map_err(store_err)?;
    range_scan(&st.live_files(), &range, snapshot_seq).map_err(|e| AppError::from_engine("range", e))
}

/// 审阅视图：层级文件 + run + 参数 + 决定 + 诊断，每个显示值带稳定身份。
pub fn review_view(st: &LsmState) -> Value {
    json!({
        "levels": {
            let mut lv: BTreeMap<String, Value> = BTreeMap::new();
            for level in 0..=6 {
                let fs: Vec<&SSTable> = st.files.values()
                    .filter(|f| f.state == FileState::Live && f.level == level).collect();
                if fs.is_empty() { continue; }
                lv.insert(format!("L{level}"), json!(fs.iter().map(|f| file_view(f)).collect::<Vec<_>>()));
            }
            lv
        },
        "obsolete_files": st.files.values().filter(|f| f.state == FileState::Obsolete)
            .map(file_view).collect::<Vec<_>>(),
        "staging_files": st.files.values().filter(|f| f.state == FileState::Staging)
            .map(file_view).collect::<Vec<_>>(),
        "snapshots": st.snapshots.values().cloned().collect::<Vec<_>>(),
        "params_versions": st.params.values().cloned().collect::<Vec<_>>(),
        "runs": st.runs.values().cloned().collect::<Vec<_>>(),
        "decisions": st.decisions.clone(),
        "events": st.events.clone(),
        "oldest_active_snapshot_seq": oldest_active_snapshot(&st.snapshots.values().cloned().collect::<Vec<_>>()),
    })
}

fn file_view(f: &SSTable) -> Value {
    json!({
        "id": f.id, "level": f.level, "state": f.state, "origin": f.origin,
        "key_range": f.key_range, "interval_basis": f.key_range.interval_basis,
        "size": f.size, "point_count": f.points.len(), "range_count": f.ranges.len(),
        "points": f.points, "ranges": f.ranges, "provenance": f.provenance,
    })
}
