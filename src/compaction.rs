//! 压实语义核心（纯函数，可被测试直接调用）。
//!
//! 1) 输入选择：seed + 同层重叠 L0 + 恰好下一层的重叠文件，闭包扩张；
//! 2) 内部键序合并，按最小活跃 snapshot seq 判可见性；
//! 3) point/range tombstone 的保留与下沉；range tomb 不得越过未参与压实的旧数据提前丢弃；
//! 4) 按祖父层重叠预算切分输出，并校验输出层不重叠约束；
//! 5) 同序号多类型 => 语义无解，保留全部合法候选交人工决定。

use crate::event::{Affected, Evidence, Fact};
use crate::types::{end_bound_max, ComputeOutcome, Entry, KeyRange, Kind, RawQuantity, Rules, Seq};
use std::collections::{BTreeMap, BTreeSet};

/// 活跃 SSTable 摘要。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sst {
    pub id: String,
    pub level: u32,
    pub source: String,
    pub entries: Vec<Entry>,
    pub declared_size: RawQuantity,
    /// 输出切分中分配的稳定输出序号；人工导入材料为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_index: Option<usize>,
}

impl Sst {
    pub fn range(&self) -> KeyRange {
        KeyRange::from_entries(&self.entries).expect("sst has entries")
    }
    pub fn est_bytes(&self) -> f64 {
        self.entries.iter().map(|e| e.est_bytes()).sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StagedOutput {
    pub sst_id: String,
    pub entries: Vec<Entry>,
    pub output_index: usize,
}

#[derive(Debug, Clone)]
pub struct StagedJob {
    pub job_id: String,
    pub chosen_ids: Vec<String>,
    pub target_level: u32,
    pub rules_version: u32,
    pub entries: Vec<Entry>,
    pub outputs: Vec<StagedOutput>,
    pub evidence: Vec<Evidence>,
}

/// 同序号多类型冲突记录。
#[derive(Debug, Clone, PartialEq)]
pub struct SameSeqConflict {
    pub key: String,
    pub seq: Seq,
    pub kinds: Vec<Kind>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone)]
pub struct Planned {
    pub job_id: String,
    pub chosen: Vec<Sst>,
    pub target_level: u32,
    pub selection_evidence: Evidence,
    pub conflict: Option<SameSeqConflict>,
    pub conflict_entries: Vec<Entry>,
    pub entries: Vec<Entry>,
}

/// 活跃快照与文件的不可变视图。
#[derive(Debug, Clone)]
pub struct View<'a> {
    pub active: &'a [Sst],
    pub smallest_active_snapshot_seq: Option<Seq>,
}

/// 选择压实输入。`target_level` 允许案例显式指定（默认 seed 层 +1，L0 为 1）。
pub fn select(job_id: &str, seed_id: &str, view: &View, forced_target: Option<u32>) -> Result<Planned, String> {
    let seed = view
        .active
        .iter()
        .find(|s| s.id == seed_id)
        .ok_or_else(|| format!("seed sst {} not found in current branch state", seed_id))?;
    let seed_level = seed.level;
    let target_level = forced_target.unwrap_or(if seed_level == 0 { 1 } else { seed_level + 1 });

    let mut chosen: BTreeSet<String> = BTreeSet::new();
    let mut reasons: Vec<Fact> = Vec::new();
    chosen.insert(seed.id.clone());
    reasons.push(Fact {
        predicate: "selection_seed".into(),
        args: crate::map! {
            "sst".into() => seed.id.clone(),
            "level".into() => seed.level.to_string(),
            "range".into() => fmt_range(&seed.range()),
        },
    });

    // 闭包扩张：L0 内相互重叠文件 + 目标层重叠文件。
    let mut span = seed.range();
    loop {
        let before = span.clone();
        for s in view.active.iter() {
            if chosen.contains(&s.id) || s.level > target_level {
                continue;
            }
            let include = if seed_level == 0 && s.level == 0 {
                s.range().overlaps(&span)
            } else {
                s.level == target_level && s.range().overlaps(&span)
            };
            if include {
                reasons.push(Fact {
                    predicate: if s.level == 0 {
                        "selection_overlapping_l0".into()
                    } else {
                        "selection_overlapping_output_level".into()
                    },
                    args: crate::map! {
                        "sst".into() => s.id.clone(),
                        "level".into() => s.level.to_string(),
                        "range".into() => fmt_range(&s.range()),
                        "span_before".into() => fmt_range(&span),
                    },
                });
                chosen.insert(s.id.clone());
                span = union_range(&span, &s.range());
            }
        }
        if span == before {
            break;
        }
    }

    let mut chosen_ssts: Vec<Sst> = view
        .active
        .iter()
        .filter(|s| chosen.contains(&s.id))
        .cloned()
        .collect();
    chosen_ssts.sort_by(|a, b| {
        a.level
            .cmp(&b.level)
            .then_with(|| a.id.cmp(&b.id))
    });

    // 汇总并按内部键序排序。
    let mut entries: Vec<Entry> = Vec::new();
    for s in &chosen_ssts {
        entries.extend(s.entries.iter().cloned());
    }
    entries.sort_by(Entry::internal_cmp);

    // 同 key 同 seq 多类型 => 语义无解（同类型重复只是重复安装/重复来源，去重）。
    let mut conflict: Option<SameSeqConflict> = None;
    let mut seen_same: BTreeMap<(String, Seq), Vec<Kind>> = BTreeMap::new();
    for e in &entries {
        let key = (e.key.clone(), e.seq);
        let list = seen_same.entry(key).or_default();
        if !list.contains(&e.kind) {
            list.push(e.kind);
        }
    }
    for ((k, seq), kinds) in seen_same {
        if kinds.len() > 1 {
            let mut ks = kinds;
            ks.sort();
            let ce: Vec<Entry> = entries
                .iter()
                .filter(|e| e.key == k && e.seq == seq)
                .cloned()
                .collect();
            conflict = Some(SameSeqConflict { key: k, seq, kinds: ks, entries: ce });
            break;
        }
    }

    let ev = Evidence {
        evidence_id: format!("{}/selection", job_id),
        title: format!("压实 {} 的输入选择理由", job_id),
        class: "selection".into(),
        basis: reasons,
        affected: chosen_ssts
            .iter()
            .map(|s| Affected {
                kind: "sst".into(),
                id: s.id.clone(),
                detail: Some(format!("L{} {}", s.level, fmt_range(&s.range()))),
            })
            .collect(),
    };

    Ok(Planned {
        job_id: job_id.into(),
        chosen: chosen_ssts,
        target_level,
        selection_evidence: ev,
        conflict_entries: entries.clone(),
        conflict,
        entries,
    })
}
