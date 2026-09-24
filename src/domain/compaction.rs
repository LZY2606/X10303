//! LSM 压实语义内核: 内部键顺序、snapshot 可见性、point/range tombstone 的
//! 保留与下沉、输出切分与非重叠层约束。
//!
//! 结果分四类, 互不混淆:
//! - 成功 (可能携带歧义候选)
//! - 资源预算耗尽 [`CompactionError::BudgetExhausted`]
//! - 格式损坏 [`CompactionError::Corrupted`]
//! - 语义无解 [`CompactionError::SemanticUnsolvable`]

use crate::domain::model::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionError {
    /// 资源预算耗尽 (例如输出条目超过上限)。
    BudgetExhausted { budget: usize, needed: usize, path: String },
    /// 输入格式损坏 (键区间倒置、条目越界等)。
    Corrupted { code: String, path: String, message: String },
    /// 语义无解: 存在多个合法候选且缺少裁决, 或存在冲突的人工决定。
    SemanticUnsolvable { code: String, path: String, message: String, candidates: Vec<String> },
}

impl CompactionError {
    pub fn code(&self) -> &'static str {
        match self {
            CompactionError::BudgetExhausted { .. } => "budget-exhausted",
            CompactionError::Corrupted { .. } => "corrupted-input",
            CompactionError::SemanticUnsolvable { .. } => "semantic-unsolvable",
        }
    }
    pub fn path(&self) -> &str {
        match self {
            CompactionError::BudgetExhausted { path, .. } => path,
            CompactionError::Corrupted { path, .. } => path,
            CompactionError::SemanticUnsolvable { path, .. } => path,
        }
    }
    pub fn to_diagnostic(&self) -> Diagnostic {
        Diagnostic {
            code: self.code().to_string(),
            severity: DiagSeverity::Error,
            path: self.path().to_string(),
            message: self.message_str(),
        }
    }
    fn message_str(&self) -> String {
        match self {
            CompactionError::BudgetExhausted { budget, needed, .. } => {
                format!("资源预算耗尽: 预算 {budget} 条, 实际需要 {needed} 条")
            }
            CompactionError::Corrupted { message, .. } => message.clone(),
            CompactionError::SemanticUnsolvable { message, .. } => message.clone(),
        }
    }
}

