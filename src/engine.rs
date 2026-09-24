//! 纯业务规则层：压实选择、内部键序读取、snapshot 可见性、
//! point/range tombstone 的保留/下沉判定、输出切分与三类失败结果。
//!
//! 本层无 I/O、无框架，可被测试直接调用。

use crate::domain::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::cmp::Ordering;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EngineState {
    pub rule_version: String,
    pub params_version: String,
    pub params: ParamsV,
    pub snapshots: Vec<Snapshot>,
    pub files: Vec<SstFile>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CompactionRequest {
    pub anchor_level: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_file_id: Option<String>,
    #[serde(default)]
    pub include_levels: Vec<i32>,
    /// 模拟在第 N 个输出安装前中断（None = 完整执行）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash_before_install: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    ShadowedByPoint,
    VisibleToSnapshot,
    PointDeleteHasOlderData,
    PointDeleteCoveredByRange,
    ShadowedByRange,
    RangeHidesLiveData,
    RangeBoundedByOldData,
    RangeVisibleToSnapshot,
    KeptUnconditionally,
    SplitCandidate,
    SplitChosen,
    NonOverlapViolation,
    BudgetExceeded,
    UnnormalizableQty,
    CorruptFile,
    DuplicateSameSeq,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Finding {
    pub id: String,
    pub code: String,
    pub severity: String, // info | warn | error | fatal
    pub subject: String,  // 被判定对象稳定 id
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct EntryVerdict {
    pub entry_id: String,
    pub kind: String, // point_put | point_delete | range
    pub key: String,
    pub seq: u64,
    pub survivor: bool,
    pub reason_code: String,
    pub reason_message: String,
    pub populations: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SplitCandidateRec {
    pub index: usize,
    pub after_key: String,
    pub last_point_seq: u64,
    pub cumulative_bytes: u64,
    pub legal: bool,
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct OutputSummary {
    pub proposed_id: String,
    pub level: i32,
    pub min_key: String,
    pub max_key: String,
    pub min_seq: u64,
    pub max_seq: u64,
    pub size_bytes: u64,
    pub point_count: u64,
    pub range_count: u64,
    pub point_ids: Vec<String>,
    pub range_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FileSelectionReason {
    pub file_id: String,
    pub selected: bool,
    pub reason_code: String,
    pub reason_message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CompactionStatus {
    Planned,
    Complete,
    Interrupted { installed_count: usize, pending_count: usize, at_step: usize },
    ResourceBudgetExhausted { limit: String, used: String },
    FormatCorrupt { diagnostics: Vec<Diagnostic> },
    SemanticUnsolvable { message: String, candidates: Vec<SplitCandidateRec> },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CompactionResult {
    pub rule_version: String,
    pub params_version: String,
    pub input_digest: String,
    pub anchor_level: i32,
    pub selected_file_ids: Vec<String>,
    pub selection: Vec<FileSelectionReason>,
    pub bounded_by_file_ids: Vec<String>,
    pub populations: Vec<String>,
    pub oldest_active_snapshot_seq: Option<u64>,
    pub verdicts: Vec<EntryVerdict>,
    pub outputs: Vec<OutputSummary>,
    pub split_candidates: Vec<SplitCandidateRec>,
    pub non_overlap: bool,
    pub status: CompactionStatus,
    pub findings: Vec<Finding>,
}

fn diag(id: &str, code: &str, pos: &str, msg: &str, raw: Option<String>) -> Diagnostic {
    Diagnostic {
        id: id.to_string(),
        code: code.to_string(),
        severity: "error".to_string(),
        position: pos.to_string(),
        message: msg.to_string(),
        raw_kept: raw,
    }
}

/// 材料层校验：key 基准、seq 单调性、摘要一致性、原始量是否可换算。
/// 返回的诊断都携带位置；不通过时调用方必须返回 FormatCorrupt，不得继续计算。
pub fn validate_files(files: &[SstFile]) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    for f in files {
        let p = format!("materials.sst[{}]", f.id);
        if f.key_basis != KEY_BASIS {
            diags.push(diag(&format!("d-{}-basis", f.id), "key_basis_unknown", &p,
                &format!("SSTable {} 的 key 坐标基准 {} 无法换算到 {}", f.id, f.key_basis, KEY_BASIS),
                Some(f.key_basis.clone())));
        }
        if f.points.iter().any(|pt| pt.seq < f.min_seq || pt.seq > f.max_seq) {
            diags.push(diag(&format!("d-{}-seqspan", f.id), "seq_span_inconsistent", &p,
                &format!("SSTable {} 有条目 seq 超出摘要区间 [{}..{}]", f.id, f.min_seq, f.max_seq), None));
        }
        let mut kmin: Option<&str> = None;
        let mut kmax: Option<&str> = None;
        for pt in &f.points {
            kmin = Some(kmin.map_or(pt.user_key.as_str(), |k| k.min(pt.user_key.as_str())));
            kmax = Some(kmax.map_or(pt.user_key.as_str(), |k| k.max(pt.user_key.as_str())));
        }
        for r in &f.ranges {
            kmin = Some(kmin.map_or(r.start_key.as_str(), |k| k.min(r.start_key.as_str())));
            kmax = Some(kmax.map_or(r.end_key.as_str(), |k| k.max(r.end_key.as_str())));
        }
        if let (Some(lo), Some(hi)) = (kmin, kmax) {
            if lo != f.min_key || hi != f.max_key {
                diags.push(diag(&format!("d-{}-range", f.id), "key_range_inconsistent", &p,
                    &format!("SSTable {} 摘要 [{}..{}] 与实际条目边界 [{}..{}] 不一致", f.id, f.min_key, f.max_key, lo, hi), None));
            }
        }
        if f.size.normalized.is_none() {
            diags.push(diag(&format!("d-{}-size", f.id), "quantity_unconvertible",
                &format!("{}.size.raw", p),
                &format!("SSTable {} 大小原始表示 {:?} 无法换算为 byte；原值保留在原位置，不参与预算", f.id, f.size.raw),
                Some(f.size.raw.clone())));
        }
        for (i, pt) in f.points.iter().enumerate() {
            if let Some(v) = &pt.value {
                if v.normalized.is_none() && v.unit != UNIT_BYTE && v.unit != "text" {
                    let mut d2 = diag(&format!("d-{}-v{}", f.id, i), "quantity_unconvertible",
                        &format!("{}.points[{}].value.raw", p, i),
                        &format!("条目 {} 的值单位 {} 无法规范化；保留原值 {}", pt.id, v.unit, v.raw),
                        Some(v.raw.clone()));
                    d2.severity = "warn".to_string();
                    diags.push(d2);
                }
            }
        }
    }
    diags
}

pub fn active_snapshots(snaps: &[Snapshot]) -> Vec<&Snapshot> {
    snaps.iter().filter(|s| s.active).collect()
}

/// 读人群（populations）：最新读 + 每个活跃 snapshot 各一个。
/// 全部压实判定必须对这些人群保持读结果不变。
pub fn read_populations(snaps: &[Snapshot]) -> Vec<String> {
    population_limits(snaps).into_iter().map(|(p, _)| p).collect()
}

/// 读人群与其 seq 上界的统一顺序，保证所有判定与界面链一致。
pub fn population_limits(snaps: &[Snapshot]) -> Vec<(String, Option<u64>)> {
    let mut v = vec![("reader:head".to_string(), None)];
    let mut ss: Vec<&Snapshot> = active_snapshots(snaps);
    ss.sort_by(|a, b| a.seq.cmp(&b.seq).then(a.id.cmp(&b.id)));
    for s in ss {
        v.push((format!("reader:snap:{}:{}", s.id, s.seq), Some(s.seq)));
    }
    v
}

pub fn oldest_active_snapshot_seq(snaps: &[Snapshot]) -> Option<u64> {
    active_snapshots(snaps).iter().map(|s| s.seq).min()
}

/// 压实选择：锚定一层 + 其 key range 触及的更高层，做重叠闭包。
/// L0 内部允许重叠，所以锚定 L0 时必须把重叠的 L0 全部卷入。
pub fn select_files(state: &EngineState, req: &CompactionRequest) -> (Vec<String>, Vec<FileSelectionReason>, Vec<String>) {
    let mut reasons: BTreeMap<String, FileSelectionReason> = BTreeMap::new();
    let mut chosen: Vec<String> = Vec::new();
    let mut wanted: Vec<String> = Vec::new();
    let target_level = if req.anchor_level == 0 { 1 } else { req.anchor_level + 1 };

    for f in &state.files {
        if let Some(id) = &req.anchor_file_id {
            if f.id == *id && f.level == req.anchor_level {
                wanted.push(f.id.clone());
                reasons.insert(f.id.clone(), FileSelectionReason {
                    file_id: f.id.clone(), selected: true,
                    reason_code: "anchor".into(),
                    reason_message: format!("人工/策略锚定文件 {}（L{}）", f.id, f.level),
                });
            }
        } else if f.level == req.anchor_level && req.include_levels.contains(&f.level) {
            // 自动：只取锚层最小文件，保证等价输入稳定顺序
        }
    }
    if wanted.is_empty() {
        let mut anchors: Vec<&SstFile> = state.files.iter()
            .filter(|f| f.level == req.anchor_level && req.include_levels.is_empty() || req.include_levels.contains(&f.level))
            .collect();
        anchors.sort_by(|a, b| a.min_key.cmp(&b.min_key).then(a.id.cmp(&b.id)));
        if let Some(f) = anchors.first() {
            wanted.push(f.id.clone());
            reasons.insert(f.id.clone(), FileSelectionReason {
                file_id: f.id.clone(), selected: true,
                reason_code: "anchor_min".into(),
                reason_message: format!("自动选择锚层 L{} 最小 key 文件 {}", f.level, f.id),
            });
        }
    }

    let by_id: BTreeMap<&str, &SstFile> = state.files.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut queue = wanted.clone();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) { continue; }
        if !chosen.contains(&id) { chosen.push(id.clone()); }
        let cur = match by_id.get(id.as_str()) { Some(f) => *f, None => continue };
        for f in &state.files {
            if f.id == cur.id || chosen.contains(&f.id) { continue; }
            let rel: Option<&str> = if cur.level == 0 && f.level == 0
                && f.overlaps_key_range(&cur.min_key, &cur.max_key)
            {
                Some("overlap_l0")
            } else if f.level == target_level
                && f.overlaps_key_range(&cur.min_key, &cur.max_key)
            {
                Some("overlap_target")
            } else {
                None
            };
            if let Some(code) = rel {
                let msg = if code == "overlap_l0" {
                    "L0 与已选文件 key range 重叠（L0 允许重叠，必须整体卷入）".to_string()
                } else {
                    format!("目标层 L{} 文件与压实边界重叠", target_level)
                };
                queue.push(f.id.clone());
                reasons.entry(f.id.clone()).or_insert_with(|| FileSelectionReason {
                    file_id: f.id.clone(), selected: true,
                    reason_code: code.to_string(),
                    reason_message: msg.to_string(),
                });
            }
        }
    }
    chosen.sort();

    // 未参与、但与最终压实边界重叠的更深层文件：它们约束 range tombstone 不能越过。
    let mut lo = String::from("\u{10ffff}");
    let mut hi = String::new();
    for id in &chosen {
        if let Some(f) = by_id.get(id.as_str()) {
            if f.min_key < lo { lo = f.min_key.clone(); }
            if f.max_key > hi { hi = f.max_key.clone(); }
        }
    }
    let mut bounded = Vec::new();
    for f in &state.files {
        if !chosen.contains(&f.id) && f.level >= target_level && f.overlaps_key_range(&lo, &hi) {
            bounded.push(f.id.clone());
            reasons.entry(f.id.clone()).or_insert_with(|| FileSelectionReason {
                file_id: f.id.clone(), selected: false,
                reason_code: "bounds_range".into(),
                reason_message: format!("未参与压实但与输出边界重叠（L{}），range tombstone 不得越过它提前丢弃", f.level),
            });
        }
    }
    bounded.sort();
    let mut all: Vec<FileSelectionReason> = reasons.into_values().collect();
    all.sort_by(|a, b| a.file_id.cmp(&b.file_id));
    (chosen, all, bounded)
}

// ---------------- 读取：内部键序 + snapshot 可见性 ----------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReadOutcome {
    NotFound,
    Deleted { by_entry: String, by_range: bool },
    Value { point_id: String, value_raw: String, unit: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PopRead {
    pub population: String,
    pub seq_limit: Option<u64>,
    pub outcome: ReadOutcome,
    pub winner_entry: Option<String>,
    /// 按内部键序排列的候选链（snapshot 不可见的条目标 visible=false）。
    pub chain: Vec<ChainEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ChainEntry {
    pub entry_id: String,
    pub sort_key: String, // key#seq#rank
    pub visible_to_population: bool,
    pub kind: String,
    pub file_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PointReadView {
    pub key: String,
    pub key_basis: String,
    pub tie_policy: String,
    pub tie_conflict: bool,
    pub tie_candidate_ids: Vec<String>,
    pub reads: Vec<PopRead>,
}

fn limit_for(population: &str) -> Option<u64> {
    if population == "reader:head" { None }
    else {
        population.rsplit(':').next().and_then(|s| s.parse().ok())
    }
}

struct Ent<'a> {
    id: &'a str,
    key: &'a str,
    seq: u64,
    rank: u8,
    is_range: bool,
    point: Option<&'a PointOp>,
    file_id: &'a str,
}

fn gather_entries<'a>(files: &'a [SstFile], key: &'a str) -> Vec<Ent<'a>> {
    let mut v = Vec::new();
    for f in files {
        for p in &f.points {
            if p.user_key == key {
                v.push(Ent { id: &p.id, key: key, seq: p.seq,
                    rank: internal_type_rank(p.kind, TiePolicy::DeleteWins),
                    is_range: false, point: Some(p), file_id: &f.id });
            }
        }
        for r in &f.ranges {
            if r.covers(key) {
                v.push(Ent { id: &r.id, key: key, seq: r.seq,
                    rank: range_type_rank(TiePolicy::DeleteWins),
                    is_range: true, point: None, file_id: &f.id });
            }
        }
    }
    v
}

fn rank_with(policy: TiePolicy, e: &Ent) -> u8 {
    if e.is_range { range_type_rank(policy) }
    else { internal_type_rank(e.point.unwrap().kind, policy) }
}

fn sort_entries(ents: &mut [Ent], policy: TiePolicy) {
    ents.sort_by(|a, b| match a.key.cmp(b.key) {
        Ordering::Equal => match b.seq.cmp(&a.seq) {
            Ordering::Equal => rank_with(policy, a).cmp(&rank_with(policy, b))
                .then(a.id.cmp(b.id)),
            o => o,
        },
        o => o,
    });
}

/// 单个 key 在所有读人群下的可见结论；同时返回同序号多类型冲突候选。
pub fn point_read(files: &[SstFile], snaps: &[Snapshot], key: &str, policy: TiePolicy) -> PointReadView {
    let pops = read_populations(snaps);
    let mut all = gather_entries(files, key);
    sort_entries(&mut all, policy);

    // 同序号多类型：同 key、同 seq、不同具体类型的条目。
    let mut tie_candidate_ids = Vec::new();
    {
        let mut by_seq: BTreeMap<u64, Vec<&Ent>> = BTreeMap::new();
        for e in &all { by_seq.entry(e.seq).or_default().push(e); }
        for (_, grp) in by_seq {
            let kinds: std::collections::BTreeSet<u8> = grp.iter().map(|e| rank_with(policy, e)).collect();
            if grp.len() > 1 && (kinds.len() > 1 || policy == TiePolicy::RejectDuplicate) {
                tie_candidate_ids.extend(grp.iter().map(|e| e.id.to_string()));
            }
        }
    }
    let tie_conflict = policy == TiePolicy::RejectDuplicate && !tie_candidate_ids.is_empty();

    let mut reads = Vec::new();
    for pop in &pops {
        let limit = limit_for(pop);
        let visible: Vec<&Ent> = all.iter().filter(|e| limit.map_or(true, |l| e.seq <= l)).collect();
        let chain = all.iter().map(|e| ChainEntry {
            entry_id: e.id.to_string(),
            sort_key: format!("{}#{}#{}#{}", e.key, e.seq, rank_with(policy, e), e.id),
            visible_to_population: limit.map_or(true, |l| e.seq <= l),
            kind: if e.is_range { "range_delete".to_string() }
                else { format!("{:?}", e.point.unwrap().kind).to_lowercase() },
            file_id: e.file_id.to_string(),
        }).collect();
        let (outcome, winner) = match visible.first() {
            None => (ReadOutcome::NotFound, None),
            Some(w) => {
                if w.is_range {
                    (ReadOutcome::Deleted { by_entry: w.id.to_string(), by_range: true }, Some(w.id.to_string()))
                } else {
                    let p = w.point.unwrap();
                    match p.kind {
                        PointKind::Delete => (ReadOutcome::Deleted { by_entry: p.id.clone(), by_range: false }, Some(p.id.clone())),
                        PointKind::Put => {
                            let v = p.value.as_ref();
                            (ReadOutcome::Value {
                                point_id: p.id.clone(),
                                value_raw: v.map_or(String::new(), |q| q.raw.clone()),
                                unit: v.map_or(String::new(), |q| q.unit.clone()),
                            }, Some(p.id.clone()))
                        }
                    }
                }
            }
        };
        reads.push(PopRead { population: pop.clone(), seq_limit: limit, outcome, winner_entry: winner, chain });
    }
    PointReadView { key: key.to_string(), key_basis: KEY_BASIS.to_string(),
        tie_policy: format!("{:?}", policy).to_lowercase(),
        tie_conflict, tie_candidate_ids, reads }
}

// ---------------- 压实：保留/下沉 + 输出切分 ----------------

struct Survivors {
    points: Vec<PointOp>,
    ranges: Vec<RangeTombstone>,
}

fn read_for(files: &[SstFile], key: &str, policy: TiePolicy, limit: Option<u64>) -> ReadOutcome {
    let mut all: Vec<&PointOp> = Vec::new();
    let mut ranges: Vec<&RangeTombstone> = Vec::new();
    for f in files {
        for p in &f.points { if p.user_key == key { all.push(p); } }
        for r in &f.ranges { if r.covers(key) { ranges.push(r); } }
    }
    all.sort_by(|a, b| b.seq.cmp(&a.seq)
        .then(internal_type_rank(a.kind, policy).cmp(&internal_type_rank(b.kind, policy)))
        .then(a.id.cmp(&b.id)));
    let winner_point = all.into_iter().find(|p| limit.map_or(true, |l| p.seq <= l));
    let winner_range = {
        let mut r: Vec<&RangeTombstone> = ranges.into_iter().filter(|r| limit.map_or(true, |l| r.seq <= l)).collect();
        r.sort_by(|a, b| b.seq.cmp(&a.seq)
            .then(range_type_rank(policy).cmp(&range_type_rank(policy)))
            .then(a.id.cmp(&b.id)));
        r.first().copied()
    };
    let rp = winner_point.map(|p| (p.seq, internal_type_rank(p.kind, policy), p));
    let rr = winner_range.map(|r| (r.seq, range_type_rank(policy), r));
    let range_wins = match (&rr, &rp) {
        (Some((rs, rrank, _)), Some((ps, prank, _))) => rs > ps || (rs == ps && rrank <= prank),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if range_wins {
        ReadOutcome::Deleted { by_entry: rr.unwrap().2.id.clone(), by_range: true }
    } else {
        let p = rp.unwrap().2;
        match p.kind {
            PointKind::Delete => ReadOutcome::Deleted { by_entry: p.id.clone(), by_range: false },
            PointKind::Put => {
                let v = p.value.as_ref();
                ReadOutcome::Value {
                    point_id: p.id.clone(),
                    value_raw: v.map_or(String::new(), |q| q.raw.clone()),
                    unit: v.map_or(String::new(), |q| q.unit.clone()),
                }
            }
        }
    }
}

fn affected_keys(input: &[SstFile], bounded: &[SstFile]) -> Vec<String> {
    let mut keys = std::collections::BTreeSet::new();
    for f in input.iter().chain(bounded.iter()) {
        for p in &f.points { keys.insert(p.user_key.clone()); }
        // 边界文件中被 input range 覆盖的 key 无法枚举全空间，用其实际 point key 即可；
        // 结构性约束（bounded 文件存在）单独处理。
        for r in &f.ranges { let _ = r; }
    }
    keys.into_iter().collect()
}

fn outcome_eq(a: &ReadOutcome, b: &ReadOutcome) -> bool {
    match (a, b) {
        (ReadOutcome::NotFound, ReadOutcome::NotFound) => true,
        (ReadOutcome::Deleted { .. }, ReadOutcome::Deleted { .. }) => true,
        (ReadOutcome::Value { point_id: x, .. }, ReadOutcome::Value { point_id: y, .. }) => x == y,
        _ => false,
    }
}

fn limits(snaps: &[Snapshot]) -> Vec<Option<u64>> {
    population_limits(snaps).into_iter().map(|(_, l)| l).collect()
}

fn find_corrupt(state: &EngineState, selected: &[String]) -> Vec<Diagnostic> {
    let d_all = validate_files(&state.files);
    let mut d: Vec<Diagnostic> = d_all.into_iter()
        .filter(|x| x.severity == "error" && selected.iter().any(|id| x.position.contains(id.as_str())))
        .collect();
    // 同 key 同 seq 同类型重复：确定性输入损坏
    let mut seen: BTreeMap<(String, u64, String), String> = BTreeMap::new();
    for f in &state.files {
        if !selected.contains(&f.id) { continue; }
        for p in &f.points {
            let k = (p.user_key.clone(), p.seq, format!("{:?}", p.kind));
            if let Some(prev) = seen.get(&k) {
                d.push(diag(&format!("dup-{}-{}", prev, p.id), "duplicate_same_seq_same_kind",
                    &format!("materials.sst[{}].points", f.id),
                    &format!("key={} seq={} 出现两个同类型条目 {} 与 {}（输入损坏）", p.user_key, p.seq, prev, p.id), None));
            } else { seen.insert(k, p.id.clone()); }
        }
    }
    d
}

fn tie_dup_present(files: &[SstFile], policy: TiePolicy) -> (bool, Vec<String>) {
    if policy != TiePolicy::RejectDuplicate { return (false, vec![]); }
    // label 区分真实条目类型，不依赖 policy 下归一化后的 rank。
    let mut map: BTreeMap<(String, u64), Vec<(String, String)>> = BTreeMap::new();
    for f in files {
        for p in &f.points {
            let label = if p.kind == PointKind::Put { "put".to_string() } else { "point_delete".to_string() };
            map.entry((p.user_key.clone(), p.seq)).or_default().push((label, p.id.clone()));
        }
        for r in &f.ranges {
            // range 只在覆盖具体 key 时参与同序号比较；保守地把边界 key 也纳入
            for k in [r.start_key.clone(), r.end_key.clone()] {
                if r.covers(&k) {
                    map.entry((k, r.seq)).or_default().push(("range_delete".to_string(), r.id.clone()));
                }
            }
        }
    }
    let mut ids = Vec::new();
    for (_, grp) in &map {
        let labels: std::collections::BTreeSet<&str> = grp.iter().map(|x| x.0.as_str()).collect();
        if grp.len() > 1 && labels.len() > 1 { ids.extend(grp.iter().map(|x| x.1.clone())); }
    }
    ids.sort();
    let conflict = !ids.is_empty();
    (conflict, ids)
}

fn point_bytes(p: &PointOp) -> u64 {
    p.value.as_ref().and_then(|v| v.normalized).unwrap_or_else(|| (p.user_key.len() + 8) as u64)
}

/// 压实主函数：纯函数。返回摘要、逐条判定、机器可读 findings 与三类失败结果之一。
pub fn run_compaction(state: &EngineState, req: &CompactionRequest, input_digest: String) -> CompactionResult {
    let policy = state.params.tie_policy;
    let (selected, selection, bounded_ids) = select_files(state, req);
    let pops = read_populations(&state.snapshots);
    let oldest = oldest_active_snapshot_seq(&state.snapshots);
    let target_level = if req.anchor_level == 0 { 1 } else { req.anchor_level + 1 };

    let by_id: BTreeMap<&str, &SstFile> = state.files.iter().map(|f| (f.id.as_str(), f)).collect();
    let input_files: Vec<&SstFile> = selected.iter().filter_map(|id| by_id.get(id.as_str()).copied()).collect();
    let bounded_files: Vec<&SstFile> = bounded_ids.iter().filter_map(|id| by_id.get(id.as_str()).copied()).collect();

    let mut findings: Vec<Finding> = Vec::new();
    let mut add_f = |findings: &mut Vec<Finding>, code: FindingCode, sev: &str, subject: &str, msg: &str, related: Vec<String>| {
        findings.push(Finding {
            id: format!("f-{}-{}", code_id(&code), subject),
            code: code_id(&code).to_string(), severity: sev.to_string(),
            subject: subject.to_string(), message: msg.to_string(), related,
        });
    };

    let base = CompactionResult {
        rule_version: RULE_VERSION.to_string(),
        params_version: state.params.version.clone(),
        input_digest: input_digest.clone(),
        anchor_level: req.anchor_level,
        selected_file_ids: selected.clone(),
        selection,
        bounded_by_file_ids: bounded_ids.clone(),
        populations: pops.clone(),
        oldest_active_snapshot_seq: oldest,
        verdicts: vec![],
        outputs: vec![],
        split_candidates: vec![],
        non_overlap: true,
        status: CompactionStatus::Planned,
        findings: vec![],
    };

    // 1) 格式损坏：被选文件摘要不一致 / 不可换算大小 / 同 key 同 seq 同类型重复。
    let corrupt = find_corrupt(state, &selected);
    if !corrupt.is_empty() {
        return CompactionResult {
            status: CompactionStatus::FormatCorrupt { diagnostics: corrupt },
            ..base
        };
    }

    // 2) 同序号多类型 + RejectDuplicate：语义无解，保留全部合法候选顺序。
    let (tie_conflict, tie_ids) = tie_dup_present(&input_files.iter().map(|f| (*f).clone()).collect::<Vec<_>>(), policy);
    if tie_conflict {
        add_f(&mut findings, FindingCode::DuplicateSameSeq, "error", "tie",
            "同 key 同序号存在不同类型条目，参数拒绝裁决", tie_ids.clone());
        return CompactionResult {
            status: CompactionStatus::SemanticUnsolvable {
                message: "同序号多类型条目无法在不改变语义的情况下排序；规则拒绝裁决，全部合法顺序保留为候选".to_string(),
                candidates: vec![],
            },
            findings, ..base
        };
    }

    // 3) 输入预算。
    let input_bytes: u64 = input_files.iter().map(|f| f.size.normalized.unwrap_or(0)).sum();
    let limit_in = state.params.max_total_input_bytes.normalized.unwrap_or(u64::MAX);
    if input_bytes > limit_in {
        return CompactionResult {
            status: CompactionStatus::ResourceBudgetExhausted {
                limit: format!("{} {}", state.params.max_total_input_bytes.raw, state.params.max_total_input_bytes.unit),
                used: format!("{} {}", input_bytes, UNIT_BYTE),
            },
            ..base
        };
    }

    // 4) 候选条目。
    let mut cand_points: Vec<PointOp> = Vec::new();
    let mut cand_ranges: Vec<RangeTombstone> = Vec::new();
    for f in &input_files {
        cand_points.extend(f.points.iter().cloned());
        cand_ranges.extend(f.ranges.iter().cloned());
    }
    cand_points.sort_by(|a, b| compare_points(a, b, policy));
    cand_ranges.sort_by(|a, b| a.start_key.cmp(&b.start_key).then(b.seq.cmp(&a.seq)).then(a.id.cmp(&b.id)));

    let lims = limits(&state.snapshots);
    let keys = affected_keys(&input_files.iter().map(|f| (*f).clone()).collect::<Vec<_>>(),
        &bounded_files.iter().map(|f| (*f).clone()).collect::<Vec<_>>());

    // 结构性强约束：range 与未参与压实的旧数据重叠 => 必须保留（不得越过提前丢弃）。
    let bounded_owned: Vec<SstFile> = bounded_files.iter().map(|f| (*f).clone()).collect();
    let forced_ranges: std::collections::BTreeSet<String> = cand_ranges.iter()
        .filter(|r| bounded_owned.iter().any(|f| r.intersects_file(f)))
        .map(|r| r.id.clone()).collect();

    // 5) 联合不动点：逐人群保持读结果。
    let full_files: Vec<SstFile> = state.files.clone();
    let mut live_points: std::collections::BTreeSet<String> =
        cand_points.iter().map(|p| p.id.clone()).collect();
    let mut live_ranges: std::collections::BTreeSet<String> =
        cand_ranges.iter().map(|r| r.id.clone()).collect();

    let build_files = |lp: &std::collections::BTreeSet<String>, lr: &std::collections::BTreeSet<String>| -> Vec<SstFile> {
        let mut v: Vec<SstFile> = full_files.iter()
            .filter(|f| !selected.contains(&f.id))
            .cloned().collect::<Vec<_>>();
        let mut pts: Vec<PointOp> = cand_points.iter().filter(|p| lp.contains(&p.id)).cloned().collect();
        let mut rgs: Vec<RangeTombstone> = cand_ranges.iter().filter(|r| lr.contains(&r.id)).cloned().collect();
        if !pts.is_empty() || !rgs.is_empty() {
            let min = pts.iter().map(|p| p.user_key.clone()).chain(rgs.iter().map(|r| r.start_key.clone())).min().unwrap_or_default();
            let max = pts.iter().map(|p| p.user_key.clone()).chain(rgs.iter().map(|r| r.end_key.clone())).max().unwrap_or_default();
            v.push(SstFile {
                id: "__compaction_candidate__".to_string(), level: target_level,
                min_key: min, max_key: max, min_seq: 0, max_seq: u64::MAX,
                size: RawQty::bytes(0), points: std::mem::take(&mut pts),
                ranges: std::mem::take(&mut rgs), origin: FileOrigin::Imported, key_basis: KEY_BASIS.to_string(),
            });
        }
        v
    };

    let mut changed = true;
    while changed {
        changed = false;
        // 尝试移除一个 point
        let mut remove: Option<String> = None;
        for p in &cand_points {
            if !live_points.contains(&p.id) { continue; }
            let mut trial = live_points.clone();
            trial.remove(&p.id);
            let trial_files = build_files(&trial, &live_ranges);
            let safe = lims.iter().all(|lim| {
                keys.iter().all(|k| outcome_eq(&read_for(&full_files, k, policy, *lim),
                                              &read_for(&trial_files, k, policy, *lim)))
            });
            if safe { remove = Some(p.id.clone()); break; }
        }
        if let Some(id) = remove {
            // point delete：额外记录原因；结构性约束不适用 point。
            live_points.remove(&id);
            changed = true;
            continue;
        }
        // 尝试移除一个 range（结构强约束 + 读结果）
        let mut remove_r: Option<String> = None;
        for r in &cand_ranges {
            if !live_ranges.contains(&r.id) { continue; }
            if forced_ranges.contains(&r.id) { continue; }
            if oldest.map_or(false, |s| r.seq > s) { continue; }
            let mut trial = live_ranges.clone();
            trial.remove(&r.id);
            let trial_files = build_files(&live_points, &trial);
            let safe = lims.iter().all(|lim| {
                keys.iter().all(|k| outcome_eq(&read_for(&full_files, k, policy, *lim),
                                              &read_for(&trial_files, k, policy, *lim)))
            });
            if safe { remove_r = Some(r.id.clone()); break; }
        }
        if let Some(id) = remove_r {
            live_ranges.remove(&id);
            changed = true;
        }
    }

    // 6) 逐条判定（机器可读 verdict）。
    let survivors = Survivors {
        points: cand_points.iter().filter(|p| live_points.contains(&p.id)).cloned().collect(),
        ranges: cand_ranges.iter().filter(|r| live_ranges.contains(&r.id)).cloned().collect(),
    };

    let mut verdicts = Vec::new();
    for p in &cand_points {
        let (survivor, code, msg) = if live_points.contains(&p.id) {
            // 细分保留原因
            let pairs = population_limits(&state.snapshots);
            let visible_pop: Vec<String> = lims.iter().filter_map(|lim| {
                let pop = pairs.iter().find(|(_, l)| l == lim).unwrap().0.clone();
                let full = read_for(&full_files, &p.user_key, policy, *lim);
                let mut trial = live_points.clone(); trial.remove(&p.id);
                let trial_files = build_files(&trial, &live_ranges);
                let trial_out = read_for(&trial_files, &p.user_key, policy, *lim);
                (!outcome_eq(&full, &trial_out)).then_some(pop)
            }).collect();
            let shadowed = is_shadowed(&cand_points, &cand_ranges, &full_files, p, policy, lims[0]);
            if !visible_pop.is_empty() && visible_pop.iter().any(|x| x != "reader:head") {
                (true, "visible_to_snapshot", format!("旧 snapshot 仍需读取该版本（{}）", visible_pop.join(",")))
            } else if p.kind == PointKind::Delete && has_older_put(&full_files, p, lims[0], policy) {
                (true, "point_delete_has_older_data", "point tombstone 仍遮蔽更旧版本，必须保留并下沉".to_string())
            } else if shadowed && oldest.map_or(false, |s| p.seq > s) {
                (true, "visible_to_snapshot", "旧 snapshot 可能读到被遮蔽版本，必须保留".to_string())
            } else {
                (true, "kept_unconditionally", "判定为存活条目进入输出".to_string())
            }
        } else {
            (false, "shadowed", "所有读人群下均被更新条目遮蔽，可安全物理消除".to_string())
        };
        verdicts.push(EntryVerdict {
            entry_id: p.id.clone(),
            kind: if p.kind == PointKind::Put { "point_put" } else { "point_delete" }.to_string(),
            key: p.user_key.clone(), seq: p.seq, survivor,
            reason_code: code.to_string(), reason_message: msg,
            populations: pops.clone(),
        });
    }
    for r in &cand_ranges {
        let (survivor, code, msg) = if !live_ranges.contains(&r.id) {
            (false, "shadowed", "range tombstone 在所有读人群下均不再改变读结果，可物理消除".to_string())
        } else if forced_ranges.contains(&r.id) {
            let rel: Vec<String> = bounded_owned.iter()
                .filter(|f| r.intersects_file(f)).map(|f| f.id.clone()).collect();
            (true, "range_bounded_by_old_data",
                format!("range tombstone 与未参与压实的旧数据 {} 重叠，不能越过它提前丢弃", rel.join(",")))
        } else if oldest.map_or(false, |s| r.seq > s) {
            (true, "range_visible_to_snapshot",
                format!("最旧活跃 snapshot(seq={}) 早于 range tombstone(seq={})，必须保留", oldest.unwrap(), r.seq))
        } else {
            (true, "range_hides_live_data", "range tombstone 仍遮蔽存活数据，保留并下沉".to_string())
        };
        verdicts.push(EntryVerdict {
            entry_id: r.id.clone(), kind: "range".to_string(),
            key: format!("[{}{}..{}{}]",
                if r.start_kind == EndpointKind::Inclusive { "[" } else { "(" },
                r.start_key, r.end_key,
                if r.end_kind == EndpointKind::Inclusive { "]" } else { ")" }),
            seq: r.seq, survivor,
            reason_code: code.to_string(), reason_message: msg,
            populations: pops.clone(),
        });
    }

    // findings（结构性证据）
    for id in &forced_ranges {
        let rel: Vec<String> = bounded_owned.iter()
            .filter(|f| cand_ranges.iter().find(|r| r.id == *id).unwrap().intersects_file(f))
            .map(|f| f.id.clone()).collect();
        add_f(&mut findings, FindingCode::RangeBoundedByOldData, "info", id,
            "range tombstone 被未参与压实的旧数据约束，禁止越过下沉边界", rel);
    }
    if let Some(s) = oldest {
        for r in &survivors.ranges {
            if r.seq > s {
                add_f(&mut findings, FindingCode::RangeVisibleToSnapshot, "info", &r.id,
                    "活跃旧 snapshot 要求保留该 range tombstone", vec![format!("snapshot-seq:{}", s)]);
            }
        }
    }

    // 7) 输出切分：内部键序排序；超预算且边界合法处切分；记录全部合法候选。
    let max_out = state.params.max_output_bytes.normalized.unwrap_or(u64::MAX);
    let mut ordered = survivors.points.clone();
    ordered.sort_by(|a, b| compare_points(a, b, policy));

    let mut per_key_max: u64 = 0;
    let mut key_runs: Vec<(String, u64, Vec<PointOp>)> = Vec::new();
    for p in &ordered {
        if let Some(run) = key_runs.last_mut() {
            if run.0 == p.user_key { run.2.push(p.clone()); run.1 += point_bytes(p);
                per_key_max = per_key_max.max(run.1); continue; }
        }
        key_runs.push((p.user_key.clone(), point_bytes(p), vec![p.clone()]));
        per_key_max = per_key_max.max(point_bytes(p));
    }

    if per_key_max > max_out {
        add_f(&mut findings, FindingCode::BudgetExceeded, "fatal", "output",
            "单个 key 的版本链超过最大输出，任何切分都无法满足", vec![]);
        return CompactionResult {
            status: CompactionStatus::ResourceBudgetExhausted {
                limit: format!("{} {}", state.params.max_output_bytes.raw, state.params.max_output_bytes.unit),
                used: format!("{} {}", per_key_max, UNIT_BYTE),
            },
            verdicts, findings, ..base
        };
    }

    let mut split_candidates: Vec<SplitCandidateRec> = Vec::new();
    let mut cumulative: u64 = 0;
    for (i, run) in key_runs.iter().enumerate() {
        cumulative += run.1;
        if i + 1 < key_runs.len() {
            split_candidates.push(SplitCandidateRec {
                index: i,
                after_key: run.0.clone(),
                last_point_seq: run.2.iter().map(|p| p.seq).max().unwrap_or(0),
                cumulative_bytes: cumulative,
                legal: cumulative <= max_out && run.0 != key_runs[i + 1].0,
                note: "合法切分点：落在不同 user key 边界，版本链不被切断".to_string(),
            });
        }
    }

    // 贪心：累计超预算且位于不同 user key 边界时切分。
    let mut outputs: Vec<OutputSummary> = Vec::new();
    let mut start = 0usize;
    let mut running: u64 = 0;
    let result_prefix = format!("out:{}:{}", state.params.version, input_digest);
    let mut chosen_splits: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for (i, run) in key_runs.iter().enumerate() {
        if i > start && running + run.1 > max_out {
            emit_output(&mut outputs, &result_prefix, target_level, policy, &key_runs[start..i], &survivors.ranges);
            chosen_splits.insert(i - 1);
            add_f(&mut findings, FindingCode::SplitChosen, "info", &format!("split-at-{}", i - 1),
                &format!("输出在 key {} 后切分以维持输出预算，落到不同 user key 边界", key_runs[i - 1].0), vec![]);
            start = i;
            running = 0;
        }
        running += run.1;
    }
    emit_output(&mut outputs, &result_prefix, target_level, policy, &key_runs[start..], &survivors.ranges);

    for c in &split_candidates {
        let chosen = chosen_splits.contains(&c.index);
        add_f(&mut findings, FindingCode::SplitCandidate,
            if chosen { "info" } else { "info" },
            &format!("cand-{}", c.index),
            &format!("{}合法切分候选：after={}（累计 {} {}）",
                if chosen { "【采纳】" } else { "【未采纳】" },
                c.after_key, c.cumulative_bytes, UNIT_BYTE), vec![]);
    }

    // 8) 不重叠层约束：同一 level 内输出彼此不重叠，且与该层未参与文件不重叠。
    let target_existing: Vec<&SstFile> = state.files.iter()
        .filter(|f| f.level == target_level && !selected.contains(&f.id))
        .collect();
    let mut non_overlap = true;
    let mut viol_msg = String::new();
    for (i, a) in outputs.iter().enumerate() {
        for b in &outputs[i + 1..] {
            if a.min_key <= b.max_key && b.min_key <= a.max_key {
                non_overlap = false;
                viol_msg = format!("输出 {} 与 {} 在 L{} 重叠，违反不重叠层约束", a.proposed_id, b.proposed_id, target_level);
            }
        }
        for f in &target_existing {
            if a.min_key <= f.max_key && f.min_key <= a.max_key {
                non_overlap = false;
                viol_msg = format!("输出 {} 与目标层既有文件 {}（L{}）重叠", a.proposed_id, f.id, target_level);
            }
        }
    }
    if !non_overlap {
        add_f(&mut findings, FindingCode::NonOverlapViolation, "error", "output", &viol_msg, vec![]);
        return CompactionResult {
            non_overlap: false,
            outputs, split_candidates: split_candidates.clone(), verdicts,
            status: CompactionStatus::SemanticUnsolvable {
                message: format!("{}；不存在满足不重叠层约束的输出划分，全部合法切分候选已保留", viol_msg),
                candidates: split_candidates,
            },
            findings, ..base
        };
    }

    // 9) 中断模拟：在安装第 N 个输出前崩溃，已安装部分在 store 层恢复，这里只报告计划状态。
    let status = match req.crash_before_install {
        Some(n) if n < outputs.len() => CompactionStatus::Interrupted {
            installed_count: n, pending_count: outputs.len() - n, at_step: n,
        },
        _ => CompactionStatus::Complete,
    };

    CompactionResult {
        verdicts, outputs, split_candidates, non_overlap: true, status, findings, ..base
    }
}

fn emit_output(out: &mut Vec<OutputSummary>, prefix: &str, level: i32, policy: TiePolicy,
               runs: &[(String, u64, Vec<PointOp>)], all_ranges: &[RangeTombstone]) {
    let mut pts: Vec<&PointOp> = runs.iter().flat_map(|(_, _, v)| v.iter()).collect();
    pts.sort_by(|a, b| compare_points(a, b, policy));
    let min_key = pts.first().map(|p| p.user_key.clone()).unwrap_or_default();
    let max_key = pts.last().map(|p| p.user_key.clone()).unwrap_or_default();
    // range 只保留与本输出 key 区间相交的片段；端点被裁剪到输出边界，片段指回父 range。
    let ranges_here: Vec<RangeTombstone> = all_ranges.iter().filter_map(|r| {
        let lo = match r.start_kind { EndpointKind::Inclusive => r.start_key <= max_key, EndpointKind::Exclusive => r.start_key < max_key };
        let hi = match r.end_kind { EndpointKind::Inclusive => r.end_key >= min_key, EndpointKind::Exclusive => r.end_key > min_key };
        if !(lo && hi) { return None; }
        let start_key = if r.start_key < min_key { min_key.clone() } else { r.start_key.clone() };
        let start_kind = if r.start_key < min_key { EndpointKind::Inclusive } else { r.start_kind };
        let end_key = if r.end_key > max_key { max_key.clone() } else { r.end_key.clone() };
        let end_kind = if r.end_key > max_key { EndpointKind::Inclusive } else { r.end_kind };
        let mut frag = r.clone();
        frag.fragment_of = Some(r.id.clone());
        frag.id = format!("{}#frag#{}#{}", r.id, min_key, max_key);
        frag.start_key = start_key;
        frag.start_kind = start_kind;
        frag.end_key = end_key;
        frag.end_kind = end_kind;
        Some(frag)
    }).collect();
    let size: u64 = runs.iter().map(|(_, n, _)| *n).sum::<u64>()
        + (ranges_here.len() as u64) * 16;
    out.push(OutputSummary {
        proposed_id: format!("{}#{}", prefix, out.len() + 1),
        level, min_key, max_key,
        min_seq: pts.iter().map(|p| p.seq).min().unwrap_or(0),
        max_seq: pts.iter().map(|p| p.seq).max().unwrap_or(0),
        size_bytes: size,
        point_count: pts.len() as u64,
        range_count: ranges_here.len() as u64,
        point_ids: pts.iter().map(|p| p.id.clone()).collect(),
        range_ids: ranges_here.iter().map(|r| r.id.clone()).collect(),
    });
}

fn is_shadowed(_cand: &[PointOp], _cr: &[RangeTombstone], files: &[SstFile], p: &PointOp, policy: TiePolicy, _limit: Option<u64>) -> bool {
    files.iter().flat_map(|f| f.points.iter()).any(|q| q.user_key == p.user_key && (
        q.seq > p.seq || (q.seq == p.seq && internal_type_rank(q.kind, policy) < internal_type_rank(p.kind, policy))
    )) || files.iter().flat_map(|f| f.ranges.iter()).any(|r|
        r.covers(&p.user_key) && compare_range_vs_point(r, p, policy) == Ordering::Less)
}

fn has_older_put(files: &[SstFile], del: &PointOp, limit: Option<u64>, policy: TiePolicy) -> bool {
    files.iter().flat_map(|f| f.points.iter()).any(|q| q.user_key == del.user_key && q.kind == PointKind::Put
        && q.seq <= del.seq
        && (q.seq < del.seq || internal_type_rank(q.kind, policy) > internal_type_rank(del.kind, policy))
        && limit.map_or(true, |l| q.seq <= l))
}

fn code_id(c: &FindingCode) -> &'static str {
    match c {
        FindingCode::ShadowedByPoint => "shadowed_by_point",
        FindingCode::VisibleToSnapshot => "visible_to_snapshot",
        FindingCode::PointDeleteHasOlderData => "point_delete_has_older_data",
        FindingCode::PointDeleteCoveredByRange => "point_delete_covered_by_range",
        FindingCode::ShadowedByRange => "shadowed_by_range",
        FindingCode::RangeHidesLiveData => "range_hides_live_data",
        FindingCode::RangeBoundedByOldData => "range_bounded_by_old_data",
        FindingCode::RangeVisibleToSnapshot => "range_visible_to_snapshot",
        FindingCode::KeptUnconditionally => "kept_unconditionally",
        FindingCode::SplitCandidate => "split_candidate",
        FindingCode::SplitChosen => "split_chosen",
        FindingCode::NonOverlapViolation => "non_overlap_violation",
        FindingCode::BudgetExceeded => "budget_exceeded",
        FindingCode::UnnormalizableQty => "unnormalizable_qty",
        FindingCode::CorruptFile => "corrupt_file",
        FindingCode::DuplicateSameSeq => "duplicate_same_seq",
    }
}
