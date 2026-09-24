//! 读取路径：按内部键顺序（user key 升序 -> seq 降序 -> 类型序）归并，
//! 再按 snapshot 判可见性。这是本项目最棘手、必须形成结构化状态的部分。
//!
//! 判定规则（与 RocksDB 兼容的可解释版本）：
//! 1. 覆盖 key 的 range tombstone 与 point 一起参与；range 会遮蔽 seq 不高于它的 point。
//! 2. 先取 seq 不大于 snapshot 的最新一条 mutation（point 或 range）。
//! 3. 同 key 同 seq 出现互斥类型（put/delete）=> 语义无解，返回全部合法候选。

use crate::domain::error::{DResult, DomainError};
use crate::domain::model::{KeyEncoding, KeySpec, PointKind, PointRecord, RangeTombstone, Seq, SstFileSpec};
use serde::{Deserialize, Serialize};

/// 内部键同序时的类型序：Merge/Delete 先于 Put（这里只需 delete < put）。
fn type_rank(k: PointKind) -> u8 {
    match k {
        PointKind::Delete => 0,
        PointKind::Put => 1,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadEvent {
    /// 机器可读判定码，例如 `range_shadows_point` / `point_delete_visible`。
    pub code: String,
    /// 涉及对象的稳定身份（record rid / tombstone rid / snapshot）。
    pub subjects: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionNode {
    pub rid: String,
    pub seq: Seq,
    pub kind: PointKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_raw: Option<String>,
    pub source_sst_cid: String,
    pub source_level: u32,
    /// 该版本在哪个 snapshot 序号区间内是“最新 point”。
    pub newest_point_for_snap: SeqInterval,
    /// 是否被某个 seq >= 自身的覆盖 range tombstone 遮蔽（按当前快照体系）。
    pub shadowed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqInterval {
    /// snapshot seq s 满足 from <= s < to_exclusive 时本节点最新。
    pub from: Seq,
    pub to_exclusive: Option<Seq>,
    pub basis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageNode {
    pub rid: String,
    pub seq: Seq,
    pub source_sst_cid: String,
    pub source_level: u32,
    /// [start,end] / [start,end) 端点原始表示。
    pub start_raw: String,
    pub end_raw: String,
    pub end_inclusive: bool,
    /// 作为“最新覆盖删除”生效的 snapshot 序号区间。
    pub effective_for_snap: SeqInterval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisibilityVerdict {
    pub snapshot_seq: Option<Seq>,
    pub outcome: String, // put / point_delete / range_delete / no_visible_record / ambiguous
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by_rids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covering_rid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_raw: Option<String>,
    /// 有效区间与失效原因（snapshot 坐标基准 global seq）。
    pub valid_interval: SeqInterval,
    pub invalid_reason: Option<String>,
    pub events: Vec<ReadEvent>,
    /// 语义无解时的全部合法候选（同序多类型）。
    #[serde(default)]
    pub candidates: Vec<serde_json::Value>,
}

pub struct VersionChain<'a> {
    pub nodes: Vec<VersionNode>,
    pub coverage: Vec<CoverageNode>,
    pub events: Vec<ReadEvent>,
    _files: &'a [SstFileSpec],
}

/// 汇总一个 user key 的版本链与 range 覆盖（跨全部层级文件）。
pub fn version_chain(files: &[SstFileSpec], key: &KeySpec) -> DResult<VersionChain<'_>> {
    let key_bytes = key.decode("read.key")?;
    let mut events = Vec::new();
    let mut points: Vec<(&PointRecord, &str, u32)> = Vec::new();
    let mut ranges: Vec<(&RangeTombstone, &str, u32)> = Vec::new();

    for f in files {
        let floc = format!("sst:{}/level:{}", f.cid, f.level);
        for r in &f.records {
            let kb = r.key.decode(&format!("{floc}/record/{}/key", r.rid))?;
            if kb == key_bytes {
                points.push((r, &f.cid, f.level));
            }
        }
        for t in &f.range_deletes {
            if t.covers(&key_bytes, &format!("{floc}/rangedelete/{}/end", t.rid))? {
                ranges.push((t, &f.cid, f.level));
            }
        }
    }

    // 内部键顺序：seq 降序，同 seq 类型序（delete 先，put 后）。
    points.sort_by(|a, b| {
        b.0.seq
            .cmp(&a.0.seq)
            .then_with(|| type_rank(a.0.kind).cmp(&type_rank(b.0.kind)))
    });
    ranges.sort_by(|a, b| b.0.seq.cmp(&a.0.seq));

    // 遮蔽阈值：对每个 point，最高的覆盖 range seq（无论快照，读取判定时再比较快照）。
    let mut nodes = Vec::new();
    for (i, (r, cid, level)) in points.iter().enumerate() {
        let next_seq = points.get(i + 1).map(|p| p.0.seq);
        let shadower = ranges
            .iter()
            .filter(|(t, _, _)| t.seq >= r.seq)
            .max_by_key(|(t, _, _)| t.seq)
            .map(|(t, _, _)| t.rid.clone());
        if let Some(ref s) = shadower {
            events.push(ReadEvent {
                code: "range_shadows_point".to_string(),
                subjects: vec![r.rid.clone(), s.clone()],
                detail: format!(
                    "range tombstone {s}(seq>={}) 遮蔽 point {}(seq={})",
                    t_seq(ranges.as_slice(), s),
                    r.rid,
                    r.seq
                ),
            });
        }
        nodes.push(VersionNode {
            rid: r.rid.clone(),
            seq: r.seq,
            kind: r.kind,
            value_raw: r.value.as_ref().map(|v| v.raw.clone()),
            source_sst_cid: cid.to_string(),
            source_level: *level,
            newest_point_for_snap: SeqInterval {
                from: r.seq,
                to_exclusive: next_seq,
                basis: "global_seq".to_string(),
            },
            shadowed_by: shadower,
        });
    }

    let mut coverage = Vec::new();
    for (i, (t, cid, level)) in ranges.iter().enumerate() {
        let next_seq = ranges.get(i + 1).map(|p| p.0.seq);
        coverage.push(CoverageNode {
            rid: t.rid.clone(),
            seq: t.seq,
            source_sst_cid: cid.to_string(),
            source_level: *level,
            start_raw: t.start.raw.clone(),
            end_raw: t.end.raw.clone(),
            end_inclusive: t.end_inclusive,
            effective_for_snap: SeqInterval {
                from: t.seq,
                to_exclusive: next_seq,
                basis: "global_seq".to_string(),
            },
        });
    }

    Ok(VersionChain { nodes, coverage, events, _files: &[] })
}

fn t_seq(ranges: &[(&RangeTombstone, &str, u32)], rid: &str) -> Seq {
    ranges.iter().find(|(t, _, _)| t.rid == rid).map(|(t, _, _)| t.seq).unwrap_or(0)
}

/// 读取判定：给定 snapshot（None 表示读最新）输出结构化结论。
pub fn visible_at(
    files: &[SstFileSpec],
    key: &KeySpec,
    snapshot_seq: Option<Seq>,
) -> DResult<VisibilityVerdict> {
    let chain = version_chain(files, key)?;
    let mut events = chain.events.clone();

    let bound = snapshot_seq.unwrap_or(Seq::MAX);
    if let Some(s) = snapshot_seq {
        events.push(ReadEvent {
            code: "snapshot_bound".to_string(),
            subjects: vec![format!("snapshot@{s}")],
            detail: format!("仅考虑 seq <= {s} 的 mutation（内部键降序扫描）"),
        });
    }

    let vis_points: Vec<&VersionNode> = chain.nodes.iter().filter(|n| n.seq <= bound).collect();
    let vis_ranges: Vec<&CoverageNode> = chain.coverage.iter().filter(|n| n.seq <= bound).collect();

    let top_range = vis_ranges.first();
    let newest = vis_points.first();

    let mk = |outcome: &str,
              rids: Option<Vec<String>>,
              cover: Option<String>,
              value: Option<String>,
              interval: SeqInterval,
              invalid: Option<String>,
              evts: Vec<ReadEvent>,
              cands: Vec<serde_json::Value>| VisibilityVerdict {
        snapshot_seq,
        outcome: outcome.to_string(),
        decided_by_rids: rids,
        covering_rid: cover,
        value_raw: value,
        valid_interval: interval,
        invalid_reason: invalid,
        events: evts,
        candidates: cands,
    };

    // 同序多类型歧义：同一最新 seq 上存在互斥 point 记录。
    if let Some(n) = newest {
        let same: Vec<&VersionNode> = vis_points.iter().filter(|x| x.seq == n.seq).cloned().collect();
        let kinds: std::collections::BTreeSet<u8> =
            same.iter().map(|x| type_rank(x.kind)).collect();
        if kinds.len() > 1 {
            let cands: Vec<serde_json::Value> = same
                .iter()
                .map(|x| serde_json::json!({"rid": x.rid, "seq": x.seq, "kind": x.kind, "source_sst_cid": x.source_sst_cid}))
                .collect();
            events.push(ReadEvent {
                code: "ambiguous_same_seq_multi_type".to_string(),
                subjects: same.iter().map(|x| x.rid.clone()).collect(),
                detail: format!("key={} 在 seq={} 同时存在 put 与 delete，无法唯一决定可见值", key.raw, n.seq),
            });
            return Ok(mk(
                "ambiguous",
                Some(same.iter().map(|x| x.rid.clone()).collect()),
                top_range.map(|t| t.rid.clone()),
                None,
                SeqInterval { from: n.seq, to_exclusive: None, basis: "global_seq".to_string() },
                Some("同序号存在多个互斥类型，保留全部合法候选".to_string()),
                events,
                cands,
            ));
        }
    }

    match (newest, top_range) {
        (None, None) => Ok(mk(
            "no_visible_record",
            None,
            None,
            None,
            SeqInterval { from: 0, to_exclusive: None, basis: "global_seq".to_string() },
            Some(format!("snapshot={snapshot_seq:?} 下不存在 seq <= {bound} 的 mutation")),
            {
                events.push(ReadEvent {
                    code: "no_visible_record".to_string(),
                    subjects: vec![key.raw.clone()],
                    detail: "该 snapshot 之前没有任何记录".to_string(),
                });
                events
            },
            vec![],
        )),
        (None, Some(t)) => {
            events.push(ReadEvent {
                code: "range_delete_visible_no_point".to_string(),
                subjects: vec![t.rid.clone()],
                detail: format!("snapshot 下无 point，最新覆盖为 range tombstone {}", t.rid),
            });
            Ok(mk(
                "range_delete",
                None,
                Some(t.rid.clone()),
                None,
                t.effective_for_snap.clone(),
                Some(format!("被 range tombstone {} 删除（seq={}）", t.rid, t.seq)),
                events,
                vec![],
            ))
        }
        (Some(p), rt) => {
            let range_seq = rt.map(|t| t.seq);
            if range_seq >= Some(p.seq) {
                let t = rt.unwrap();
                events.push(ReadEvent {
                    code: "range_delete_wins".to_string(),
                    subjects: vec![p.rid.clone(), t.rid.clone()],
                    detail: format!("range {} (seq={}) >= point {} (seq={})，point 被遮蔽", t.rid, t.seq, p.rid, p.seq),
                });
                Ok(mk(
                    "range_delete",
                    Some(vec![p.rid.clone()]),
                    Some(t.rid.clone()),
                    None,
                    t.effective_for_snap.clone(),
                    Some(format!("range tombstone {} 遮蔽（seq {} >= {}）", t.rid, t.seq, p.seq)),
                    events,
                    vec![],
                ))
            } else if p.kind == PointKind::Delete {
                events.push(ReadEvent {
                    code: "point_delete_visible".to_string(),
                    subjects: vec![p.rid.clone()],
                    detail: format!("point tombstone {} 是最新可见 mutation", p.rid),
                });
                Ok(mk(
                    "point_delete",
                    Some(vec![p.rid.clone()]),
                    None,
                    None,
                    p.newest_point_for_snap.clone(),
                    Some(format!("point tombstone {}（seq={}）", p.rid, p.seq)),
                    events,
                    vec![],
                ))
            } else {
                events.push(ReadEvent {
                    code: "put_visible".to_string(),
                    subjects: vec![p.rid.clone()],
                    detail: format!("put {} 是最新可见 mutation", p.rid),
                });
                Ok(mk(
                    "put",
                    Some(vec![p.rid.clone()]),
                    None,
                    p.value_raw.clone(),
                    p.newest_point_for_snap.clone(),
                    None,
                    events,
                    vec![],
                ))
            }
        }
    }
}

/// 便捷：用原始字符串键（默认 utf8）。
pub fn visible_at_raw(files: &[SstFileSpec], key: &str, snap: Option<Seq>) -> DResult<VisibilityVerdict> {
    visible_at(files, &KeySpec::new(key, KeyEncoding::Utf8), snap)
}
