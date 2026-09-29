//! 读取语义：按内部键顺序合并多层 SSTable，依据 snapshot 判可见性；
//! 提供 key 版本链与 range tombstone 覆盖证据（供 UI 与压实共用）。

use crate::domain::*;
use serde::Serialize;

/// 一条点记录的出处（文件 + level），UI 每个显示值都靠 file_id/entry 定位。
#[derive(Debug, Clone, Serialize)]
pub struct PointOccurrence {
    pub file_id: Sid,
    pub level: u32,
    pub entry: PointEntry,
}

/// 一条范围删除的出处。
#[derive(Debug, Clone, Serialize)]
pub struct RangeOccurrence {
    pub file_id: Sid,
    pub level: u32,
    pub tomb: RangeTomb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainEntryKind {
    Put,
    Delete,
    RangeDelete,
}

/// key 版本链上的一个版本（审阅“key 版本链”页面的最小显示单元）。
#[derive(Debug, Clone, Serialize)]
pub struct ChainEntry {
    pub seq: Seq,
    pub kind: ChainEntryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub file_id: Sid,
    pub level: u32,
    /// 对给定 snapshot 是否可见（seq < snap.last_seq）。
    pub visible: bool,
    /// 该版本是否是其 (key,seq) 上类型并列中的最高优先级（同序号多类型证据）。
    pub wins_kind_tie: bool,
    pub dup_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadVerdict {
    /// 返回值。
    Value,
    /// 被点墓碑遮蔽。
    PointTombstoned,
    /// 被范围墓碑遮蔽。
    RangeTombstoned,
    /// 无记录。
    NotFound,
}

/// 一次点读的结构化结果与证据。
#[derive(Debug, Clone, Serialize)]
pub struct PointRead {
    pub key: String,
    pub snapshot_id: Option<Sid>,
    pub snapshot_last_seq: Option<Seq>,
    pub verdict: ReadVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// 生效的遮蔽记录（点/范围），含出处。
    pub decided_by: Option<ChainEntry>,
    pub chain: Vec<ChainEntry>,
}

fn all_point_occurrences(files: &[SSTable], key: &str) -> Vec<PointOccurrence> {
    let mut out = Vec::new();
    for f in files {
        if !f.covers_key(key) {
            continue;
        }
        for e in &f.points {
            if e.key == key {
                out.push(PointOccurrence { file_id: f.id.clone(), level: f.level, entry: e.clone() });
            }
        }
    }
    out
}

fn all_range_occurrences(files: &[SSTable], key: &str) -> Vec<RangeOccurrence> {
    let mut out = Vec::new();
    for f in files {
        if !f.covers_key(key) {
            continue;
        }
        for t in &f.ranges {
            if t.covers(key) {
                out.push(RangeOccurrence { file_id: f.id.clone(), level: f.level, tomb: t.clone() });
            }
        }
    }
    out
}

/// 构造某 key 的版本链，严格按内部键顺序排序。
pub fn key_chain(files: &[SSTable], key: &str) -> Vec<ChainEntry> {
    let mut occ = all_point_occurrences(files, key);
    occ.sort_by(|a, b| {
        InternalOrderKey {
            key: &a.entry.key,
            seq: a.entry.seq,
            kind_rank: point_kind_rank(a.entry.kind),
            dup_index: a.entry.dup_index,
        }
        .cmp(&InternalOrderKey {
            key: &b.entry.key,
            seq: b.entry.seq,
            kind_rank: point_kind_rank(b.entry.kind),
            dup_index: b.entry.dup_index,
        })
        .then_with(|| a.file_id.cmp(&b.file_id))
    });
    let mut chain: Vec<ChainEntry> = occ
        .iter()
        .map(|o| ChainEntry {
            seq: o.entry.seq,
            kind: match o.entry.kind {
                PointKind::Put => ChainEntryKind::Put,
                PointKind::Delete => ChainEntryKind::Delete,
            },
            value: o.entry.value.clone(),
            file_id: o.file_id.clone(),
            level: o.level,
            visible: true,
            wins_kind_tie: true,
            dup_index: o.entry.dup_index,
        })
        .collect();

    // 范围墓碑也进入版本链（RangeDelete 类型序高于点类型），作为遮蔽候选。
    let mut ranges = all_range_occurrences(files, key);
    ranges.sort_by(|a, b| {
        b.tomb
            .seq
            .cmp(&a.tomb.seq)
            .then_with(|| KIND_RANK_RANGE_DEL.cmp(&KIND_RANK_RANGE_DEL))
            .then_with(|| a.file_id.cmp(&b.file_id))
    });
    for r in ranges {
        chain.push(ChainEntry {
            seq: r.tomb.seq,
            kind: ChainEntryKind::RangeDelete,
            value: None,
            file_id: r.file_id.clone(),
            level: r.level,
            visible: true,
            wins_kind_tie: true,
            dup_index: 0,
        });
    }
    chain.sort_by(|a, b| {
        let rank_of = |k: ChainEntryKind| match k {
            ChainEntryKind::RangeDelete => KIND_RANK_RANGE_DEL,
            ChainEntryKind::Delete => KIND_RANK_DELETE,
            ChainEntryKind::Put => KIND_RANK_PUT,
        };
        b.seq
            .cmp(&a.seq)
            .then_with(|| rank_of(b.kind).cmp(&rank_of(a.kind)))
            .then_with(|| a.dup_index.cmp(&b.dup_index))
            .then_with(|| a.file_id.cmp(&b.file_id))
    });

    // 标注“同序号多类型”并列：该 seq 下类型序最高者 wins_kind_tie=true。
    let mut i = 0;
    while i < chain.len() {
        let seq = chain[i].seq;
        let mut j = i;
        let mut best_rank = 0u8;
        while j < chain.len() && chain[j].seq == seq {
            best_rank = best_rank.max(match chain[j].kind {
                ChainEntryKind::RangeDelete => KIND_RANK_RANGE_DEL,
                ChainEntryKind::Delete => KIND_RANK_DELETE,
                ChainEntryKind::Put => KIND_RANK_PUT,
            });
            j += 1;
        }
        for e in &mut chain[i..j] {
            let rank = match e.kind {
                ChainEntryKind::RangeDelete => KIND_RANK_RANGE_DEL,
                ChainEntryKind::Delete => KIND_RANK_DELETE,
                ChainEntryKind::Put => KIND_RANK_PUT,
            };
            e.wins_kind_tie = rank == best_rank;
        }
        i = j;
    }
    chain
}

/// 点读：给定 snapshot（None 表示最新视图），按内部键顺序找第一个“可见且类型并列胜出”的版本。
pub fn point_read(files: &[SSTable], snaps: &[Snapshot], key: &str, snapshot_id: Option<&Sid>) -> PointRead {
    let snap = snapshot_id.and_then(|id| snaps.iter().find(|s| &s.id == id));
    let last = snap.map(|s| s.last_seq);
    let mut chain = key_chain(files, key);
    for e in &mut chain {
        e.visible = last.map(|l| visible_to(e.seq, l)).unwrap_or(true);
    }

    let winner = chain
        .iter()
        .filter(|e| e.visible && e.wins_kind_tie)
        // 同 (seq, 类型) 多值（dup_index）属于语义无解：调用方用 ambiguous 检查；这里仍取稳定第一条。
        .next();

    let (verdict, value, decided_by) = match winner {
        None => (ReadVerdict::NotFound, None, None),
        Some(w) => match w.kind {
            ChainEntryKind::Put => (ReadVerdict::Value, w.value.clone(), Some(w.clone())),
            ChainEntryKind::Delete => (ReadVerdict::PointTombstoned, None, Some(w.clone())),
            ChainEntryKind::RangeDelete => (ReadVerdict::RangeTombstoned, None, Some(w.clone())),
        },
    };

    PointRead {
        key: key.to_string(),
        snapshot_id: snap.map(|s| s.id.clone()),
        snapshot_last_seq: last,
        verdict,
        value,
        decided_by,
        chain,
    }
}

/// 检测同 (key, seq, 类型) 上存在不同 value 的并列（无法唯一决定时返回全部合法候选）。
pub fn ambiguous_point_values(files: &[SSTable], key: &str) -> Vec<PointOccurrence> {
    let occ = all_point_occurrences(files, key);
    let mut groups: std::collections::BTreeMap<(Seq, PointKind), Vec<&PointOccurrence>> = Default::default();
    for o in &occ {
        groups.entry((o.entry.seq, o.entry.kind)).or_default().push(o);
    }
    let mut out = Vec::new();
    for (_, g) in groups {
        let distinct: std::collections::BTreeSet<&str> =
            g.iter().filter_map(|o| o.entry.value.as_deref()).collect();
        if distinct.len() > 1 {
            out.extend(g.iter().map(|o| (*o).clone()));
        }
    }
    out
}

/// tombstone 覆盖证据：某范围删除当前覆盖到哪些用户键（含端点判定）。
#[derive(Debug, Clone, Serialize)]
pub struct TombCoverageRow {
    pub tomb_file_id: Sid,
    pub level: u32,
    pub seq: Seq,
    pub start: String,
    pub end: String,
    pub end_policy: EndPolicy,
    pub covered_keys: Vec<String>,
    pub endpoint_keys: Vec<String>,
}

pub fn tombstone_coverage(files: &[SSTable]) -> Vec<TombCoverageRow> {
    let mut keys: std::collections::BTreeSet<String> = Default::default();
    let mut rows = Vec::new();
    for f in files {
        for e in &f.points {
            keys.insert(e.key.clone());
        }
    }
    for f in files {
        for t in &f.ranges {
            let mut covered = Vec::new();
            let mut endpoints = Vec::new();
            for k in &keys {
                if t.covers(k) {
                    covered.push(k.clone());
                }
                if k == &t.start || k == &t.end {
                    endpoints.push(k.clone());
                }
            }
            rows.push(TombCoverageRow {
                tomb_file_id: f.id.clone(),
                level: f.level,
                seq: t.seq,
                start: t.start.clone(),
                end: t.end.clone(),
                end_policy: t.end_policy,
                covered_keys: covered,
                endpoint_keys: endpoints,
            });
        }
    }
    rows.sort_by(|a, b| a.tomb_file_id.cmp(&b.tomb_file_id).then_with(|| a.seq.cmp(&b.seq)));
    rows
}
