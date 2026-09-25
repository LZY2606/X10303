//! 压实与墓碑语义引擎（纯函数、确定性、可直接单测）。
//!
//! 三类互不等价的失败：
//! - `BudgetExhausted`：资源预算耗尽 —— 产出 partial + 游标，可从中断处续跑；
//! - `Corrupt`：格式/摘要损坏（区间非法、端点无法解码、文件身份冲突）；
//! - `Unresolvable`：语义无解（强制切分使不重叠层约束无法满足等），列出全部合法候选。
//!
//! 读取语义（与 RocksDB 同构的简化坐标）：
//! 内部键序 (key↑, seq↓, Delete 先于 Set)；snapshot@S 只可见 seq<=S；
//! 一个 key 的 point 结果按内部键序取第一条可见记录；range tomb@R 覆盖
//! range∩[start,end) 内、seq<=R 且无更新 point 的键。
//! GC 双地平线：point 版本只在对“现在”和“最旧活跃 snapshot”两读者都不再需要时才可丢弃。

use crate::domain::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineParams {
    pub version: String,
    #[serde(default = "default_l0_trigger")]
    pub l0_trigger_files: usize,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: i64,
    #[serde(default = "default_level_size_bytes")]
    pub level_size_bytes: i64,
    #[serde(default = "default_budget_keys")]
    pub budget_key_groups: usize,
}
fn default_l0_trigger() -> usize { 4 }
fn default_max_output_bytes() -> i64 { 64 }
fn default_level_size_bytes() -> i64 { 96 }
fn default_budget_keys() -> usize { usize::MAX }

impl EngineParams {
    pub fn v1() -> Self {
        EngineParams {
            version: "params-v1".into(),
            l0_trigger_files: 4,
            max_output_bytes: 64,
            level_size_bytes: 96,
            budget_key_groups: usize::MAX,
        }
    }
}

#[derive(Debug, Clone)]
pub enum EngineError {
    BudgetExhausted { cursor_key: Option<String>, processed: usize, message: String },
    Corrupt(Diagnostic),
    Unresolvable(Diagnostic),
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            EngineError::BudgetExhausted { .. } => "budget_exhausted",
            EngineError::Corrupt(_) => "corrupt",
            EngineError::Unresolvable(_) => "unresolvable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Staged,
    Installed,
    Interrupted,
    FailedCorrupt,
    FailedUnresolvable,
}

