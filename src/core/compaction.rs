//! 压实计划语义（纯函数，可被测试直接调用）。
//!
//! 关键判断（也作为机器可读事件里的 basis 固化）：
//! R1 读取按内部键顺序判可见性：同 user key 按 (seq 降序, 类型 rank 降序)，
//!    仅 seq <= snapshot 的记录参与；point delete/range delete 遮蔽更早版本。
//! R2 point 版本仅当被“任何存活文件中更高内部键序的版本”遮蔽、且没有活跃 snapshot
//!    的 seq 落在 [old.seq+1, newer.seq] 窗口内时才可丢弃，否则下沉。
//! R3 range delete 不能越过未参与压实的旧数据提前丢弃；只有在某子区间上既无外部
//!    重叠旧数据、输出中也无 seq 更小的存活 point，且无活跃 snapshot 需要时才丢弃，
//!    否则按 key 边界切片段并下沉到对应输出（保留/下沉）。
//! R4 输出切分必须维持目标层不重叠约束；预算耗尽（单 key 超预算）是 budget_exhausted。
//! R5 多个 L0 同分且策略为 reject_ambiguous 时是 semantic_unresolved，并保留全部候选。

use crate::core::errors::{Error, Result};
use crate::core::events::{ReadResult, VersionLink};
use crate::core::model::{
    CompactionParams, PointEntry, PointKind, RangeTombstone, SstSummary, SstOrigin,
};
use serde::{Deserialize, Serialize};

pub const RULE_VERSION: &str = "lsm-review.rules.v1";