/// 机器可读取的单条判定依据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Judgment {
    pub id: String,
    pub subject: String,
    pub rule: String,
    pub basis: String,
    pub decision: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub witnesses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeptEntry {
    pub origin_sst: String,
    pub origin_entry: String,
    pub seq: u64,
    pub key: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputRangeDel {
    pub origin_range_id: String,
    pub seq: u64,
    pub start_key: String,
    pub start_inclusive: bool,
    pub end_key: String,
    pub end_inclusive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputFile {
    pub candidate_id: String,
    pub proposed_file: String,
    pub level: u8,
    pub key_min: String,
    pub key_max: String,
    pub entries: Vec<KeptEntry>,
    pub range_dels: Vec<OutputRangeDel>,
    /// 与未参与压实的旧数据的重叠违背 (非重叠层约束)。
    #[serde(default)]
    pub overlaps_with: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionPlan {
    pub scenario: String,
    pub rule_version: String,
    pub input_ssts: Vec<String>,
    pub target_level: u8,
    pub selected_by: String,
    pub outputs: Vec<OutputFile>,
    pub judgments: Vec<Judgment>,
    /// 全部合法候选方案 (输出切分/同序号多类型无法唯一决定时)。
    pub legal_candidates: Vec<PlanCandidate>,
    pub dropped_entries: Vec<String>,
    pub dropped_range_dels: Vec<String>,
    pub preserved_below: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanCandidate {
    pub id: String,
    pub description: String,
    pub output_count: usize,
    pub split_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareOptions {
    pub scenario: String,
    pub target_level: u8,
    pub input_ssts: Vec<String>,
    pub seed_levels: Vec<u8>,
    pub output_split_keys: Vec<String>,
    pub output_entry_budget: Option<usize>,
    pub manual_candidate_pick: Option<String>,
}

fn err_corrupt(code: &str, path: &str, message: impl Into<String>) -> CompactionError {
    CompactionError::Corrupted { code: code.to_string(), path: path.to_string(), message: message.into() }
}

fn key_in_range(key: &str, lo: &str, hi: &str) -> bool {
    key.as_bytes() >= lo.as_bytes() && key.as_bytes() <= hi.as_bytes()
}

/// 半开/半闭端点区间是否包含某 user key (端点含斥属于坐标基准)。
pub fn interval_covers(start: &str, start_inc: bool, end: &str, end_inc: bool, key: &str) -> bool {
    let b = key.as_bytes();
    let after_start = if start_inc { b >= start.as_bytes() } else { b > start.as_bytes() };
    let before_end = if end_inc { b <= end.as_bytes() } else { b < end.as_bytes() };
    after_start && before_end
}

fn intervals_overlap(a: (&str, bool, &str, bool), b: (&str, bool, &str, bool)) -> bool {
    let (al, ai, ah, ahi) = a;
    let (bl, bi, bh, bhi) = b;
    let left = if ai && bi { al.as_bytes() <= bh.as_bytes() } else { al.as_bytes() < bh.as_bytes() };
    let right = if ahi && bhi { bl.as_bytes() <= ah.as_bytes() } else { bl.as_bytes() < ah.as_bytes() };
    left && right
}

/// 输入材料的结构校验: 键区间、条目归属、端点合法性、身份重复。
pub fn validate_materials(m: &Materials) -> Result<(), CompactionError> {
    let mut seen = std::collections::HashSet::new();
    for (si, sst) in m.sstables.iter().enumerate() {
        if !seen.insert(sst.id.clone()) {
            return Err(err_corrupt("duplicate-sst-id", "/sstables", format!("SSTable id 重复: {}", sst.id)));
        }
        if sst.key_min.as_bytes() > sst.key_max.as_bytes() {
            return Err(err_corrupt(
                "inverted-key-range",
                &format!("/sstables/{si}/key_max"),
                format!("{} 键区间倒置: {} > {}", sst.id, sst.key_min, sst.key_max),
            ));
        }
        for (ei, e) in sst.entries.iter().enumerate() {
            if !key_in_range(&e.key, &sst.key_min, &sst.key_max) {
                return Err(err_corrupt(
                    "entry-outside-range",
                    &format!("/sstables/{si}/entries/{ei}/key"),
                    format!("{} 的 key {} 不在 [{}, {}] 内", e.id, e.key, sst.key_min, sst.key_max),
                ));
            }
        }
        for (ri, r) in sst.range_dels.iter().enumerate() {
            let path = format!("/sstables/{si}/range_dels/{ri}");
            if r.start_key.as_bytes() > r.end_key.as_bytes() {
                return Err(err_corrupt("inverted-range-endpoint", &path, format!("{} 端点倒置", r.id)));
            }
            if r.start_key == r.end_key && !(r.start_inclusive && r.end_inclusive) {
                return Err(err_corrupt("empty-range-endpoint", &path, format!("{} 单键区间但端点非双闭", r.id)));
            }
            if !intervals_overlap(
                (&r.start_key, r.start_inclusive, &r.end_key, r.end_inclusive),
                (&sst.key_min, true, &sst.key_max, true),
            ) {
                return Err(err_corrupt("range-outside-sst", &path, format!("{} 完全落在 SSTable 区间之外", r.id)));
            }
        }
    }
    Ok(())
}

fn sst_indexes(m: &Materials, ids: &[String], path: &str) -> Result<Vec<usize>, CompactionError> {
    ids.iter()
        .map(|id| {
            m.sstables
                .iter()
                .position(|s| &s.id == id)
                .ok_or_else(|| err_corrupt("unknown-sst-id", path, format!("压实输入引用了不存在的 SSTable: {id}")))
        })
        .collect()
}

/// L0 可能重叠: 从显式种子出发闭包地拉入全部重叠的 L0 文件,
/// 再拉入目标层重叠文件; 每一步都生成选择理由。
fn select_inputs(
    m: &Materials,
    opt: &PrepareOptions,
) -> Result<(Vec<usize>, Vec<Judgment>), CompactionError> {
    let mut judgments = Vec::new();
    let mut chosen: Vec<usize> = if opt.input_ssts.is_empty() {
        let src = opt.target_level.saturating_sub(1);
        let levels = if opt.seed_levels.is_empty() { vec![src] } else { opt.seed_levels.clone() };
        m.sstables
            .iter()
            .enumerate()
            .filter(|(_, s)| levels.contains(&s.level))
            .map(|(i, _)| i)
            .collect()
    } else {
        sst_indexes(m, &opt.input_ssts, "/request/input_ssts")?
    };
    if chosen.is_empty() {
        return Err(err_corrupt("empty-compaction-input", "/request", "压实选择不到任何输入 SSTable"));
    }

    // L0 重叠闭包 (L0 文件之间允许重叠, 压实必须一起带走)。
    loop {
        let mut added = false;
        for (i, sst) in m.sstables.iter().enumerate() {
            if sst.level != 0 || chosen.contains(&i) {
                continue;
            }
            if chosen.iter().any(|&j| {
                let other = &m.sstables[j];
                other.level == 0
                    && sst.key_min.as_bytes() <= other.key_max.as_bytes()
                    && other.key_min.as_bytes() <= sst.key_max.as_bytes()
            }) {
                judgments.push(Judgment {
                    id: format!("jdg-select-l0-{}", sst.id),
                    subject: format!("sst:{}", sst.id),
                    rule: "overlapping-L0-must-join".into(),
                    basis: format!("L0 允许重叠; {} 与已选 L0 键区间重叠, 漏读会丢失同层较新数据", sst.file),
                    decision: "pulled-into-inputs".into(),
                    witnesses: chosen.iter().map(|j| format!("sst:{}", m.sstables[*j].id)).collect(),
                });
                chosen.push(i);
                added = true;
            }
        }
        if !added {
            break;
        }
    }

    // 目标层重叠文件随压实一起输入 (非 L0 层本应互不重叠)。
    loop {
        let mut added = false;
        for (i, sst) in m.sstables.iter().enumerate() {
            if sst.level != opt.target_level || chosen.contains(&i) {
                continue;
            }
            if chosen.iter().any(|&j| {
                let o = &m.sstables[j];
                sst.key_min.as_bytes() <= o.key_max.as_bytes() && o.key_min.as_bytes() <= sst.key_max.as_bytes()
            }) {
                judgments.push(Judgment {
                    id: format!("jdg-select-ln-{}", sst.id),
                    subject: format!("sst:{}", sst.id),
                    rule: "target-level-overlap-joins".into(),
                    basis: format!("目标层 L{} 文件 {} 与输入键区间重叠, 合并以维持非重叠层约束", opt.target_level, sst.file),
                    decision: "pulled-into-inputs".into(),
                    witnesses: vec![],
                });
                chosen.push(i);
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    chosen.sort();
    Ok((chosen, judgments))
}

/// 有效版本: 点条目或覆盖该键的范围删除 (在读取路径上等价于该键的一次 delete)。
#[derive(Debug, Clone)]
struct Version<'a> {
    seq: u64,
    kind: PointKind,
    /// 稳定身份: `entry:<id>` 或 `rangedel:<id>`。
    subject: String,
    entry: Option<&'a PointEntry>,
    origin_sst: &'a str,
}

/// 每个键在每个“视界”(最新读 + 每个活跃快照) 下的 keeper 计算。
/// 读取按内部键顺序 (seq 降序, 同序号按类型序) 取第一个 seq <= horizon 的版本。
fn compute_keepers<'v>(versions: &[&'v Version<'v>], horizons: &[u64]) -> Vec<usize> {
    let mut keepers = Vec::new();
    for &h in horizons {
        for (i, v) in versions.iter().enumerate() {
            if v.seq <= h {
                keepers.push(i);
                break;
            }
        }
    }
    keepers.sort_unstable();
    keepers.dedup();
    keepers
}

/// 未参与压实、且层级不浅于目标层的旧数据中, 是否存在比 seq 更旧的该键版本。
fn older_version_below(
    m: &Materials,
    key: &str,
    seq: u64,
    input_ids: &std::collections::HashSet<String>,
    target_level: u8,
) -> Option<(String, String, u8)> {
    for sst in &m.sstables {
        if input_ids.contains(&sst.id) || sst.level < target_level {
            continue;
        }
        if key_in_range(key, &sst.key_min, &sst.key_max) {
            for e in &sst.entries {
                if e.key == key && e.seq < seq {
                    return Some((sst.id.clone(), e.id.clone(), sst.level));
                }
            }
        }
    }
    None
}

/// 范围删除是否被未参与压实的旧数据挡住 (不能越过它提前丢弃)。
fn range_blocked_below<'m>(
    m: &'m Materials,
    r: &RangeTombstone,
    input_ids: &std::collections::HashSet<String>,
    target_level: u8,
) -> Option<&'m Sstable> {
    m.sstables.iter().find(|s| {
        if input_ids.contains(&s.id) || s.level < target_level {
            return false;
        }
        let file_overlaps = intervals_overlap(
            (&r.start_key, r.start_inclusive, &r.end_key, r.end_inclusive),
            (&s.key_min, true, &s.key_max, true),
        );
        file_overlaps
            && s.entries.iter().any(|e| {
                interval_covers(&r.start_key, r.start_inclusive, &r.end_key, r.end_inclusive, &e.key)
                    && e.seq < r.seq
            })
    })
}

struct KeyOutcome {
    kept: Vec<KeptEntry>,
    dropped: Vec<String>,
    judgments: Vec<Judgment>,
    /// 该键最旧视界 keeper 是同序号 put/delete 歧义对。
    ambiguous_top: Option<(u64, Vec<String>)>,
}

fn resolve_key(
    m: &Materials,
    key: &str,
    versions: &[&Version],
    horizons: &[u64],
    snap_ids: &[String],
    input_ids: &std::collections::HashSet<String>,
    target_level: u8,
    rules: &RuleSet,
) -> KeyOutcome {
    let mut out = KeyOutcome {
        kept: Vec::new(),
        dropped: Vec::new(),
        judgments: Vec::new(),
        ambiguous_top: None,
    };
    let keeper_idx = compute_keepers(versions, horizons);
    let latest_horizon = u64::MAX;
    let snap_horizons: Vec<u64> = horizons.iter().copied().filter(|&h| h != latest_horizon).collect();

    // 同序号多类型歧义: 该键最大 seq 上同时有 put 与 delete。
    if let Some(max_seq) = versions.iter().map(|v| v.seq).max() {
        let tops: Vec<&&Version> = versions.iter().filter(|v| v.seq == max_seq).collect();
        let has_put = tops.iter().any(|v| v.kind == PointKind::Put);
        let has_del = tops.iter().any(|v| v.kind == PointKind::Delete);
        if has_put && has_del {
            out.ambiguous_top = Some((
                max_seq,
                tops.iter().map(|v| v.subject.clone()).collect(),
            ));
        }
    }

    for (i, v) in versions.iter().enumerate() {
        // 范围删除版本只参与 keeper 排序 (遮蔽点条目); 其自身的保留下沉在范围层统一判定。
        if v.entry.is_none() {
            continue;
        }
        let is_keeper = keeper_idx.contains(&i);
        if !is_keeper {
            // 被更新版本遮蔽: 对每个视界都有更新版本可见。
            let shadow = versions
                .iter()
                .find(|o| o.seq > v.seq || (o.seq == v.seq && o.kind.type_order() > v.kind.type_order()))
                .map(|o| o.subject.clone())
                .unwrap_or_default();
            out.judgments.push(Judgment {
                id: format!("jdg-shadow-{}-{}", key, v.subject.replace(':', "-")),
                subject: v.subject.clone(),
                rule: "shadowed-by-newer-visible-version".into(),
                basis: format!(
                    "内部键顺序下 {key}@{} 对每个读取视界都有更新版本 ({shadow}) 可见; 无任何快照需要它",
                    v.seq
                ),
                decision: "drop".into(),
                witnesses: vec![shadow],
            });
            if let Some(e) = v.entry {
                out.dropped.push(e.id.clone());
            }
            continue;
        }

        // keeper: 判断是否为 tombstone 且可随底部压实丢弃。
        let keeper_for_latest = versions
            .iter()
            .position(|x| x.seq <= latest_horizon)
            .map(|p| p == i)
            .unwrap_or(false);
        let needed_by_snapshot = snap_horizons.iter().any(|&h| {
            v.seq <= h && versions.iter().take(i).all(|o| o.seq > h)
        });

        match v.kind {
            PointKind::Put => {
                out.judgments.push(Judgment {
                    id: format!("jdg-keep-{}-{}", key, v.subject.replace(':', "-")),
                    subject: v.subject.clone(),
                    rule: "keeper-put-retained".into(),
                    basis: format!("{key}@{} 是某个读取视界 (最新读或快照 {}) 的最旧可见版本", v.seq, snap_ids.join(",")),
                    decision: "retain".into(),
                    witnesses: vec![],
                });
                if let Some(e) = v.entry {
                    out.kept.push(KeptEntry {
                        origin_sst: v.origin_sst.to_string(),
                        origin_entry: e.id.clone(),
                        seq: e.seq,
                        key: e.key.clone(),
                        kind: "put".into(),
                        value: e.value.clone(),
                    });
                }
            }
            PointKind::Delete => {
                let below = older_version_below(m, key, v.seq, input_ids, target_level);
                if needed_by_snapshot {
                    out.judgments.push(Judgment {
                        id: format!("jdg-snap-{}-{}", key, v.subject.replace(':', "-")),
                        subject: v.subject.clone(),
                        rule: "active-old-snapshot-pins-tombstone".into(),
                        basis: format!(
                            "{key}@{} 的删除对活跃旧快照 ({}) 仍是其视界内的最旧可见版本, 提前丢弃会让旧值在快照读中复活",
                            v.seq, snap_ids.join(",")
                        ),
                        decision: "retain-tombstone".into(),
                        witnesses: snap_ids.iter().map(|s| format!("snapshot:{s}")).collect(),
                    });
                    push_kept_delete(&mut out, v);
                } else if let Some((sst_id, entry_id, level)) = below {
                    out.judgments.push(Judgment {
                        id: format!("jdg-pin-{}-{}", key, v.subject.replace(':', "-")),
                        subject: v.subject.clone(),
                        rule: "tombstone-cannot-cross-uncompacted-old-data".into(),
                        basis: format!(
                            "{key}@{} 的删除下方 L{level} 还有未参与压实的旧版本 {} ({}); 提前丢弃会让旧值复活, tombstone 必须随压实继续下沉",
                            v.seq, entry_id, sst_id
                        ),
                        decision: "retain-tombstone-sinks-down".into(),
                        witnesses: vec![format!("entry:{entry_id}"), format!("sst:{sst_id}")],
                    });
                    push_kept_delete(&mut out, v);
                } else if keeper_for_latest {
                    out.judgments.push(Judgment {
                        id: format!("jdg-drop-{}-{}", key, v.subject.replace(':', "-")),
                        subject: v.subject.clone(),
                        rule: rules.point_tombstone_drop.clone(),
                        basis: format!(
                            "{key}@{} 的删除在 L{target_level} 以下没有更旧版本, 且无任何活跃快照需要: 底部压实下沉丢弃",
                            v.seq
                        ),
                        decision: "drop".into(),
                        witnesses: vec![],
                    });
                    if let Some(e) = v.entry {
                        out.dropped.push(e.id.clone());
                    }
                } else {
                    // keeper 只服务快照, 已被 needed_by_snapshot 覆盖; 防御性保留。
                    push_kept_delete(&mut out, v);
                }
            }
        }
    }
    out
}

fn push_kept_delete(out: &mut KeyOutcome, v: &Version) {
    if let Some(e) = v.entry {
        out.kept.push(KeptEntry {
            origin_sst: v.origin_sst.to_string(),
            origin_entry: e.id.clone(),
            seq: e.seq,
            key: e.key.clone(),
            kind: "delete".into(),
            value: None,
        });
    }
}

fn collect_versions<'m>(
    m: &'m Materials,
    input_idx: &[usize],
) -> BTreeMap<String, Vec<Version<'m>>> {
    let mut groups: BTreeMap<String, Vec<Version<'m>>> = BTreeMap::new();
    for &i in input_idx {
        let sst = &m.sstables[i];
        for e in &sst.entries {
            groups.entry(e.key.clone()).or_default().push(Version {
                seq: e.seq,
                kind: e.kind,
                subject: format!("entry:{}", e.id),
                entry: Some(e),
                origin_sst: &sst.id,
            });
        }
        for r in &sst.range_dels {
            for &j in input_idx {
                for e in &m.sstables[j].entries {
                    if interval_covers(&r.start_key, r.start_inclusive, &r.end_key, r.end_inclusive, &e.key) {
                        groups.entry(e.key.clone()).or_default().push(Version {
                            seq: r.seq,
                            kind: PointKind::Delete,
                            subject: format!("rangedel:{}", r.id),
                            entry: None,
                            origin_sst: &sst.id,
                        });
                    }
                }
            }
        }
    }
    for g in groups.values_mut() {
        g.sort_by(|a, b| b.seq.cmp(&a.seq).then(b.kind.type_order().cmp(&a.kind.type_order())));
        // 同一 range del 可能经多个输入文件重复登记到同一 key: 按 subject 去重。
        g.dedup_by(|a, b| a.subject == b.subject);
    }
    groups
}

/// 在切分边界处把范围删除碎片化为落在窗口内的多段 (半开字节序窗口)。
fn fragment_range(r: &RangeTombstone, lo: &str, hi_excl: &str) -> Option<OutputRangeDel> {
    let start = max_key(&r.start_key, lo);
    let end = min_key(&r.end_key, hi_excl);
    if start.as_bytes() >= end.as_bytes() {
        return None;
    }
    // 与半开窗口 [lo, hi_excl) 求交: 左边界若取自原 start 沿用 start_inclusive,
    // 否则切分点被窗口包含 (闭); 右边界若恰好等于 hi_excl, 窗口恒斥,
    // 取自原 end 且严格在窗口内时才沿用 end_inclusive。
    let start_inclusive = if start == r.start_key { r.start_inclusive } else { true };
    let end_inclusive = if end == hi_excl {
        false
    } else if end == r.end_key {
        r.end_inclusive
    } else {
        false
    };
    if start == end && !(start_inclusive && end_inclusive) {
        return None;
    }
    Some(OutputRangeDel {
        origin_range_id: r.id.clone(),
        seq: r.seq,
        start_key: start,
        start_inclusive,
        end_key: end,
        end_inclusive,
    })
}

fn max_key(a: &str, b: &str) -> String {
    if a.as_bytes() >= b.as_bytes() { a.to_string() } else { b.to_string() }
}
fn min_key(a: &str, b: &str) -> String {
    if a.as_bytes() <= b.as_bytes() { a.to_string() } else { b.to_string() }
}

/// 输出切分 + 非重叠层约束校验。
fn build_outputs(
    m: &Materials,
    opt: &PrepareOptions,
    input_idx: &[usize],
    input_ids: &std::collections::HashSet<String>,
    mut kept: Vec<KeptEntry>,
    surviving_ranges: Vec<&RangeTombstone>,
    candidate_split: Option<&[String]>,
    judgments: &mut Vec<Judgment>,
) -> Result<Vec<OutputFile>, CompactionError> {
    kept.sort_by(|a, b| a.key.cmp(&b.key).then(b.seq.cmp(&a.seq)));
    let splits = candidate_split.unwrap_or(&opt.output_split_keys);
    let mut boundaries: Vec<String> = splits.iter().cloned().collect();
    boundaries.sort();
    boundaries.dedup();
    // 跳过落在数据范围外或造成空窗口的切分点仍保留为边界 (空窗口不产出文件)。
    let windows: Vec<(String, String)> = {
        let keys: Vec<&str> = kept.iter().map(|k| k.key.as_str()).collect();
        let rmin = keys.first().copied().unwrap_or("");
        let rmax = keys.last().copied().unwrap_or("");
        let mut wins = Vec::new();
        let mut lo = if rmin.is_empty() { "".to_string() } else { rmin.to_string() };
        for b in &boundaries {
            if b.as_bytes() > lo.as_bytes() && b.as_bytes() <= rmax.as_bytes() {
                wins.push((lo.clone(), b.clone()));
                lo = b.clone();
            }
        }
        if lo.as_bytes() <= rmax.as_bytes() {
            wins.push((lo, "\u{10ffff}".to_string()));
        }
        wins
    };

    let mut outputs = Vec::new();
    for (wi, (lo, hi_excl)) in windows.iter().enumerate() {
        let entries: Vec<KeptEntry> = kept
            .iter()
            .filter(|k| k.key.as_bytes() >= lo.as_bytes() && k.key.as_bytes() < hi_excl.as_bytes())
            .cloned()
            .collect();
        let mut range_frags: Vec<OutputRangeDel> = Vec::new();
        for r in &surviving_ranges {
            if let Some(frag) = fragment_range(r, lo, hi_excl) {
                range_frags.push(frag);
            }
        }
        if entries.is_empty() && range_frags.is_empty() {
            continue;
        }
        let key_min = entries
            .first()
            .map(|e| e.key.clone())
            .or_else(|| range_frags.first().map(|f| f.start_key.clone()))
            .unwrap();
        let key_max = entries
            .last()
            .map(|e| e.key.clone())
            .or_else(|| {
                range_frags
                    .iter()
                    .map(|f| if f.end_inclusive { f.end_key.clone() } else { prev_byte_key(&f.end_key) })
                    .max()
            })
            .unwrap();

        // 非重叠层约束: 输出不得与未参与压实的目标层文件重叠。
        let mut overlaps_with = Vec::new();
        for s in &m.sstables {
            if s.level != opt.target_level || input_ids.contains(&s.id) {
                continue;
            }
            if key_min.as_bytes() <= s.key_max.as_bytes() && s.key_min.as_bytes() <= key_max.as_bytes() {
                overlaps_with.push(s.id.clone());
            }
        }
        if !overlaps_with.is_empty() {
            return Err(CompactionError::Corrupted {
                code: "output-level-overlap".into(),
                path: format!("/scenarios/{}/output_split_keys", opt.scenario),
                message: format!(
                    "输出 {}..{} 将与未参与压实的目标层文件 {} 重叠, 违反非重叠层约束; 需扩大输入或调整切分",
                    key_min, key_max,
                    overlaps_with.join(",")
                ),
            });
        }

        outputs.push(OutputFile {
            candidate_id: format!("candidate:primary#{}", wi + 1),
            proposed_file: format!("{}-L{}-{}.sst", opt.scenario, opt.target_level, wi + 1),
            level: opt.target_level,
            key_min,
            key_max,
            entries,
            range_dels: range_frags,
            overlaps_with,
        });
    }
    let _ = input_idx;
    Ok(outputs)
}

fn prev_byte_key(k: &str) -> String {
    let mut b = k.as_bytes().to_vec();
    if let Some(last) = b.last_mut() {
        if *last > 0 {
            *last -= 1;
            return String::from_utf8_lossy(&b).to_string();
        }
    }
    k.to_string()
}

/// 准备一次压实: 返回确定性的计划 (等价输入得到稳定顺序)。
#[allow(clippy::too_many_arguments)]
pub fn prepare_compaction(
    m: &Materials,
    rules: &RuleSet,
    opt: &PrepareOptions,
) -> Result<CompactionPlan, CompactionError> {
    validate_materials(m)?;
    let (input_idx, mut judgments) = select_inputs(m, opt)?;
    let input_ids: std::collections::HashSet<String> =
        input_idx.iter().map(|&i| m.sstables[i].id.clone()).collect();

    // 视界: 最新读 + 每个活跃快照。
    let mut horizons: Vec<u64> = m.snapshots.iter().map(|s| s.seq).collect();
    horizons.push(u64::MAX);
    horizons.sort_unstable();
    horizons.dedup();
    let snap_ids: Vec<String> = m.snapshots.iter().map(|s| s.id.clone()).collect();

    let mut groups = collect_versions(m, &input_idx);

    // 同序号 put/delete 并列: 找到每组键中“最大 seq 上同时存在两类”的歧义对。
    let mut ambiguity: Option<(String, u64, Vec<String>)> = None;
    {
        let mut ambiguous_key: Option<(String, u64, Vec<String>)> = None;
        for (key, versions) in &groups {
            if let Some(max_seq) = versions.iter().map(|v| v.seq).max() {
                let tops: Vec<&Version> = versions.iter().filter(|v| v.seq == max_seq).collect();
                if tops.iter().any(|v| v.kind == PointKind::Put)
                    && tops.iter().any(|v| v.kind == PointKind::Delete)
                {
                    ambiguous_key = Some((
                        key.clone(),
                        max_seq,
                        tops.iter().map(|v| v.subject.clone()).collect(),
                    ));
                    break;
                }
            }
        }
        if let Some((key, seq, subjects)) = ambiguous_key {
            if rules.same_seq_tiebreak == SameSeqTiebreak::AllCandidates {
                return Err(CompactionError::SemanticUnsolvable {
                    code: "same-seq-type-conflict".into(),
                    path: format!("/entries?key={key}"),
                    message: format!(
                        "{key}@{seq} 上同序号 put 与 delete 并存; delete-wins 与 put-wins 给出不同的下沉/保留结论, 规则版本 {} 未给裁决, 无法唯一决定",
                        rules.version
                    ),
                    candidates: vec!["candidate:delete-wins".into(), "candidate:put-wins".into()],
                });
            }
            let winner_kind = match rules.same_seq_tiebreak {
                SameSeqTiebreak::DeleteWins => PointKind::Delete,
                SameSeqTiebreak::PutWins => PointKind::Put,
                SameSeqTiebreak::AllCandidates => unreachable!(),
            };
            let loser_kind = match winner_kind {
                PointKind::Delete => PointKind::Put,
                PointKind::Put => PointKind::Delete,
            };
            groups.get_mut(&key).unwrap().retain(|v| {
                !(v.seq == seq && v.kind == loser_kind && v.entry.is_some())
            });
            judgments.push(Judgment {
                id: format!("jdg-sameseq-{}-{}", key, seq),
                subject: format!("key:{key}@{seq}"),
                rule: "same-seq-tiebreak".into(),
                basis: format!(
                    "同序号 put/delete 按规则版本 {} 的 {:?} 裁决; 败者内部键不进入输出, 原始并列版本仍保留在材料中可反查",
                    rules.version, rules.same_seq_tiebreak
                ),
                decision: "resolved-by-rule".into(),
                witnesses: subjects,
            });
            ambiguity = Some((key, seq, vec![]));
        }
    }

    let mut kept: Vec<KeptEntry> = Vec::new();
    let mut dropped_entries: Vec<String> = Vec::new();
    let mut preserved_below: Vec<String> = Vec::new();

    for (key, versions) in &groups {
        let refs: Vec<&Version> = versions.iter().collect();
        let outcome = resolve_key(m, key, &refs, &horizons, &snap_ids, &input_ids, opt.target_level, rules);
        kept.extend(outcome.kept);
        dropped_entries.extend(outcome.dropped);
        judgments.extend(outcome.judgments);
    }

    // 范围删除的整体保留/下沉判定 (点遮蔽证据已在 collect_versions 中通过
    // “把 range del 当作该键的 delete 版本”统一参与 keeper 计算)。
    let mut surviving_ranges: Vec<&RangeTombstone> = Vec::new();
    let mut dropped_range_dels: Vec<String> = Vec::new();
    for &i in &input_idx {
        for r in &m.sstables[i].range_dels {
            let cutoff = m.snapshots.iter().map(|s| s.seq).min().unwrap_or(u64::MAX);
            let needed_by_snapshot = m.snapshots.iter().any(|s| r.seq <= s.seq);
            let _ = cutoff;
            if let Some(s) = range_blocked_below(m, r, &input_ids, opt.target_level) {
                judgments.push(Judgment {
                    id: format!("jdg-rangpin-{}", r.id),
                    subject: format!("rangedel:{}", r.id),
                    rule: "range-del-cannot-cross-uncompacted-old-data".into(),
                    basis: format!(
                        "{} [{},{}) 与未参与压实的 {} (L{}) 中更旧的点数据相交; 不能越过它提前丢弃, 删除记录随压实继续下沉",
                        r.id, r.start_key, r.end_key, s.file, s.level
                    ),
                    decision: "retain-range-sinks-down".into(),
                    witnesses: vec![format!("sst:{}", s.id)],
                });
                preserved_below.push(format!("sst:{}", s.id));
                surviving_ranges.push(r);
            } else if needed_by_snapshot {
                judgments.push(Judgment {
                    id: format!("jdg-rangsnap-{}", r.id),
                    subject: format!("rangedel:{}", r.id),
                    rule: "active-old-snapshot-pins-range".into(),
                    basis: format!(
                        "{} 的 seq {} <= 活跃快照 ({}) 上界; 快照读路径仍必须看到该范围删除, 不得提前丢弃",
                        r.id, r.seq,
                        m.snapshots.iter().map(|s| format!("{}@{}", s.id, s.seq)).collect::<Vec<_>>().join(",")
                    ),
                    decision: "retain-range".into(),
                    witnesses: snap_ids.iter().map(|s| format!("snapshot:{s}")).collect(),
                });
                surviving_ranges.push(r);
            } else {
                judgments.push(Judgment {
                    id: format!("jdg-rangdrop-{}", r.id),
                    subject: format!("rangedel:{}", r.id),
                    rule: rules.range_tombstone_drop.clone(),
                    basis: format!(
                        "{} 在 L{} 以下不再与未参与压实的更旧点数据相交, 且无活跃快照需要: 底部压实下沉丢弃",
                        r.id, opt.target_level
                    ),
                    decision: "drop".into(),
                    witnesses: vec![],
                });
                dropped_range_dels.push(r.id.clone());
            }
        }
    }

    let legal_candidates = {
        let spanned_split = surviving_ranges.iter().any(|r| {
            opt.output_split_keys.iter().any(|b| {
                b.as_bytes() > r.start_key.as_bytes() && b.as_bytes() < r.end_key.as_bytes()
            })
        });
        if spanned_split {
            vec![
                PlanCandidate {
                    id: "candidate:fragment-range-del".into(),
                    description: "保持切分, 跨边界范围删除在输出边界处碎片化为多段".into(),
                    output_count: opt.output_split_keys.len() + 1,
                    split_keys: opt.output_split_keys.clone(),
                },
                PlanCandidate {
                    id: "candidate:widen-output".into(),
                    description: "合并跨边界输出为一个更宽文件, 范围删除保持完整, 文件数更少但更宽".into(),
                    output_count: 1,
                    split_keys: vec![],
                },
            ]
        } else {
            vec![PlanCandidate {
                id: "candidate:primary".into(),
                description: "唯一合法方案".into(),
                output_count: opt.output_split_keys.len() + 1,
                split_keys: opt.output_split_keys.clone(),
            }]
        }
    };

    // 冲突的人工决定: 指定的候选不在合法候选集合中。
    if let Some(pick) = &opt.manual_candidate_pick {
        if !legal_candidates.iter().any(|c| c.id == *pick) {
            return Err(CompactionError::SemanticUnsolvable {
                code: "conflicting-manual-decision".into(),
                path: format!("/scenarios/{}/manual_candidate_pick", opt.scenario),
                message: format!("人工决定选择了 {pick}, 但它不在全部合法候选中, 决定与规则/输入冲突"),
                candidates: legal_candidates.iter().map(|c| c.id.clone()).collect(),
            });
        }
    }

    let chosen_splits: Vec<String> = match opt.manual_candidate_pick.as_deref() {
        Some("candidate:widen-output") => vec![],
        _ => opt.output_split_keys.clone(),
    };

    let outputs = build_outputs(
        m, opt, &input_idx, &input_ids, kept.clone(),
        surviving_ranges.clone(), Some(&chosen_splits), &mut judgments,
    )?;

    // 资源预算: 输出点条目 + 范围删除片段总数。
    let needed: usize = outputs.iter().map(|o| o.entries.len() + o.range_dels.len()).sum();
    if let Some(budget) = opt.output_entry_budget {
        if needed > budget {
            return Err(CompactionError::BudgetExhausted {
                budget,
                needed,
                path: format!("/scenarios/{}/output_entry_budget", opt.scenario),
            });
        }
    }

    let _ = ambiguity;
    let input_ssts = input_idx.iter().map(|&i| m.sstables[i].id.clone()).collect();
    Ok(CompactionPlan {
        scenario: opt.scenario.clone(),
        rule_version: rules.version.clone(),
        input_ssts,
        target_level: opt.target_level,
        selected_by: if opt.input_ssts.is_empty() { "auto(overlap-closure)".into() } else { "explicit+overlap-closure".into() },
        outputs,
        judgments,
        legal_candidates,
        dropped_entries,
        dropped_range_dels,
        preserved_below: preserved_below.into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect(),
        diagnostics: vec![],
    })
}
