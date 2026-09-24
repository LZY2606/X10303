//! 读取路径：内部键顺序、snapshot 可见性、范围删除覆盖。
//!
//! 关键判定（案例第二条分支专门审阅）：
//! 读取按内部键顺序与 snapshot 判可见性；
//! 范围删除不能越过未参与压实的旧数据而提前丢弃。

use crate::domain::model::{Entry, OpType, Seq, SstFile, Snapshot};

/// 内部键比较：user_key 升序；同键 seq 降序；同 seq 类型权重降序。
pub fn internal_order(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    a.key
        .cmp(&b.key)
        .then_with(|| b.seq.cmp(&a.seq))
        .then_with(|| b.op.cmp(&a.op))
        .then_with(|| {
            if a.entry_id == b.entry_id {
                Ordering::Equal
            } else {
                a.entry_id.cmp(&b.entry_id)
            }
        })
}

/// 一个条目是否对持有序号 `snapshot_seq` 的读者可见。
pub fn visible(entry_seq: Seq, snapshot_seq: Seq) -> bool {
    entry_seq <= snapshot_seq
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRange {
    pub name: String,
    pub seq: Seq,
    pub begin: String,
    pub end_exclusive: String,
}

/// 收集对该 snapshot 可见的范围删除（来自给定文件集合，按查找顺序排列）。
pub fn active_ranges<'a>(files: impl IntoIterator<Item = &'a SstFile>, snapshot_seq: Seq) -> Vec<ActiveRange> {
    let mut ranges = Vec::new();
    for file in files {
        for frag in &file.range_fragments {
            if visible(frag.seq, snapshot_seq) {
                ranges.push(ActiveRange {
                    name: frag.name.clone(),
                    seq: frag.seq,
                    begin: frag.begin.clone(),
                    end_exclusive: frag.end_exclusive.clone(),
                });
            }
        }
    }
    ranges
}

fn range_covers<'a>(ranges: &'a [ActiveRange], key: &str, key_seq: Seq) -> Option<&'a ActiveRange> {
    let mut hit = None;
    let mut best_seq = None;
    for range in ranges {
        if key >= range.begin.as_str() && key < range.end_exclusive.as_str() && key_seq < range.seq {
            if best_seq.is_none_or(|s| range.seq > s) {
                best_seq = Some(range.seq);
                hit = Some(range);
            }
        }
    }
    hit
}

#[derive(Debug, Clone, PartialEq)]
pub enum PointRead {
    Value { entry: Entry },
    PointTombstone { entry: Entry },
    RangeTombstone { range: ActiveRange, key_seq: Seq },
    NotFound,
}

/// point lookup：跨文件收集内部条目，按内部键顺序排序后取第一个对 snapshot 可见者，
/// 再判断是否被更新的范围删除遮蔽。
pub fn point_lookup(files: &[&SstFile], key: &str, snapshot: &Snapshot) -> PointRead {
    let ranges = active_ranges(files.iter().copied(), snapshot.seq);
    let mut candidates: Vec<&Entry> = Vec::new();
    for file in files {
        if file.contains_key(key) {
            candidates.extend(file.entries.iter().filter(|e| e.key == key && visible(e.seq, snapshot.seq)));
        }
    }
    candidates.sort_by(|a, b| internal_order(a, b));
    let point = candidates.into_iter().next();
    match point {
        Some(entry) if entry.op == OpType::Put => {
            if let Some(range) = range_covers(&ranges, key, entry.seq) {
                PointRead::RangeTombstone { range: range.clone(), key_seq: entry.seq }
            } else {
                PointRead::Value { entry: entry.clone() }
            }
        }
        Some(entry) => PointRead::PointTombstone { entry: entry.clone() },
        None => PointRead::NotFound,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScannedKey {
    pub key: String,
    pub read: PointRead,
}

/// range scan：在 [begin, end_exclusive) 内按 user_key 升序产出每个键的可见结论。
pub fn range_scan(files: &[&SstFile], begin: &str, end_exclusive: &str, snapshot: &Snapshot) -> Vec<ScannedKey> {
    let ranges = active_ranges(files.iter().copied(), snapshot.seq);
    let mut keys: Vec<String> = Vec::new();
    for file in files {
        for entry in &file.entries {
            if entry.key.as_str() >= begin
                && entry.key.as_str() < end_exclusive
                && visible(entry.seq, snapshot.seq)
                && keys.last() != Some(&entry.key)
            {
                if !keys.contains(&entry.key) {
                    keys.push(entry.key.clone());
                }
            }
        }
    }
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .map(|key| {
            let mut candidates: Vec<&Entry> = Vec::new();
            for file in files {
                if file.contains_key(&key) {
                    candidates.extend(
                        file.entries
                            .iter()
                            .filter(|e| e.key == key && visible(e.seq, snapshot.seq)),
                    );
                }
            }
            candidates.sort_by(|a, b| internal_order(a, b));
            let read = match candidates.into_iter().next() {
                Some(entry) if entry.op == OpType::Put => {
                    match range_covers(&ranges, &key, entry.seq) {
                        Some(range) => {
                            PointRead::RangeTombstone { range: range.clone(), key_seq: entry.seq }
                        }
                        None => PointRead::Value { entry: entry.clone() },
                    }
                }
                Some(entry) => PointRead::PointTombstone { entry: entry.clone() },
                None => PointRead::NotFound,
            };
            ScannedKey { key, read }
        })
        .collect()
}

/// tombstone 覆盖报告：返回某层文件集合里每个范围片段对给定 snapshot 的遮蔽判定。
#[derive(Debug, Clone, PartialEq)]
pub struct CoverageReport {
    pub fragment_id: String,
    pub name: String,
    pub seq: Seq,
    pub begin: String,
    pub end_exclusive: String,
    pub visible: bool,
    pub covered_point_entries: Vec<String>,
}

pub fn tombstone_coverage(files: &[&SstFile], snapshot: &Snapshot) -> Vec<CoverageReport> {
    let mut reports = Vec::new();
    for file in files {
        for frag in &file.range_fragments {
            let visible_frag = visible(frag.seq, snapshot.seq);
            let mut covered = Vec::new();
            if visible_frag {
                for entry in &file.entries {
                    if entry.op == OpType::Put
                        && entry.seq < frag.seq
                        && frag.begin.as_str() <= entry.key.as_str()
                        && entry.key.as_str() < frag.end_exclusive.as_str()
                    {
                        covered.push(entry.entry_id.clone());
                    }
                }
            }
            reports.push(CoverageReport {
                fragment_id: frag.fragment_id.clone(),
                name: frag.name.clone(),
                seq: frag.seq,
                begin: frag.begin.clone(),
                end_exclusive: frag.end_exclusive.clone(),
                visible: visible_frag,
                covered_point_entries: covered,
            });
        }
    }
    reports.sort_by(|a, b| {
        a.begin
            .cmp(&b.begin)
            .then_with(|| b.seq.cmp(&a.seq))
            .then_with(|| a.fragment_id.cmp(&b.fragment_id))
    });
    reports
}

/// 老 snapshot 是否存在（tombstone 下沉/保留的依据之一）。
pub fn oldest_active_snapshot(snapshots: &[Snapshot]) -> Option<&Snapshot> {
    snapshots
        .iter()
        .filter(|s| s.active)
        .min_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)))
}
