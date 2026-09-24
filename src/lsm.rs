//! LSM 读路径与压实语义。
//!
//! 关键不变量（也是审阅机器可读事件的判定依据）：
//!
//! 1. 读取严格按内部键顺序 `(user_key asc, seq desc, type tie-break desc)`，
//!    可见性由 snapshot 的 last_seq 判定；point 墓碑遮蔽更旧版本，
//!    范围删除在其 seq 与覆盖区间内遮蔽 point。
//! 2. 墓碑下沉：只有在“最底层且没有旧 snapshot 会越过墓碑读到更旧版本”时
//!    才能丢弃 point/range 墓碑。参与文件集合之外、与输出/墓碑区间重叠的
//!    旧 SST 使本次压实不构成最底层，墓碑必须保留。
//! 3. 输出切分必须使目标非 L0 层满足不重叠约束；可行切点多于一个且
//!    规则无法唯一裁决时返回 [`FailureClass::SemanticAmbiguous`] 与全部候选。

use crate::diagnostic::{Diagnostic, FailureClass};
use crate::domain::{
    sort_internal, CompactionParams, EndpointKind, InternalKey, PointEntry, PointKind,
    RangeTombstone, Seq, Snapshot, SstFile, TieBreak,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 读取视角：某 key 在某 snapshot 下的解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePoint {
    pub user_key: String,
    pub seq: Seq,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub unit: Option<String>,
    /// 遮蔽来源：point 墓碑或 range tombstone 的 id。
    pub obscured_by: Option<String>,
    /// 解析时扫描过的内部键链（稳定 id 用）。
    pub considered: Vec<String>,
}

/// 一次读取判定（机器可读证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadVerdict {
    pub snapshot_id: String,
    pub last_seq: Seq,
    pub user_key: String,
    pub ordered_chain: Vec<ChainLink>,
    pub visible: Option<VisiblePoint>,
    pub rationale: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainLink {
    pub source_file: Option<String>,
    pub user_key: String,
    pub seq: Seq,
    pub kind: String,
    pub tie_break: u8,
    pub visible_to_snapshot: bool,
    pub shadowed: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub shadow_reason: Option<String>,
}

fn link_key(source_file: Option<&str>, e: &PointEntry, last_seq: Seq, reason: Option<String>) -> ChainLink {
    ChainLink {
        source_file: source_file.map(str::to_string),
        user_key: e.user_key.clone(),
        seq: e.seq,
        kind: e.kind.as_str().to_string(),
        tie_break: e.kind.tie() as u8,
        visible_to_snapshot: e.seq <= last_seq,
        shadowed: reason.is_some(),
        shadow_reason: reason,
    }
}

/// 参与一次读判定的全部文件（按内部键全局有序扫描）。
pub fn build_global_chain<'a>(
    files: impl IntoIterator<Item = &'a SstFile>,
    user_key: &str,
) -> Vec<(Option<String>, PointEntry)> {
    let mut all: Vec<(Option<String>, PointEntry)> = Vec::new();
    for f in files {
        for e in &f.entries {
            if e.user_key == user_key {
                all.push((Some(f.file_id.clone()), e.clone()));
            }
        }
    }
    all.sort_by(|a, b| {
        InternalKey::cmp_ik(a.1.internal_key(), b.1.internal_key())
            .then_with(|| a.0.cmp(&b.0))
    });
    all
}