/// 压实起点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactionStart {
    L0 { seed_sst_id: String },
    Level { level: u32, seed_sst_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub id: String,
    pub start: CompactionStart,
    pub params_version: u32,
    pub rule_version: String,
    pub input_sst_ids: Vec<String>,
    pub target_level: u32,
    /// L0 选择时的全部合法候选（同分也保留），含排序依据。
    pub candidates: Vec<CandidateInfo>,
    /// 选择理由（机器可读）。
    pub selection_basis: Vec<String>,
    /// 参与合并的 point 逐条判定。
    pub point_decisions: Vec<EntryDecision>,
    /// range tombstone 逐条、逐片段判定。
    pub range_decisions: Vec<RangeDecision>,
    pub outputs: Vec<PlannedOutput>,
    /// 输出边界与不重叠层约束校验。
    pub boundaries: Vec<OutputBoundary>,
    /// 最老活跃 snapshot seq；无则 None。
    pub barrier_seq: Option<u64>,
    /// 生成时世界里存活文件集合的摘要。
    pub live_input_digest: String,
    /// 可接受性结论。
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateInfo {
    pub seed_sst_id: String,
    pub closure_sst_ids: Vec<String>,
    pub overlap_l0_count: usize,
    pub overlap_bytes: u64,
    pub smallest: String,
    pub largest: String,
    pub score: String,
    pub rank_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDecision {
    pub entry_id: String,
    pub sst_id: String,
    pub user_key: String,
    pub seq: u64,
    pub kind: String,
    /// keep / drop。
    pub decision: String,
    /// 遮蔽者条目身份（drop 时）。
    pub shadowed_by: Option<String>,
    pub reason_basis: String,
    /// 落入的输出 SSTable（keep 时）。
    pub output_sst_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeDecision {
    pub range_id: String,
    pub source_sst_id: String,
    pub seq: u64,
    pub start: String,
    pub end: String,
    pub fragments: Vec<RangeFragmentDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeFragmentDecision {
    /// 半开 [frag_start, frag_end)。
    pub frag_start: String,
    pub frag_end: String,
    pub decision: String,
    pub reason_basis: String,
    pub output_sst_id: Option<String>,
    /// 阻止提前丢弃的未参与压实旧数据（身份列表）。
    pub blocked_by_external: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedOutput {
    pub sst_id: String,
    pub level: u32,
    pub smallest: String,
    pub largest: String,
    pub point_ids: Vec<String>,
    pub range_fragments: Vec<PlannedRangeFragment>,
    pub estimated_size_bytes: u64,
    pub size_unit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedRangeFragment {
    pub range_id: String,
    pub start: String,
    pub end: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputBoundary {
    pub output_sst_id: String,
    pub smallest: String,
    pub largest: String,
    /// 与目标层既有（非输入）文件的重叠检查结果。
    pub non_overlap_ok: bool,
    pub violates_against: Vec<String>,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Ready,
    Ambiguous,
    BudgetExceeded,
}

/// 内部键序比较：user key 升序，seq 降序，类型 rank 降序（同 seq 多类型确定顺序）。
pub fn internal_order(a_key: &str, a_seq: u64, a_kind: PointKind,
                      b_key: &str, b_seq: u64, b_kind: PointKind) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match a_key.cmp(b_key) {
        Ordering::Equal => match b_seq.cmp(&a_seq) {
            Ordering::Equal => b_kind.internal_rank().cmp(&a_kind.internal_rank()),
            o => o,
        },
        o => o,
    }
}

/// 估算 point 条目序列化大小（byte）：user key + seq 8B + kind 1B + value。
pub fn point_size_bytes(p: &PointEntry) -> u64 {
    let value_len = p.value.as_ref().map(|v| v.as_bytes().len() as u64).unwrap_or(0);
    p.user_key.as_bytes().len() as u64 + 8 + 1 + value_len
}

pub fn range_size_bytes(r: &PlannedRangeFragment) -> u64 {
    r.start.as_bytes().len() as u64 + r.end.as_bytes().len() as u64 + 8 + 1
}

fn file_size(sst: &SstSummary) -> u64 {
    sst.file_size.parsed.unwrap_or_else(|| {
        let mut n = 0u64;
        for p in &sst.points { n += point_size_bytes(p); }
        for r in &sst.range_deletes {
            n += r.start.as_bytes().len() as u64 + r.end.as_bytes().len() as u64 + 9;
        }
        n
    }) as u64
}

/// 一个带所属文件身份的 point 视图。
#[derive(Debug, Clone)]
pub struct LivePoint<'a> {
    pub sst: &'a SstSummary,
    pub p: &'a PointEntry,
}
#[derive(Debug, Clone)]
pub struct LiveRange<'a> {
    pub sst: &'a SstSummary,
    pub r: &'a RangeTombstone,
}

/// 在某 snapshot seq 下读取 [key_start, key_end)（None 表示单点/全部）。
/// `live` 为当前（或历史事件重放得到的）存活 SSTable。
/// 纯函数：不读库，供服务端、存储层与测试共用。
pub fn read_at<'a>(
    live: &[&'a SstSummary],
    snapshot_seq: u64,
    key_start: Option<&str>,
    key_end: Option<&str>,
) -> Vec<ReadResult> {
    // 收集范围命中的 point 与 range。
    let mut keys: Vec<String> = Vec::new();
    let mut by_key: std::collections::BTreeMap<String, Vec<LivePoint<'a>>> =
        std::collections::BTreeMap::new();
    let mut ranges: Vec<LiveRange<'a>> = Vec::new();

    for sst in live {
        for r in &sst.range_deletes {
            let hit = match (key_start, key_end) {
                (Some(s), Some(e)) => !(r.end.as_str() <= s || r.start.as_str() >= e),
                (Some(s), None) => r.end.as_str() > s,
                _ => true,
            };
            if hit { ranges.push(LiveRange { sst, r }); }
        }
        for p in &sst.points {
            let in_window = match (key_start, key_end) {
                (Some(s), Some(e)) => p.user_key.as_str() >= s && p.user_key.as_str() < e,
                (Some(s), None) => p.user_key.as_str() >= s,
                _ => true,
            };
            if !in_window { continue; }
            keys.push(p.user_key.clone());
            by_key.entry(p.user_key.clone()).or_default().push(LivePoint { sst, p });
        }
    }
    keys.sort();
    keys.dedup();

    let mut out = Vec::new();
    for key in keys {
        let mut versions = by_key.remove(&key).unwrap_or_default();
        // 版本链按内部键顺序。
        versions.sort_by(|a, b| {
            internal_order(&a.p.user_key, a.p.seq, a.p.kind,
                           &b.p.user_key, b.p.seq, b.p.kind)
        });
        let mut chain = Vec::new();
        for v in &versions {
            let visible = v.p.seq <= snapshot_seq;
            chain.push(VersionLink {
                entry_id: v.p.id.clone(),
                sst_id: v.sst.id.clone(),
                seq: v.p.seq,
                kind: format!("{:?}", v.p.kind).to_lowercase(),
                visible,
                invisible_reason: if visible { None } else { Some("seq>snapshot".into()) },
            });
        }
        // 找可见的最高内部键序 point。
        let top = versions.iter().find(|v| v.p.seq <= snapshot_seq);
        // 找覆盖该 key、可见（seq<=snapshot）的最高 seq range tombstone。
        let cover = ranges.iter()
            .filter(|lr| lr.r.seq <= snapshot_seq && lr.r.contains(&key))
            .max_by(|a, b| a.r.seq.cmp(&b.r.seq));

        let outcome;
        let mut visible_id = None;
        let mut visible_seq = None;
        let mut covered_by = None;

        let point_gt = top.map(|t| t.p.seq > snapshot_seq).unwrap_or(false);
        let _ = point_gt;

        match (top, cover) {
            (Some(t), Some(cr)) => {
                // 比较 point 与 range 的内部键序：point 以 (key,seq,rank)，
                // range 按 (seq) 影响；同 seq 时 point 与 range 共存，
                // range 作为独立删除记录，按“range 与 point 同 seq 时 point 胜出”判定。
                let point_wins = t.p.seq > cr.r.seq
                    || (t.p.seq == cr.r.seq);
                if point_wins {
                    if t.p.kind.is_tombstone() {
                        outcome = "point-tombstone".into();
                    } else {
                        outcome = "value".into();
                    }
                    visible_id = Some(t.p.id.clone());
                    visible_seq = Some(t.p.seq);
                } else {
                    outcome = "range-tombstone".into();
                    covered_by = Some(cr.r.id.clone());
                    visible_seq = Some(cr.r.seq);
                }
            }
            (Some(t), None) => {
                if t.p.kind.is_tombstone() { outcome = "point-tombstone".into(); }
                else { outcome = "value".into(); }
                visible_id = Some(t.p.id.clone());
                visible_seq = Some(t.p.seq);
            }
            (None, Some(cr)) => {
                outcome = "range-tombstone".into();
                covered_by = Some(cr.r.id.clone());
                visible_seq = Some(cr.r.seq);
            }
            (None, None) => outcome = "absent".into(),
        }

        out.push(ReadResult {
            user_key: key,
            visible_entry_id: visible_id,
            visible_seq,
            outcome,
            covered_by_range: covered_by,
            version_chain: chain,
        });
    }
    out
}

/// 计算压实输入闭包。返回 (输入文件, 目标层)。
pub fn input_closure(
    live: &[SstSummary],
    start: &CompactionStart,
) -> (Vec<String>, u32) {
    let mut input = std::collections::BTreeSet::new();
    match start {
        CompactionStart::L0 { seed_sst_id } => {
            // L0 互相重叠：从 seed 出发按 key range 传递闭包拉入全部重叠 L0。
            let mut frontier = vec![seed_sst_id.clone()];
            while let Some(id) = frontier.pop() {
                if !input.insert(id.clone()) { continue; }
                let cur = live.iter().find(|s| s.id == id);
                if let Some(cur) = cur {
                    for other in live.iter() {
                        if other.level == 0
                            && !input.contains(&other.id)
                            && ranges_overlap_inclusive(&cur.smallest, &cur.largest,
                                                        &other.smallest, &other.largest)
                        {
                            frontier.push(other.id.clone());
                        }
                    }
                }
            }
            // 拉入目标 L1 中与并集重叠的文件（再传递扩张，L1 常态不重叠）。
            let union = union_span(live, &input);
            if let Some((us, ul)) = union {
                let mut frontier = Vec::new();
                for s in live.iter() {
                    if s.level == 1 && ranges_overlap_inclusive(&us, &ul, &s.smallest, &s.largest) {
                        frontier.push(s.id.clone());
                    }
                }
                while let Some(id) = frontier.pop() {
                    if !input.insert(id.clone()) { continue; }
                    let cur = live.iter().find(|s| s.id == id);
                    if let Some(cur) = cur {
                        for other in live.iter() {
                            if other.level == 1
                                && !input.contains(&other.id)
                                && ranges_overlap_inclusive(&cur.smallest, &cur.largest,
                                                            &other.smallest, &other.largest)
                            {
                                frontier.push(other.id.clone());
                            }
                        }
                    }
                }
            }
            (input.into_iter().collect(), 1)
        }
        CompactionStart::Level { level, seed_sst_id } => {
            let target = level + 1;
            let mut frontier = vec![seed_sst_id.clone()];
            while let Some(id) = frontier.pop() {
                if !input.insert(id.clone()) { continue; }
                let cur = live.iter().find(|s| s.id == id);
                if let Some(cur) = cur {
                    for other in live.iter() {
                        let relevant = other.level == cur.level || other.level == target;
                        if relevant
                            && !input.contains(&other.id)
                            && ranges_overlap_inclusive(&cur.smallest, &cur.largest,
                                                        &other.smallest, &other.largest)
                        {
                            frontier.push(other.id.clone());
                        }
                    }
                }
            }
            (input.into_iter().collect(), target)
        }
    }
}

fn ranges_overlap_inclusive(a0: &str, a1: &str, b0: &str, b1: &str) -> bool {
    !(a1 < b0 || b1 < a0)
}

fn union_span(live: &[SstSummary], ids: &std::collections::BTreeSet<String>)
    -> Option<(String, String)> {
    let mut lo: Option<String> = None;
    let mut hi: Option<String> = None;
    for s in live {
        if ids.contains(&s.id) {
            lo = Some(match lo { Some(v) => v.min(s.smallest.clone()), None => s.smallest.clone() });
            hi = Some(match hi { Some(v) => v.max(s.largest.clone()), None => s.largest.clone() });
        }
    }
    lo.zip(hi)
}

/// 评估所有 L0 seed 候选（依据：拉入的 L0 数、重叠字节、smallest、id）。
pub fn l0_candidates(live: &[SstSummary]) -> Vec<CandidateInfo> {
    let l0: Vec<&SstSummary> = live.iter().filter(|s| s.level == 0).collect();
    let mut out = Vec::new();
    for seed in &l0 {
        let (closure_ids, _target) =
            input_closure(live, &CompactionStart::L0 { seed_sst_id: seed.id.clone() });
        let closure_l0 = closure_ids.iter()
            .filter(|id| live.iter().any(|s| &s.id == *id && s.level == 0))
            .count();
        let bytes: u64 = live.iter()
            .filter(|s| closure_ids.contains(&s.id))
            .map(file_size)
            .sum();
        let (sm, lg) = union_span(live, &closure_ids.iter().cloned().collect())
            .unwrap_or((seed.smallest.clone(), seed.largest.clone()));
        out.push(CandidateInfo {
            seed_sst_id: seed.id.clone(),
            closure_sst_ids: closure_ids,
            overlap_l0_count: closure_l0,
            overlap_bytes: bytes,
            smallest: sm,
            largest: lg,
            score: format!("l0={};bytes={}", closure_l0, bytes),
            rank_keys: vec![format!("{:010}", u32::MAX - closure_l0 as u32),
                            format!("{:020}", u64::MAX - bytes)],
        });
    }
    // 确定性排序：拉入 L0 多 > 重叠字节大 > smallest 小 > seed id 小。
    out.sort_by(|a, b| {
        b.overlap_l0_count.cmp(&a.overlap_l0_count)
            .then(b.overlap_bytes.cmp(&a.overlap_bytes))
            .then(a.smallest.cmp(&b.smallest))
            .then(a.seed_sst_id.cmp(&b.seed_sst_id))
    });
    out
}

/// 字节 key 的后继 key（用于半开片段上界）：在末尾加一个 0x00 字节。
pub fn successor_key(k: &str) -> String {
    let mut s = String::from(k);
    s.push('\u{0000}');
    s
}

struct AllPoint<'a> { sst_id: String, p: &'a PointEntry }

/// 生成压实计划。`ambiguous_candidates` 在 reject_ambiguous 命中时返回全部同分候选。
pub fn plan_compaction(
    live: &[SstSummary],
    params: &CompactionParams,
    start: CompactionStart,
    barrier_seq: Option<u64>,
    live_digest: String,
) -> Result<CompactionPlan> {
    let (input_ids, target_level) = input_closure(live, &start);
    let input_set: std::collections::HashSet<String> = input_ids.iter().cloned().collect();

    // L0 选择理由与同分歧义。
    let mut selection_basis: Vec<String> = Vec::new();
    let mut candidates: Vec<CandidateInfo> = Vec::new();
    if let CompactionStart::L0 { .. } = &start {
        candidates = l0_candidates(live);
        if let Some(top) = candidates.first() {
            selection_basis.push(format!(
                "R5: seed={} chosen; rank=overlap_l0_count desc, overlap_bytes desc, smallest asc, id asc",
                top.seed_sst_id));
            let tied: Vec<&CandidateInfo> = candidates.iter()
                .filter(|c| c.overlap_l0_count == top.overlap_l0_count
                    && c.overlap_bytes == top.overlap_bytes).collect();
            if tied.len() > 1 {
                let tie_ids: Vec<String> = tied.iter().map(|c| c.seed_sst_id.clone()).collect();
                selection_basis.push(format!(
                    "R5: {} equivalent L0 seeds {:?}; tie_policy={}",
                    tied.len(), tie_ids, params.tie_policy));
                if params.tie_policy == "reject_ambiguous" {
                    return Err(Error::SemanticUnresolved {
                        candidates: tie_ids,
                        basis: "R5: overlapping L0 closure yields multiple equivalent compaction \
                                starts and tie_policy=reject_ambiguous"
                            .into(),
                        message: "multiple legal L0 compaction seeds are equally preferred"
                            .into(),
                    });
                }
                selection_basis.push(
                    "R5: tie_policy=first; deterministic smallest/id ordering resolves it".into());
            }
        }
    } else {
        selection_basis.push("R4/R3: level compaction seed with transitive overlap closure".into());
    }

    // 收集所有存活 point（参与全局遮蔽判定）。
    let mut all_points: Vec<AllPoint> = Vec::new();
    for s in live {
        for p in &s.points {
            all_points.push(AllPoint { sst_id: s.id.clone(), p });
        }
    }
    all_points.sort_by(|a, b| internal_order(&a.p.user_key, a.p.seq, a.p.kind,
                                             &b.p.user_key, b.p.seq, b.p.kind));

    // 所有存活 range（外部 range 也能遮蔽输入 point）。
    let all_ranges: Vec<LiveRange> = live.iter().flat_map(|s|
        s.range_deletes.iter().map(move |r| LiveRange { sst: s, r })).collect();

    let mut point_decisions: Vec<EntryDecision> = Vec::new();
    // 判定后保留的 point（带来源 sst 身份）。
    let mut kept_points: Vec<(String, &PointEntry)> = Vec::new();

    for s in live {
        if !input_set.contains(&s.id) { continue; }
        for p in &s.points {
            // 寻找任何存活文件中、内部键序更高的遮蔽者（point 或 range）。
            let newer_point = all_points.iter().find(|ap| {
                ap.p.user_key == p.user_key
                    && internal_order(&ap.p.user_key, ap.p.seq, ap.p.kind,
                                      &p.user_key, p.seq, p.kind) == std::cmp::Ordering::Less
            });
            let newer_range = all_ranges.iter()
                .filter(|lr| lr.r.contains(&p.user_key) && lr.r.seq >= p.seq)
                .max_by(|a, b| a.r.seq.cmp(&b.r.seq));

            let decision;
            let mut shadowed_by = None;
            let mut basis = String::new();

            if let Some(np) = newer_point {
                // 被更高 point 遮蔽；检查活跃 snapshot 窗口 [p.seq+1, newer.seq]。
                let pinned = matches!(barrier_seq, Some(b) if b <= np.p.seq);
                if pinned {
                    decision = "keep";
                    basis = format!(
                        "R2: shadowed by {} but active snapshot barrier seq={} lies in \
                         [old={}+1, newer={}], old value must remain visible",
                        np.p.id, barrier_seq.unwrap_or(0), p.seq, np.p.seq);
                } else {
                    decision = "drop";
                    shadowed_by = Some(np.p.id.clone());
                    basis = format!(
                        "R1/R2: {} shadows {} and no active snapshot needs seq in \
                         ({}, {}]", np.p.id, p.id, p.seq, np.p.seq);
                }
            } else if let Some(nr) = newer_range.filter(|lr| lr.r.seq > p.seq) {
                let pinned = matches!(barrier_seq, Some(b) if b <= nr.r.seq);
                if pinned {
                    decision = "keep";
                    basis = format!(
                        "R3: covered by {} but barrier seq={} <= range seq={}; retain",
                        nr.r.id, barrier_seq.unwrap_or(0), nr.r.seq);
                } else {
                    decision = "drop";
                    shadowed_by = Some(nr.r.id.clone());
                    basis = format!("R1: range {} covers {} with greater seq", nr.r.id, p.id);
                }
            } else {
                decision = "keep";
                if p.kind.is_tombstone() {
                    basis = "R2: topmost point tombstone retained (may shadow later \
                             ingested older keys)"
                        .into();
                } else {
                    basis = "R1: newest live version of key".into();
                }
            }
            point_decisions.push(EntryDecision {
                entry_id: p.id.clone(),
                sst_id: s.id.clone(),
                user_key: p.user_key.clone(),
                seq: p.seq,
                kind: format!("{:?}", p.kind).to_lowercase(),
                decision: decision.into(),
                shadowed_by,
                reason_basis: basis,
                output_sst_id: None,
            });
            if decision == "keep" {
                kept_points.push((p.id.clone(), p));
            }
        }
    }

    // ---- range tombstone 分片：不能越过未参与压实的旧数据提前丢弃 ----
    let mut range_decisions: Vec<RangeDecision> = Vec::new();
    // 保留片段，稍后分配输出。
    let mut kept_fragments: Vec<(String, PlannedRangeFragment)> = Vec::new();

    for s in live {
        if !input_set.contains(&s.id) { continue; }
        for r in &s.range_deletes {
            // 外部（未参与压实）文件与该 range 的重叠区间（inclusive span）。
            let mut external_spans: Vec<(String, String, String)> = Vec::new();
            for ext in live {
                if input_set.contains(&ext.id) { continue; }
                let lo = ext.smallest.max(r.start.clone());
                let hi = ext.largest.min(r.end_predecessor().unwrap_or_else(|| r.end.clone()));
                if lo <= hi && ranges_overlap_inclusive(&ext.smallest, &ext.largest,
                                                        &r.start, &r.end_predecessor()
                                                            .unwrap_or_else(|| r.end.clone())) {
                    external_spans.push((ext.id.clone(), lo, hi));
                }
            }
            // 输出中存活、seq 更小的 point（落在 range 内）。
            let live_pts: Vec<&PointEntry> = kept_points.iter()
                .filter(|(_, p)| r.contains(&p.user_key) && p.seq < r.seq)
                .map(|(_, p)| *p)
                .collect();
            let pinned = matches!(barrier_seq, Some(b) if b <= r.seq);

            // 以端点与 point key 切半开子区间。
            let mut cuts: Vec<String> = vec![r.start.clone(), r.end.clone()];
            for p in &live_pts { cuts.push(p.user_key.clone()); }
            for (_id, lo, hi) in &external_spans { cuts.push(lo.clone()); cuts.push(successor_key(hi)); }
            cuts.sort(); cuts.dedup();
            // 去除超出 [start,end) 的切点。
            cuts.retain(|c| c.as_str() >= r.start.as_str() && c.as_str() <= r.end.as_str());

            let mut segs: Vec<(String, String, bool, Vec<String>)> = Vec::new();
            for w in cuts.windows(2) {
                let (s, e) = (w[0].clone(), w[1].clone());
                if s >= e { continue; }
                // 该子区间的代表 key：用起点（半开，包含 s）。
                let rep = s.clone();
                let has_live_pt = live_pts.iter().any(|p| p.user_key == rep);
                let blockers: Vec<String> = external_spans.iter().filter(|(_id, lo, hi)| {
                    // 子区间 [s,e) 与外部 inclusive span 相交。
                    !(rep.as_str() > hi.as_str() || successor_key(rep).as_str() <= lo.as_str())
                }).map(|(id, _, _)| id.clone()).collect();
                let keep = has_live_pt || !blockers.is_empty() || pinned;
                segs.push((s, e, keep, blockers));
            }
            // 合并相邻 keep 片段。
            let mut fragments: Vec<RangeFragmentDecision> = Vec::new();
            let mut i = 0;
            while i < segs.len() {
                let (s, e, keep, blockers) = &segs[i];
                if !keep {
                    fragments.push(RangeFragmentDecision {
                        frag_start: s.clone(), frag_end: e.clone(),
                        decision: "drop".into(),
                        reason_basis: format!(
                            "R3: no surviving point with seq<{}, no external non-compacting \
                             old data, no active snapshot barrier over subrange", r.seq),
                        output_sst_id: None, blocked_by_external: Vec::new(),
                    });
                    i += 1;
                } else {
                    let (mut fs, mut fe) = (s.clone(), e.clone());
                    let mut blk = blockers.clone();
                    let mut j = i + 1;
                    while j < segs.len() && segs[j].2 && segs[j].0 == fe {
                        fe = segs[j].1.clone();
                        blk.extend(segs[j].3.clone());
                        j += 1;
                    }
                    fragments.push(RangeFragmentDecision {
                        frag_start: fs.clone(), frag_end: fe.clone(),
                        decision: "keep".into(),
                        reason_basis: format!(
                            "R3: range tombstone retained and sunk; protects live older point \
                             or external non-compacting old data or active snapshot", ),
                        output_sst_id: None,
                        blocked_by_external: { blk.sort(); blk.dedup(); blk },
                    });
                    kept_fragments.push((r.id.clone(), PlannedRangeFragment {
                        range_id: r.id.clone(), start: fs, end: fe, seq: r.seq,
                    }));
                    i = j;
                }
            }
            range_decisions.push(RangeDecision {
                range_id: r.id.clone(), source_sst_id: s.id.clone(), seq: r.seq,
                start: r.start.clone(), end: r.end.clone(), fragments,
            });
        }
    }

    // ---- 输出切分（预算，byte），维持不重叠层约束 ----
    kept_points.sort_by(|(_, a), (_, b)| internal_order(&a.user_key, a.seq, a.kind,
                                                        &b.user_key, b.seq, b.kind));
    let mut groups: Vec<Vec<&(String, &PointEntry)>> = Vec::new();
    let mut cur: Vec<&(String, &PointEntry)> = Vec::new();
    let mut cur_size = 0u64;
    for item in kept_points.iter() {
        let sz = point_size_bytes(item.1);
        if sz > params.max_output_bytes {
            return Err(Error::BudgetExhausted {
                resource: "compaction_output".into(),
                limit: params.max_output_bytes.to_string(),
                used: sz.to_string(),
                unit: params.size_unit.clone(),
                message: format!("single key {} entry {} exceeds output budget and cannot be split",
                                 item.1.user_key, item.1.id),
            });
        }
        if !cur.is_empty() && cur_size + sz > params.max_output_bytes {
            groups.push(std::mem::take(&mut cur));
            cur_size = 0;
        }
        cur_size += sz;
        cur.push(item);
    }
    if !cur.is_empty() { groups.push(cur); }

    let plan_id = format!("plan-{}", &sha_id(&live_digest,
        &format!("{:?}:{:?}:{}:{}", start, input_ids, params.version, params.max_output_bytes))[..16]);

    let mut outputs: Vec<PlannedOutput> = Vec::new();
    for (gi, group) in groups.iter().enumerate() {
        let sst_id = format!("{}#out{}", plan_id, gi);
        let keys: Vec<&str> = group.iter().map(|(_, p)| p.user_key.as_str()).collect();
        let smallest = keys.first().unwrap().to_string();
        let largest = keys.last().unwrap().to_string();
        let point_ids: Vec<String> = group.iter().map(|(id, _)| id.clone()).collect();
        // 把与该输出 key 范围相交的保留 range 片段挂到输出（按 key 边界下沉）。
        let mut frags_here: Vec<PlannedRangeFragment> = Vec::new();
        for (_rid, frag) in &kept_fragments {
            if frag.end.as_str() <= smallest.as_str()
               || frag.start.as_str() > successor_key(largest).as_str() {
                // 明显不相交（half-open vs inclusive）。
                let overlap = frag.start.as_str() <= largest.as_str()
                    && frag.end_predecessor().as_deref().map(|p| p.as_str() >= smallest.as_str())
                        .unwrap_or(false);
                if !overlap { continue; }
            }
            frags_here.push(frag.clone());
        }
        let size: u64 = group.iter().map(|(_, p)| point_size_bytes(p)).sum::<u64>()
            + frags_here.iter().map(range_size_bytes).sum::<u64>();
        outputs.push(PlannedOutput {
            sst_id, level: target_level, smallest, largest,
            point_ids, range_fragments: frags_here,
            estimated_size_bytes: size, size_unit: "byte".into(),
        });
    }