/// 压实选择理由（机器可读事件载荷）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelectionReason {
    pub policy: String,
    pub level: i64,
    pub target_level: i64,
    pub trigger: String,
    pub seed_file_id: String,
    #[serde(default)]
    pub selected_file_ids: Vec<String>,
    #[serde(default)]
    pub skipped_file_ids: Vec<String>,
    #[serde(default)]
    pub tie_break: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PointDisposition {
    pub point_id: String,
    pub key_raw: String,
    pub seq: u64,
    pub kind: PointKind,
    pub action: String,
    pub reason_code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub horizons: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub covering_range_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeDisposition {
    pub range_id: String,
    pub seq: u64,
    pub action: String,
    pub reason_code: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outside_overlap_file_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fragments: Vec<RangeFragment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeFragment {
    pub fragment_id: String,
    pub range: KeyRange,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputBoundary {
    pub file_id: String,
    pub range: KeyRange,
    pub cut_reason: String,
    pub point_ids: Vec<String>,
    pub range_fragment_ids: Vec<String>,
    pub size: Measured,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionPlan {
    pub run_id: String,
    pub params_version: String,
    pub selection: SelectionReason,
    pub input_file_ids: Vec<String>,
    #[serde(default)]
    pub forced_cuts: Vec<Endpoint>,
    pub input_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionResult {
    pub plan: CompactionPlan,
    pub status: RunStatus,
    pub boundaries: Vec<OutputBoundary>,
    #[serde(default)]
    pub point_dispositions: Vec<PointDisposition>,
    #[serde(default)]
    pub range_dispositions: Vec<RangeDisposition>,
    #[serde(default)]
    pub events: Vec<Value>,
    pub oldest_active_snapshot_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_key: Option<String>,
    pub processed_key_groups: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
}

pub struct DecodedView<'a> {
    pub files: Vec<&'a SSTable>,
}

/// 解出全部 live 文件的区间；任一损坏即 Corrupt。
pub fn decode_live_ranges(files: &[SSTable]) -> Result<Vec<(String, i64, DecodedRange)>, EngineError> {
    let mut out = Vec::new();
    for f in files {
        if f.state != FileState::Live {
            continue;
        }
        let r = f.key_range.decoded().map_err(EngineError::Corrupt)?;
        if !r.is_valid_half_open() {
            return Err(EngineError::Corrupt(Diagnostic {
                code: "invalid_half_open_range".into(),
                severity: DiagSeverity::Error,
                location: format!("sstable[{}].key_range", f.id),
                message: format!("半开区间非法：start 必须严格小于 end（文件 {}）", f.id),
                candidates: vec![],
            }));
        }
        out.push((f.id.clone(), f.level, r));
    }
    Ok(out)
}

fn overlaps_any(r: &DecodedRange, ranges: &[(String, i64, DecodedRange)], levels: Option<&BTreeSet<i64>>) -> Vec<String> {
    ranges
        .iter()
        .filter(|(_, lvl, rr)| levels.map_or(true, |ls| ls.contains(lvl)) && rr.intersects(r))
        .map(|(id, _, _)| id.clone())
        .collect()
}

/// 确定性压实选择：先看 L0（重叠连通簇达到 trigger），再看 L>=1 的超预算层。
pub fn select_compaction(
    run_id: &str,
    files: &[SSTable],
    params: &EngineParams,
) -> Result<SelectionReason, EngineError> {
    let decoded = decode_live_ranges(files)?;
    let l0: Vec<&(String, i64, DecodedRange)> = decoded.iter().filter(|(_, l, _)| *l == 0).collect();

    // L0 重叠图：每个文件取与之重叠的最小 id 文件作为种子，取达到阈值的最大簇（tie: 种子键最小）。
    if l0.len() >= params.l0_trigger_files {
        let mut clusters: Vec<(String, BTreeSet<String>)> = Vec::new();
        for (id, _, r) in &l0 {
            let seed = l0
                .iter()
                .filter(|(oid, _, or)| or.intersects(r))
                .map(|(oid, _, _)| oid.clone())
                .min()
                .unwrap_or_else(|| id.clone());
            if let Some(slot) = clusters.iter_mut().find(|(s, _)| *s == seed) {
                slot.1.insert(id.clone());
            } else {
                let mut set = BTreeSet::new();
                set.insert(id.clone());
                clusters.push((seed, set));
            }
        }
        if let Some((_, members)) = clusters.into_iter().filter(|(_, s)| s.len() >= params.l0_trigger_files).max_by(
            |(a, sa), (b, sb)| sa.len().cmp(&sb.len()).then_with(|| b.cmp(a)),
        ) {
            let seed_pick = members.iter().next().cloned().unwrap_or_default();
            return Ok(SelectionReason {
                policy: "l0_overlap_fanin".into(),
                level: 0,
                target_level: 1,
                trigger: format!("l0_live_files={} >= trigger={}", l0.len(), params.l0_trigger_files),
                seed_file_id: seed_pick,
                selected_file_ids: members.into_iter().collect(),
                skipped_file_ids: l0.iter().map(|(id, _, _)| id.clone()).collect(),
                tie_break: "largest_cluster;then_min_file_id".into(),
            });
        }
    }

    // L>=1：找 size 总和超预算的最低层，种子取起点最小文件；与种子区间相交的同层文件 + 下一层重叠文件一起参与。
    for level in 1.. {
        let same: Vec<&(String, i64, DecodedRange)> =
            decoded.iter().filter(|(_, l, _)| *l == level).collect();
        if same.is_empty() {
            if level > 6 { break; }
            continue;
        }
        let total: i64 = files
            .iter()
            .filter(|f| f.state == FileState::Live && f.level == level)
            .map(|f| f.size.magnitude.unwrap_or(0))
            .sum();
        if total > params.level_size_bytes {
            let (seed_id, _, seed_range) = same
                .iter()
                .min_by(|(a, _, ar), (b, _, br)| ar.start.cmp(&br.start).then_with(|| a.cmp(b)))
                .map(|(id, l, r)| (id.clone(), *l, (*r).clone()))
                .unwrap();
            let mut selected: BTreeSet<String> = BTreeSet::new();
            for (id, lvl, r) in &decoded {
                if *lvl == level && r.intersects(&seed_range) {
                    selected.insert(id.clone());
                }
                if *lvl == level + 1 && r.intersects(&seed_range) {
                    selected.insert(id.clone());
                }
            }
            let skipped: Vec<String> = decoded
                .iter()
                .filter(|(_, l, _)| (*l == level || *l == level + 1))
                .map(|(id, _, _)| id.clone())
                .filter(|id| !selected.contains(id))
                .collect();
            return Ok(SelectionReason {
                policy: "level_size_tier".into(),
                level,
                target_level: level + 1,
                trigger: format!("level{}_bytes={} > budget={}", level, total, params.level_size_bytes),
                seed_file_id: seed_id,
                selected_file_ids: selected.into_iter().collect(),
                skipped_file_ids: skipped,
                tie_break: "lowest_oversized_level;min_start_key;then_min_file_id".into(),
            });
        }
        if level > 6 { break; }
    }

    // 无可压实：以最小 live 文件构造空选择（调用方可据此不生成 run）。
    let fallback = decoded
        .iter()
        .min_by(|(a, _, ar), (b, _, br)| ar.start.cmp(&br.start).then_with(|| a.cmp(b)))
        .map(|(id, _, _)| id.clone())
        .unwrap_or_default();
    Ok(SelectionReason {
        policy: "none".into(),
        level: -1,
        target_level: -1,
        trigger: "no_compaction_needed".into(),
        seed_file_id: fallback,
        selected_file_ids: vec![],
        skipped_file_ids: decoded.iter().map(|(id, _, _)| id.clone()).collect(),
        tie_break: "deterministic_scan".into(),
    })
}

#[derive(Debug, Clone)]
pub struct LiveStateInput<'a> {
    pub files: &'a [SSTable],
    pub snapshots: &'a [Snapshot],
}

pub fn oldest_active_snapshot(snaps: &[Snapshot]) -> Option<u64> {
    snaps.iter().filter(|s| s.active).map(|s| s.seq).min()
}

/// 一次压实执行。`forced_cuts` 是人工决定；预算可被调小以制造“压实中断”。
pub fn run_compaction(
    plan: &CompactionPlan,
    state: LiveStateInput,
    files_by_id: &BTreeMap<String, SSTable>,
    params: &EngineParams,
    forced_cuts: &[Endpoint],
    budget_override: Option<usize>,
) -> Result<CompactionResult, EngineError> {
    let budget = budget_override.unwrap_or(params.budget_key_groups);
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut events: Vec<Value> = Vec::new();

    let mut input_files: Vec<&SSTable> = plan
        .input_file_ids
        .iter()
        .map(|id| {
            files_by_id
                .get(id)
                .ok_or_else(|| {
                    EngineError::Corrupt(Diagnostic {
                        code: "missing_input_sstable".into(),
                        severity: DiagSeverity::Error,
                        location: format!("compaction[{}].input[{}]", plan.run_id, id),
                        message: format!("压实输入引用了不存在的 SSTable: {}", id),
                        candidates: vec![],
                    })
                })
        })
        .collect::<Result<_, _>>()?;
    let target_level = plan.selection.target_level;
    let oldest = oldest_active_snapshot(state.snapshots);

    // range tombstone fanout 扩张：目标层上任何被输入 range 覆盖的文件必须一起参与，
    // 否则 range 下沉后会丢失对未移动数据的覆盖（循环到不动点）。
    let mut expanded: Vec<String> = Vec::new();
    let mut guard = 0usize;
    loop {
        guard += 1;
        if guard > 32 {
            return Err(EngineError::Unresolvable(Diagnostic {
                code: "fanout_expansion_diverges".into(), severity: DiagSeverity::Error,
                location: format!("compaction[{}].fanout", plan.run_id),
                message: "range tombstone 牵连扩张不收敛".into(), candidates: vec![],
            }));
        }
        let cur_ids: BTreeSet<String> = input_files.iter().map(|f| f.id.clone()).collect();
        let mut cur_ranges = Vec::new();
        for f in &input_files {
            for r in &f.ranges { cur_ranges.push(r.range.decoded()?); }
        }
        let mut add: Vec<String> = Vec::new();
        for f in state.files {
            if f.state != FileState::Live || cur_ids.contains(&f.id) || f.level != target_level { continue; }
            let fr = f.key_range.decoded()?;
            if cur_ranges.iter().any(|rr| rr.intersects(&fr)) { add.push(f.id.clone()); }
        }
        if add.is_empty() { break; }
        for id in &add {
            let f = files_by_id.get(id).ok_or_else(|| EngineError::Corrupt(Diagnostic {
                code: "missing_expanded_sstable".into(), severity: DiagSeverity::Error,
                location: format!("compaction[{}].expanded[{}]", plan.run_id, id),
                message: format!("扩张输入文件不存在: {id}"), candidates: vec![],
            }))?;
            input_files.push(f);
            expanded.push(id.clone());
        }
    }
    if !expanded.is_empty() {
        events.push(json!({"type":"compaction_input_expanded","run_id":plan.run_id,
            "target_level":target_level,"added_file_ids":expanded,
            "reason":"range_tombstone_must_cover_unmoved_target_level_data"}));
    }

    // ---- 收集输入内 point/range，按 (id, seq, kind) 检测同序号多类型 ----
    let mut all_points: Vec<PointRecord> = Vec::new();
    let mut all_ranges: Vec<RangeTomb> = Vec::new();
    let mut seen_internal: BTreeMap<String, PointRecord> = BTreeMap::new();
    for f in &input_files {
        for p in &f.points {
            let key = p.key.bytes().map_err(EngineError::Corrupt)?;
            let k = format!("{}:{}:{}", String::from_utf8_lossy(&key), p.seq, kind_rank(p.kind));
            all_points.push(p.clone());
            let conflict_key = format!("{}:{}", String::from_utf8_lossy(&key), p.seq);
            if let Some(prev) = seen_internal.get(&conflict_key) {
                if prev.kind != p.kind {
                    let d = Diagnostic {
                        code: "same_seq_multiple_types".into(),
                        severity: DiagSeverity::Warning,
                        location: format!("compaction[{}].key[{}].seq[{}]", plan.run_id, p.key.raw, p.seq),
                        message: format!(
                            "同一用户键 {} 同序号 {} 出现多种类型（{} 与 {}）；按内部键序 Delete 优先，并保留两条材料供审阅",
                            p.key.raw, p.seq, prev.kind_id(), p.kind_id()
                        ),
                        candidates: vec![],
                    };
                    diagnostics.push(d.clone());
                    events.push(json!({"type":"same_seq_multiple_types","run_id":plan.run_id,
                        "key":p.key.raw,"seq":p.seq,"point_ids":[prev.id.clone(),p.id.clone()],
                        "resolution":"internal_key_order_delete_first","diagnostic":d}));
                }
            } else {
                seen_internal.insert(conflict_key, p.clone());
            }
        }
        for r in &f.ranges {
            r.range.decoded().map_err(EngineError::Corrupt)?;
            all_ranges.push(r.clone());
        }
    }

    // 内部键排序：key↑ seq↓ Delete-first。
    all_points.sort_by(|a, b| {
        let ak = a.key.bytes().unwrap_or_default();
        let bk = b.key.bytes().unwrap_or_default();
        internal_cmp(&ak, a.seq, a.kind, b, &bk)
    });

    // 输入文件外的 live 文件（未参与压实的旧数据不能被越过）。
    let input_ids: BTreeSet<&str> = plan.input_file_ids.iter().map(String::as_str).collect();
    let outside: Vec<&(String, i64, DecodedRange)> = decode_live_ranges(state.files)?
        .iter()
        .filter(|(id, _, _)| !input_ids.contains(id.as_str()))
        .collect();

    // range 解码缓存。
    let mut range_decoded: BTreeMap<String, DecodedRange> = BTreeMap::new();
    for r in &all_ranges {
        range_decoded.insert(r.id.clone(), r.range.decoded()?);
    }

    // ---- 按用户键分组执行双地平线 GC ----
    let mut retained_points: Vec<PointRecord> = Vec::new();
    let mut point_disp: Vec<PointDisposition> = Vec::new();
    let mut idx = 0usize;
    let mut processed = 0usize;
    let mut cursor_key: Option<String> = None;
    // 双地平线扩展为“现在 + 每一个活跃 snapshot”：只看最旧 snapshot 会漏掉中间读者。
    let mut horizons: Vec<u64> = state.snapshots.iter().filter(|s| s.active).map(|s| s.seq).collect();
    horizons.push(u64::MAX);
    horizons.sort_unstable();
    horizons.dedup();
    let min_horizon = horizons.iter().copied().min().unwrap_or(u64::MAX);

    while idx < all_points.len() {
        let key_bytes = all_points[idx].key.bytes().map_err(EngineError::Corrupt)?;
        let mut group: Vec<&PointRecord> = Vec::new();
        while idx < all_points.len() && all_points[idx].key.bytes().map_err(EngineError::Corrupt)? == key_bytes {
            group.push(&all_points[idx]);
            idx += 1;
        }
        processed += 1;
        if processed > budget {
            cursor_key = Some(group[0].key.raw.clone());
            let err = EngineError::BudgetExhausted {
                cursor_key: cursor_key.clone(),
                processed: processed - 1,
                message: format!("压实预算耗尽：已处理 {} 个 key 组，游标停在 {:?}", processed - 1, cursor_key),
            };
            // partial 结果随错误返回给调用方（通过 interrupted 结果落库）。
            return Err(with_partial(err, plan, vec![], point_disp, vec![], oldest, cursor_key.clone(), processed - 1, diagnostics, events));
        }

        // 覆盖该 key 的输入内 range（seq 升序），用于影子丢弃。
        let covering: Vec<&RangeTomb> = all_ranges
            .iter()
            .filter(|r| range_decoded.get(&r.id).unwrap().contains(&key_bytes))
            .collect();

        // 输入外文件是否含此 key（决定 point tombstone 是否能下沉丢弃）。
        let outside_has_key = outside.iter().any(|(_, _, r)| r.contains(&key_bytes));
        let mut picked: BTreeSet<String> = BTreeSet::new();

        for &h in &horizons {
            // 按内部键序找第一组判定结果。
            let mut winner: Option<(&PointRecord, Option<&RangeTomb>)> = None;
            for p in &group {
                if p.seq > h {
                    continue;
                }
                let rcover = covering.iter().filter(|r| r.seq >= p.seq && r.seq <= h).copied().max_by_key(|r| r.seq);
                if let Some(rc) = rcover {
                    winner = Some((p, Some(rc)));
                    break;
                }
                winner = Some((*p, None));
                break;
            }
            let Some((rec, range_shadow)) = winner else { continue };
            if picked.contains(&rec.id) {
                // 已被更大地平线保留：追加地平线证据，不重复保留。
                if let Some(d) = point_disp.iter_mut().find(|d| d.point_id == rec.id) {
                    if !d.horizons.contains(&h) { d.horizons.push(h); }
                    if let Some(rc) = range_shadow {
                        if !d.covering_range_ids.contains(&rc.id) { d.covering_range_ids.push(rc.id.clone()); }
                    }
                }
                continue;
            }
            let (action, reason, detail) = if let Some(rc) = range_shadow {
                ("drop".into(), "shadowed_by_range".into(),
                    format!("在地平线 seq={} 下被 range tombstone {}@{} 遮蔽", horizon_label(h), rc.id, rc.seq))
            } else if rec.kind == PointKind::Delete {
                if rec.seq < min_horizon && !outside_has_key {
                    ("drop".into(), "point_tombstone_elided".into(),
                        format!("point tombstone seq={} 旧于最旧活跃 snapshot({}) 且输入外文件不再含该键：下沉并丢弃", rec.seq, min_horizon))
                } else if rec.seq < min_horizon && outside_has_key {
                    ("retain".into(), "tombstone_blocks_outside_data".into(),
                        format!("存在未参与压实且含该键的旧数据，point tombstone seq={} 必须下沉保留", rec.seq))
                } else {
                    ("retain".into(), "tombstone_visible_to_snapshot".into(),
                        format!("最旧活跃 snapshot seq={} 仍可见 tombstone seq={}，保留", min_horizon, rec.seq))
                }
            } else {
                ("retain".into(), "newest_visible_set".into(),
                    format!("地平线 seq={} 下最新可见的 Set，保留", horizon_label(h)))
            };
            picked.insert(rec.id.clone());
            if action == "retain" {
                if let Some(full) = all_points.iter().find(|p| p.id == rec.id) {
                    retained_points.push(full.clone());
                }
            }
            point_disp.push(PointDisposition {
                point_id: rec.id.clone(),
                key_raw: rec.key.raw.clone(),
                seq: rec.seq,
                kind: rec.kind,
                action,
                reason_code: reason,
                detail,
                horizons: vec![h],
                covering_range_ids: range_shadow.map(|r| vec![r.id.clone()]).unwrap_or_default(),
            });
            if let Some(rc) = range_shadow {
                events.push(json!({"type":"point_shadowed_by_range","run_id":plan.run_id,
                    "point_id":rec.id,"key":rec.key.raw,"point_seq":rec.seq,
                    "range_id":rc.id,"range_seq":rc.seq,"horizon":h}));
            }
        }

        // 其余记录：标记被内部键序遮蔽的判定（仅证据，不保留）。
        for p in &group {
            if !point_disp.iter().any(|d| d.point_id == p.id) {
                point_disp.push(PointDisposition {
                    point_id: p.id.clone(),
                    key_raw: p.key.raw.clone(),
                    seq: p.seq,
                    kind: p.kind,
                    action: "drop".into(),
                    reason_code: "shadowed_by_newer_internal_key".into(),
                    detail: format!("内部键序上存在更新记录，seq={} 的版本不再对任何读者可见", p.seq),
                    horizons: vec![],
                    covering_range_ids: vec![],
                });
            }
        }
    }

    // ---- range tombstone 下沉判定：旧于最旧 snapshot 且输入外无任何区间相交才能丢弃 ----
    let mut range_disp: Vec<RangeDisposition> = Vec::new();
    let mut retained_ranges: Vec<RangeTomb> = Vec::new();
    for r in &all_ranges {
        let rr = range_decoded.get(&r.id).unwrap();
        let overlap_files: Vec<String> = outside
            .iter()
            .filter(|(_, _, or)| or.intersects(rr))
            .map(|(id, _, _)| id.clone())
            .collect();
        let older = oldest.map_or(true, |s| r.seq < s);
        if older && overlap_files.is_empty() {
            range_disp.push(RangeDisposition {
                range_id: r.id.clone(), seq: r.seq, action: "drop".into(),
                reason_code: "range_tombstone_elided".into(),
                detail: format!("range tombstone seq={} 旧于最旧活跃 snapshot 且输入外文件无区间相交，下沉丢弃", r.seq),
                outside_overlap_file_ids: vec![], fragments: vec![],
            });
            events.push(json!({"type":"range_tombstone_elided","run_id":plan.run_id,"range_id":r.id,"seq":r.seq}));
        } else {
            retained_ranges.push(r.clone());
            let reason = if !older {
                "range_visible_to_snapshot"
            } else {
                "range_blocks_outside_data"
            };
            range_disp.push(RangeDisposition {
                range_id: r.id.clone(), seq: r.seq, action: "retain".into(),
                reason_code: reason.into(),
                detail: if !older {
                    format!("最旧活跃 snapshot 仍可见 range tombstone seq={}，保留", r.seq)
                } else {
                    format!("输入外文件存在区间相交 {:?}：range tombstone 不能越过未参与压实的旧数据提前丢弃，保留并切分", overlap_files)
                },
                outside_overlap_file_ids: overlap_files, fragments: vec![],
            });
        }
    }

    // ---- 输出切分：不重叠层约束 + 人工强制切点 ----
    let (boundaries, split_events, split_diags, fragment_table) = split_outputs(
        plan, target_level, input_files, &retained_points, &retained_ranges, range_decoded.clone(),
        &outside, forced_cuts, params,
    )?;
    events.extend(split_events);
    diagnostics.extend(split_diags);

    // 回填 range fragments 到 dispositions。
    for (fid, rid, kr) in &fragment_table {
        if let Some(d) = range_disp.iter_mut().find(|d| d.range_id == *rid) {
            d.fragments.push(RangeFragment { fragment_id: fid.clone(), range: kr.clone() });
        }
    }

    Ok(CompactionResult {
        plan: plan.clone(),
        status: RunStatus::Staged,
        boundaries,
        point_dispositions: point_disp,
        range_dispositions: range_disp,
        events,
        oldest_active_snapshot_seq: oldest,
        cursor_key: None,
        processed_key_groups: processed,
        diagnostics,
    })
}

fn horizon_label(h: u64) -> String {
    if h == u64::MAX { "now".into() } else { h.to_string() }
}

impl PointKind {
    pub fn id(&self) -> &'static str {
        match self { PointKind::Set => "Set", PointKind::Delete => "Delete" }
    }
}

#[allow(clippy::too_many_arguments)]
fn with_partial(
    err: EngineError,
    plan: &CompactionPlan,
    boundaries: Vec<OutputBoundary>,
    point_disp: Vec<PointDisposition>,
    range_disp: Vec<RangeDisposition>,
    oldest: Option<u64>,
    cursor: Option<String>,
    processed: usize,
    diagnostics: Vec<Diagnostic>,
    events: Vec<Value>,
) -> EngineError {
    let partial = CompactionResult {
        plan: plan.clone(), status: RunStatus::Interrupted, boundaries,
        point_dispositions: point_disp, range_dispositions: range_disp, events,
        oldest_active_snapshot_seq: oldest, cursor_key: cursor,
        processed_key_groups: processed, diagnostics,
    };
    match err {
        EngineError::BudgetExhausted { cursor_key, processed, message } =>
            EngineError::BudgetExhausted { cursor_key, processed, message: format!("{message}|PARTIAL={}", serde_json::to_string(&partial).unwrap_or_default()) },
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn split_outputs(
    plan: &CompactionPlan,
    target_level: i64,
    input_files: Vec<&SSTable>,
    retained_points: &[PointRecord],
    retained_ranges: &[RangeTomb],
    range_decoded: BTreeMap<String, DecodedRange>,
    outside: &[&(String, i64, DecodedRange)],
    forced_cuts: &[Endpoint],
    params: &EngineParams,
) -> Result<(Vec<OutputBoundary>, Vec<Value>, Vec<Diagnostic>, Vec<(String, String, KeyRange)>), EngineError> {
    let mut events = Vec::new();
    let mut diags = Vec::new();
    let mut fragment_table: Vec<(String, String, KeyRange)> = Vec::new();

    // 目标层在输入外的已有区间：输出不得与之相交（不重叠层约束，L>=1）。
    let target_existing: Vec<&DecodedRange> = outside
        .iter()
        .filter(|(_, l, _)| *l == target_level)
        .map(|(_, _, r)| r)
        .collect();
    // 切分网格的候选切点：目标层边界（与输入总跨度相交处）+ 人工强制切点。
    let input_span = total_span(&input_files)?;

    let mut cut_bytes: Vec<(Vec<u8>, Endpoint, String)> = Vec::new();
    for (id, lvl, r) in outside.iter() {
        if *lvl == target_level && r.intersects(&input_span) {
            cut_bytes.push((r.start.clone(), encode_endpoint(&r.start), format!("target_level_edge:{}", id)));
            cut_bytes.push((r.end.clone(), encode_endpoint(&r.end), format!("target_level_edge:{}", id)));
        }
    }
    for cut in forced_cuts {
        let b = cut.bytes().map_err(EngineError::Corrupt)?;
        cut_bytes.push((b, cut.clone(), "forced_cut".into()));
    }
    cut_bytes.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    cut_bytes.dedup_by(|a, b| a.0 == b.0);

    // 受限于输入跨度内部的切点（开区间 (start,end)）。
    let inner_cuts: Vec<(Vec<u8>, Endpoint, String)> = cut_bytes
        .into_iter()
        .filter(|(b, _, _)| input_span.start < *b && *b < input_span.end)
        .collect();

    // 逐段构建：[span.start, cut1), [cut1, cut2), ... [cutN, span.end)
    let mut segments: Vec<(DecodedRange, String)> = Vec::new();
    let mut cur_start = input_span.start.clone();
    for (b, _ep, why) in &inner_cuts {
        let end = b.clone();
        if cur_start < end {
            segments.push((DecodedRange { start: cur_start.clone(), end: end.clone() }, why.clone()));
        }
        cur_start = b.clone();
    }
    segments.push((DecodedRange { start: cur_start, end: input_span.end.clone() }, "input_span_end".into()));

    // size 预算再细分：对包含过多 point 的段按 key 边界滚动切。
    let mut final_segments: Vec<(DecodedRange, String)> = Vec::new();
    for (seg, why) in segments {
        let pts: Vec<&PointRecord> = retained_points
            .iter()
            .filter(|p| {
                let kb = p.key.bytes().unwrap_or_default();
                seg.contains(&kb)
            })
            .collect();
        let approx: i64 = pts.iter().map(|p| p.value.as_ref().map(|v| v.representation.len() as i64).unwrap_or(0) + 8).sum::<i64>().max(pts.len() as i64 * 8);
        if approx <= params.max_output_bytes {
            final_segments.push((seg, why));
            continue;
        }
        // 预算滚动切点（确定性：按内部键序）。
        let mut start = seg.start.clone();
        let mut used = 0i64;
        let mut last_key: Option<Vec<u8>> = None;
        for p in &pts {
            let kb = p.key.bytes().map_err(EngineError::Corrupt)?;
            let cost = p.value.as_ref().map(|v| v.representation.len() as i64).unwrap_or(0) + 8;
            if used > 0 && used + cost > params.max_output_bytes {
                if let Some(lk) = &last_key {
                    let cut = next_key(lk);
                    if cut > start && kb >= cut {
                        final_segments.push((DecodedRange { start: start.clone(), end: cut.clone() }, "size_budget".into()));
                        start = cut;
                        used = 0;
                    }
                }
            }
            used += cost;
            last_key = Some(kb);
        }
        final_segments.push((DecodedRange { start, end: seg.end }, why));
    }

    // 不重叠层约束校验：target_level>=1 时，每段不得与 target_existing 相交。
    for (seg, why) in &final_segments {
        for ex in &target_existing {
            if seg.intersects(ex) {
                // 合法候选：列出可以解除冲突的全部切点（冲突区间两端）。
                let candidates: Vec<Value> = vec![
                    json!({"cut_before": encode_endpoint(&ex.start).raw, "effect":"绕开目标层已有文件"}),
                    json!({"cut_after": encode_endpoint(&ex.end).raw, "effect":"绕开目标层已有文件"}),
                ];
                return Err(EngineError::Unresolvable(Diagnostic {
                    code: "non_overlapping_level_violation".into(),
                    severity: DiagSeverity::Error,
                    location: format!("compaction[{}].target_level[{}]", plan.run_id, target_level),
                    message: format!(
                        "输出段 [{},{})（切点来源 {}）与目标层已有文件区间 [{},{}) 相交，违反不重叠层约束；人工强制切点语义无解",
                        String::from_utf8_lossy(&seg.start), String::from_utf8_lossy(&seg.end), why,
                        String::from_utf8_lossy(&ex.start), String::from_utf8_lossy(&ex.end)
                    ),
                    candidates,
                }));
            }
        }
    }
    // 段之间自身也必须两两不相交（构造保证，这里防御性校验）。
    for w in final_segments.windows(2) {
        if w[0].0.end != w[1].0.start || w[0].0.intersects(&w[1].0) {
            return Err(EngineError::Unresolvable(Diagnostic {
                code: "overlapping_outputs".into(),
                severity: DiagSeverity::Error,
                location: format!("compaction[{}].outputs", plan.run_id),
                message: "输出切分之间出现重叠，违反不重叠层约束".into(),
                candidates: vec![],
            }));
        }
    }

    let mut boundaries = Vec::new();
    let mut fragment_index: BTreeMap<String, usize> = BTreeMap::new();
    for (i, (seg, why)) in final_segments.iter().enumerate() {
        let file_id = format!("{}.out{}", plan.run_id, i + 1);
        let mut point_ids = Vec::new();
        let mut size_total = 8i64;
        for p in retained_points {
            let kb = p.key.bytes().map_err(EngineError::Corrupt)?;
            if seg.contains(&kb) {
                point_ids.push(p.id.clone());
                size_total += p.value.as_ref().map(|v| v.representation.len() as i64).unwrap_or(0) + 8;
            }
        }
        let mut range_fragment_ids = Vec::new();
        for r in retained_ranges {
            let rr = range_decoded.get(&r.id).unwrap();
            if let Some(clipped) = rr.clip(seg) {
                let n = fragment_index.entry(r.id.clone()).or_insert(0);
                *n += 1;
                let fid = format!("{}#frag{}", r.id, *n);
                range_fragment_ids.push(fid.clone());
                let kr = KeyRange {
                    start: encode_endpoint(&clipped.start),
                    end: encode_endpoint(&clipped.end),
                    interval_basis: "byte-lexicographic [start,end)".into(),
                };
                let kr_evt = kr.clone();
                events.push(json!({"type":"range_fragmented","run_id":plan.run_id,
                    "range_id":r.id,"fragment_id":fid.clone(),"output_file_id":file_id,"range":kr_evt,"seq":r.seq}));
                fragment_table.push((fid.clone(), r.id.clone(), kr));
                size_total += 8;
            }
        }
        let kr = KeyRange {
            start: encode_endpoint(&seg.start),
            end: encode_endpoint(&seg.end),
            interval_basis: "byte-lexicographic [start,end)".into(),
        };
        events.push(json!({"type":"output_boundary","run_id":plan.run_id,
            "file_id":file_id,"index":i,"range":kr,"cut_reason":why,
            "point_count":point_ids.len(),"range_fragment_count":range_fragment_ids.len()}));
        boundaries.push(OutputBoundary {
            file_id, range: kr, cut_reason: why.clone(), point_ids, range_fragment_ids,
            size: Measured::bytes(&format!("{} byte", size_total), size_total),
        });
    }

    // 片段表回填到 range dispositions 由调用方不便完成：这里通过事件已留证，另在结果上用诊断补充合法候选提示。
    if forced_cuts.is_empty() {
        // no-op
    } else {
        diags.push(Diagnostic {
            code: "forced_cuts_applied".into(),
            severity: DiagSeverity::Info,
            location: format!("compaction[{}].forced_cuts", plan.run_id),
            message: format!("应用 {} 个人工切点；若造成不重叠层冲突会返回 unresolvable 及全部合法候选", forced_cuts.len()),
            candidates: inner_cuts.iter().map(|(_, ep, why)| json!({"cut":ep.raw,"reason":why})).collect(),
        });
    }
    let _ = &mut diags;
    Ok((boundaries, events, diags, fragment_table))
}

fn total_span(input_files: &[&SSTable]) -> Result<DecodedRange, EngineError> {
    let mut start: Option<Vec<u8>> = None;
    let mut end: Option<Vec<u8>> = None;
    for f in input_files {
        let r = f.key_range.decoded().map_err(EngineError::Corrupt)?;
        start = Some(match start { Some(s) => s.min(r.start), None => r.start });
        end = Some(match end { Some(e) => e.max(r.end), None => r.end });
    }
    Ok(DecodedRange { start: start.unwrap_or_default(), end: end.unwrap_or_default() })
}

fn encode_endpoint(b: &[u8]) -> Endpoint {
    match std::str::from_utf8(b) {
        Ok(s) if s.chars().all(|c| !c.is_control()) => Endpoint::utf8(s),
        _ => Endpoint { raw: hex::encode(b), encoding: Encoding::Hex },
    }
}

/// 字节键的“紧随其后”边界：追加 0x00，用于 [k...) 滚动切分。
fn next_key(b: &[u8]) -> Vec<u8> {
    let mut v = b.to_vec();
    v.push(0x00);
    v
}

// ============================ 读取可见性 ============================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionLink {
    pub point_id: String,
    pub file_id: String,
    pub level: i64,
    pub seq: u64,
    pub kind: PointKind,
    pub visible_at_snapshot: bool,
    pub internal_order: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeCoverage {
    pub range_id: String,
    pub file_id: String,
    pub level: i64,
    pub seq: u64,
    pub range: KeyRange,
    pub covers_key: bool,
    pub visible_at_snapshot: bool,
    pub blocks_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadOutcome {
    pub key: Endpoint,
    pub snapshot_seq: u64,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<PointValue>,
    pub chain: Vec<VersionLink>,
    pub range_coverage: Vec<RangeCoverage>,
    pub decision: String,
}

/// 读取：先按内部键序扫 point 版本链，再与可见 range tombstone 比较，全部留证。
pub fn point_read(files: &[SSTable], key: &Endpoint, snapshot_seq: u64) -> Result<ReadOutcome, EngineError> {
    let kb = key.bytes().map_err(EngineError::Corrupt)?;
    let mut hits: Vec<(usize, &SSTable, &PointRecord)> = Vec::new();
    let mut ranges: Vec<(&SSTable, &RangeTomb, DecodedRange)> = Vec::new();
    for f in files {
        if f.state != FileState::Live { continue; }
        for p in &f.points {
            if p.seq <= snapshot_seq && p.key.bytes().map_err(EngineError::Corrupt)? == kb {
                hits.push((0, f, p));
            }
        }
        for r in &f.ranges {
            if r.seq <= snapshot_seq {
                let rr = r.range.decoded().map_err(EngineError::Corrupt)?;
                ranges.push((f, r, rr));
            }
        }
    }
    // 内部键序：seq↓, Delete-first。
    hits.sort_by(|a, b| b.2.seq.cmp(&a.2.seq).then_with(|| kind_rank(b.2.kind).cmp(&kind_rank(a.2.kind))));

    let mut chain = Vec::new();
    for (i, (_, f, p)) in hits.iter().enumerate() {
        chain.push(VersionLink {
            point_id: p.id.clone(), file_id: f.id.clone(), level: f.level,
            seq: p.seq, kind: p.kind, visible_at_snapshot: true, internal_order: i,
        });
    }
    let mut coverage = Vec::new();
    for (f, r, rr) in &ranges {
        let covers = rr.contains(&kb);
        coverage.push(RangeCoverage {
            range_id: r.id.clone(), file_id: f.id.clone(), level: f.level, seq: r.seq,
            range: r.range.clone(), covers_key: covers, visible_at_snapshot: true,
            blocks_reason: if covers { Some(format!("range {}@{} 覆盖该键", r.id, r.seq)) } else { None },
        });
    }

    // 判定：第一版本与覆盖它的最高 seq range 谁更新。
    let newest_point = hits.first().map(|(_, _, p)| *p);
    let covering_range = ranges
        .iter()
        .filter(|(_, r, rr)| rr.contains(&kb))
        .map(|(_, r, _)| *r)
        .max_by_key(|r| r.seq);

    let mut result = "not_found".to_string();
    let mut value = None;
    let mut decision;
    match (newest_point, covering_range) {
        (Some(p), Some(r)) if r.seq >= p.seq => {
            result = "range_deleted".into();
            decision = format!("range tombstone {}@{} >= point {}@{}，判为范围删除", r.id, r.seq, p.id, p.seq);
        }
        (Some(p), _) if p.kind == PointKind::Delete => {
            result = "point_deleted".into();
            decision = format!("内部键序第一可见记录为 point tombstone {}@{}", p.id, p.seq);
        }
        (Some(p), r) => {
            result = "value".into();
            value = p.value.clone();
            decision = format!("point {}@{} 为最新可见 Set{}", p.id, p.seq,
                r.map(|rr| format!("（range {}@{} 更旧，不生效）", rr.id, rr.seq)).unwrap_or_default());
        }
        (None, Some(r)) => {
            result = "range_deleted".into();
            decision = format!("无 point 版本，range tombstone {}@{} 生效", r.id, r.seq);
        }
        (None, None) => {
            decision = "snapshot 范围内无 point、无覆盖 range：不存在".into();
        }
    }

    Ok(ReadOutcome { key: key.clone(), snapshot_seq, result, value, chain, range_coverage: coverage, decision })
}

/// range 扫描：返回某 snapshot 下覆盖给定区间的全部 tombstone（tombstone 覆盖视图）。
pub fn range_scan(files: &[SSTable], range: &KeyRange, snapshot_seq: u64) -> Result<Vec<RangeCoverage>, EngineError> {
    let q = range.decoded().map_err(EngineError::Corrupt)?;
    let mut out = Vec::new();
    for f in files {
        if f.state != FileState::Live { continue; }
        for r in &f.ranges {
            if r.seq > snapshot_seq { continue; }
            let rr = r.range.decoded().map_err(EngineError::Corrupt)?;
            let cover = rr.intersects(&q);
            out.push(RangeCoverage {
                range_id: r.id.clone(), file_id: f.id.clone(), level: f.level, seq: r.seq,
                range: r.range.clone(), covers_key: cover, visible_at_snapshot: true,
                blocks_reason: if cover { Some("与查询区间相交".into()) } else { None },
            });
        }
    }
    out.sort_by(|a, b| a.range.start.raw.cmp(&b.range.start.raw).then_with(|| b.seq.cmp(&a.seq)));
    Ok(out)
}