/// 按内部键顺序 + snapshot 判可见性的核心读路径。
pub fn read_at(
    files: &[&SstFile],
    snapshots: &[Snapshot],
    snapshot_id: &str,
    user_key: &str,
) -> Result<ReadVerdict, Diagnostic> {
    let snap = snapshots
        .iter()
        .find(|s| s.id == snapshot_id)
        .ok_or_else(|| {
            Diagnostic::ambiguous(
                "snapshot.missing",
                format!("snapshot {snapshot_id} 不存在，无法判定可见性"),
                None,
            )
        })?;
    let last_seq = snap.last_seq;

    let chain = build_global_chain(files.iter().copied(), user_key);

    // 收集覆盖该 key、且对 snapshot 可见的 range tombstone（seq 最大者优先）。
    let mut ranges: Vec<&RangeTombstone> = files
        .iter()
        .flat_map(|f| f.range_tombstones.iter())
        .filter(|r| r.seq <= last_seq && r.covers(user_key))
        .collect();
    ranges.sort_by(|a, b| b.seq.cmp(&a.seq).then(a.id.cmp(&b.id)));

    let mut ordered_chain = Vec::new();
    let mut rationale = Vec::new();
    let mut visible: Option<VisiblePoint> = None;

    // 同序号多类型：对该 key 的可见版本做冲突检测。
    let mut by_seq: BTreeMap<Seq, Vec<(Option<String>, &PointEntry)>> = BTreeMap::new();
    for (src, e) in &chain {
        by_seq.entry(e.seq).or_default().push((src.as_deref(), e));
    }
    let mut conflict_at: Option<Seq> = None;
    for (seq, group) in by_seq.iter().rev() {
        if *seq > last_seq {
            continue;
        }
        let kinds: Vec<PointKind> = group.iter().map(|(_, e)| e.kind).collect();
        let mutually_exclusive = kinds.iter().any(|k| {
            matches!(k, PointKind::Delete | PointKind::SingleDelete)
        }) && kinds.iter().any(|k| matches!(k, PointKind::Put | PointKind::Merge));
        if mutually_exclusive {
            conflict_at = Some(*seq);
            break;
        }
    }

    for (src, e) in &chain {
        let mut reason: Option<String> = None;
        if e.seq > last_seq {
            reason = Some(format!("seq {} > snapshot last_seq {last_seq}：不可见", e.seq));
        } else if let Some(cs) = conflict_at {
            if e.seq <= cs {
                // 冲突序号之后的版本读取结果取决于冲突如何裁决
                if e.seq == cs {
                    reason = Some(format!("seq {cs} 同序号多类型冲突（put 与 delete 并存），读取语义无解"));
                } else {
                    reason = Some(format!("上游 seq {cs} 同序号多类型冲突，遮蔽关系无法确定"));
                }
            }
        } else if visible.is_none() {
            if let Some(r) = ranges.first() {
                if r.seq >= e.seq {
                    reason = Some(format!("range tombstone {} @seq {} 覆盖并遮蔽", r.id, r.seq));
                }
            }
        } else {
            reason = Some("已被内部键顺序中更新的可见版本遮蔽".to_string());
        }
        ordered_chain.push(link_key(src.as_deref(), e, last_seq, reason));
    }

    if let Some(cs) = conflict_at {
        rationale.push(format!("snapshot {snapshot_id}(last_seq={last_seq}) 在 seq {cs} 遇到同序号多类型冲突"));
        return Ok(ReadVerdict {
            snapshot_id: snapshot_id.to_string(),
            last_seq,
            user_key: user_key.to_string(),
            ordered_chain,
            visible: None,
            rationale,
        });
    }

    for (src, e) in &chain {
        let _ = src;
        if e.seq > last_seq {
            continue;
        }
        if let Some(r) = ranges.first() {
            if r.seq >= e.seq && r.covers(user_key) {
                rationale.push(format!(
                    "key {user_key} 被范围删除 {}[{},{})@{} 遮蔽（snapshot last_seq={last_seq}）",
                    r.id, r.start_key, r.end_key, r.seq
                ));
                visible = Some(VisiblePoint {
                    user_key: user_key.to_string(),
                    seq: r.seq,
                    kind: PointKind::Delete,
                    value: None,
                    unit: None,
                    obscured_by: Some(r.id.clone()),
                    considered: chain.iter().map(|(s, e)| format!("{}#{}@{}", s.as_deref().unwrap_or("mem"), e.user_key, e.seq)).collect(),
                });
                break;
            }
        }
        let v = VisiblePoint {
            user_key: e.user_key.clone(),
            seq: e.seq,
            kind: e.kind,
            value: e.value.clone(),
            unit: e.unit.clone(),
            obscured_by: if e.is_tombstone() {
                Some(format!("point-tombstone@{}", e.seq))
            } else {
                None
            },
            considered: chain.iter().map(|(s, e)| format!("{}#{}@{}", s.as_deref().unwrap_or("mem"), e.user_key, e.seq)).collect(),
        };
        rationale.push(format!(
            "内部键顺序首个 seq<={last_seq} 的记录为 {} @seq {}（{}）",
            e.kind.as_str(),
            e.seq,
            if e.is_tombstone() { "point tombstone" } else { "存活值" }
        ));
        visible = Some(v);
        break;
    }

    Ok(ReadVerdict {
        snapshot_id: snapshot_id.to_string(),
        last_seq,
        user_key: user_key.to_string(),
        ordered_chain,
        visible,
        rationale,
    })
}
